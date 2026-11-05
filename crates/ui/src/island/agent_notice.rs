//! Agent notices and the island's actionable ones: a pending question with its own answers (two to four), and
//! Connect T3. `Notices` is the platform-neutral state of the island's question card; `mac/notices.rs` draws it.
//! A plain notice (no action) is an island line: an animated icon for its kind and one to three words, with an
//! optional pull-down `detail` (`mac/island/`, `mac/detail.rs`).

/// What an island line is about: its animated icon and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
    Error,
    /// Heimdall is looking at something (a screenshot, the screen): the eye.
    Looking,
    /// The agent started on a request: stays as a small glyph until the result replaces it or the turn ends.
    Working,
}

impl NoticeKind {
    /// The contract's kind names (`island_notice.kind`); anything else is information.
    pub fn parse(kind: &str) -> NoticeKind {
        match kind {
            "success" => NoticeKind::Success,
            "warning" => NoticeKind::Warning,
            "error" => NoticeKind::Error,
            "looking" => NoticeKind::Looking,
            "working" => NoticeKind::Working,
            _ => NoticeKind::Info,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            NoticeKind::Info => "info",
            NoticeKind::Success => "success",
            NoticeKind::Warning => "warning",
            NoticeKind::Error => "error",
            NoticeKind::Looking => "looking",
            NoticeKind::Working => "working",
        }
    }
}

/// An island line's pull-down content (`island_notice.detail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    Text(String),
    Table { headers: Vec<String>, rows: Vec<Vec<String>> },
    /// An SVG document, drawn on the panel's black.
    Svg(String),
    /// An archived image (a screen frame, an attachment), fetched through the attachment loader.
    Image { archive_id: String },
}

/// One option of an `ask` question: its label (the answer text), an optional one-line description, and what the user's
/// click on it approves for the deny-list hook (`grant`, drawn in full under the option; empty for most options).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub grant: Vec<GrantItem>,
}

/// One thing an option's click approves (the app's `heimdall_emergency::grants::Scope`), as the card writes it out:
/// whole, never cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantItem {
    /// Deleting exactly this path (`~/` for the home folder).
    Delete(String),
    /// Removing the login item of exactly this name.
    LoginItem(String),
    /// Making and filling exactly this one new keychain file (bug note 2026-11-04T23-01-22; `~/` for the home folder).
    Keychain(String),
}

/// One question of an `ask` card. `id` is the agent's question id ("q1".."q4"), the key of its answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskQuestion {
    pub id: String,
    /// The step chip's word (one to twelve characters), when the card has tabs.
    pub header: Option<String>,
    pub text: String,
    /// The user may pick several options (a box per option) rather than one (a dot).
    pub multi_select: bool,
    /// The Other row: the user may type his own answer.
    pub allow_other: bool,
    /// The Other row as a field of its own (the voice cards' name field), when `allow_other`.
    pub field: Option<OtherField>,
    pub options: Vec<AskOption>,
}

/// An Other row with its own placeholder. `focus_until_ms` (Unix ms, 0 for never): it takes the keyboard once, when it
/// first shows before then; only for a card that answers the user's own click a moment ago, never one he did not ask for
/// or one that waited (behind another card, tucked away), as it would take his typing in another app. The voice cards
/// always come on their own and send 0.
/// A question with such a field is a naming card: its options before the field are one-click
/// chips in one row, the field follows, and the last `after` options come after it as quiet buttons ("Not important").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OtherField {
    pub placeholder: String,
    pub focus_until_ms: u64,
    pub after: usize,
}

impl AskOption {
    pub fn label(label: &str) -> Self {
        Self { label: label.into(), description: None, grant: vec![] }
    }
}

impl AskQuestion {
    /// One single-select question with plain labels, no header, Other allowed.
    pub fn plain(id: &str, text: &str, labels: &[&str]) -> Self {
        Self { id: id.into(), header: None, text: text.into(), multi_select: false, allow_other: true, field: None, options: labels.iter().map(|l| AskOption::label(l)).collect() }
    }
}

impl NoticeAction {
    /// A card of one single-select question (`AskQuestion::plain`, id "q1") with plain labels.
    pub fn single(request_id: &str, text: &str, labels: &[&str], expires_ms: u64) -> NoticeAction {
        NoticeAction::Ask { request_id: request_id.into(), questions: vec![AskQuestion::plain("q1", text, labels)], expires_ms, countdown: false }
    }
}

