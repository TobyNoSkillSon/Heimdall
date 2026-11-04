use super::*;
use serde_json::json;

const DAY: u64 = 1_792_706_400_000; // well inside 23 Oct 2026 in any European zone
fn at(min: u64) -> u64 {
    day_start(DAY) + 15 * 3_600_000 + min * 60_000
}

fn user(ms: u64, text: &str) -> Value {
    json!({"type":"message","message":{"role":"user","timestamp":ms,"content":[{"type":"text","text":text}]}})
}
fn assistant(ms: u64, stop: &str, blocks: Value) -> Value {
    json!({"type":"message","message":{"role":"assistant","timestamp":ms,"stopReason":stop,"content":blocks}})
}
fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"toolCall","id":id,"name":format!("mcp__heimdall__{name}"),"arguments":args})
}
fn result(ms: u64, id: &str, text: &str) -> Value {
    json!({"type":"message","message":{"role":"toolResult","timestamp":ms,"toolCallId":id,"toolName":"x","isError":false,"content":[{"type":"text","text":text}]}})
}
fn iso(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).unwrap().to_rfc3339()
}
fn text(t: &str) -> Value {
    json!({"type":"text","text":t})
}

/// A day as the session file records it: a wake (its transcript is never read for the user's words), tools, a typed
/// message, notices, and replies the user talked over or never heard.
fn session() -> Vec<Value> {
    vec![
        // Yesterday: not today's conversation.
        user(day_start(DAY) - 60_000, "[Heimdall delivery: y]\n[Heimdall typed message]\nold news"),
        assistant(day_start(DAY) - 50_000, "stop", json!([text("Old reply.")])),
        user(at(1), "[Heimdall delivery: a]\nTranscript:\n[15:01:00 Discord · Dave] did you push it\n[15:01:05 Mic · User] Heimdall, how much RAM is free?\n\nOwner's wake instruction:\nAct."),
        assistant(at(2), "toolUse", json!([text("Checking."), call("t1", "island_notice", json!({"text":"Memory","detail":{"type":"table","headers":["App","RAM"],"rows":[["Heimdall","41 GB"]]}}))])),
        result(at(2), "t1", "Notice shown: notice-4d5c."),
        assistant(at(2), "toolUse", json!([call("t2", "island_notice", json!({"text":"Just a line"}))])),
        result(at(2), "t2", "Notice shown: notice-plain."),
        assistant(at(3), "stop", json!([text("About 60 GB free.\n\nNo swap.")])),
        user(at(4), "[Heimdall delivery: b]\n[Now: …]\n[Heimdall typed message]\nclear the cache?"),
        assistant(at(4), "toolUse", json!([call("t3", "ask", json!({"question":"Clear the model cache?","options":["Yes","No"]}))])),
        result(at(4), "t3", "Question pending: request_id ask-77aa. His click arrives at once."),
        assistant(at(4), "stop", json!([text("NO_REPLY")])),
        user(at(5), "[Heimdall delivery: notice-x]\n[Heimdall notice: ask-77aa]\nQuestion ask-77aa: User clicked Yes. Question: Clear the model cache?"),
        assistant(at(6), "toolUse", json!([call("t4", "delegate_task", json!({"title":"Cache cleanup","task":"…"})), call("t5", "speak", json!({"text":"Starting the cleanup now, it takes a minute."}))])),
        result(at(6), "t4", "Started 'Cache cleanup' as task mcp:1 (claude-opus-5-5)."),
        result(at(6), "t5", "Queued."),
        assistant(at(6), "stop", json!([text("Starting the cleanup now, it takes a minute.")])),
        json!({"type":"custom_message","customType":"heimdall_context","timestamp":iso(at(7)),"content":"[Heimdall notice: unheard-speech:cut:1]\nOwner talked over you; he heard only the start of: Starting the cleanup now, it takes a minute. He did not hear: …"}),
        user(at(8), "[Heimdall delivery: notice-y]\n[Heimdall notice: terminal:mcp:1:run:2:completed]\nTask 'Cache cleanup' (mcp:1) completed. Final reply:\n\nDone."),
        assistant(at(9), "stop", json!([text("The cache is clear: 38 GB back.")])),
        user(at(10), "[Heimdall delivery: notice-z]\n[Heimdall notice: skipped-reply:9]\nThis never reached User: he was talking when it would have played, so it was neither said nor shown, and it will not be replayed. Answer fresh in your next turn, folding in what still matters with his new words: The cache is clear: 38 GB back."),
    ]
}

