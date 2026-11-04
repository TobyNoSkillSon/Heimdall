//! Friends' bridge: event to card or line, the user's answers, the friend rows and names, and which drops are invites.
//! Nothing here starts a node or redeems a link; a redeem is the node's (crates/friends tests).

use super::live::{
    ask_card, ask_options, ask_pending, choose_name, decision, drop_expired, free_name, friend_rows, intake, live_friend, name_taken, notice_for,
    owns, plan, read_qr_file, redeem_failure, reply_text, shelf_file, shelf_files, worth_scanning, Decision, Pending, ReplyError, Slot, Waits,
    MAX_SCAN_SIDE,
};
use ::friends::book::{InviteState, IssuedInvite, Peer};
use ::friends::identity::IdentityStore;
use ::friends::invite::{self, Invite};
use ::friends::{Event, PairError};
use std::collections::HashMap;
use ui::agent_notice::{AgentNotice, NoticeAction, NoticeKind};
use ui::drop::{DropItem, DropKind, Dropped};

const NOW_MS: u64 = 1_800_000_000_000;
const NOW_S: u64 = NOW_MS / 1000;
/// The node's own waits (`NodeConfig::new`): five minutes for a pairing, fifteen for an ask.
const WAITS: Waits = Waits { pair_ms: 5 * 60 * 1000, ask_ms: 15 * 60 * 1000 };

fn always_local(_: &std::path::Path) -> bool {
    true
}

fn never_local(_: &std::path::Path) -> bool {
    false
}

fn peer(label: &str, revoked: bool) -> Peer {
    Peer {
        id: format!("id-{label}"),
        label: label.into(),
        shelf: label.to_lowercase(),
        paired: 1,
        revoked: revoked.then_some(2),
    }
}

fn issued(label: &str, state: InviteState, expires: u64) -> IssuedInvite {
    IssuedInvite { id: format!("inv-{label}"), label: label.into(), secret: String::new(), expires, state, reason: None }
}

fn item(kind: DropKind, path: Option<std::path::PathBuf>, url: Option<&str>, text: Option<&str>) -> DropItem {
    DropItem { kind, path, url: url.map(str::to_owned), text: text.map(str::to_owned) }
}

fn fresh_link() -> String {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = ::friends::identity::load_or_create(&IdentityStore::File(dir.path().join("id.key"))).unwrap().public();
    invite::new_invite(endpoint, "User", invite::INVITE_TTL).unwrap().link()
}

/// The options an ask card's action offers, and its request ID.
fn ask_parts(card: &AgentNotice) -> (String, Vec<String>, u64) {
    match &card.action {
        Some(NoticeAction::Ask { request_id, questions, expires_ms, .. }) => {
            let options = questions.first().map(|q| q.options.iter().map(|o| o.label.clone()).collect()).unwrap_or_default();
            (request_id.clone(), options, *expires_ms)
        }
        other => panic!("not a question card: {other:?}"),
    }
}

#[test]
fn a_pairing_request_is_an_allow_or_deny_card_named_by_owner_label() {
    let event = Event::PairRequested {
        request_id: "r1".into(),
        label: "Morgan".into(),
        friend_id: "endpoint".into(),
        // The friend's endpoint reports its display name, which is "the user" for every Mac running this build.
        friend_name: "User".into(),
    };
    let card = notice_for(&event, NOW_MS, WAITS);
    assert_eq!(card.text, "Morgan wants to connect", "the card shows User's label for the friend");
    assert_eq!(card.id, "friends:pair:r1");
    let (request_id, options, expires_ms) = ask_parts(&card);
    assert_eq!(request_id, "friends:pair:r1", "the card's request ID is its own ID");
    assert_eq!(options, ["Allow", "Deny"]);
    assert_eq!(expires_ms, NOW_MS + WAITS.pair_ms, "the card expires when the node stops waiting");
    assert_eq!(decision(&request_id, "Allow", None), Some(Decision::Pair { request_id: "r1".into(), allow: true }));
    assert_eq!(decision(&request_id, "Deny", None), Some(Decision::Pair { request_id: "r1".into(), allow: false }));
}

