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
    let owner = |s: &str| ChatKind::User(s.into());
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

/// VM run 5 Nov 05:43: the public edition sends the user's name, or "the user", where the store says User
/// (`agent::identity::public_wording`); the session it leaves reads as the same conversation.
#[test]
fn the_public_wording_reads_as_the_same_conversation() {
    fn worded(v: &mut Value, names: &agent::identity::Names) {
        match v {
            Value::String(s) => *s = agent::identity::public_wording(s, names),
            Value::Array(a) => a.iter_mut().for_each(|v| worded(v, names)),
            Value::Object(m) => m.iter_mut().filter(|(k, _)| *k != "timestamp").for_each(|(_, v)| worded(v, names)),
            _ => {}
        }
    }
    let voice = [(at(1), "mic:shure", "User", "Heimdall, how much RAM is free?", "send")];
    let personal = tempfile::tempdir().unwrap();
    write_day(personal.path(), &session(), &voice);
    let want = kinds(&read_once(personal.path(), at(20)));
    for user in ["User", "Tester"] {
        let names = agent::identity::Names { assistant: "Heimdall".into(), user: user.into() };
        let mut entries = session();
        entries.iter_mut().for_each(|e| worded(e, &names));
        let text = serde_json::to_string(&entries).unwrap();
        assert!(!text.contains(agent::identity::WAKE_MARKER), "{user}: {text}");
        if user != agent::identity::PUBLIC_USER {
            assert!(!text.split(|c: char| !c.is_alphanumeric()).any(|w| w == agent::identity::DEFAULT_USER), "{user}: {text}");
        }
        let public = tempfile::tempdir().unwrap();
        write_day(public.path(), &entries, &voice);
        assert_eq!(kinds(&read_once(public.path(), at(20))), want, "{user}");
    }
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
    assert_eq!(kinds(&l.history.rows(l.day)), vec![ChatKind::User("fresh".into())]);
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
    // "the user talked over you; he heard only the start of: …" is a cut too.
    h.heimdall("Third one.", at(3), Heard::Whole);
    h.marks("[Heimdall notice: unheard-speech:cut:2]\nOwner talked over you; he heard only the start of: Third one.");
    assert!(matches!(&h.rows(0)[2].kind, ChatKind::Heimdall { heard: Heard::Cut, .. }));
    assert!(matches!(&h.rows(0)[1].kind, ChatKind::Heimdall { heard: Heard::Never, .. }));
    assert!(matches!(&h.rows(0)[0].kind, ChatKind::Heimdall { heard: Heard::Whole, .. }));
    // Review 14 Nov should-fix 4: `missed_lines` still writes "the user talked over you before he heard: …" when none of
    // the reply played (and "Someone on the call … before he heard: …" for a guest); both mark it cut.
    h.heimdall("Fourth one.", at(4), Heard::Whole);
    h.marks("[Heimdall notice: unheard-speech:cut:3]\nOwner talked over you before he heard: Fourth one.");
    assert!(matches!(&h.rows(0)[3].kind, ChatKind::Heimdall { heard: Heard::Cut, .. }));
    h.heimdall("Fifth one.", at(5), Heard::Whole);
    h.marks("[Heimdall notice: unheard-speech:cut:4]\nSomeone on the call started talking, so you stopped before he heard: Fifth one.");
    assert!(matches!(&h.rows(0)[4].kind, ChatKind::Heimdall { heard: Heard::Cut, .. }));
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
    assert_eq!(kinds(&out), vec![ChatKind::User("first half second half".into()), ChatKind::User("to Heimdall".into())]);
    assert_eq!(out[0].at_ms, 100_000);
}