fn write_day(data: &Path, entries: &[Value], voice: &[(u64, &str, &str, &str, &str)]) {
    std::fs::create_dir_all(data.join("agent")).unwrap();
    let file = data.join("pi-session.jsonl");
    std::fs::write(&file, entries.iter().map(|e| format!("{e}\n")).collect::<String>()).unwrap();
    std::fs::write(data.join("agent/active-session.json"), serde_json::to_vec(&file).unwrap()).unwrap();
    std::fs::create_dir_all(data.join("archive")).unwrap();
    let db = rusqlite::Connection::open(data.join("archive/heimdall.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE IF NOT EXISTS utterances(session INTEGER NOT NULL, id TEXT NOT NULL, wall_ms INTEGER, source_id TEXT, source TEXT, speaker TEXT, start_ns INTEGER, end_ns INTEGER, text TEXT, words TEXT, notes TEXT, decision TEXT, PRIMARY KEY(session, id));").unwrap();
    for (k, (ms, source_id, speaker, text, outcome)) in voice.iter().enumerate() {
        db.execute(
            "INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,text,decision) VALUES (1,?1,?2,?3,'x',?4,?5,?6)",
            rusqlite::params![format!("u{k}"), *ms as i64, source_id, speaker, text, json!({"outcome": outcome}).to_string()],
        )
        .unwrap();
    }
}

fn kinds(rows: &[ChatEntry]) -> Vec<ChatKind> {
    rows.iter().map(|e| e.kind.clone()).collect()
}

#[test]
fn todays_conversation_is_owners_words_the_replies_and_compact_actions_only() {
    let d = tempfile::tempdir().unwrap();
    let voice = [
        (at(1), "mic:shure", "User", "Heimdall, how much RAM", "wait"),
        (at(1) + 4_000, "mic:shure", "User", "is free?", "send"),
        (at(1) + 9_000, "mic:shure", "User", "Hello Jarvis", "not_for_heimdall"),
        (at(1) + 10_000, "app:discord", "Dave", "did you push it", "send"),
        (at(1) + 11_000, "mic:shure", "Speaker 2", "talking to User", "send"),
    ];
    write_day(d.path(), &session(), &voice);
    let rows = read_once(d.path(), at(20));
    let owner = |s: &str| ChatKind::Owner(s.into());
    let him = |s: &str, heard| ChatKind::Heimdall { text: s.into(), heard };
    let act = |kind, s: &str, a: Option<&str>| ChatKind::Action { kind, text: s.into(), answer: a.map(Into::into) };
    assert_eq!(
        kinds(&rows),
        vec![
            owner("Heimdall, how much RAM is free?"),
            ChatKind::Note { id: "notice-4d5c".into(), title: "Memory".into(), shape: NoteShape::Table },
            him("About 60 GB free.\n\nNo swap.", Heard::Whole),
            owner("clear the cache?"),
            act(ActionKind::Question, "Clear the model cache?", Some("Yes")),
            act(ActionKind::Started, "Cache cleanup", None),
            him("Starting the cleanup now, it takes a minute.", Heard::Cut),
            act(ActionKind::Finished, "Cache cleanup", None),
            him("The cache is clear: 38 GB back.", Heard::Never),
        ],
        "never yesterday, Discord, another speaker, his talk with others, a plain line or the silent reply"
    );
    assert!(rows.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
}

#[test]
fn the_reader_tails_the_session_file_and_rereads_it_when_it_is_rewritten() {
    let d = tempfile::tempdir().unwrap();
    let entries = session();
    write_day(d.path(), &entries[..8], &[]);
    let mut l = Live { history: History::default(), session: None, offset: 0, partial: vec![], day: 0, sent: vec![] };
    assert!(l.refresh(d.path(), at(20)));
    assert!(!l.refresh(d.path(), at(20)), "nothing new: nothing read");
    let first = l.history.rows(l.day).len();
    assert_eq!(note_detail("notice-4d5c").map(|(t, _)| t), Some("Memory".into()), "kept for pulling out, outside the reader");
    // Appended, with half a line still being written: the half waits for its end.
    let file = d.path().join("pi-session.jsonl");
    let more: String = entries[8..].iter().map(|e| format!("{e}\n")).collect();
    let (a, b) = more.split_at(more.len() - 20);
    std::fs::OpenOptions::new().append(true).open(&file).unwrap().write_all(a.as_bytes()).unwrap();
    l.refresh(d.path(), at(20));
    std::fs::OpenOptions::new().append(true).open(&file).unwrap().write_all(b.as_bytes()).unwrap();
    l.refresh(d.path(), at(20));
    assert_eq!(kinds(&l.history.rows(l.day)), kinds(&read_once(d.path(), at(20))), "tailing reads what one pass reads");
    assert!(l.history.rows(l.day).len() > first);
    // Rotated (rewritten shorter): read again from the start.
    std::fs::write(&file, format!("{}\n", user(at(30), "[Heimdall typed message]\nfresh"))).unwrap();
    l.refresh(d.path(), at(31));
    assert_eq!(kinds(&l.history.rows(l.day)), vec![ChatKind::Owner("fresh".into())]);
    // A new day starts empty.
    l.refresh(d.path(), at(31) + 86_400_000);
    assert!(l.history.rows(l.day).is_empty());
}

#[test]
fn a_withdrawn_reply_is_marked_unheard_and_a_cut_never_undoes_it() {
    let mut h = History::default();
    h.heimdall("First answer here.", at(1), Heard::Whole);
    h.heimdall("Second answer, longer than the first one.", at(2), Heard::Whole);
    assert!(h.mark("Second answer, longer than the first one.", Heard::Never, 0));
    assert!(h.mark("Second answer, longer", Heard::Cut, 0));
    assert!(!h.mark("First answer here.", Heard::Cut, at(2)), "a mark never reaches a reply older than its time");
    // "the user talked over you before he heard: …" is a cut too.
    h.heimdall("Third one.", at(3), Heard::Whole);
    h.marks("[Heimdall notice: unheard-speech:cut:2]\nUser talked over you before he heard: Third one.");
    assert!(matches!(&h.rows(0)[2].kind, ChatKind::Heimdall { heard: Heard::Cut, .. }));
    assert!(matches!(&h.rows(0)[1].kind, ChatKind::Heimdall { heard: Heard::Never, .. }));
    assert!(matches!(&h.rows(0)[0].kind, ChatKind::Heimdall { heard: Heard::Whole, .. }));
}

/// The same words in a later run are a new reply; in the same run (a `speak` line the final reply repeats), once.
#[test]
fn a_reply_repeated_in_a_later_run_shows_again() {
    let mut h = History::default();
    let since = day_start(DAY);
    h.entry(&user(at(1), "[Heimdall delivery: a]\nTranscript:\n[15:01 Mic · User] status?"), since);
    h.entry(&assistant(at(1), "stop", json!([text("Done.")])), since);
    h.entry(&user(at(30), "[Heimdall delivery: b]\nTranscript:\n[15:30 Mic · User] and now?"), since);
    h.entry(&assistant(at(30), "toolUse", json!([call("s", "speak", json!({"text": "Done."}))])), since);
    h.entry(&result(at(30), "s", "Queued."), since);
    h.entry(&assistant(at(30), "stop", json!([text("Done.")])), since);
    assert_eq!(h.rows(0).len(), 2, "{:?}", h.rows(0));
}

#[test]
fn a_wait_joins_the_send_after_it_but_not_one_long_before_or_after_other_talk() {
    let rows = vec![
        (1_000, "lone wait".to_string(), "wait".to_string()),
        (100_000, "first half".to_string(), "wait".to_string()),
        (104_000, "second half".to_string(), "send".to_string()),
        (200_000, "aside".to_string(), "wait".to_string()),
        (201_000, "to Erin".to_string(), "not_for_heimdall".to_string()),
        (202_000, "to Heimdall".to_string(), "send".to_string()),
    ];
    let out = History::voice_rows(&rows);
    assert_eq!(kinds(&out), vec![ChatKind::Owner("first half second half".into()), ChatKind::Owner("to Heimdall".into())]);
    assert_eq!(out[0].at_ms, 100_000);
}

/// Bug note 2026-10-28T19-12-56: in relay mode the island skipped every typed message and relay reply (held and
/// mirrored context, never user or assistant messages) and showed words to friends a trigger forced as the user's turn.
#[test]
fn relay_mode_shows_typed_messages_and_relay_replies_but_not_a_forced_wake_the_relay_left_unanswered() {
    let d = tempfile::tempdir().unwrap();
    let ctx = |ms: u64, text: String| json!({"type":"custom_message","customType":"heimdall_context","display":true,"timestamp":iso(ms),"content":text});
    let held = |ms: u64, body: &str| ctx(ms, format!("{}\n{body}", agent::emergency::HELD));
    let cover = |ms: u64, id: &str, line: &str| ctx(ms, format!("[Heimdall notice: emergency-cover:{id}]\n{line}"));
    let wake = |words: &str| format!("Transcript:\n[15:00:00 Discord · Dave] which config\n[15:00:01 Mic · User] {words}\n\nOwner's wake instruction:\nAct on User's latest request.");
    let entries = vec![
        held(at(1), &wake("Just send it on the config.")),
        held(at(11), "[Heimdall typed message]\nwhats the status"),
        cover(at(11) + 20_000, "a1", "[Relay agent] t3_thread_read {\"last\":1} → Thread read."),
        cover(at(11) + 30_000, "mirror:mcp_x:405:0", "[Relay agent] Four results are ready."),
        held(at(13), "[Heimdall delivery: m2]\n[Heimdall typed message]\nopen the ready ones"),
        cover(at(13) + 5_000, "a2", "[Relay agent] said to User: \"Opened three tabs.\" → Speech queued: 1."),
        cover(at(13) + 6_000, "mirror:mcp_x:412:0", "[Relay agent] Opened three tabs."),
        held(at(14), "[Heimdall notice: terminal:mcp:1:run:2:completed]\nTask 'Build' (mcp:1) completed."),
        cover(at(14) + 1_000, "mirror:mcp_x:413:0", "[Relay agent] NO_REPLY"),
        // A forced wake his next words followed at once: answered with them, so it stays.
        held(at(20), &wake("Heimdall, hello?")),
        held(at(20) + 15_000, "[Heimdall typed message]\nhow is nemotron?"),
        cover(at(21), "mirror:mcp_x:420:0", "[Relay agent] Downloading, half done."),
    ];
    let voice = [
        (at(1) - 200, "mic:shure", "User", "Just send it on the config.", "forced"),
        (at(20) - 200, "mic:shure", "User", "Heimdall, hello?", "forced"),
    ];
    write_day(d.path(), &entries, &[]);
    let db = rusqlite::Connection::open(d.path().join("archive/heimdall.sqlite")).unwrap();
    for (k, (ms, source_id, speaker, text, _)) in voice.iter().enumerate() {
        let decision = json!({"outcome":"send","choice":"not_for_heimdall","call_gate":{"trigger":"send_it","wakes":true}});
        db.execute("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,text,decision) VALUES (1,?1,?2,?3,'x',?4,?5,?6)",
            rusqlite::params![format!("u{k}"), *ms as i64, source_id, speaker, text, decision.to_string()]).unwrap();
    }
    let rows = read_once(d.path(), at(30));
    let owner = |s: &str| ChatKind::Owner(s.into());
    let him = |s: &str| ChatKind::Heimdall { text: s.into(), heard: Heard::Whole };
    assert_eq!(
        kinds(&rows),
        vec![
            owner("whats the status"),
            him("Four results are ready."),
            owner("open the ready ones"),
            him("Opened three tabs."),
            ChatKind::Action { kind: ActionKind::Finished, text: "Build".into(), answer: None },
            owner("Heimdall, hello?"),
            owner("how is nemotron?"),
            him("Downloading, half done."),
        ],
        "typed messages and relay replies in order, a speak line once, no silent reply or action line, and not the forced wake nobody answered"
    );
}

/// Bug note 2026-10-28T19-12-56, review: a held relay day read straight into `History`, with the spoken rows and the
/// forced sends among them as `refresh` sets them.
fn relay_day(entries: &[Value], voice: &[(u64, &str)]) -> Vec<ChatKind> {
    let mut h = History::default();
    for e in entries { h.entry(e, day_start(DAY)); }
    h.voice = voice.iter().map(|(at, t)| ChatEntry { at_ms: *at, kind: ChatKind::Owner((*t).into()) }).collect();
    h.forced = voice.iter().map(|(at, _)| *at).collect();
    kinds(&h.rows(0))
}
fn held(ms: u64, body: &str) -> Value {
    json!({"type":"custom_message","customType":"heimdall_context","timestamp":iso(ms),"content":format!("{}\n{body}", agent::emergency::HELD)})
}
fn cover(ms: u64, id: &str, line: &str) -> Value {
    json!({"type":"custom_message","customType":"heimdall_context","timestamp":iso(ms),"content":format!("[Heimdall notice: emergency-cover:{id}]\n{line}")})
}
const WAKE: &str = "Transcript:\n[15:01:00 Mic · User] Just send it on the config.\n\nOwner's wake instruction:\nAct.";

/// The relay's NO_REPLY and its action lines never take a forced wake: it stays unanswered.
#[test]
fn a_forced_wake_the_relay_answered_with_no_reply_is_not_a_turn() {
    let rows = relay_day(&[
        held(at(1), WAKE),
        cover(at(1) + 3_000, "a1", "[Relay agent] screen_frames {} → Frame shown."),
        cover(at(1) + 4_000, "mirror:mcp_x:400:0", "[Relay agent] NO_REPLY"),
        held(at(11), "[Heimdall typed message]\nwhats the status"),
    ], &[(at(1) - 200, "Just send it on the config.")]);
    assert_eq!(rows, vec![ChatKind::Owner("whats the status".into())]);
}

/// The public edition names its user: a speak-only relay reply is still his.
#[test]
fn a_relay_speak_line_to_a_renamed_user_shows() {
    let rows = relay_day(&[
        held(at(1), "[Heimdall typed message]\nhow is it going?"),
        cover(at(1) + 5_000, "a2", "[Relay agent] said to Erin: \"Fine: two tasks run.\" → Speech queued: 1."),
    ], &[]);
    assert_eq!(rows, vec![ChatKind::Owner("how is it going?".into()), ChatKind::Heimdall { text: "Fine: two tasks run.".into(), heard: Heard::Whole }]);
}

/// A hand-back ends the relay's wait: a wake Pi's own turn answered is never hidden by a later relay's input.
#[test]
fn a_hand_back_ends_the_relays_pending_wake() {
    let rows = relay_day(&[
        held(at(1), WAKE),
        user(at(2), "[Heimdall delivery: emergency-back-1]\n[Heimdall notice: emergency-back-1]\nYou are back."),
        assistant(at(2), "stop", json!([text("Sent it to the config.")])),
        held(at(20), "[Heimdall typed message]\nthanks"),
    ], &[(at(1) - 200, "Just send it on the config.")]);
    assert_eq!(rows[0], ChatKind::Owner("Just send it on the config.".into()), "{rows:?}");
}

use std::io::Write as _;

/// Lab run, read only: today's conversation from a real data directory (`CHAT_DATA`), printed row by row.
#[test]
#[ignore]
fn lab_reads_a_real_day() {
    let Ok(data) = std::env::var("CHAT_DATA") else { return };
    for e in read_once(Path::new(&data), agent::now_ms()) {
        let line = match &e.kind {
            ChatKind::Owner(t) => format!("OWNER  {t}"),
            ChatKind::Heimdall { text, heard } => format!("HEIM  {heard:?} {text}"),
            ChatKind::Action { kind, text, answer } => format!("ACT   {kind:?} {text} -> {answer:?}"),
            ChatKind::Note { id, title, shape } => format!("NOTE  {shape:?} {title} ({id})"),
        };
        println!("{} {}", e.at_ms, line.replace('\n', " ⏎ ").chars().take(150).collect::<String>());
    }
}

/// Bug note 2026-11-01T00-20-27: his typed message shows in the pulled-down notch the moment he sends it, and the
/// session file's copy takes its place (one row, never two); a failed send's row goes.
#[test]
fn a_typed_message_shows_at_once_and_its_session_copy_takes_its_place() {
    let mut h = History::default();
    let sent = at(10);
    let row = |text: &str| ChatEntry { at_ms: sent, kind: ChatKind::Owner(text.into()) };
    h.typed(vec![("a".into(), row("helo aparenly you recovered")), ("b".into(), row("never sent"))], &[], sent);
    let rows = h.rows(day_start(DAY));
    assert_eq!(rows.len(), 2, "both show before the agent has either: {rows:?}");
    h.typed(vec![], &["b".into()], sent + 1);
    h.entry(&user(sent + 900, "[Heimdall typed message]\nhelo aparenly you recovered"), day_start(DAY));
    h.typed(vec![], &[], sent + 1000);
    assert_eq!(h.rows(day_start(DAY)), vec![ChatEntry { at_ms: sent + 900, kind: ChatKind::Owner("helo aparenly you recovered".into()) }], "the session's copy only");
    h.typed(vec![("c".into(), row("lost on the way"))], &[], sent);
    h.typed(vec![], &[], sent + TYPED_WAIT_MS);
    assert_eq!(h.rows(day_start(DAY)).len(), 1, "a row the session never got goes after its wait");
}