#[test]
fn an_escalated_ask_is_an_answer_share_deny_card_carrying_its_request_id() {
    let event = Event::AskEscalated { request_id: "a7".into(), friend: "Morgan".into(), question: "Is User free Friday?".into() };
    let card = notice_for(&event, NOW_MS, WAITS);
    assert_eq!(card.text, "Morgan asks: Is User free Friday?");
    let (request_id, options, expires_ms) = ask_parts(&card);
    assert_eq!(request_id, "friends:ask:a7");
    assert_eq!(options, ["Answer…", "Share…", "Deny"]);
    assert_eq!(expires_ms, NOW_MS + WAITS.ask_ms, "an ask waits as long as the node does");
    assert_eq!(decision(&request_id, "Answer…", None), Some(Decision::Answer));
    assert_eq!(decision(&request_id, "Share…", None), Some(Decision::Shelf));
    assert_eq!(decision(&request_id, "Deny", None), Some(Decision::Deny { request_id: "a7".into() }));
    assert_eq!(decision(&request_id, "Yes", None), None, "Yes and No are no longer on the card");
    assert_eq!(decision(&request_id, "No", None), None);
}

#[test]
fn a_share_list_answers_its_files_and_back_and_nothing_else() {
    let files: Vec<String> = ["plan.pdf", "Deny", "notes.md"].map(String::from).to_vec();
    let id = "friends:ask:a7";
    assert_eq!(decision(id, "plan.pdf", Some(&files)), Some(Decision::File { name: "plan.pdf".into() }));
    assert_eq!(decision(id, "Deny", Some(&files)), Some(Decision::File { name: "Deny".into() }), "a file named like an answer is a file on the list");
    assert_eq!(decision(id, "Back", Some(&files)), Some(Decision::Back));
    assert_eq!(decision(id, "Answer…", Some(&files)), None, "the three answers are not on the list");
    assert_eq!(decision(id, "other.txt", Some(&files)), None, "a name not on the list decides nothing");
}

#[test]
fn the_card_offers_the_three_answers_or_the_shelf_with_back_last() {
    assert_eq!(ask_options(None), ["Answer…", "Share…", "Deny"]);
    let files = vec!["a.pdf".to_string(), "b.md".to_string()];
    assert_eq!(ask_options(Some(&files)), ["a.pdf", "b.md", "Back"]);
}

/// A shelf folder with `names` in it, each a minute older than the one before: `names[0]` is the newest.
fn shelf_with(names: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now();
    for (i, name) in names.iter().enumerate() {
        let path = dir.path().join(name);
        std::fs::write(&path, b"x").unwrap();
        let when = now - std::time::Duration::from_secs(60 * i as u64);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
    }
    dir
}

#[test]
fn the_share_list_is_the_five_newest_regular_files_newest_first() {
    let shelf = shelf_with(&["f1.txt", "f2.txt", "f3.txt", "f4.txt", "f5.txt", "f6.txt", "f7.txt"]);
    assert_eq!(shelf_files(shelf.path()), ["f1.txt", "f2.txt", "f3.txt", "f4.txt", "f5.txt"], "newest first, five at most");
}

#[test]
fn the_share_list_skips_hidden_names_symlinks_folders_and_a_file_called_back() {
    let shelf = shelf_with(&["visible.txt", ".hidden.txt", "Back", "old.txt"]);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.txt");
    std::fs::write(&target, b"secret").unwrap();
    // The newest entry of all, but a symlink.
    std::os::unix::fs::symlink(&target, shelf.path().join("link.txt")).unwrap();
    std::fs::create_dir(shelf.path().join("folder")).unwrap();
    assert_eq!(shelf_files(shelf.path()), ["visible.txt", "old.txt"]);
}

#[test]
fn an_empty_or_missing_shelf_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(shelf_files(dir.path()).is_empty());
    assert!(shelf_files(&dir.path().join("never-made")).is_empty());
}