/// A countdown's time left as m:ss (`0:47`, `1:00`), rounded up to the whole second, so `0:00` shows only once the time
/// has run out; never negative.
pub fn countdown_text(left_ms: u64) -> String {
    let secs = left_ms.div_ceil(1000);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The longest Other answer the island keeps. The agent refuses more (crates/agent/src/actions.rs `OTHER_CHARS`), and a
/// refused answer drops the card, so the text is cut here and the user's card still answers.
pub const OTHER_CHARS: usize = 500;

/// The user's answer to one question: the option labels he picked (one unless multi-select), and his typed Other text.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
    pub other: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    /// A card of one to four questions (`AskQuestion`), each with two to four options. Silence is never an answer.
    /// `countdown`: the card shows the time left until `expires_ms` (m:ss and a ring that empties, once a second while
    /// it is on screen). Such a card is the controller's to close or replace: at `expires_ms` it shows `0:00` and stays
    /// open rather than expiring on its own (the quota recovery card).
    Ask { request_id: String, questions: Vec<AskQuestion>, expires_ms: u64, countdown: bool },
    /// Only a click may open the T3 OAuth approval page.
    ConnectT3,
}
/// Typed outcomes replace/dismiss the notice with the same id. Text remains user-facing copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeOutcome { T3Connected, T3ConnectionFailed, T3NeedsAgent, Answered { choice: String }, QuestionUnavailable }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentNotice {
    pub id: String,
    pub text: String,
    pub kind: NoticeKind,
    pub action: Option<NoticeAction>,
    pub outcome: Option<NoticeOutcome>,
    /// T3 credential expiry in Unix milliseconds; None when no credential exists.
    pub expires_ms: Option<u64>,
    /// Pull-down content for an island line.
    pub detail: Option<Detail>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentNoticeAction {
    /// The user's card is complete: one answer per question (a click on a single-question card, or Send). The app commits it.
    /// `by_hand`: of those picks, the options his own pointer clicked (`click_source`), when the answer itself was sent by
    /// such a click (the pick that answers a one-question card, or Send); empty otherwise. Only these write a grant.
    Answer { request_id: String, answers: Vec<QuestionAnswer>, by_hand: Vec<QuestionAnswer> },
    /// The user dismissed an open question (the x or Esc): the card closes and the agent is told it was dismissed.
    Dismiss { request_id: String },
    /// One question's pick changed while the card is still incomplete (the app keeps it for a voice answer).
    Pick { request_id: String, answer: QuestionAnswer },
    ConnectT3,
    /// Send the clicked notice id for an outcome addressed to that card.
    ConnectT3Notice { id: String },
}

impl AgentNotice {
    /// An island line: `kind`'s icon and `text`.
    pub fn line(id: impl Into<String>, text: impl Into<String>, kind: NoticeKind) -> Self {
        Self { id: id.into(), text: text.into(), kind, action: None, outcome: None, expires_ms: None, detail: None }
    }
    pub fn outcome(id: String, outcome: NoticeOutcome, text: String) -> Self {
        let kind = match outcome {
            NoticeOutcome::T3Connected | NoticeOutcome::Answered { .. } => NoticeKind::Success,
            _ => NoticeKind::Warning,
        };
        Self { id, text, kind, action: None, outcome: Some(outcome), expires_ms: None, detail: None }
    }
    /// A plain line for the island (no card action, no outcome of a card's click).
    pub fn is_line(&self) -> bool {
        self.action.is_none() && self.outcome.is_none()
    }
}

/// The island renders each question with its own answers (`mac/notices.rs`), clicks reach the app through the
/// buttons' target/action and accessibility press, and an expired or unavailable question shows no answer buttons.
pub const ACTIONS_RENDERED: bool = true;

/// A T3 thread's question card's request id starts so (`islands::binding::card_id` in the app).
pub const T3_CARD_PREFIX: &str = "t3:";

/// Compatibility health labels for the current card. New consumers use AgentNotice.id/outcome.
pub const T3_CONNECTED: &str = "T3 connected";
pub const T3_CONNECT_FAILED: &str = "T3 connection failed; click Connect T3 to retry";
pub const QUESTION_UNAVAILABLE: &str = "Question unavailable; ask again";
/// Connect T3 clicked while Agent is off (the app's own label): the button comes back.
pub const T3_NEEDS_AGENT: &str = "Turn Agent on to connect T3";

/// Where an actionable notice is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the user.
    Open,
    /// The card is complete and its answers are on their way. Every control is off, so a second send cannot answer again.
    Answered,
    /// The agent closed it after the user answered by voice: the card shows `answer`, then closes.
    Closed { answer: String },
    /// Connect T3 clicked; the approval page is opening.
    Connecting,
    /// Past its expiry: no answer controls, only Dismiss.
    Expired,
    /// The backend could not take the answer (Agent off, the question retired): only Dismiss.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub notice: AgentNotice,
    pub phase: Phase,
    /// The question tab on screen: an index into the card's questions.
    pub tab: usize,
    /// The user's picks so far, one entry per question (empty until he picks).
    pub picks: Vec<QuestionAnswer>,
    /// When the card arrived, or its expiry last changed (wall-clock ms): a countdown's full span starts here.
    pub since_ms: u64,
    /// Per question, the picked option labels a real pointer click chose (`Event::HardwareClick`); a key, his voice or
    /// typed Other drops them.
    pub hand: Vec<Vec<String>>,
}

/// A countdown card's time left: milliseconds until expiry (0 once it has passed) and the share of its span still
/// left (1 on arrival, 0 at expiry), in thousandths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Countdown {
    pub left_ms: u64,
    pub left_milli: u16,
}

impl Countdown {
    pub fn text(&self) -> String {
        countdown_text(self.left_ms)
    }
}

/// The questions of a card's action (none for a connection notice or a plain line).
fn questions_of(action: &Option<NoticeAction>) -> &[AskQuestion] {
    match action {
        Some(NoticeAction::Ask { questions, .. }) => questions,
        _ => &[],
    }
}

fn expiry_of(action: &Option<NoticeAction>) -> Option<u64> {
    match action {
        Some(NoticeAction::Ask { expires_ms, .. }) => Some(*expires_ms),
        _ => None,
    }
}

