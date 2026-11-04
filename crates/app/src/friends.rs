//! Friends (personal edition only): a friend pairs with this Mac over iroh (`crates/friends`) and asks questions that a
//! guest answers from the friend's shelf. This bridge starts the node on its own thread, turns its events into island
//! cards and lines, sends the user's decisions back, reads invite links from drops, and makes the invites.
//!
//! The public edition compiles the `live` module as no-ops (same names), so no node starts and no card, menu row or
//! drop is ever taken there (docs/editions.md).

#[cfg(feature = "friends")]
mod live {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, SystemTime};

    use ::friends::book::{InviteState, IssuedInvite, Peer};
    use ::friends::files::safe_file_name;
    use ::friends::guest::{AskAlways, Guest, HaikuGuest};
    use ::friends::identity::IdentityStore;
    use ::friends::invite::{self, Invite};
    use ::friends::{AskReply, Event, Network, Node, NodeConfig, PairError};
    use ui::agent_notice::{AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};
    use ui::drop::{DropItem, DropKind, Dropped};
    use ui::{FriendInvite, FriendRow, UiUpdate};

    pub const ENABLED: bool = true;

    /// The guest's model call goes out only while Agent is on (`background_gate`); with Agent off every question goes
    /// to the user, as without a key.
    #[derive(Debug)]
    pub(super) struct Gated(pub(super) Arc<dyn Guest>);
    #[cfg(test)]
    pub(crate) fn gated(inner: Arc<dyn Guest>) -> Arc<dyn Guest> { Arc::new(Gated(inner)) }
    #[async_trait::async_trait]
    impl Guest for Gated {
        async fn decide(&self, question: &str, shelf: &[::friends::shelf::ShelfDoc]) -> anyhow::Result<::friends::guest::Decision> {
            if !crate::background_gate::allowed() { return AskAlways.decide(question, shelf).await; }
            self.0.decide(question, shelf).await
        }
    }

    /// The request-id prefix of Friends' cards. The island's answers with this prefix come back to this module.
    pub const CARD_PREFIX: &str = "friends:";
    /// Image files larger than this are not read for a QR code.
    const MAX_QR_IMAGE_BYTES: u64 = 40 * 1024 * 1024;
    const LINK_PREFIX: &str = "heimdall://friend/";
    const IDENTITY_SERVICE: &str = "Heimdall friends identity";
    const DEFAULT_FRIEND: &str = "Friend";
    /// The most files an ask card offers to share (Share…), newest first.
    const SHARE_LIST_MAX: usize = 5;
    /// The longest reply the user can send, in bytes. A longer one is refused, not cut.
    const REPLY_MAX_BYTES: usize = 16 * 1024;
    /// An ask card's three answers.
    const ANSWER: &str = "Answer…";
    const SHARE: &str = "Share…";
    const DENY: &str = "Deny";
    /// The last option of the Share list: the card's three answers again.
    const BACK: &str = "Back";

    /// The node's two waits, read from its config so the cards expire when the node stops waiting.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Waits {
        /// How long a pairing waits for the user's Allow or Deny.
        pub pair_ms: u64,
        /// How long an escalated ask waits for the user's answer.
        pub ask_ms: u64,
    }

    impl Waits {
        fn of(cfg: &NodeConfig) -> Waits {
            Waits { pair_ms: cfg.pair_wait.as_millis() as u64, ask_ms: cfg.ask_wait.as_millis() as u64 }
        }
    }

    /// An escalated ask's card: what its answer needs, and the file list while Share… shows it. Kept until the card is
    /// answered, dismissed or expired, under the card's ID.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct Pending {
        pub request_id: String,
        /// The label the user gave the friend.
        pub friend: String,
        pub question: String,
        /// When the card expires (Unix ms). Kept when the card is sent again with other options.
        pub expires_ms: u64,
        /// The files Share… lists, while the card shows them.
        pub shelf: Option<Vec<String>>,
    }

    struct Bridge {
        node: Node,
        handle: tokio::runtime::Handle,
        ui: ui::Ui,
        waits: Waits,
        /// Pairing cards waiting for the user: request ID to the invite's label, which `PairDenied` names.
        pairs: Mutex<HashMap<String, String>>,
        /// Ask cards waiting for the user: card ID to what its answer needs.
        asks: Mutex<HashMap<String, Pending>>,
    }

    static BRIDGE: OnceLock<Bridge> = OnceLock::new();
    /// Set when the node failed to start. Requests then say so instead of "starting".
    static FAILED: AtomicBool = AtomicBool::new(false);
    /// Which invite panel is open. A panel's expiry timer closes the panel only while it is still the same one.
    static INVITE_GENERATION: AtomicU64 = AtomicU64::new(0);
    static LINE_ID: AtomicU64 = AtomicU64::new(0);

    /// The name friends see for this Mac: the typed name in settings, else the macOS account's full name, else the login.
    pub(crate) fn display_name(settings_file: Option<&Path>) -> String {
        let typed = settings_file
            .and_then(|p| crate::settings::Settings::load(p).ok())
            .map(|s| s.friends_name)
            .unwrap_or_default();
        choose_name(&typed, full_name().as_deref(), std::env::var("USER").ok().as_deref())
    }

    /// The first non-blank of: the typed name, the full name, the login name, "Heimdall". At most 64 characters.
    pub(crate) fn choose_name(typed: &str, full: Option<&str>, login: Option<&str>) -> String {
        // Control characters are dropped: an invite refuses them, and a name must still reach the friend.
        let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_owned();
        let name = [Some(typed), full, login].into_iter().flatten().map(clean).find(|s| !s.is_empty());
        name.unwrap_or_else(|| "Heimdall".into()).chars().take(64).collect()
    }

    /// The account's full name. macOS keeps it in the password entry's GECOS field, before the first comma.
    fn full_name() -> Option<String> {
        // SAFETY: getpwuid returns a pointer into static storage (or null), read here and not kept.
        unsafe {
            let entry = libc::getpwuid(libc::getuid());
            if entry.is_null() || (*entry).pw_gecos.is_null() {
                return None;
            }
            let gecos = std::ffi::CStr::from_ptr((*entry).pw_gecos).to_string_lossy();
            gecos.split(',').next().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty())
        }
    }

    /// Starts the node on its own thread and returns at once. Nothing waits for it; the app runs without friends if it
    /// cannot start.
    pub fn start(data: &Path, ui: ui::Ui) {
        let data = data.join("friends");
        let downloads = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Downloads");
        let spawned = std::thread::Builder::new().name("friends".into()).spawn(move || run(data, downloads, ui));
        if let Err(e) = spawned {
            eprintln!("friends: the thread did not start: {e}");
            failed(ui);
        }
    }

    fn run(data: PathBuf, downloads: PathBuf, ui: ui::Ui) {
        let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("friends-io").build() {
            Ok(runtime) => runtime,
            Err(e) => {
                eprintln!("friends: no runtime: {e}");
                failed(ui);
                return;
            }
        };
        runtime.block_on(async move {
            let identity = if agent::provider::is_installed_app() {
                IdentityStore::Keychain { service: IDENTITY_SERVICE }
            } else {
                IdentityStore::File(data.join("identity.key"))
            };
            // The guest's model call uses the user's Anthropic key; without one every question is escalated to him.
            let guest: Arc<dyn Guest> = match anthropic_key().map(HaikuGuest::new) {
                Some(Ok(guest)) => Arc::new(Gated(Arc::new(guest))),
                _ => {
                    eprintln!("friends: no Anthropic key, so every question goes to User");
                    Arc::new(AskAlways)
                }
            };
            let name = display_name(data.parent().map(|p| p.join("settings.json")).as_deref());
            let mut cfg = NodeConfig::new(data, downloads, identity, guest);
            cfg.display_name = name;
            cfg.network = Network::Public;
            let waits = Waits::of(&cfg);
            let (node, mut events) = match Node::start(cfg).await {
                Ok(started) => started,
                Err(e) => {
                    eprintln!("friends: did not start: {e:#}");
                    failed(ui);
                    return;
                }
            };
            let bridge = Bridge {
                node,
                handle: tokio::runtime::Handle::current(),
                ui,
                waits,
                pairs: Mutex::new(HashMap::new()),
                asks: Mutex::new(HashMap::new()),
            };
            let _ = BRIDGE.set(bridge);
            let bridge = BRIDGE.get().expect("just set");
            bridge.ui.send(UiUpdate::Friends(friend_rows(&bridge.node.peers())));
            while let Some(event) = events.recv().await {
                bridge.on_event(event);
            }
        });
    }

    /// The node did not start: requests say so from now on, and the user sees one line.
    fn failed(ui: ui::Ui) {
        FAILED.store(true, Ordering::SeqCst);
        ui.send(UiUpdate::AgentNotice(line("Friends did not start", NoticeKind::Error)));
    }

    /// What a request gets while the node is not up.
    fn not_ready() -> &'static str {
        if FAILED.load(Ordering::SeqCst) { "Friends did not start" } else { "Friends are starting" }
    }

    /// The user's Anthropic key through the provider module, never printed or logged. `None` when there is none.
    fn anthropic_key() -> Option<String> {
        let provider = agent::provider::provider(agent::provider::ANTHROPIC)?;
        agent::provider::api_key(provider).ok().filter(|key| !key.trim().is_empty())
    }

    fn say(text: impl Into<String>, kind: NoticeKind) {
        ui::Ui::handle().send(UiUpdate::AgentNotice(line(text, kind)));
    }

    impl Bridge {
        fn on_event(&self, event: Event) {
            let now = agent::now_ms();
            drop_expired(&mut self.asks.lock().unwrap(), now);
            let notice = notice_for(&event, now, self.waits);
            if let Some(pending) = ask_pending(&event, &notice) {
                // The record goes when its card expires, so an expired card cannot be answered from here.
                let until = Duration::from_millis(pending.expires_ms.saturating_sub(now));
                self.handle.spawn(async move {
                    tokio::time::sleep(until).await;
                    if let Some(bridge) = BRIDGE.get() {
                        drop_expired(&mut bridge.asks.lock().unwrap(), agent::now_ms());
                    }
                });
                self.asks.lock().unwrap().insert(ask_card_id(&pending.request_id), pending);
            }
            match &event {
                Event::PairRequested { request_id, label, .. } => {
                    self.pairs.lock().unwrap().insert(request_id.clone(), label.clone());
                }
                Event::PairDenied { label } => {
                    // A denied or timed-out pairing: its card (if still open) goes too.
                    let mut pairs = self.pairs.lock().unwrap();
                    pairs.retain(|request_id, pending| {
                        if pending == label {
                            self.ui.send(UiUpdate::DismissAgentNotice(pair_card_id(request_id)));
                            false
                        } else {
                            true
                        }
                    });
                }
                Event::Paired { .. } => {
                    self.ui.send(UiUpdate::Friends(friend_rows(&self.node.peers())));
                    self.ui.send(UiUpdate::FriendInvite(None));
                }
                Event::Revoked { .. } => self.ui.send(UiUpdate::Friends(friend_rows(&self.node.peers()))),
                Event::Refused { reason, .. } | Event::PairFailed { reason, .. } | Event::FileFailed { reason, .. } => {
                    eprintln!("friends: {reason}");
                }
                _ => {}
            }
            self.ui.send(UiUpdate::AgentNotice(notice));
        }
    }

    impl Bridge {
        /// The ask record of a card, while the card is open.
        fn pending(&self, card_id: &str) -> Option<Pending> {
            self.asks.lock().unwrap().get(card_id).cloned()
        }

        fn dismiss(&self, card_id: &str) {
            self.ui.send(UiUpdate::DismissAgentNotice(card_id.into()));
        }

        /// The ask card again, with the options it has now and its own expiry.
        fn send_ask(&self, pending: &Pending) {
            self.ui.send(UiUpdate::AgentNotice(ask_card(pending)));
        }

        /// The friend with this label, if still paired and not revoked.
        fn live_peer(&self, label: &str) -> Option<Peer> {
            let peers = self.node.peers();
            live_friend(&peers, label).cloned()
        }

        /// Answer…: the reply prompt opens with the question as its quote. The card goes out again with its options,
        /// because the island marked the click as the answer. Cancel then leaves the card as it was.
        fn open_reply(&self, card_id: &str) {
            let Some(pending) = self.pending(card_id) else { return self.dismiss(card_id) };
            self.send_ask(&pending);
            self.ui.send(UiUpdate::FriendReply { card_id: card_id.into(), question: pending.question });
        }

        /// Share…: the friend's newest files, with Back last. An empty shelf is a line, and the card keeps its options.
        fn show_shelf(&self, card_id: &str) {
            let Some(mut pending) = self.pending(card_id) else { return self.dismiss(card_id) };
            let Some(peer) = self.live_peer(&pending.friend) else {
                self.ui.send(UiUpdate::AgentNotice(line(format!("{} is not paired", short(&pending.friend)), NoticeKind::Warning)));
                return self.send_ask(&pending);
            };
            let files = shelf_files(&self.node.shelf_dir(&peer));
            if files.is_empty() {
                self.ui.send(UiUpdate::AgentNotice(line(format!("{}'s shelf is empty", short(&pending.friend)), NoticeKind::Info)));
                return self.send_ask(&pending);
            }
            pending.shelf = Some(files);
            self.asks.lock().unwrap().insert(card_id.into(), pending.clone());
            self.send_ask(&pending);
        }

        /// Back: the card's three answers again.
        fn show_options(&self, card_id: &str) {
            let Some(mut pending) = self.pending(card_id) else { return self.dismiss(card_id) };
            pending.shelf = None;
            self.asks.lock().unwrap().insert(card_id.into(), pending.clone());
            self.send_ask(&pending);
        }

        /// A file on the Share list: sent to the friend as the answer. A name that is no longer a file on the shelf is
        /// refused, and the list stays.
        fn share(&self, card_id: &str, name: &str) {
            let Some(pending) = self.pending(card_id) else { return self.dismiss(card_id) };
            let path = self.live_peer(&pending.friend).and_then(|peer| shelf_file(&self.node.shelf_dir(&peer), name));
            let Some(path) = path else {
                self.ui.send(UiUpdate::AgentNotice(line(format!("{} can't be shared", short(name)), NoticeKind::Warning)));
                return self.send_ask(&pending);
            };
            self.asks.lock().unwrap().remove(card_id);
            if !self.node.answer_ask(&pending.request_id, AskReply::Share(path)) {
                self.ui.send(UiUpdate::AgentNotice(line("That question is no longer open", NoticeKind::Warning)));
            }
            self.dismiss(card_id);
        }
    }

    /// The island's card or line for one node event. Pairing requests and escalated asks are cards with answers; the
    /// rest are lines of a few words, with the kind the island shows them in. A card is named by the user's label for the
    /// friend: the name the friend's own endpoint reports is not used (every Mac running this build reports "the user").
    pub(crate) fn notice_for(event: &Event, now_ms: u64, waits: Waits) -> AgentNotice {
        match event {
            Event::PairRequested { request_id, label, .. } => card(
                &pair_card_id(request_id),
                &format!("{label} wants to connect"),
                &["Allow", "Deny"],
                now_ms + waits.pair_ms,
            ),
            Event::AskEscalated { request_id, friend, question } => ask_card(&Pending {
                request_id: request_id.clone(),
                friend: friend.clone(),
                question: question.clone(),
                expires_ms: now_ms + waits.ask_ms,
                shelf: None,
            }),
            Event::Paired { label, .. } => line(format!("Paired with {label}"), NoticeKind::Success),
            Event::PairFailed { label, .. } => line(format!("{label} did not pair"), NoticeKind::Warning),
            Event::PairDenied { label } => line(format!("{label} was not allowed"), NoticeKind::Warning),
            Event::Refused { .. } => line("Refused a connection", NoticeKind::Warning),
            Event::Pinged { friend, .. } => line(format!("Pinged {friend}"), NoticeKind::Success),
            Event::AskAnsweredFromShelf { friend, .. } => line(format!("{friend}'s question answered"), NoticeKind::Success),
            Event::AskAnswered { friend, .. } => line(format!("{friend} answered"), NoticeKind::Success),
            Event::OfferReceived { friend, name, .. } => line(format!("{friend} sends {}", short(name)), NoticeKind::Info),
            Event::FileStored { friend, path, .. } => {
                let file = Path::new(path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                line(format!("Saved {} from {friend}", short(&file)), NoticeKind::Success)
            }
            Event::FileFailed { friend, .. } => line(format!("{friend}'s file failed"), NoticeKind::Error),
            Event::Revoked { friend } => line(format!("Revoked {friend}"), NoticeKind::Warning),
        }
    }

    fn pair_card_id(request_id: &str) -> String {
        format!("{CARD_PREFIX}pair:{request_id}")
    }

    fn ask_card_id(request_id: &str) -> String {
        format!("{CARD_PREFIX}ask:{request_id}")
    }

    /// The ask card as it is now: its text, its options (the three answers, or the Share list) and its own expiry.
    pub(crate) fn ask_card(pending: &Pending) -> AgentNotice {
        let options = ask_options(pending.shelf.as_deref());
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        card(&ask_card_id(&pending.request_id), &format!("{} asks: {}", pending.friend, pending.question), &refs, pending.expires_ms)
    }

    /// The options an ask card offers: the three answers, or while Share… is open, the shelf's files with Back last.
    pub(crate) fn ask_options(shelf: Option<&[String]>) -> Vec<String> {
        match shelf {
            Some(files) => files.iter().cloned().chain(std::iter::once(BACK.to_string())).collect(),
            None => vec![ANSWER.into(), SHARE.into(), DENY.into()],
        }
    }

    /// The record an escalated ask's card needs, read from its event and from the card as made (so the expiry is the
    /// card's own). `None` for any other event or card.
    pub(crate) fn ask_pending(event: &Event, card: &AgentNotice) -> Option<Pending> {
        let Event::AskEscalated { request_id, friend, question } = event else { return None };
        let Some(NoticeAction::Ask { expires_ms, .. }) = &card.action else { return None };
        Some(Pending {
            request_id: request_id.clone(),
            friend: friend.clone(),
            question: question.clone(),
            expires_ms: *expires_ms,
            shelf: None,
        })
    }

    /// Drops every ask record whose card has expired by `now_ms`.
    pub(crate) fn drop_expired(asks: &mut HashMap<String, Pending>, now_ms: u64) {
        asks.retain(|_, pending| pending.expires_ms > now_ms);
    }

    /// The live friend with this label. Labels of live friends are unique: the node refuses a second one.
    pub(crate) fn live_friend<'a>(peers: &'a [Peer], label: &str) -> Option<&'a Peer> {
        peers.iter().find(|p| p.revoked.is_none() && p.label == label)
    }

    /// The files Share… lists: up to five regular files in the shelf, newest first. Hidden names (`safe_file_name`
    /// refuses them), symlinks, folders and a file named "Back" (its label is the Back option) are skipped.
    pub(crate) fn shelf_files(shelf: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(shelf) else { return Vec::new() };
        let mut files: Vec<(SystemTime, String)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                // The entry's own type and metadata: a symlink is neither a file nor followed.
                if !entry.file_type().ok()?.is_file() {
                    return None;
                }
                let name = safe_file_name(&entry.file_name().into_string().ok()?).ok()?;
                if name == BACK {
                    return None;
                }
                Some((entry.metadata().ok()?.modified().ok()?, name))
            })
            .collect();
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        files.into_iter().take(SHARE_LIST_MAX).map(|(_, name)| name).collect()
    }

    /// The path Share… sends for `name`: the name must pass `safe_file_name`, and it must name a regular file in
    /// `shelf`, not a symlink. `None` otherwise.
    pub(crate) fn shelf_file(shelf: &Path, name: &str) -> Option<PathBuf> {
        let name = safe_file_name(name).ok()?;
        let path = shelf.join(name);
        std::fs::symlink_metadata(&path).ok()?.file_type().is_file().then_some(path)
    }

    /// Why a typed reply is not sent.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ReplyError {
        Empty,
        TooLong,
    }

    /// The user's typed reply, trimmed. Refused when it is empty, or longer than 16 KiB.
    pub(crate) fn reply_text(typed: &str) -> Result<String, ReplyError> {
        let text = typed.trim();
        if text.is_empty() {
            return Err(ReplyError::Empty);
        }
        if text.len() > REPLY_MAX_BYTES {
            return Err(ReplyError::TooLong);
        }
        Ok(text.to_owned())
    }

    fn card(id: &str, text: &str, options: &[&str], expires_ms: u64) -> AgentNotice {
        AgentNotice {
            id: id.into(),
            text: text.into(),
            kind: NoticeKind::Info,
            action: Some(NoticeAction::Ask {
                request_id: id.into(),
                questions: vec![AskQuestion {
                    id: "q1".into(),
                    header: None,
                    text: text.into(),
                    multi_select: false,
                    allow_other: false, field: None,
                    options: options.iter().map(|o| AskOption::label(o)).collect(),
                }],
                expires_ms,
                countdown: false,
            }),
            outcome: None,
            expires_ms: None,
            detail: None,
        }
    }

    /// A plain island line. Each gets its own id, so two lines in a row are two lines.
    fn line(text: impl Into<String>, kind: NoticeKind) -> AgentNotice {
        AgentNotice::line(format!("friends-line-{}", LINE_ID.fetch_add(1, Ordering::Relaxed)), text, kind)
    }

    /// A name shortened for a line of a few words.
    fn short(name: &str) -> String {
        const MAX: usize = 32;
        if name.chars().count() <= MAX {
            return name.to_string();
        }
        let head: String = name.chars().take(MAX - 1).collect();
        format!("{head}…")
    }

    /// The Friends submenu's rows: every friend the node knows, revoked ones included.
    pub(crate) fn friend_rows(peers: &[Peer]) -> Vec<FriendRow> {
        peers
            .iter()
            .map(|p| FriendRow { id: p.id.clone(), label: p.label.clone(), revoked: p.revoked.is_some() })
            .collect()
    }

    /// The user's answer on a card: what the node should do with it, or what the card shows next.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Decision {
        Pair { request_id: String, allow: bool },
        /// Deny: the friend is refused.
        Deny { request_id: String },
        /// Answer…: the reply prompt opens.
        Answer,
        /// Share…: the friend's shelf is listed.
        Shelf,
        /// Back: the card's three answers return.
        Back,
        /// A file on the Share list.
        File { name: String },
    }

    /// The decision a card's button makes. `shelf` is the file list the card shows (Share… is open), else `None`.
    /// `None` for a card that is not Friends' or a button it does not offer.
    pub(crate) fn decision(card_id: &str, choice: &str, shelf: Option<&[String]>) -> Option<Decision> {
        let rest = card_id.strip_prefix(CARD_PREFIX)?;
        if let Some(request_id) = rest.strip_prefix("pair:") {
            let allow = match choice {
                "Allow" => true,
                "Deny" => false,
                _ => return None,
            };
            return Some(Decision::Pair { request_id: request_id.into(), allow });
        }
        let request_id = rest.strip_prefix("ask:")?;
        match shelf {
            Some(files) => match choice {
                BACK => Some(Decision::Back),
                name if files.iter().any(|file| file == name) => Some(Decision::File { name: name.into() }),
                _ => None,
            },
            None => match choice {
                ANSWER => Some(Decision::Answer),
                SHARE => Some(Decision::Shelf),
                DENY => Some(Decision::Deny { request_id: request_id.into() }),
                _ => None,
            },
        }
    }

    /// A card's button, from the island. Answer…, Share… and Back change the card and it goes out again; the other
    /// choices decide on the node, and the card goes.
    pub fn answer(card_id: &str, choice: &str) {
        let Some(bridge) = BRIDGE.get() else { return };
        let shelf = bridge.pending(card_id).and_then(|pending| pending.shelf);
        match decision(card_id, choice, shelf.as_deref()) {
            Some(Decision::Pair { request_id, allow }) => {
                bridge.pairs.lock().unwrap().remove(&request_id);
                bridge.node.decide_pairing(&request_id, allow);
                bridge.dismiss(card_id);
            }
            Some(Decision::Deny { request_id }) => {
                bridge.asks.lock().unwrap().remove(card_id);
                bridge.node.answer_ask(&request_id, AskReply::Deny);
                bridge.dismiss(card_id);
            }
            Some(Decision::Answer) => bridge.open_reply(card_id),
            Some(Decision::Shelf) => bridge.show_shelf(card_id),
            Some(Decision::Back) => bridge.show_options(card_id),
            Some(Decision::File { name }) => bridge.share(card_id, &name),
            None => {
                eprintln!("friends: no card answers {choice:?}");
                bridge.dismiss(card_id);
            }
        }
    }

    /// Answer…: the user's typed reply, sent to the friend as the answer, and the card goes. An empty reply does nothing;
    /// a reply over 16 KiB is refused with a line, and the card stays open.
    pub fn reply(card_id: &str, typed: &str) {
        let Some(bridge) = BRIDGE.get() else { return };
        let text = match reply_text(typed) {
            Ok(text) => text,
            Err(ReplyError::Empty) => return,
            Err(ReplyError::TooLong) => {
                say("A reply is at most 16 KiB", NoticeKind::Warning);
                return;
            }
        };
        let Some(pending) = bridge.asks.lock().unwrap().remove(card_id) else { return bridge.dismiss(card_id) };
        if !bridge.node.answer_ask(&pending.request_id, AskReply::Answer(text)) {
            bridge.ui.send(UiUpdate::AgentNotice(line("That question is no longer open", NoticeKind::Warning)));
        }
        bridge.dismiss(card_id);
    }

    /// Whether an island answer belongs to a Friends card (so the agent path must not take it).
    pub fn owns(card_id: &str) -> bool {
        card_id.starts_with(CARD_PREFIX)
    }

    /// Whether `name` is taken: a live friend has it, or an invite for it is still open (waiting, or pairing) and has
    /// not expired. Two invites never share a name.
    pub(crate) fn name_taken(name: &str, peers: &[Peer], issued: &[IssuedInvite], now: u64) -> bool {
        peers.iter().any(|p| p.revoked.is_none() && p.label == name)
            || issued
                .iter()
                .any(|i| i.label == name && matches!(i.state, InviteState::Open | InviteState::Pairing) && i.expires > now)
    }

    /// The first name, "Friend", "Friend 2", …, that is not taken.
    pub(crate) fn free_name(peers: &[Peer], issued: &[IssuedInvite], now: u64) -> String {
        (1..)
            .map(|n| if n == 1 { DEFAULT_FRIEND.to_string() } else { format!("{DEFAULT_FRIEND} {n}") })
            .find(|name| !name_taken(name, peers, issued, now))
            .expect("an unbounded search")
    }

    /// Friends ▸ Invite a friend…: the user's name prompt opens with the first free name in its field.
    pub fn ask_name() {
        let Some(bridge) = BRIDGE.get() else {
            say(not_ready(), NoticeKind::Info);
            return;
        };
        let suggested = free_name(&bridge.node.peers(), &bridge.node.issued_invites(), invite::now_unix());
        bridge.ui.send(UiUpdate::FriendName(suggested));
    }

    /// The invite for the name the user typed: a QR code in the invite panel until it expires or pairs. The QR code is
    /// drawn from memory; nothing is written to disk.
    pub fn invite(typed: &str) {
        let Some(bridge) = BRIDGE.get() else {
            say(not_ready(), NoticeKind::Info);
            return;
        };
        let name = match invite::clean_name(typed) {
            Ok(name) => name,
            Err(e) => {
                say(e.to_string(), NoticeKind::Warning);
                return;
            }
        };
        if name_taken(&name, &bridge.node.peers(), &bridge.node.issued_invites(), invite::now_unix()) {
            say(format!("{name} is already taken"), NoticeKind::Warning);
            return;
        }
        let issued = match bridge.node.create_invite(&name) {
            Ok(issued) => issued,
            Err(e) => {
                eprintln!("friends: invite failed: {e:#}");
                say("Could not invite", NoticeKind::Error);
                return;
            }
        };
        let png = match invite::qr_png(&issued.link()) {
            Ok(png) => png,
            Err(e) => {
                eprintln!("friends: QR code failed: {e:#}");
                say("Could not make the invite", NoticeKind::Error);
                return;
            }
        };
        let generation = INVITE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        bridge.ui.send(UiUpdate::FriendInvite(Some(FriendInvite { label: name, png })));
        // The panel closes when the invite expires, unless a newer invite has opened since.
        let until = issued.expires.saturating_sub(invite::now_unix());
        let ui = bridge.ui;
        bridge.handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(until)).await;
            if INVITE_GENERATION.load(Ordering::SeqCst) == generation {
                ui.send(UiUpdate::FriendInvite(None));
            }
        });
    }

    /// Revoke a friend by endpoint ID (Friends ▸ a friend ▸ Revoke).
    pub fn revoke(friend_id: &str) {
        let Some(bridge) = BRIDGE.get() else { return };
        if let Err(e) = bridge.node.revoke(friend_id) {
            eprintln!("friends: revoke failed: {e:#}");
        }
    }

    /// What the drop path does with one dropped item.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Slot {
        /// An invite link (text or a URL), trimmed: redeemed.
        Link(String),
        /// An image file that is here on this Mac: its QR code is read, off the live loop.
        Scan,
        /// Anything else: the existing drop path, unchanged.
        Keep,
    }

    /// The slot of each item, in order. `local` says whether an image's bytes are on this Mac (not iCloud-only).
    pub(crate) fn plan(items: &[DropItem], local: &dyn Fn(&Path) -> bool) -> Vec<Slot> {
        items
            .iter()
            .map(|item| {
                if let Some(link) = link_of(item) {
                    return Slot::Link(link);
                }
                match (item.kind, item.path.as_deref()) {
                    (DropKind::Image, Some(path)) if local(path) => Slot::Scan,
                    _ => Slot::Keep,
                }
            })
            .collect()
    }

    /// The longest side of an image that is read for a QR code on a drop. A 2880x1800 screenshot reads in about 35 ms and
    /// a 5K one in about 95 ms on this Mac (lab/friends/qr_bench, release build), so the larger ones are not read.
    pub(crate) const MAX_SCAN_SIDE: u32 = 2880;
    /// The name of the invite PNG the user drags out of the island. Always read, whatever its size.
    const INVITE_FILE_PREFIX: &str = "heimdall-invite-";

    /// Whether a dropped image is worth reading for a QR code: it is named as an invite, or its longest side is at most
    /// `MAX_SCAN_SIDE`. An unreadable header means no.
    pub(crate) fn worth_scanning(path: &Path) -> bool {
        let named = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(INVITE_FILE_PREFIX));
        if named {
            return true;
        }
        let dims = image::ImageReader::open(path).ok().and_then(|r| r.with_guessed_format().ok()).and_then(|r| r.into_dimensions().ok());
        dims.is_some_and(|(w, h)| w.max(h) <= MAX_SCAN_SIDE)
    }

    /// The invite link in a dropped text or URL: one that starts with the invite prefix, trimmed.
    fn link_of(item: &DropItem) -> Option<String> {
        [item.url.as_deref(), item.text.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|text| text.starts_with(LINK_PREFIX))
            .map(str::to_owned)
    }

    /// A drop on the notch. Invite links are redeemed, and an image's QR code is read. Returns the drop to take on the
    /// spot, unchanged, when nothing needs reading (every other drop), or when only links need redeeming. Returns None
    /// when an image must be read first: then what is left of the drop goes to `forward` from the reading thread, and
    /// the caller takes it from there.
    pub fn intake(d: Dropped, forward: impl FnOnce(Dropped) + Send + 'static) -> Option<Dropped> {
        let slots = plan(&d.items, &|path: &Path| !crate::drops::dataless(path) && worth_scanning(path));
        if slots.iter().all(|slot| *slot == Slot::Keep) {
            return Some(d);
        }
        if !slots.iter().any(|slot| *slot == Slot::Scan) {
            let rest = settle(d.items, slots);
            return remaining(rest, d.at_ms);
        }
        let backup = d.clone();
        let at_ms = d.at_ms;
        let spawned = std::thread::Builder::new().name("friends-scan".into()).spawn(move || {
            let rest = settle(d.items, slots);
            if let Some(rest) = remaining(rest, at_ms) {
                forward(rest);
            }
        });
        match spawned {
            Ok(_) => None,
            Err(e) => {
                // The drop is never lost: without a reading thread it takes the existing path as it came.
                eprintln!("friends: the QR reader did not start: {e}");
                Some(backup)
            }
        }
    }

    fn remaining(items: Vec<DropItem>, at_ms: u64) -> Option<Dropped> {
        (!items.is_empty()).then_some(Dropped { items, at_ms })
    }

    /// Each item's job, in order: a link is redeemed; an image is read for its QR code (on the calling thread); anything
    /// else is kept. A link that cannot be redeemed yet is kept with a line, so the drop is never discarded. Returns
    /// what is left for the drop path.
    fn settle(items: Vec<DropItem>, slots: Vec<Slot>) -> Vec<DropItem> {
        let mut rest = Vec::new();
        for (item, slot) in items.into_iter().zip(slots) {
            let link = match slot {
                Slot::Keep => {
                    rest.push(item);
                    continue;
                }
                Slot::Link(link) => Some(link),
                Slot::Scan => item.path.as_deref().and_then(read_qr_file),
            };
            match (link, BRIDGE.get()) {
                (Some(link), Some(bridge)) => redeem(bridge, link),
                (Some(_), None) => {
                    say(not_ready(), NoticeKind::Info);
                    rest.push(item);
                }
                (None, _) => rest.push(item),
            }
        }
        rest
    }

    /// The QR code of an image file, when it is an invite link (trimmed). Refuses images too large to read.
    pub(crate) fn read_qr_file(path: &Path) -> Option<String> {
        if std::fs::metadata(path).ok()?.len() > MAX_QR_IMAGE_BYTES {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let text = invite::read_qr_image(&bytes).ok()?.trim().to_string();
        Invite::parse(&text).ok().map(|_| text)
    }

    fn redeem(bridge: &Bridge, link: String) {
        if let Ok(invite) = Invite::parse(&link) {
            bridge.ui.send(UiUpdate::AgentNotice(line(format!("Pairing with {}…", invite.name), NoticeKind::Info)));
        }
        let node = bridge.node.clone();
        let ui = bridge.ui;
        bridge.handle.spawn(async move {
            if let Err(e) = node.redeem(&link).await {
                if let Some((text, kind)) = redeem_failure(&e) {
                    ui.send(UiUpdate::AgentNotice(line(text, kind)));
                }
            }
        });
    }

    /// The line for a failed redeem. A denial is already a line (the node emits `PairDenied`), so it has none here.
    pub(crate) fn redeem_failure(e: &PairError) -> Option<(&'static str, NoticeKind)> {
        use ::friends::InviteError as I;
        Some(match e {
            PairError::Denied => return None,
            PairError::Invite(I::Expired) => ("Invite expired", NoticeKind::Warning),
            PairError::Invite(I::Used) => ("Invite already used", NoticeKind::Warning),
            PairError::Invite(I::Malformed) => ("Not a Heimdall invite", NoticeKind::Warning),
            PairError::Invite(I::Unknown) => ("Invite not known here", NoticeKind::Warning),
            PairError::WrongSecret => ("Invite did not match", NoticeKind::Error),
            PairError::Refused(_) => ("Pairing refused", NoticeKind::Warning),
            PairError::TimedOut => ("Pairing timed out", NoticeKind::Warning),
            PairError::Transport(_) => ("Could not reach the friend", NoticeKind::Error),
        })
    }
}

#[cfg(all(test, feature = "friends"))]
#[path = "friends_tests.rs"]
mod tests;

#[cfg(not(feature = "friends"))]
mod live {
    //! Without the `friends` feature (the public edition): nothing starts, and no card, menu row or drop is taken.

    pub const ENABLED: bool = false;

    pub fn start(_data: &std::path::Path, _ui: ui::Ui) {}
    pub fn ask_name() {}
    pub fn invite(_typed: &str) {}
    pub fn revoke(_friend_id: &str) {}
    pub fn answer(_card_id: &str, _choice: &str) {}
    pub fn reply(_card_id: &str, _typed: &str) {}
    pub fn owns(_card_id: &str) -> bool {
        false
    }
    pub fn intake(d: ui::drop::Dropped, _forward: impl FnOnce(ui::drop::Dropped) + Send + 'static) -> Option<ui::drop::Dropped> {
        Some(d)
    }
}

pub use live::*;