#[test]
fn a_share_path_is_refused_unless_it_names_a_regular_file_of_the_shelf() {
    let root = tempfile::tempdir().unwrap();
    let shelf = root.path().join("shelf");
    std::fs::create_dir(&shelf).unwrap();
    std::fs::write(shelf.join("plan.pdf"), b"plan").unwrap();
    std::fs::write(root.path().join("secret.txt"), b"private").unwrap();
    std::os::unix::fs::symlink(root.path().join("secret.txt"), shelf.join("link.txt")).unwrap();
    std::fs::create_dir(shelf.join("folder")).unwrap();

    assert_eq!(shelf_file(&shelf, "plan.pdf"), Some(shelf.join("plan.pdf")));
    assert_eq!(shelf_file(&shelf, "../secret.txt"), None, "a name cannot climb out of the shelf");
    assert_eq!(shelf_file(&shelf, &root.path().join("secret.txt").display().to_string()), None, "nor name a path");
    assert_eq!(shelf_file(&shelf, "link.txt"), None, "a symlink is never sent, even one to a file");
    assert_eq!(shelf_file(&shelf, "folder"), None, "a folder is not a file");
    assert_eq!(shelf_file(&shelf, ".env"), None, "a hidden name is refused");
    assert_eq!(shelf_file(&shelf, "missing.pdf"), None);
}

#[test]
fn an_empty_or_oversized_reply_is_refused_and_the_rest_is_trimmed() {
    assert_eq!(reply_text("  Yes, Friday  "), Ok("Yes, Friday".to_string()));
    assert_eq!(reply_text(" \n\t "), Err(ReplyError::Empty));
    let longest = "a".repeat(16 * 1024);
    assert_eq!(reply_text(&longest).map(|text| text.len()), Ok(16 * 1024), "16 KiB is the most");
    assert_eq!(reply_text(&format!("{longest}a")), Err(ReplyError::TooLong));
    // 8193 two-byte characters are 16386 bytes: the limit counts bytes.
    assert_eq!(reply_text(&"é".repeat(8193)), Err(ReplyError::TooLong));
}

#[test]
fn a_card_sent_again_with_other_options_keeps_its_expiry() {
    let event = Event::AskEscalated { request_id: "a7".into(), friend: "Morgan".into(), question: "Is User free Friday?".into() };
    let card = notice_for(&event, NOW_MS, WAITS);
    let pending = ask_pending(&event, &card).expect("an escalated ask keeps a record");
    assert_eq!(pending.expires_ms, NOW_MS + WAITS.ask_ms, "the record holds the card's expiry");

    // Share… lists the shelf: the same card (ID and text), with the list and the same expiry.
    let shared = ask_card(&Pending { shelf: Some(vec!["plan.pdf".into()]), ..pending.clone() });
    assert_eq!(shared.id, "friends:ask:a7");
    assert_eq!(shared.text, card.text);
    let (_, options, expires_ms) = ask_parts(&shared);
    assert_eq!(options, ["plan.pdf", "Back"]);
    assert_eq!(expires_ms, NOW_MS + WAITS.ask_ms, "the expiry is the card's, not a new one");

    // Back restores the three answers, still with that expiry.
    let back = ask_card(&Pending { shelf: None, ..pending });
    let (_, options, expires_ms) = ask_parts(&back);
    assert_eq!(options, ["Answer…", "Share…", "Deny"]);
    assert_eq!(expires_ms, NOW_MS + WAITS.ask_ms);
}

#[test]
fn an_ask_record_goes_when_its_card_expires() {
    let event = Event::AskEscalated { request_id: "a7".into(), friend: "Morgan".into(), question: "Is User free Friday?".into() };
    let card = notice_for(&event, NOW_MS, WAITS);
    let pending: Pending = ask_pending(&event, &card).unwrap();
    let mut asks = HashMap::from([("friends:ask:a7".to_string(), pending)]);
    drop_expired(&mut asks, NOW_MS + WAITS.ask_ms - 1);
    assert_eq!(asks.len(), 1, "still open a moment before it expires");
    drop_expired(&mut asks, NOW_MS + WAITS.ask_ms);
    assert!(asks.is_empty(), "gone at its expiry");
}