/// A question is answered by one or more options, or by its Other text.
fn answered(p: &QuestionAnswer) -> bool {
    !p.labels.is_empty() || p.other.is_some()
}

/// Whether card `id` with these picks can be sent: something answered and, on a T3 card, no question left that takes
/// only its options (`Item::sendable`; the layout lights Send by the same rule).
pub fn can_send(id: &str, questions: &[AskQuestion], picks: &[QuestionAnswer]) -> bool {
    picks.iter().any(answered)
        && (!id.starts_with(T3_CARD_PREFIX) || questions.iter().enumerate().all(|(k, q)| q.allow_other || picks.get(k).is_some_and(answered)))
}

/// The next question after `from` (wrapping) that has no answer yet.
fn next_open(picks: &[QuestionAnswer], from: usize) -> Option<usize> {
    (1..picks.len()).map(|k| (from + k) % picks.len()).find(|&i| !answered(&picks[i]))
}

impl Item {
    pub fn request_id(&self) -> Option<&str> {
        match &self.notice.action {
            Some(NoticeAction::Ask { request_id, .. }) => Some(request_id),
            _ => None,
        }
    }
    /// The card's questions, in the agent's order.
    pub fn questions(&self) -> &[AskQuestion] {
        questions_of(&self.notice.action)
    }
    pub fn is_connect(&self) -> bool {
        self.notice.action == Some(NoticeAction::ConnectT3)
    }
    /// The card's countdown at `now_ms`; None unless it is an ask card with `countdown`.
    pub fn countdown(&self, now_ms: u64) -> Option<Countdown> {
        let Some(NoticeAction::Ask { expires_ms, countdown: true, .. }) = &self.notice.action else { return None };
        let left_ms = expires_ms.saturating_sub(now_ms);
        let span = expires_ms.saturating_sub(self.since_ms).max(1);
        // Whole seconds, as the text shows them, so the ring and the numerals step together.
        let shown = left_ms.div_ceil(1000) * 1000;
        let left_milli = ((shown.min(span) as u128 * 1000) / span as u128) as u16;
        Some(Countdown { left_ms, left_milli })
    }
    /// A countdown card: its expiry is the controller's, never the clock's.
    pub fn counts_down(&self) -> bool {
        matches!(self.notice.action, Some(NoticeAction::Ask { countdown: true, .. }))
    }
    /// The card's text: a question in full; a connection notice as what happened.
    pub fn title(&self) -> String {
        if self.is_connect() {
            return if self.notice.id.starts_with("t3-connect-expiry") {
                "T3 connection expires soon".into()
            } else {
                "T3 is disconnected".into()
            };
        }
        self.notice.text.trim().to_string()
    }
    /// Every question has an answer: Send can commit the card.
    pub fn complete(&self) -> bool {
        let n = self.questions().len();
        n > 0 && self.picks.len() == n && self.picks.iter().all(answered)
    }
    /// Something is answered: Send can commit the card, a question without a pick going as unanswered (bug note
    /// 2026-11-02T16-27-21).
    /// A T3 card (`T3_CARD_PREFIX`) cannot leave a question that takes only its options (T3 needs an answer for every
    /// question, and only words can say "unanswered"), so it sends only once those are answered.
    pub fn sendable(&self) -> bool {
        self.request_id().is_some_and(|r| can_send(r, self.questions(), &self.picks))
    }
    /// Whether a picked option carries a grant.
    pub fn grant_picked(&self) -> bool {
        self.questions().iter().zip(&self.picks).any(|(q, p)| q.options.iter().any(|o| !o.grant.is_empty() && p.labels.contains(&o.label)))
    }
    /// The picks his own pointer clicked, per question, that are still picked (questions with none left out).
    pub fn by_hand(&self) -> Vec<QuestionAnswer> {
        self.picks.iter().zip(&self.hand).filter_map(|(p, h)| {
            let labels: Vec<String> = p.labels.iter().filter(|l| h.contains(l)).cloned().collect();
            (!labels.is_empty()).then(|| QuestionAnswer { question_id: p.question_id.clone(), labels, other: None })
        }).collect()
    }
    /// The user's picks across the card (options and Other texts): the count on Send.
    pub fn pick_count(&self) -> usize {
        self.picks.iter().map(|p| p.labels.len() + usize::from(p.other.is_some())).sum()
    }
}

/// The island's actionable notices, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Notices {
    pub items: Vec<Item>,
}