/// Bug note 2026-11-14T11-58-47: his two lines the call gate held (11:55:55, 11:56:34) never became rows, so the
/// panel looked dead. A held line of his is a row now; a line to someone else still is not.
#[test]
fn his_lines_held_in_a_call_are_rows() {
    let rows = vec![
        (1_000, "Okay, you can actually check the Fluid Speech Pro, yeah.".to_string(), "held_in_call".to_string()),
        (2_000, "to Mo".to_string(), "not_for_heimdall".to_string()),
        (3_000, "And by the way, can you hear me?".to_string(), "held_in_call".to_string()),
    ];
    assert_eq!(kinds(&History::voice_rows(&rows)), vec![ChatKind::User("Okay, you can actually check the Fluid Speech Pro, yeah.".into()),
        ChatKind::User("And by the way, can you hear me?".into())]);
}

/// Night 1115: a reply held in a call and then dropped unsaid is marked never heard in the panel.
#[test]
fn a_held_reply_dropped_unsaid_is_marked_never_heard() {
    let mut h = History::default();
    h.heimdall("Your T3 task finished: the build passed.", 1_000, Heard::Whole);
    assert!(h.mark("Your T3 task finished: the build passed.", Heard::Never, 0));
    assert!(matches!(&h.pi[0].kind, ChatKind::Heimdall { heard: Heard::Never, .. }));
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
    let owner = |s: &str| ChatKind::User(s.into());
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
    h.voice = voice.iter().map(|(at, t)| ChatEntry { at_ms: *at, kind: ChatKind::User((*t).into()) }).collect();
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
    assert_eq!(rows, vec![ChatKind::User("whats the status".into())]);
}

/// The public edition names its user: a speak-only relay reply is still his.
#[test]
fn a_relay_speak_line_to_a_renamed_user_shows() {
    let rows = relay_day(&[
        held(at(1), "[Heimdall typed message]\nhow is it going?"),
        cover(at(1) + 5_000, "a2", "[Relay agent] said to Erin: \"Fine: two tasks run.\" → Speech queued: 1."),
    ], &[]);
    assert_eq!(rows, vec![ChatKind::User("how is it going?".into()), ChatKind::Heimdall { text: "Fine: two tasks run.".into(), heard: Heard::Whole }]);
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
    assert_eq!(rows[0], ChatKind::User("Just send it on the config.".into()), "{rows:?}");
}

use std::io::Write as _;