#[test]
fn a_friend_is_found_by_label_only_while_not_revoked() {
    let peers = [peer("Pat", true), peer("Morgan", false)];
    assert_eq!(live_friend(&peers, "Morgan").map(|p| p.id.as_str()), Some("id-Morgan"));
    assert_eq!(live_friend(&peers, "Pat"), None, "a revoked friend is not reached");
    assert_eq!(live_friend(&peers, "Nobody"), None);
}

#[test]
fn every_other_event_is_a_short_line_of_its_kind() {
    let lines: Vec<(Event, NoticeKind)> = vec![
        (Event::Paired { friend_id: "x".into(), label: "Morgan".into(), shelf: "foster".into() }, NoticeKind::Success),
        (Event::PairFailed { label: "Morgan".into(), reason: "bad proof".into() }, NoticeKind::Warning),
        (Event::PairDenied { label: "Morgan".into() }, NoticeKind::Warning),
        (Event::Refused { friend_id: "x".into(), reason: "revoked".into() }, NoticeKind::Warning),
        (Event::Pinged { friend: "Morgan".into(), rtt_ms: 12.0, path: "direct".into() }, NoticeKind::Success),
        (Event::AskAnsweredFromShelf { friend: "Morgan".into(), question: "Hours?".into(), answer: "9 to 5".into() }, NoticeKind::Success),
        (Event::AskAnswered { friend: "Morgan".into(), from: "User".into(), text: "Yes".into() }, NoticeKind::Success),
        (Event::OfferReceived { friend: "Morgan".into(), name: "photo.jpg".into(), size: 10 }, NoticeKind::Info),
        (Event::FileStored { friend: "Morgan".into(), path: "/Users/x/Downloads/Heimdall Quarantine/Morgan/photo.jpg".into(), size: 10, seconds: 0.1 }, NoticeKind::Success),
        (Event::FileFailed { friend: "Morgan".into(), reason: "too large".into() }, NoticeKind::Error),
        (Event::Revoked { friend: "Morgan".into() }, NoticeKind::Warning),
    ];
    for (event, kind) in lines {
        let line = notice_for(&event, NOW_MS, WAITS);
        assert!(line.action.is_none(), "{event:?} is a line, not a card");
        assert_eq!(line.kind, kind, "{event:?}");
        assert!(!line.text.is_empty() && line.text.chars().count() <= 40, "a few words: {:?}", line.text);
    }
}

#[test]
fn a_file_line_names_the_file_and_the_friend_without_the_folder() {
    let event = Event::FileStored { friend: "Morgan".into(), path: "/Users/x/Downloads/Heimdall Quarantine/Morgan/photo.jpg".into(), size: 1, seconds: 0.1 };
    assert_eq!(notice_for(&event, NOW_MS, WAITS).text, "Saved photo.jpg from Morgan");
}

#[test]
fn an_island_answer_belongs_to_friends_only_when_it_has_the_prefix() {
    assert!(owns("friends:pair:r1"));
    assert!(owns("friends:ask:a7"));
    assert!(!owns("voice-name:Speaker 3"), "the voice card's answers stay on their own path");
    assert!(!owns("agent-question-7"));
}

#[test]
fn an_unknown_choice_or_card_decides_nothing() {
    assert_eq!(decision("friends:pair:r1", "Yes", None), None, "a pairing offers Allow and Deny only");
    assert_eq!(decision("friends:ask:a7", "Yes", None), None, "Yes is no longer an answer");
    assert_eq!(decision("friends:ask:a7", "Back", None), None, "Back only means something on the Share list");
    assert_eq!(decision("voice-name:Speaker 3", "Yes", None), None);
}

#[test]
fn friend_rows_show_every_friend_and_mark_the_revoked_ones() {
    let rows = friend_rows(&[peer("Morgan", false), peer("Pat", true)]);
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].label.as_str(), rows[0].revoked), ("Morgan", false));
    assert_eq!((rows[1].label.as_str(), rows[1].revoked), ("Pat", true));
    assert_eq!(rows[0].id, "id-Morgan", "rows carry the endpoint ID that Revoke names");
}