impl Notices {
    /// Replace by id, or add. Only notices with an action are kept; plain ones are the health pill's and the Agent
    /// window's. A card re-sent with the same questions keeps the user's picks and tab. Returns whether the card changed.
    pub fn upsert(&mut self, notice: AgentNotice, now_ms: u64) -> bool {
        if notice.action.is_none() {
            return false;
        }
        let phase = match &notice.action {
            Some(NoticeAction::Ask { expires_ms, countdown: false, .. }) if *expires_ms <= now_ms => Phase::Expired,
            _ => Phase::Open,
        };
        let fresh = questions_of(&notice.action).iter().map(|q| QuestionAnswer { question_id: q.id.clone(), ..Default::default() }).collect::<Vec<_>>();
        match self.items.iter().position(|i| i.notice.id == notice.id) {
            Some(i) if self.items[i].notice == notice && self.items[i].phase == phase => false,
            Some(i) => {
                let old = &self.items[i];
                let n = fresh.len();
                let (tab, picks, hand) = if old.questions() == questions_of(&notice.action) { (old.tab, old.picks.clone(), old.hand.clone()) } else { (0, fresh, vec![vec![]; n]) };
                // A countdown keeps its span while the controller re-sends the card; a new expiry starts a new one.
                let since_ms = if expiry_of(&old.notice.action) == expiry_of(&notice.action) { old.since_ms } else { now_ms };
                self.items[i] = Item { notice, phase, tab, picks, since_ms, hand };
                true
            }
            None => {
                let hand = vec![vec![]; fresh.len()];
                self.items.push(Item { notice, phase, tab: 0, picks: fresh, since_ms: now_ms, hand });
                true
            }
        }
    }