/// Lab run, read only: today's conversation from a real data directory (`CHAT_DATA`), printed row by row.
#[test]
#[ignore]
fn lab_reads_a_real_day() {
    let Ok(data) = std::env::var("CHAT_DATA") else { return };
    for e in read_once(Path::new(&data), agent::now_ms()) {
        let line = match &e.kind {
            ChatKind::User(t) => format!("USER  {t}"),
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
    let row = |text: &str| ChatEntry { at_ms: sent, kind: ChatKind::User(text.into()) };
    h.typed(vec![("a".into(), row("helo aparenly you recovered")), ("b".into(), row("never sent"))], &[], sent);
    let rows = h.rows(day_start(DAY));
    assert_eq!(rows.len(), 2, "both show before the agent has either: {rows:?}");
    h.typed(vec![], &["b".into()], sent + 1);
    h.entry(&user(sent + 900, "[Heimdall typed message]\nhelo aparenly you recovered"), day_start(DAY));
    h.typed(vec![], &[], sent + 1000);
    assert_eq!(h.rows(day_start(DAY)), vec![ChatEntry { at_ms: sent + 900, kind: ChatKind::User("helo aparenly you recovered".into()) }], "the session's copy only");
    h.typed(vec![("c".into(), row("lost on the way"))], &[], sent);
    h.typed(vec![], &[], sent + TYPED_WAIT_MS);
    assert_eq!(h.rows(day_start(DAY)).len(), 1, "a row the session never got goes after its wait");
}

// ---- point 9: one chat with the phone ----

fn ids(rows: &[ChatRow]) -> Vec<String> {
    rows.iter().map(|r| r.id.clone()).collect()
}

/// Every row has an id, the same when the day is read again from scratch (a restart, a rewritten file), and the
/// phone gets exactly the Mac panel's rows.
#[test]
fn row_ids_are_stable_across_rebuilds_and_the_rows_are_the_mac_panels() {
    let d = tempfile::tempdir().unwrap();
    let voice = [(at(1), "mic:shure", "User", "Heimdall, how much RAM", "wait"), (at(1) + 4_000, "mic:shure", "User", "is free?", "send")];
    write_day(d.path(), &session(), &voice);
    let (day, first, older) = read_once_rows(d.path(), at(20));
    let (_, again, _) = read_once_rows(d.path(), at(21));
    assert_eq!(ids(&first), ids(&again), "the same ids every look");
    assert_eq!(older, 0);
    let unique: std::collections::HashSet<_> = ids(&first).into_iter().collect();
    assert_eq!(unique.len(), first.len(), "ids are unique");
    assert_eq!(first.iter().map(|r| r.entry.clone()).collect::<Vec<_>>(), read_once(d.path(), at(20)), "one source of truth");
    assert_eq!(first[0].id, "u1:u1", "the spoken row is keyed on the utterance the decider sent");
    assert_eq!(first[0].via, Some("mac_voice"));
    assert_eq!(chat_events(None, &[], day, &first, older)[0].0, "chat", "the first look is a snapshot");
    let v = row_json(&first[0]);
    assert_eq!((v["kind"].as_str(), v["via"].as_str(), v["at"].as_u64()), (Some("owner"), Some("mac_voice"), Some(at(1))));
    let note = first.iter().map(row_json).find(|r| r["kind"] == "note").unwrap();
    assert_eq!(note["note"], json!({"id": "notice-4d5c", "title": "Memory", "shape": "table"}));
    let question = first.iter().map(row_json).find(|r| r["kind"] == "action").unwrap();
    assert_eq!(question["action"], json!({"kind": "question", "answer": "Yes"}));
    assert!(first.iter().map(row_json).any(|r| r["heard"] == "cut") && first.iter().map(row_json).any(|r| r["heard"] == "never"));
}

/// A `wait` the decider holds shows nothing; the `send` after it is one row with both, under the send's id. Before it
/// came, a send alone had that id too: a later `wait` joining in front changes the row's words, never its id.
#[test]
fn a_wait_joined_to_its_send_keeps_the_sends_id() {
    let u = |key: &str, at: u64, text: &str, outcome: &str| Utterance { key: key.into(), at_ms: at, text: text.into(), outcome: outcome.into(), source: "mic:shure".into() };
    let before = History::voice_rows_keyed(&[u("1:b", 104_000, "second half", "send")]);
    let after = History::voice_rows_keyed(&[u("1:a", 100_000, "first half", "wait"), u("1:b", 104_000, "second half", "send")]);
    assert_eq!((before[0].1.as_str(), after[0].1.as_str()), ("u1:b", "u1:b"));
    assert_eq!(after[0].0.kind, ChatKind::User("first half second half".into()));
}

/// The diff by id: a new or changed row is an upsert, a row gone a removal, an unchanged one nothing; a new day is a
/// snapshot.
#[test]
fn the_diff_is_upserts_and_removals_by_id_and_a_new_day_a_snapshot() {
    let row = |id: &str, at: u64, t: &str| ChatRow { id: id.into(), via: Some("mac_typed"), entry: ChatEntry { at_ms: at, kind: ChatKind::User(t.into()) } };
    let old = vec![row("s1.0", 1, "a"), row("s2.0", 2, "b"), row("s3.0", 3, "c")];
    let new = vec![row("s1.0", 1, "a"), row("s3.0", 3, "c, edited"), row("s4.0", 4, "d")];
    let events = chat_events(Some(DAY), &old, DAY, &new, 0);
    let named: Vec<(&str, String)> = events.iter().map(|(k, v)| (*k, v["id"].as_str().or(v["row"]["id"].as_str()).unwrap().to_owned())).collect();
    assert_eq!(named, [("chat_remove", "s2.0".to_owned()), ("chat_upsert", "s3.0".to_owned()), ("chat_upsert", "s4.0".to_owned())]);
    assert!(chat_events(Some(DAY), &new, DAY, &new, 0).is_empty(), "nothing changed, nothing sent");
    let next = day_start(DAY) + 25 * 3_600_000;
    let rolled = chat_events(Some(DAY), &new, day_start(next), &[], 3);
    assert_eq!(rolled.len(), 1);
    assert_eq!(rolled[0].0, "chat");
    assert_eq!((rolled[0].1["rows"].clone(), rolled[0].1["older"].as_u64()), (json!([]), Some(3)));
    assert_eq!(rolled[0].1["day"].as_str(), Some(day_name(day_start(next)).as_str()));
}

/// The user's lines on the phone are the same conversation: the live phone microphone's sent lines (`phone_voice`), a
/// push-to-talk transcript (its `[Phone · the user]` tag: `phone_voice`, whatever the user's name) and a typed turn from the
/// phone (`phone_typed`).
#[cfg(feature = "phone")]
#[test]
fn the_phones_lines_are_in_the_conversation() {
    let d = tempfile::tempdir().unwrap();
    from_phone_at("echo-phone-typed-1", "from the phone, typed", at(3));
    let entries = vec![
        user(at(2), "[Heimdall delivery: c]\n[Heimdall typed message]\n[Phone · User] said on the phone"),
        user(at(3), "[Heimdall delivery: d]\n[Heimdall typed message]\nfrom the phone, typed"),
        user(at(4), "[Heimdall delivery: e]\n[Heimdall typed message]\ntyped at the Mac"),
        user(at(5), "[Heimdall delivery: f]\n[Heimdall typed message]\n[Phone · Ada Lovelace] a renamed user on the phone"),
    ];
    let voice = [(at(1), "phone:airpods:3", "User", "what's the weather", "send"), (at(1) + 1_000, "phone:airpods:3", "User", "not for you", "not_for_heimdall")];
    write_day(d.path(), &entries, &voice);
    let (_, rows, _) = read_once_rows(d.path(), at(20));
    let got: Vec<(String, Option<&str>)> = rows.iter().map(|r| (match &r.entry.kind { ChatKind::User(t) => t.clone(), k => format!("{k:?}") }, r.via)).collect();
    assert_eq!(got, [
        ("what's the weather".to_owned(), Some("phone_voice")),
        ("[Phone · User] said on the phone".to_owned(), Some("phone_voice")),
        ("from the phone, typed".to_owned(), Some("phone_typed")),
        ("typed at the Mac".to_owned(), Some("mac_typed")),
        ("[Phone · Ada Lovelace] a renamed user on the phone".to_owned(), Some("phone_voice")),
    ]);
}

// ---- review of the true microphone: chat ids, via, the window ----

/// Finding 3: a typed row keeps its id (`t<echo>`) when the session file's copy takes its place.
#[test]
fn a_typed_row_keeps_its_id_when_the_session_catches_up() {
    let mut h = History::default();
    let sent = at(10);
    h.typed(vec![("echo-keep".into(), ChatEntry { at_ms: sent, kind: ChatKind::User("keep my id".into()) })], &[], sent);
    assert_eq!(ids(&h.rows_with_ids(day_start(DAY)).0), ["techo-keep"]);
    h.entry(&user(sent + 900, "[Heimdall typed message]\nkeep my id"), day_start(DAY));
    h.entry(&user(sent + 950, "[Heimdall typed message]\nkeep my id"), day_start(DAY)); // the same words, said again
    h.typed(vec![], &[], sent + 1000);
    let rows = h.rows_with_ids(day_start(DAY)).0;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].id, "techo-keep", "the session's copy (the newest such row) carries the typed row's id");
    assert!(rows[0].id.starts_with(&format!("s{}.", sent + 900)), "the other is its own row: {:?}", ids(&rows));
}