#[test]
fn a_new_invite_suggests_the_first_name_no_live_friend_or_open_invite_has() {
    assert_eq!(free_name(&[], &[], NOW_S), "Friend");
    assert_eq!(free_name(&[peer("Friend", false)], &[], NOW_S), "Friend 2");
    assert_eq!(free_name(&[peer("Friend", true)], &[], NOW_S), "Friend", "a revoked name is free again");
    assert_eq!(free_name(&[peer("Friend", false), peer("Friend 2", false)], &[], NOW_S), "Friend 3");
}

#[test]
fn an_open_invite_takes_its_name_until_it_expires_or_is_burned() {
    let open = [issued("Friend", InviteState::Open, NOW_S + 60)];
    assert_eq!(free_name(&[], &open, NOW_S), "Friend 2", "two clicks never suggest the same name");
    assert!(name_taken("Friend", &[], &open, NOW_S));
    let pairing = [issued("Friend", InviteState::Pairing, NOW_S + 60)];
    assert!(name_taken("Friend", &[], &pairing, NOW_S), "a friend who is pairing keeps the name");
    let expired = [issued("Friend", InviteState::Open, NOW_S - 1)];
    assert!(!name_taken("Friend", &[], &expired, NOW_S), "an expired invite frees its name");
    let burned = [issued("Friend", InviteState::Burned, NOW_S + 60)];
    assert!(!name_taken("Friend", &[], &burned, NOW_S), "a burned invite frees its name");
}

#[test]
fn an_invite_link_dropped_as_text_or_as_a_url_is_a_link_slot() {
    let link = fresh_link();
    let items = vec![item(DropKind::Text, None, None, Some(&link))];
    assert_eq!(plan(&items, &always_local), vec![Slot::Link(link.clone())], "an invite link as text is redeemed");
    assert!(Invite::parse(&link).is_ok(), "the link is one the node can redeem");

    let items = vec![item(DropKind::Link, None, Some(&format!("  {link}  ")), None)];
    assert_eq!(plan(&items, &always_local), vec![Slot::Link(link)], "a dragged link is taken, trimmed");
}

#[test]
fn a_qr_png_of_an_invite_is_read_to_its_link() {
    let dir = tempfile::tempdir().unwrap();
    let link = fresh_link();
    let path = dir.path().join("invite.png");
    std::fs::write(&path, invite::qr_png(&link).unwrap()).unwrap();
    assert_eq!(read_qr_file(&path), Some(link.clone()), "the QR's text is the invite the node redeems");

    let items = vec![item(DropKind::Image, Some(path), None, None)];
    assert_eq!(plan(&items, &always_local), vec![Slot::Scan], "an image is read for its QR code");
}

#[test]
fn a_random_png_is_not_an_invite_and_keeps_its_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.png");
    image::RgbImage::from_fn(96, 96, |x, y| image::Rgb([(x * 2) as u8, (y * 2) as u8, 90])).save(&path).unwrap();
    assert_eq!(read_qr_file(&path), None, "an ordinary image has no invite");
    let items = vec![item(DropKind::Image, Some(path), None, None)];
    assert_eq!(plan(&items, &always_local), vec![Slot::Scan], "it is read, then it goes on unchanged");
}

#[test]
fn an_unreadable_or_missing_image_is_not_an_invite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.png");
    std::fs::write(&path, b"not a picture").unwrap();
    assert_eq!(read_qr_file(&path), None);
    assert_eq!(read_qr_file(&dir.path().join("missing.png")), None);
}

#[test]
fn an_image_that_is_not_local_yet_is_never_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("in-icloud.png");
    let items = vec![item(DropKind::Image, Some(path), None, None)];
    assert_eq!(plan(&items, &never_local), vec![Slot::Keep], "a dataless image takes the existing path untouched");
}

#[test]
fn plain_text_files_and_links_keep_their_drop_and_their_slots() {
    let items = vec![
        item(DropKind::Text, None, None, Some("Meet at 9, bring the plan")),
        item(DropKind::File, Some("/tmp/notes.md".into()), None, None),
        item(DropKind::Link, None, Some("https://example.com/a"), None),
    ];
    assert_eq!(plan(&items, &always_local), vec![Slot::Keep, Slot::Keep, Slot::Keep]);
}