    /// The backend retired this request (answered); other questions stay.
    pub fn dismiss(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.notice.id != id && i.request_id() != Some(id));
        self.items.len() != before
    }

    /// A new stop epoch, a reload or shutdown: every actionable control goes.
    pub fn clear(&mut self) -> bool {
        let had = !self.items.is_empty();
        self.items.clear();
        had
    }

    /// The user picked option `option` of question `q` on card `id`. A card of one single-select question answers at once:
    /// the returned action is the answer, sent once. Otherwise the pick is kept (a single-select pick moves to the next
    /// open question; a multi-select pick toggles its option) and the returned action is the pick, for the app.
    /// `None` for an unknown, expired or answered card, or an option it does not offer.
    pub fn pick(&mut self, id: &str, q: usize, option: usize, now_ms: u64) -> Option<AgentNoticeAction> {
        self.pick_by(id, q, option, now_ms, false)
    }

    /// `pick`, saying whether the user's own pointer made it (`by_hand`, `Event::HardwareClick`). An option with a grant
    /// takes only such a pick (review 4 Nov evening): a key, an in-process tap or a posted event picks nothing there, so
    /// the card never answers yes to a grant that was not clicked.
    pub fn pick_by(&mut self, id: &str, q: usize, option: usize, now_ms: u64, by_hand: bool) -> Option<AgentNoticeAction> {
        self.expire(now_ms);
        let item = self.items.iter_mut().find(|i| i.notice.id == id)?;
        if item.phase != Phase::Open {
            return None;
        }
        let Some(NoticeAction::Ask { request_id, questions, .. }) = &item.notice.action else { return None };
        let question = questions.get(q)?;
        let chosen = question.options.get(option)?;
        if !by_hand && !chosen.grant.is_empty() {
            return None;
        }
        let label = chosen.label.clone();
        let request_id = request_id.clone();
        item.hand.resize(questions.len(), vec![]);
        if questions.len() == 1 && !question.multi_select {
            item.picks[0] = QuestionAnswer { question_id: question.id.clone(), labels: vec![label.clone()], other: None };
            item.hand[0] = if by_hand { vec![label] } else { vec![] };
            item.phase = Phase::Answered;
            let by_hand = item.by_hand();
            return Some(AgentNoticeAction::Answer { request_id, answers: item.picks.clone(), by_hand });
        }
        let pick = &mut item.picks[q];
        let hand = &mut item.hand[q];
        hand.retain(|l| *l != label);
        if question.multi_select {
            match pick.labels.iter().position(|l| *l == label) {
                Some(at) => {
                    pick.labels.remove(at);
                }
                None => {
                    if by_hand {
                        hand.push(label.clone());
                    }
                    pick.labels.push(label)
                }
            }
            pick.labels.sort_by_key(|l| question.options.iter().position(|o| o.label == *l));
        } else {
            *hand = if by_hand { vec![label.clone()] } else { vec![] };
            pick.labels = vec![label];
            pick.other = None;
            item.tab = next_open(&item.picks, q).unwrap_or(q);
        }
        Some(AgentNoticeAction::Pick { request_id, answer: item.picks[q].clone() })
    }

    /// The user typed `text` as question `q`'s Other answer (empty clears it). Single-select questions drop their option
    /// pick; the returned action is the pick, for the app. `None` when nothing changed or Other is not offered.
    pub fn other(&mut self, id: &str, q: usize, text: &str, now_ms: u64) -> Option<AgentNoticeAction> {
        self.expire(now_ms);
        let item = self.items.iter_mut().find(|i| i.notice.id == id)?;
        if item.phase != Phase::Open {
            return None;
        }
        let Some(NoticeAction::Ask { request_id, questions, .. }) = &item.notice.action else { return None };
        let question = questions.get(q)?;
        if !question.allow_other {
            return None;
        }
        let text: String = text.trim().chars().take(OTHER_CHARS).collect();
        let other = (!text.is_empty()).then_some(text);
        let pick = &mut item.picks[q];
        if pick.other == other {
            return None;
        }
        pick.other = other;
        if !question.multi_select && pick.other.is_some() {
            pick.labels.clear();
            if let Some(hand) = item.hand.get_mut(q) {
                hand.clear();
            }
        }
        let request_id = request_id.clone();
        Some(AgentNoticeAction::Pick { request_id, answer: item.picks[q].clone() })
    }

    /// Show question `q` of card `id` on its tab. Whether it changed (an open card only).
    pub fn tab(&mut self, id: &str, q: usize) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| i.notice.id == id) else { return false };
        if item.phase != Phase::Open || q >= item.questions().len() || item.tab == q {
            return false;
        }
        item.tab = q;
        true
    }

    /// Send on card `id`, once something is answered: the answer to send, once. A question without a pick goes with an
    /// empty answer, which the app delivers as unanswered (bug note 2026-11-02T16-27-21).
    pub fn send(&mut self, id: &str, now_ms: u64) -> Option<AgentNoticeAction> {
        self.send_by(id, now_ms, false)
    }

    /// `send`, saying whether the user's own pointer clicked Send (`by_hand`): only then do the options he clicked go as
    /// `by_hand` (Return, or a key, sends them as plain picks that grant nothing).
    pub fn send_by(&mut self, id: &str, now_ms: u64, by_hand: bool) -> Option<AgentNoticeAction> {
        self.expire(now_ms);
        let item = self.items.iter_mut().find(|i| i.notice.id == id)?;
        if item.phase != Phase::Open || !item.sendable() {
            return None;
        }
        // A card holding a picked grant sends only by his own click on Send (never Return or a key).
        if !by_hand && item.grant_picked() {
            return None;
        }
        let request_id = item.request_id()?.to_string();
        item.phase = Phase::Answered;
        let by_hand = if by_hand { item.by_hand() } else { vec![] };
        Some(AgentNoticeAction::Answer { request_id, answers: item.picks.clone(), by_hand })
    }

    /// Return in card `id`'s Other field: on a card with a question still open, the next open question comes up (his
    /// words are one answer, not the whole card); otherwise the card sends. Whether the tab moved; the send, if any.
    pub fn returned(&mut self, id: &str, now_ms: u64) -> (bool, Option<AgentNoticeAction>) {
        self.expire(now_ms);
        let Some(item) = self.items.iter_mut().find(|i| i.notice.id == id) else { return (false, None) };
        if item.phase == Phase::Open && !item.complete() {
            if let Some(next) = next_open(&item.picks, item.tab) {
                item.tab = next;
                return (true, None);
            }
        }
        (false, self.send(id, now_ms))
    }

    /// Voice answered one question of card `id` (the app's `AskSet`): the card shows that pick and moves to the next
    /// open question. Nothing is sent; the app commits the card when it is complete. Whether it changed.
    pub fn set(&mut self, id: &str, answer: &QuestionAnswer, now_ms: u64) -> bool {
        self.expire(now_ms);
        let Some(item) = self.items.iter_mut().find(|i| i.notice.id == id) else { return false };
        if item.phase != Phase::Open {
            return false;
        }
        let Some(q) = item.questions().iter().position(|x| x.id == answer.question_id) else { return false };
        let question = &item.questions()[q];
        // His voice never picks an option with a grant (only his click does).
        let labels: Vec<String> = answer.labels.iter().filter(|l| question.options.iter().any(|o| o.label == **l && o.grant.is_empty())).cloned().collect();
        let other = answer.other.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned);
        if labels.is_empty() && other.is_none() {
            return false;
        }
        let allow_other = question.allow_other;
        item.picks[q] = QuestionAnswer { question_id: answer.question_id.clone(), labels, other: if allow_other { other } else { None } };
        // His voice set this question: none of its picks is a click any more.
        if let Some(hand) = item.hand.get_mut(q) {
            hand.clear();
        }
        item.tab = next_open(&item.picks, q).unwrap_or(q);
        true
    }

    /// The agent closed question `id` with the user's spoken `answer`. Whether it changed (an open question only).
    pub fn close_spoken(&mut self, id: &str, answer: &str) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| i.notice.id == id || i.request_id() == Some(id)) else { return false };
        if !matches!(item.phase, Phase::Open | Phase::Expired) {
            return false;
        }
        item.phase = Phase::Closed { answer: answer.trim().into() };
        true
    }

    /// The option a spoken answer names ("yes" for "Yes"), when it names exactly one.
    pub fn option_named<'a>(labels: &'a [String], answer: &str) -> Option<&'a str> {
        let a = answer.trim().trim_end_matches(['.', '!']).to_lowercase();
        let mut found = labels.iter().filter(|o| o.to_lowercase() == a);
        let first = found.next()?;
        found.next().is_none().then_some(first.as_str())
    }

    /// Connect T3 on notice `id`: the action to send, once while it is opening.
    pub fn connect(&mut self, id: &str) -> Option<AgentNoticeAction> {
        let item = self.items.iter_mut().find(|i| i.notice.id == id && i.is_connect())?;
        if item.phase != Phase::Open {
            return None;
        }
        item.phase = Phase::Connecting;
        Some(AgentNoticeAction::ConnectT3)
    }

    /// The user dismissed an open question (the x or Esc): it leaves the island and the app tells the agent "dismissed",
    /// as it would an answer. Silence is still never an answer; only his dismiss closes it early.
    pub fn dismiss_open(&mut self, id: &str) -> Option<AgentNoticeAction> {
        let item = self.items.iter().find(|i| i.notice.id == id && i.phase == Phase::Open)?;
        let request_id = item.request_id()?.to_string();
        self.dismiss(id);
        Some(AgentNoticeAction::Dismiss { request_id })
    }

    /// The user's own Dismiss (an expired or unavailable question) or Later (a connection notice).
    pub fn close(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| !(i.notice.id == id && (i.is_connect() || matches!(i.phase, Phase::Expired | Phase::Unavailable))));
        self.items.len() != before
    }

    /// Questions past their expiry lose their controls. Returns whether any changed.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            if let Some(NoticeAction::Ask { expires_ms, countdown: false, .. }) = &item.notice.action {
                if *expires_ms <= now_ms && matches!(item.phase, Phase::Open) {
                    item.phase = Phase::Expired;
                    changed = true;
                }
            }
        }
        changed
    }

    /// The next open question's expiry (one one-shot timer; none when nothing can expire).
    pub fn next_expiry(&self) -> Option<u64> {
        self.items
            .iter()
            .filter(|i| i.phase == Phase::Open)
            .filter_map(|i| match &i.notice.action {
                Some(NoticeAction::Ask { expires_ms, countdown: false, .. }) => Some(*expires_ms),
                _ => None,
            })
            .min()
    }

    /// A backend island notice that settles a click: a connection result, or an answer the backend could not take.
    pub fn health(&mut self, label: &str) -> bool {
        let mut changed = false;
        if label == T3_CONNECTED {
            let before = self.items.len();
            self.items.retain(|i| !i.is_connect());
            changed = self.items.len() != before;
        } else if label == T3_CONNECT_FAILED || label == T3_NEEDS_AGENT {
            for item in self.items.iter_mut().filter(|i| i.phase == Phase::Connecting) {
                item.phase = Phase::Open;
                changed = true;
            }
        } else if label == QUESTION_UNAVAILABLE {
            for item in self.items.iter_mut().filter(|i| i.phase == Phase::Answered) {
                item.phase = Phase::Unavailable;
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(label: &str) -> AskOption {
        AskOption { label: label.into(), description: None, grant: vec![] }
    }
    fn item(id: &str, header: Option<&str>, multi: bool, labels: &[&str]) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            header: header.map(str::to_owned),
            text: format!("{id}?"),
            multi_select: multi,
            allow_other: true, field: None,
            options: labels.iter().map(|l| opt(l)).collect(),
        }
    }
    fn card(id: &str, questions: Vec<AskQuestion>, expires_ms: u64) -> AgentNotice {
        AgentNotice {
            action: Some(NoticeAction::Ask { request_id: id.into(), questions, expires_ms, countdown: false }),
            ..AgentNotice::line(id, "Pick one", NoticeKind::Info)
        }
    }
    fn one(id: &str, labels: &[&str], expires_ms: u64) -> AgentNotice {
        card(id, vec![item("q1", None, false, labels)], expires_ms)
    }
    fn answer(id: &str, labels: &[&str]) -> QuestionAnswer {
        QuestionAnswer { question_id: id.into(), labels: labels.iter().map(|l| l.to_string()).collect(), other: None }
    }

    #[test]
    fn a_single_question_click_answers_at_once_and_only_once() {
        let mut n = Notices::default();
        n.upsert(one("q1", &["Yes", "No"], 1_000_000), 0);
        assert_eq!(n.pick("q1", 0, 1, 1), Some(AgentNoticeAction::Answer { request_id: "q1".into(), answers: vec![answer("q1", &["No"])], by_hand: vec![] }));
        assert_eq!(n.pick("q1", 0, 0, 2), None, "a second click never answers again");
        assert_eq!(n.items[0].phase, Phase::Answered);
    }

    #[test]
    fn a_click_on_an_option_the_card_does_not_offer_answers_nothing() {
        let mut n = Notices::default();
        n.upsert(one("q1", &["Yes", "No"], 1_000_000), 0);
        assert_eq!(n.pick("q1", 0, 2, 1), None, "only offered options answer");
        assert_eq!(n.pick("q1", 3, 0, 1), None, "only offered questions");
        assert_eq!(n.pick("nope", 0, 0, 1), None);
        assert_eq!(n.items[0].phase, Phase::Open);
    }

    #[test]
    fn multi_select_toggles_options_and_only_send_commits() {
        let mut n = Notices::default();
        let q = card("m", vec![item("q1", Some("Branches"), true, &["resampler-old", "resampler-v2", "meter-fix", "stash"])], 1_000_000);
        n.upsert(q, 0);
        assert_eq!(n.pick("m", 0, 2, 1), Some(AgentNoticeAction::Pick { request_id: "m".into(), answer: answer("q1", &["meter-fix"]) }));
        n.pick("m", 0, 0, 2);
        assert_eq!(n.items[0].picks[0].labels, vec!["resampler-old", "meter-fix"], "labels keep option order");
        n.pick("m", 0, 2, 3);
        assert_eq!(n.items[0].picks[0].labels, vec!["resampler-old"], "a second click on an option unticks it");
        assert_eq!(n.send("m", 4), Some(AgentNoticeAction::Answer { request_id: "m".into(), answers: vec![answer("q1", &["resampler-old"])], by_hand: vec![] }));
        assert_eq!(n.send("m", 5), None, "one send");
    }

    /// Bug note 2026-11-02T16-27-21: Send goes with what is answered; an open question goes empty, as unanswered.
    #[test]
    fn send_goes_once_anything_is_answered_with_the_open_questions_empty() {
        let mut n = Notices::default();
        n.upsert(card("t", vec![item("q1", Some("Branches"), false, &["A1", "B1"]), item("q2", Some("Push"), false, &["Yes", "No"])], 1_000_000), 0);
        assert!(!n.items[0].sendable());
        assert_eq!(n.send("t", 1), None, "nothing picked");
        assert_eq!(n.pick("t", 0, 1, 2), Some(AgentNoticeAction::Pick { request_id: "t".into(), answer: answer("q1", &["B1"]) }));
        assert_eq!(n.items[0].tab, 1, "a single-select pick moves to the next open question");
        assert!(n.items[0].sendable() && !n.items[0].complete());
        let sent = n.send("t", 3).expect("a partly answered card sends");
        assert_eq!(sent, AgentNoticeAction::Answer { request_id: "t".into(), answers: vec![answer("q1", &["B1"]), answer("q2", &[])], by_hand: vec![] });
        assert_eq!(n.send("t", 4), None, "one send");
    }

    /// A T3 card sends with a question left only when that question takes words (T3 answers it "(unanswered)").
    #[test]
    fn a_t3_card_waits_for_its_option_only_questions() {
        let mut n = Notices::default();
        let mut strict = item("q1", None, false, &["A1", "B1"]);
        strict.allow_other = false;
        n.upsert(card("t3:job:r1", vec![strict, item("q2", None, false, &["Yes", "No"])], 1_000_000), 0);
        n.pick("t3:job:r1", 1, 0, 1);
        assert!(!n.items[0].sendable(), "q1 takes only its options");
        assert_eq!(n.send("t3:job:r1", 2), None);
        n.pick("t3:job:r1", 0, 1, 3);
        assert!(n.items[0].sendable());
        let mut m = Notices::default();
        let mut strict = item("q1", None, false, &["A1", "B1"]);
        strict.allow_other = false;
        m.upsert(card("t3:job:r2", vec![strict, item("q2", None, false, &["Yes", "No"])], 1_000_000), 0);
        m.pick("t3:job:r2", 0, 0, 1);
        assert!(m.items[0].sendable(), "q2 takes words, so it can be left");
    }

    /// Return in the Other field: the next open question while one is left, then the send.
    #[test]
    fn return_brings_up_the_next_open_question_before_it_sends() {
        let mut n = Notices::default();
        n.upsert(card("r", vec![item("q1", None, false, &["A1", "B1"]), item("q2", None, false, &["Yes", "No"]), item("q3", None, false, &["X", "Y"])], 1_000_000), 0);
        n.other("r", 0, "mine", 1);
        assert_eq!(n.returned("r", 2), (true, None));
        assert_eq!(n.items[0].tab, 1);
        n.pick("r", 1, 0, 3);
        assert_eq!(n.items[0].tab, 2, "the pick moved on to q3");
        let (moved, sent) = n.returned("r", 4);
        assert!(!moved && sent.is_some(), "q3 is the last open one: Return on it sends, q3 unanswered");
    }

    #[test]
    fn other_text_is_an_answer_and_clears_the_single_select_pick() {
        let mut n = Notices::default();
        n.upsert(card("o", vec![item("q1", None, false, &["Yes", "No"]), item("q2", None, false, &["A", "B"])], 1_000_000), 0);
        n.pick("o", 0, 0, 1);
        let changed = n.other("o", 0, "  keep the old one  ", 2);
        assert_eq!(changed, Some(AgentNoticeAction::Pick { request_id: "o".into(), answer: QuestionAnswer { question_id: "q1".into(), labels: vec![], other: Some("keep the old one".into()) } }));
        assert_eq!(n.other("o", 0, "keep the old one", 3), None, "unchanged text is not a new pick");
        n.pick("o", 1, 1, 4);
        assert!(n.items[0].complete());
        assert_eq!(n.items[0].pick_count(), 2, "the Other text and the option pick");
        assert_eq!(n.other("o", 0, "", 5), Some(AgentNoticeAction::Pick { request_id: "o".into(), answer: answer("q1", &[]) }), "empty clears Other");
        assert!(!n.items[0].complete(), "q1 is unanswered again");
    }

    #[test]
    fn other_text_is_cut_to_the_agent_limit() {
        let mut n = Notices::default();
        n.upsert(card("long", vec![item("q1", None, false, &["Yes", "No"])], 1_000_000), 0);
        let Some(AgentNoticeAction::Pick { answer, .. }) = n.other("long", 0, &"é".repeat(600), 1) else { panic!("no pick") };
        assert_eq!(answer.other.map(|t| t.chars().count()), Some(OTHER_CHARS), "600 characters are cut to the agent's 500");
    }

    #[test]
    fn other_is_refused_when_the_question_does_not_offer_it() {
        let mut n = Notices::default();
        let mut q = item("q1", None, false, &["Yes", "No"]);
        q.allow_other = false;
        n.upsert(card("x", vec![q], 1_000_000), 0);
        assert_eq!(n.other("x", 0, "maybe", 1), None);
        assert!(n.items[0].picks[0].other.is_none());
    }

    #[test]
    fn tabs_move_only_on_an_open_card_and_only_to_a_real_question() {
        let mut n = Notices::default();
        n.upsert(card("t", vec![item("q1", Some("A"), false, &["x", "y"]), item("q2", Some("B"), false, &["x", "y"])], 1_000_000), 0);
        assert!(n.tab("t", 1));
        assert!(!n.tab("t", 1), "already there");
        assert!(!n.tab("t", 2), "no third question");
        n.pick("t", 1, 0, 1);
        n.pick("t", 0, 1, 2);
        assert_eq!(n.items[0].phase, Phase::Open, "a complete card waits for Send");
        assert!(n.tab("t", 1), "a complete card still lets User look at its tabs");
    }

    #[test]
    fn a_voice_pick_sets_one_question_and_moves_on_without_sending() {
        let mut n = Notices::default();
        n.upsert(card("v", vec![item("q1", None, false, &["A1", "B1"]), item("q2", None, false, &["Y", "N"])], 1_000_000), 0);
        assert!(n.set("v", &answer("q1", &["B1"]), 1));
        assert_eq!(n.items[0].picks[0].labels, vec!["B1"]);
        assert_eq!(n.items[0].tab, 1, "moves to the next open question");
        assert_eq!(n.items[0].phase, Phase::Open, "a voice pick never sends");
        assert!(!n.set("v", &answer("q1", &["Nonsense"]), 2), "a label the card does not offer sets nothing");
        assert!(!n.set("v", &answer("q9", &["Y"]), 2), "an unknown question sets nothing");
    }

    #[test]
    fn expiry_takes_the_controls_and_refuses_every_pick() {
        let mut n = Notices::default();
        n.upsert(card("e", vec![item("q1", None, false, &["Yes", "No"])], 100), 0);
        assert_eq!(n.pick("e", 0, 0, 100), None, "expired at its time");
        assert_eq!(n.items[0].phase, Phase::Expired);
        assert_eq!(n.other("e", 0, "later", 101), None);
        assert_eq!(n.send("e", 101), None);
        assert_eq!(n.next_expiry(), None);
        n.upsert(one("late", &["Yes"], 5), 10);
        assert_eq!(n.items[1].phase, Phase::Expired, "a question that arrives expired");
    }

    #[test]
    fn a_spoken_answer_closes_an_open_question_and_names_its_option() {
        let mut n = Notices::default();
        n.upsert(one("q", &["Yes", "No"], 1_000_000), 0);
        assert!(n.close_spoken("q", "yes."));
        assert_eq!(n.items[0].phase, Phase::Closed { answer: "yes.".into() });
        assert_eq!(n.pick("q", 0, 0, 1), None, "a closed question takes no click");
        assert_eq!(Notices::option_named(&["Yes".to_string(), "No".to_string()], "yes."), Some("Yes"));
        assert_eq!(Notices::option_named(&["Yes".to_string(), "No".to_string()], "after lunch"), None);
    }

    #[test]
    fn a_card_re_sent_with_the_same_questions_keeps_the_picks_and_the_tab() {
        let mut n = Notices::default();
        let first = card("r", vec![item("q1", None, false, &["A1", "B1"]), item("q2", None, false, &["Y", "N"])], 1_000_000);
        n.upsert(first.clone(), 0);
        n.pick("r", 0, 1, 1);
        assert!(!n.upsert(first, 2), "the same card changes nothing");
        assert_eq!(n.items[0].picks[0].labels, vec!["B1"]);
        assert_eq!(n.items[0].tab, 1);
    }

    #[test]
    fn replace_by_id_expiry_unavailable_and_clear() {
        let mut n = Notices::default();
        n.upsert(one("q", &["Yes", "No"], 100), 0);
        n.upsert(one("u", &["Yes", "No"], 1000), 10);
        assert_eq!(n.next_expiry(), Some(100));
        assert!(n.expire(100));
        assert_eq!(n.items[0].phase, Phase::Expired);
        assert!(n.close("q"));
        assert!(n.pick("u", 0, 0, 11).is_some());
        assert!(n.health(QUESTION_UNAVAILABLE));
        assert_eq!(n.items[0].phase, Phase::Unavailable);
        assert!(!n.upsert(AgentNotice::line("p", "Worker update", NoticeKind::Info), 0), "plain notices are not the card's");
        assert!(n.clear());
        assert!(n.items.is_empty());
    }

    #[test]
    fn connect_t3_is_one_click_and_settles_on_the_result() {
        let mut n = Notices::default();
        let connect = |id: &str| AgentNotice { action: Some(NoticeAction::ConnectT3), ..AgentNotice::line(id, "Connect T3 · click", NoticeKind::Info) };
        n.upsert(connect("t3-connect-expiry:k"), 0);
        assert_eq!(n.items[0].title(), "T3 connection expires soon");
        assert_eq!(n.connect("t3-connect-expiry:k"), Some(AgentNoticeAction::ConnectT3));
        assert_eq!(n.connect("t3-connect-expiry:k"), None);
        assert!(n.health(T3_CONNECT_FAILED));
        assert_eq!(n.items[0].phase, Phase::Open, "a failed connection offers the button again");
        assert!(n.health(T3_CONNECTED));
        assert!(n.items.is_empty());
    }

    #[test]
    fn kinds_parse_from_the_contract() {
        assert_eq!(NoticeKind::parse("success"), NoticeKind::Success);
        assert_eq!(NoticeKind::parse("looking"), NoticeKind::Looking);
        assert_eq!(NoticeKind::parse("nonsense"), NoticeKind::Info);
    }
}