/// Finding 10: a session row's id does not depend on its place among rows of the same time; true duplicates get a
/// suffix.
#[test]
fn session_row_ids_do_not_depend_on_their_place() {
    let t = at(5);
    let read = |entries: &[Value]| {
        let mut h = History::default();
        for e in entries { h.entry(e, day_start(DAY)); }
        h.rows_with_ids(day_start(DAY)).0.into_iter().map(|r| (format!("{:?}", r.entry.kind), r.id)).collect::<std::collections::BTreeMap<_, _>>()
    };
    let a = user(t, "[Heimdall typed message]\nfirst");
    let b = user(t, "[Heimdall typed message]\nsecond");
    assert_eq!(read(&[a.clone(), b.clone()]), read(&[b, a]), "the same ids in either order");
    let mut h = History::default();
    for _ in 0..2 { h.entry(&user(t, "[Heimdall typed message]\nsame"), day_start(DAY)); }
    let ids = ids(&h.rows_with_ids(day_start(DAY)).0);
    assert_eq!(ids[1], format!("{}-2", ids[0]), "a true duplicate is suffixed: {ids:?}");
}

/// Finding 4: a phone-typed row stays `phone_typed` once decided, after the phone's list forgot it and after a
/// restart (the marks are kept for the day in the data folder).
#[test]
fn a_phone_typed_row_stays_phone_typed() {
    let d = tempfile::tempdir().unwrap();
    let words = "typed on the phone, finding four";
    from_phone_at("echo-finding-4", words, at(3));
    write_day(d.path(), &[user(at(3), &format!("[Heimdall delivery: f]\n[Heimdall typed message]\n{words}"))], &[]);
    let mut l = Live { history: History::default(), session: None, offset: 0, partial: vec![], day: 0, sent: vec![] };
    l.refresh(d.path(), at(20));
    let first = l.history.rows_with_ids(l.day).0;
    l.save_marks(d.path());
    assert_eq!(first[0].via, Some("phone_typed"));
    // The phone's list forgets it (pruned, or a restart).
    phone_typed().lock().unwrap().retain(|(e, _, _)| e != "echo-finding-4");
    assert_eq!(l.history.rows_with_ids(l.day).0[0].via, Some("phone_typed"), "decided once, kept");
    let (_, again, _) = read_once_rows(d.path(), at(21));
    assert_eq!((again[0].via, &again[0].id), (Some("phone_typed"), &first[0].id), "after a restart: the marks file");
    let (_, next_day, _) = read_once_rows(d.path(), at(20) + 24 * 3_600_000);
    assert!(next_day.is_empty() && load_marks(d.path(), day_start(at(20) + 24 * 3_600_000)).is_empty(), "the marks are the day's only");
}