#[test]
fn a_drop_with_nothing_to_read_comes_back_as_it_was() {
    let d = Dropped {
        items: vec![item(DropKind::Text, None, None, Some("the plan")), item(DropKind::File, Some("/tmp/a.md".into()), None, None)],
        at_ms: 7,
    };
    let back = intake(d.clone(), |_| panic!("nothing is forwarded later"));
    assert_eq!(back, Some(d), "ordinary drops take the existing path with no change");
}

#[test]
fn an_ordinary_image_is_read_off_the_drop_path_and_forwarded_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.png");
    image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8, y as u8, 200])).save(&path).unwrap();
    let d = Dropped { items: vec![item(DropKind::Image, Some(path), None, None)], at_ms: 9 };
    let (tx, rx) = std::sync::mpsc::channel();
    let now = intake(d.clone(), move |rest| {
        let _ = tx.send(rest);
    });
    assert_eq!(now, None, "the image is read on another thread, so the live loop does not wait");
    let forwarded = rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the reader hands the drop back");
    assert_eq!(forwarded, d, "the existing path gets the same drop");
}

#[test]
fn a_failed_redeem_has_a_line_except_a_denial_the_node_already_showed() {
    use ::friends::InviteError;
    assert_eq!(redeem_failure(&PairError::Denied), None, "PairDenied is the line for a denial");
    assert_eq!(redeem_failure(&PairError::Invite(InviteError::Expired)).map(|(t, _)| t), Some("Invite expired"));
    assert_eq!(redeem_failure(&PairError::WrongSecret).map(|(_, k)| k), Some(NoticeKind::Error));
    assert_eq!(redeem_failure(&PairError::Transport("x".into())).map(|(t, _)| t), Some("Could not reach the friend"));
}

/// Renders only: `FRIENDS_RENDER_QR=<file.png> cargo test -p app friends::tests::render_fixture_qr -- --ignored` writes
/// the QR code of a fresh test invite (made by `friends::invite::qr_png`) for the UI's invite render. The invite is never
/// registered with a node, so the code cannot be redeemed.
#[test]
#[ignore]
fn render_fixture_qr() {
    let Some(path) = std::env::var_os("FRIENDS_RENDER_QR") else { return };
    std::fs::write(path, invite::qr_png(&fresh_link()).unwrap()).unwrap();
}

#[test]
fn the_typed_name_wins_then_the_account_name_then_the_login() {
    assert_eq!(choose_name("  Alice N.  ", Some("Alice Example"), Some("alice")), "Alice N.");
    assert_eq!(choose_name("", Some("Alice Example"), Some("alice")), "Alice Example");
    assert_eq!(choose_name("   ", None, Some("alice")), "alice");
    assert_eq!(choose_name("", None, None), "Heimdall");
}

#[test]
fn a_name_keeps_no_control_characters_and_at_most_64_characters() {
    assert_eq!(choose_name("User\u{7}\n", None, None), "User");
    assert_eq!(choose_name(&"é".repeat(80), None, None).chars().count(), 64);
}

fn png_file(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
    let path = dir.join(name);
    image::RgbImage::new(w, h).save(&path).unwrap();
    path
}

#[test]
fn a_big_screenshot_is_not_read_but_a_small_one_and_an_invite_file_are() {
    let dir = tempfile::tempdir().unwrap();
    let five_k = png_file(dir.path(), "Screenshot 5K.png", 5120, 2880);
    let retina = png_file(dir.path(), "Screenshot retina.png", 2880, 1800);
    let small = png_file(dir.path(), "photo.png", 800, 600);
    let big_invite = png_file(dir.path(), "heimdall-invite-abc123.png", 5120, 2880);
    assert!(!worth_scanning(&five_k), "a 5K screenshot is over {MAX_SCAN_SIDE} px");
    assert!(worth_scanning(&retina));
    assert!(worth_scanning(&small));
    assert!(worth_scanning(&big_invite), "an invite file is always read");
    assert!(!worth_scanning(&dir.path().join("missing.png")), "an unreadable file is not read");
}