/// Finding 9: a row that only rolled out of the window is not removed on the phone; one really gone is. The live
/// `chat` is capped at `remote::CHAT_BYTES` as a new client's is.
#[cfg(feature = "phone")]
#[test]
fn a_row_rolled_out_of_the_window_is_not_removed_and_the_live_chat_is_capped() {
    let row = |id: &str, at: u64, t: &str| ChatRow { id: id.into(), via: Some("mac_typed"), entry: ChatEntry { at_ms: at, kind: ChatKind::User(t.into()) } };
    let old = vec![row("a", 1, "a"), row("b", 2, "b"), row("c", 3, "c")];
    let new = vec![row("b", 2, "b"), row("c", 3, "c"), row("d", 4, "d")];
    let kinds: Vec<&str> = chat_events(Some(DAY), &old, DAY, &new, 1).iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, ["chat_upsert"], "a rolled out: no removal");
    let gone = vec![row("a", 1, "a"), row("c", 3, "c")];
    let events = chat_events(Some(DAY), &old, DAY, &gone, 0);
    assert_eq!((events[0].0, events[0].1["id"].as_str()), ("chat_remove", Some("b")), "a row really gone is removed");
    let big = "x".repeat(4_000);
    let many: Vec<ChatRow> = (0..300).map(|k| row(&format!("r{k}"), k, &big)).collect();
    let snap = &chat_events(None, &[], DAY, &many, 2)[0].1;
    let kept = snap["rows"].as_array().unwrap().len();
    assert!(kept < 300 && snap.to_string().len() <= crate::remote::CHAT_BYTES + 4_096, "{kept} rows, {} bytes", snap.to_string().len());
    assert_eq!(snap["older"].as_u64(), Some((300 - kept) as u64 + 2));
}
