//! Goals shadow tests: synthetic sessions and a scratch islands folder, simulated time, never a model call.
use super::ops::{self, Op};
use super::shadow::{Answer, Shadow};
use super::stream::{render, Kind, Stream};
use crate::islands::files::Root;
use serde_json::{json, Map, Value};
use std::io::Write;

const T0: i64 = 1_793_440_800_000; // 2026-10-31 10:00 CET
const S: i64 = 1000;

fn iso(ms: i64) -> String { chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339() }

fn owner(id: &str, parent: Option<&str>, at: i64, text: &str) -> Value {
    json!({"type":"message","id":id,"parentId":parent,"timestamp":iso(at),
           "message":{"role":"user","content":[{"type":"text","text":format!("[Heimdall delivery: d-{id}]\n[Now: Sat 10:00]\n[Mic · User] {text}")}]}})
}
fn heimdall(id: &str, parent: &str, at: i64, text: &str) -> Value {
    json!({"type":"message","id":id,"parentId":parent,"timestamp":iso(at),"message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":text}]}})
}
fn task_done(id: &str, parent: &str, at: i64, text: &str) -> Value {
    json!({"type":"custom_message","id":id,"parentId":parent,"timestamp":iso(at),"customType":"heimdall_context","content":format!("[Heimdall notice: terminal:th-1]\n{text}")})
}

struct Lab { _d: tempfile::TempDir, root: Root, dir: std::path::PathBuf }
fn lab() -> Lab {
    let d = tempfile::tempdir().unwrap();
    let root = Root(d.path().join("fi"));
    root.ensure().unwrap();
    let dir = d.path().join("data/goals");
    Lab { root, dir, _d: d }
}
fn goals(root: &Root) -> Map<String, Value> { root.read("goals").unwrap() }
fn op(v: Value) -> Op { Op::from_value(&v) }

#[test]
fn the_stream_renders_what_the_agent_got_and_drops_images_bookkeeping_and_the_refreshers_notices() {
    let long = "x".repeat(2000);
    let entries = [
        owner("e1", None, T0, "Ship the opening film today."),
        json!({"type":"message","id":"e2","parentId":"e1","timestamp":iso(T0 + S),"message":{"role":"assistant","content":[
            {"type":"text","text":"On it."},{"type":"toolCall","name":"mcp__heimdall__delegate_task","arguments":{"title":"Film"}}]}}),
        json!({"type":"message","id":"e3","parentId":"e2","timestamp":iso(T0 + 2 * S),"message":{"role":"toolResult","toolName":"read","content":[
            {"type":"text","text":long},{"type":"image","data":"AAAA","mimeType":"image/png"}]}}),
        json!({"type":"custom","id":"e4","parentId":"e3","customType":"heimdall_receipt","data":{}}),
        json!({"type":"message","id":"e5","parentId":"e4","timestamp":iso(T0 + 3 * S),"message":{"role":"user","content":"[Heimdall notice: plan-refresh-1]\nPlan: added \"X\"."}}),
        task_done("e6", "e5", T0 + 4 * S, "Task \"Film\" finished: final cut exported."),
        json!({"type":"custom_message","id":"e7","parentId":"e6","timestamp":iso(T0 + 5 * S),"customType":"heimdall_context","content":[{"type":"text","text":"[Heimdall notice: islands-context:1]\nOwner ticked 'Privacy scan' in Goals."}]}),
    ];
    let lines: Vec<_> = entries.iter().filter_map(|e| render(e, 0)).collect();
    assert_eq!(lines.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["e1", "e2", "e3", "e6", "e7"]);
    assert_eq!(lines[0].lines, ["Incoming: [Mic · User] Ship the opening film today."], "delivery and clock headers go");
    assert_eq!(lines[0].kind, Kind::Owner);
    assert_eq!(lines[0].at, T0);
    assert_eq!(lines[1].lines, ["Heimdall: On it.", "Heimdall called delegate_task {\"title\":\"Film\"}"]);
    assert!(!lines[1].text().contains("secret"), "thinking never");
    assert!(lines[2].text().chars().count() < 330 && !lines[2].text().contains("AAAA"), "long tool output cut, image dropped");
    assert_eq!((lines[3].kind, lines[4].kind), (Kind::Task, Kind::Notice));
    assert!(lines[3].text().starts_with("Notice: [Heimdall notice: terminal:th-1]"));
}

#[test]
fn the_relays_boilerplate_and_a_silent_turn_never_reach_the_stream() {
    let relayed = "[Relayed: this reached the relay agent.] Transcript:\n[20:01:02 Mic · User] Ship it.\nOwner's wake instruction:\nAct on User's latest request to you in the transcript, if there is one. Everyone else is context.";
    let l = render(&owner("e1", None, T0, relayed), 0).unwrap();
    assert!(l.text().contains("Ship it.") && !l.text().contains("wake instruction") && !l.text().contains("Everyone else is context"));
    let silent = json!({"type":"message","id":"e2","parentId":"e1","timestamp":iso(T0 + S),"message":{"role":"assistant","content":[{"type":"text","text":"NO_REPLY"}]}});
    assert!(render(&silent, 0).is_none(), "a turn that said nothing is not a line");
}

#[test]
fn the_relays_own_words_are_heimdalls_and_its_silent_turns_say_nothing() {
    let note = |id: &str, text: &str| json!({"type":"custom_message","id":id,"timestamp":iso(T0),"customType":"heimdall_context",
        "content":[{"type":"text","text":format!("[Heimdall notice: emergency-cover:mirror:mcp_1:{id}:0]\n{text}")}]});
    let said = render(&note("4", "[Relay agent] The Homebrew tap is published and tested.\nNext: the site."), 0).unwrap();
    assert_eq!(said.lines, ["Heimdall: The Homebrew tap is published and tested.", "  Next: the site."]);
    assert_eq!(said.kind, Kind::Heimdall);
    assert!(render(&note("5", "[Relay agent] NO_REPLY"), 0).is_none(), "a silent relay turn is not a line");
    let ran = render(&note("6", "[Relay agent ran] git log -1 → completed"), 0).unwrap();
    assert_eq!((ran.lines.as_slice(), ran.kind), (["Heimdall ran: git log -1 → completed".to_owned()].as_slice(), Kind::Tool));
    let own = render(&note("7", "[Relay agent] said to User: \"Arsenal's tap is live.\" → ok"), 0).unwrap();
    assert_eq!(own.kind, Kind::Heimdall);
    // a call of Heimdall's own tools (`record`, an id of its own) is a tool line, even a failed one
    let call = json!({"type":"custom_message","id":"9","timestamp":iso(T0),"customType":"heimdall_context",
        "content":[{"type":"text","text":"[Heimdall notice: emergency-cover:5f0c]\n[Relay agent] plan add 'Tap' → failed: no list"}]});
    let call = render(&call, 0).unwrap();
    assert_eq!((call.lines.as_slice(), call.kind), (["Heimdall called plan add 'Tap' → failed: no list".to_owned()].as_slice(), Kind::Tool));
    // Heimdall's words to the relay are still a notice
    assert_eq!(render(&note("8", "[Heimdall to the relay agent] Stop acting now."), 0).unwrap().kind, Kind::Notice);
}

#[test]
fn the_stream_cursor_survives_a_pi_compaction_and_a_restart_with_a_new_session_file() {
    let l = lab();
    let path = l.dir.join("stream.jsonl");
    let (mut s, _) = Stream::open(&path).unwrap();
    let mut entries = vec![owner("e1", None, T0, "one"), heimdall("e2", "e1", T0 + S, "two")];
    assert_eq!(s.ingest(&entries, T0, None).unwrap().len(), 2);
    // Pi compacts: a compaction entry on the same branch, then more.
    entries.push(json!({"type":"compaction","id":"c1","parentId":"e2","firstKeptEntryId":"e2","summary":"sum"}));
    entries.push(owner("e3", Some("c1"), T0 + 2 * S, "three"));
    let new = s.ingest(&entries, T0, None).unwrap();
    assert_eq!(new.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["e3"]);
    // Restart: the stream file is the cursor. A new session file that starts over (a reload) repeats nothing.
    let (mut s, all) = Stream::open(&path).unwrap();
    assert_eq!(all.len(), 3);
    let next = vec![owner("e3", None, T0 + 2 * S, "three"), owner("f1", Some("e3"), T0 + 3 * S, "four")];
    assert_eq!(s.ingest(&next, T0, None).unwrap().iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["f1"]);
    // An entry stamped after `upto` waits (the lab's simulated clock).
    let later = vec![owner("f2", Some("f1"), T0 + 100 * S, "five")];
    assert!(s.ingest(&later, T0, Some(T0 + 50 * S)).unwrap().is_empty());
    assert_eq!(s.ingest(&later, T0, Some(T0 + 100 * S)).unwrap().len(), 1);
}

#[test]
fn ops_whose_quote_is_not_in_the_new_lines_are_dropped() {
    let mut f = ops::fresh();
    f.insert("goals".into(), Value::Array(ops::seed_goals()));
    let lines = "10:00 Incoming: [Mic · User] Okay, the privacy scan passed — good.";
    let out = ops::apply(&mut f, &[
        op(json!({"op":"progress","goal":"g-heimdall-public","title":"Privacy scan","state":"done","now":"Privacy scan passed","quote":"the Privacy-scan passed, good"})),
        op(json!({"op":"progress","goal":"g-heimdall-public","title":"Signing","state":"done","quote":"signing works"})),
        op(json!({"op":"progress","goal":"g-heimdall-public","title":"Updater","state":"done","quote":""})),
        op(json!({"op":"progress","goal":"g-heimdall-public","title":"Tiny","state":"done","quote":"ok"})),
    ], &ops::Evidence::text(lines), 100);
    assert_eq!(out.iter().map(|o| o.kept).collect::<Vec<_>>(), [true, false, false, false], "{out:?}");
    assert_eq!(out[1].reason, "quote not in the new lines");
    assert_eq!(out[2].reason, "no quote");
    let g = &f["goals"][2];
    assert_eq!((g["now"].as_str(), g["moved"].as_i64()), (Some("Privacy scan passed"), Some(100)));
    assert_eq!(g["tasks"][0]["title"], "Privacy scan");
    assert_eq!((g["tasks"][0]["state"].as_str(), g["tasks"][0]["at"].as_i64()), (Some("done"), Some(100)));
    assert!(g["tasks"][0]["id"].as_str().unwrap().starts_with("t-"));
}

#[test]
fn caps_unknown_goals_and_tasks_are_enforced_in_code() {
    let mut f = ops::fresh();
    f.insert("goals".into(), Value::Array(ops::seed_goals()));
    let lines = "my goal this month is a long title that goes on well past forty characters; and a second; and a third";
    let q = "my goal this month";
    let out = ops::apply(&mut f, &[
        op(json!({"op":"add_goal","title":"A long title that goes on well past forty characters","icon":"Not A Symbol","now":"n".repeat(80),"quote":q})),
        op(json!({"op":"add_goal","title":"Second big goal","icon":"star","quote":q})),
        op(json!({"op":"add_goal","title":"Seventh goal","quote":q})),
        op(json!({"op":"add_goal","title":"get a JOB","quote":q})),
        op(json!({"op":"progress","goal":"g-nope","title":"X","state":"now","quote":q})),
        op(json!({"op":"progress","goal":"g-job","task":"t-nope","state":"now","quote":q})),
        op(json!({"op":"progress","goal":"g-job","title":"X","state":"late","quote":q})),
        op(json!({"op":"ask","goal":"g-nope","title":"Pick","quote":q})),
        op(json!({"op":"ask","goal":"g-job","title":"Pick the CV photo","detail":"d".repeat(90),"kind":"pick","quote":q})),
        op(json!({"op":"ask","goal":"","title":"pick the CV photo!","quote":q})),
        op(json!({"op":"answered","ask":"a-nope","quote":q})),
        op(json!({"op":"rename","quote":q})),
    ], &ops::Evidence::text(lines), 7);
    let kept: Vec<(bool, &str)> = out.iter().map(|o| (o.kept, o.reason.as_str())).collect();
    assert!(kept[0].0 && kept[1].0, "{kept:?}");
    assert_eq!(kept[2], (false, "6 active goals already"));
    assert_eq!(kept[3], (false, "a goal of that title exists"));
    assert_eq!(kept[4], (false, "unknown goal g-nope"));
    assert_eq!(kept[5], (false, "unknown task t-nope"));
    assert!(!kept[6].0 && kept[6].1.contains("not done, now or next"));
    // Asks are never the shadow's: what needs him comes as a card.
    assert!((7..=9).all(|i| !kept[i].0 && kept[i].1.contains("card")), "{kept:?}");
    assert!(!kept[10].0 && !kept[11].0);
    // New goals wait for the user's yes (2 Nov): proposed, not added.
    let g = f["goals"].as_array().unwrap();
    assert_eq!(g.len(), 4);
    let queued = crate::islands::proposals::queue(&f);
    assert_eq!(queued.len(), 2);
    let added = &queued[0];
    assert!(added["title"].as_str().unwrap().chars().count() <= ops::TITLE_CHARS, "{added:?}");
    assert_eq!((added.get("icon"), queued[1]["icon"].as_str()), (None, Some("star")), "an undrawable icon is left out (the flag on a yes)");
    assert_eq!((added["kind"].as_str(), added["front"].as_i64(), queued[1].get("front").is_none()), (Some("goal"), Some(7), true));
    assert!(f.get("items").and_then(Value::as_array).is_none_or(|a| a.is_empty()));
    // A paused goal moving again comes back, unless six are active.
    let mut f2 = ops::fresh();
    f2.insert("goals".into(), json!([{"id":"g-a","title":"A","state":"paused","tasks":[]}]));
    let r2 = ops::apply(&mut f2, &[op(json!({"op":"progress","goal":"g-a","title":"T","state":"now","quote":q}))], &ops::Evidence::text(lines), 1);
    assert!(r2[0].kept, "{r2:?}");
    assert_eq!(f2["goals"][0]["state"], "active");
    // A task id under another goal is not this goal's.
    let mut f3 = ops::fresh();
    f3.insert("goals".into(), json!([{"id":"g-a","title":"A","tasks":[{"id":"t-1","title":"T","state":"now"}]},{"id":"g-b","title":"B","tasks":[]}]));
    let r = ops::apply(&mut f3, &[op(json!({"op":"progress","goal":"g-b","task":"t-1","state":"done","quote":q}))], &ops::Evidence::text(lines), 1);
    assert_eq!(r[0].reason, "task t-1 is under another goal");
}

#[test]
fn apply_keeps_owners_ticks_and_unknown_keys_and_asks_he_answered_by_click_leave() {
    let l = lab();
    l.root.put("goals", json!({"preset":"todo","list":"goals","title":"Goals","mine":1,
        "items":[{"id":"a-1","title":"Pick a cut","chosen":"yes"},{"id":"a-2","title":"Test on iPhone","done":true},
                 {"id":"a-3","title":"Later one","chosen":"later"},{"id":"a-4","title":"Open","extra":true}],
        "goals":[{"id":"g-a","title":"Ship A","state":"active","x":2,"expanded":true,
                  "tasks":[{"id":"t-1","title":"Scan","state":"done","ticked":true,"at":5},{"id":"t-2","title":"Sign","state":"now"}]}]}).as_object().unwrap()).unwrap();
    let lines = "User: actually the scan needs another pass; signing is done; drop the open thing";
    let (out, after) = ops::apply_file(&l.root, &[
        op(json!({"op":"progress","goal":"g-a","task":"t-1","state":"now","quote":"the scan needs another pass"})),
        op(json!({"op":"progress","goal":"g-a","task":"t-2","state":"done","now":"Signing works","quote":"signing is done"})),
        op(json!({"op":"drop_task","goal":"g-a","task":"t-1","quote":"the scan needs another pass"})),
        op(json!({"op":"drop_ask","ask":"a-4","quote":"drop the open thing"})),
    ], &ops::Evidence::text(lines), 50).unwrap();
    assert_eq!(out.iter().map(|o| o.kept).collect::<Vec<_>>(), [false, true, false, true]);
    assert_eq!(out[0].reason, "User ticked it done");
    let f = goals(&l.root);
    assert_eq!(f, after);
    assert_eq!((f["mine"].as_i64(), f["goals"][0]["x"].as_i64(), f["goals"][0]["expanded"].as_bool()), (Some(1), Some(2), Some(true)));
    assert_eq!(f["goals"][0]["tasks"][0], json!({"id":"t-1","title":"Scan","state":"done","ticked":true,"at":5}));
    assert_eq!((f["goals"][0]["tasks"][1]["state"].as_str(), f["goals"][0]["now"].as_str()), (Some("done"), Some("Signing works")));
    let asks: Vec<&str> = f["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(asks, ["a-3"], "chosen and ticked asks left; later stays; the dropped one went");
    // Nothing to change: the file is not rewritten.
    let ino = || std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(l.root.file("goals")).unwrap());
    let before = ino();
    ops::apply_file(&l.root, &[], &ops::Evidence::text(""), 60).unwrap();
    assert_eq!(ino(), before);
}

#[test]
fn first_run_seeds_four_goals_and_migrates_waiting_todo_items_once_after_a_backup() {
    let l = lab();
    let todo = json!({"preset":"todo","list":"todo","title":"To do","items":[
        {"id":"w1","title":"Approve the voice","group":"waiting","kind":"approve","detail":"Two takes","proposal":{"text":"t"}},
        {"id":"w2","title":"Old ticked","group":"waiting","done":true},
        {"id":"d1","title":"Discussed thing","group":"discussed"}]});
    l.root.put("todo", todo.as_object().unwrap()).unwrap();
    let did = ops::ensure(&l.root, 9, true).unwrap();
    assert_eq!(did.len(), 3, "{did:?}");
    let f = goals(&l.root);
    assert_eq!((f["preset"].as_str(), f["list"].as_str(), f["title"].as_str(), f["icon"].as_str()), (Some("todo"), Some("goals"), Some("Goals"), Some("flag")));
    let titles: Vec<&str> = f["goals"].as_array().unwrap().iter().map(|g| g["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Get a job", "Ship Arsenal", "Ship Heimdall publicly", "Make Jarvis efficient"]);
    let icons: Vec<&str> = f["goals"].as_array().unwrap().iter().map(|g| g["icon"].as_str().unwrap()).collect();
    assert_eq!(icons, ["briefcase", "shippingbox", "eye", "waveform"]);
    assert!(f["goals"].as_array().unwrap().iter().all(|g| g["tasks"] == json!([])));
    let asks = f["items"].as_array().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0]["id"], "w1");
    assert!(asks[0].get("group").is_none() && asks[0]["goal"] == "" && asks[0]["kind"] == "approve" && asks[0]["proposal"]["text"] == "t");
    assert!(l.root.read("todo").is_none(), "todo.json is gone so the dock shows Goals in its place");
    let kept = crate::islands::files::read_object(&l.root.islands().join(".todo-migrated.json")).unwrap();
    assert_eq!(Value::Object(kept)["items"], todo["items"]);
    let backups: Vec<_> = std::fs::read_dir(&l.root.0).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("backup-goals-")).collect();
    assert_eq!(backups.len(), 1);
    assert!(backups[0].path().join("todo.json").exists());
    // Once: a second start changes nothing, and a To do some old writer brought back is not migrated again.
    assert!(ops::ensure(&l.root, 10, true).unwrap().is_empty());
    l.root.put("todo", json!({"items":[{"id":"z","title":"Z","group":"waiting"}]}).as_object().unwrap()).unwrap();
    assert!(ops::ensure(&l.root, 11, true).unwrap().is_empty());
    assert_eq!(goals(&l.root)["items"].as_array().unwrap().len(), 1);
    // The public edition starts empty.
    let p = lab();
    ops::ensure(&p.root, 1, false).unwrap();
    assert_eq!(goals(&p.root)["goals"], json!([]));
}

fn write_session(path: &std::path::Path, entries: &[Value]) {
    let mut f = std::fs::File::create(path).unwrap();
    for e in entries { writeln!(f, "{e}").unwrap(); }
}

#[test]
fn a_day_in_simulated_time_calls_on_owners_quiet_minute_applies_ops_and_keeps_the_turn() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    // The first lines on a fresh start are the past: the header's, not news.
    assert_eq!(s.ingest(&[owner("p1", None, T0 - 600 * S, "Earlier words.")], T0, None).unwrap(), 0);
    assert!(s.segment.header.contains("Earlier words.") && s.segment.header.contains("g-heimdall-public"));
    let day = [
        owner("e1", Some("p1"), T0 + 10 * S, "Let's do the privacy scan for Heimdall now."),
        heimdall("e2", "e1", T0 + 12 * S, "Starting the privacy scan."),
        task_done("e3", "e2", T0 + 300 * S, "Task \"Privacy scan\" finished: no secrets found, the scan passed."),
    ];
    s.ingest(&day[..2], T0 + 12 * S, None).unwrap();
    assert_eq!(s.due(), Some(T0 + 70 * S), "a minute after User's words");
    assert!(s.request(T0 + 69 * S).is_none());
    let req = s.request(T0 + 70 * S).unwrap();
    assert_eq!(req.why.as_str(), "owner");
    assert_eq!(req.turn, 0);
    assert!(req.stable.starts_with("<goals_at_start") && req.stable.contains("Let's do the privacy scan"));
    assert!(req.volatile.contains("<goals_now>") && req.volatile.contains("Now: "));
    let answer = json!({"ops":[{"quote":"privacy scan for Heimdall now","op":"progress","goal":"g-heimdall-public","task":"","title":"Privacy scan","state":"now","now":"Privacy scan under way","kind":"","detail":"","ask":"","icon":""}]}).to_string();
    let done = s.settle(&req, Answer::Text(answer.clone()), T0 + 72 * S).unwrap().unwrap();
    assert!(done.outcomes[0].kept && done.ended.is_none() && done.malformed.is_none());
    let task = goals(&l.root)["goals"][2]["tasks"][0].clone();
    assert_eq!((task["state"].as_str(), task["at"].as_i64()), (Some("now"), Some(T0 / 1000 + 10)), "dated by its quoted line, not the call");
    assert_eq!(s.segment.turns.len(), 1);
    assert_eq!(s.segment.turns[0].assistant, answer);
    assert!(s.pending.is_empty() && s.due().is_none());
    // The task's notice: 20 s later, its id in the Goals now, the turn after the first one carries no header.
    s.ingest(&day[2..], T0 + 300 * S, None).unwrap();
    let req = s.request(T0 + 320 * S).unwrap();
    assert_eq!((req.why.as_str(), req.turn, req.history.len()), ("task", 1, 1));
    assert!(req.stable.starts_with("<new_lines>"));
    let tid = task["id"].as_str().unwrap();
    assert!(req.volatile.contains(&format!("now {tid} Privacy scan")), "{}", req.volatile);
    // A failed call keeps its lines and retries; a restart keeps the segment and what is still pending.
    s.settle(&req, Answer::Retry("boom".into()), T0 + 321 * S).unwrap();
    assert_eq!(s.pending.len(), 1);
    let s2 = Shadow::open(&l.dir, &l.root, T0 + 400 * S, true).unwrap();
    assert_eq!((s2.segment.turns.len(), s2.pending.len()), (1, 1));
    assert!(s2.due().is_some());
    // A malformed answer applies nothing and keeps a well-formed turn.
    let mut s = s2;
    let req = s.request(T0 + 400 * S).unwrap();
    let done = s.settle(&req, Answer::Text("I think the scan passed".into()), T0 + 401 * S).unwrap().unwrap();
    assert!(done.malformed.is_some() && done.outcomes.is_empty());
    assert_eq!(s.segment.turns[1].assistant, "{\"ops\":[]}");
}

#[test]
fn a_segment_past_its_budget_compacts_into_a_header_of_the_goals_and_the_last_lines() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    s.compact_tokens = super::segment::tokens(super::shadow::fixed_bytes()) + 50;
    let old_key = s.segment.key();
    s.ingest(&[owner("e1", None, T0, &"We finished the signing step today. ".repeat(10))], T0, None).unwrap();
    let req = s.request(T0 + 60 * S).unwrap();
    let answer = json!({"ops":[{"quote":"We finished the signing step","op":"progress","goal":"g-heimdall-public","task":"","title":"Signing","state":"done","now":"Signing works","kind":"","detail":"","ask":"","icon":""}]}).to_string();
    let done = s.settle(&req, Answer::Text(answer), T0 + 61 * S).unwrap().unwrap();
    assert_eq!(done.ended.as_deref(), Some(old_key.as_str()), "the old segment's session ends");
    assert_ne!(s.segment.key(), old_key);
    assert!(s.segment.turns.is_empty());
    assert!(s.segment.header.contains("done t-") && s.segment.header.contains("Signing"), "finished tasks are the memory: {}", s.segment.header);
    assert!(s.segment.header.contains("<earlier_lines") && s.segment.header.contains("We finished the signing step"));
    // Saved: a restart continues in the new segment.
    assert_eq!(Shadow::open(&l.dir, &l.root, T0 + 62 * S, true).unwrap().segment.key(), s.segment.key());
}

#[test]
fn the_call_carries_the_turns_as_history_and_the_segment_as_its_session() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    s.ingest(&[owner("e1", None, T0, "first")], T0, None).unwrap();
    let r = s.request(T0 + 60 * S).unwrap();
    let sent = super::call("claude-haiku-5-5", &r).prompt;
    s.settle(&r, Answer::Text("{\"ops\":[]}".into()), T0 + 61 * S).unwrap();
    s.ingest(&[owner("e2", Some("e1"), T0 + 200 * S, "second")], T0, None).unwrap();
    let r = s.request(T0 + 260 * S).unwrap();
    let c = super::call("claude-haiku-5-5", &r);
    // The resent turn has the very blocks the first call sent, so the provider's prefix cache still matches it.
    assert_eq!(c.history[0].user, sent);
    assert_eq!((c.job, c.effort, c.session.as_deref()), ("goals", super::EFFORT, Some(s.segment.key().as_str())));
    assert_eq!(c.history.len(), 1);
    assert!(c.history[0].user.text().contains("first") && c.history[0].assistant == "{\"ops\":[]}");
    assert_eq!(c.prompt.blocks.len(), 2);
    assert!(c.prompt.blocks[0].cache && c.prompt.blocks[0].text.contains("second"), "the new lines are stable");
    assert!(!c.prompt.blocks[1].cache && c.prompt.blocks[1].text.contains("<goals_now>"));
    assert_eq!(c.schema, ops::schema());
    assert!(super::shadow::SYSTEM.len() < 6_000 * 3, "goals.md v0 stays under 6,000 tokens");
}

#[test]
fn the_schema_requires_every_key_and_quotes_first() {
    let s = ops::schema();
    let item = &s["properties"]["ops"]["items"];
    let props: Vec<&String> = item["properties"].as_object().unwrap().keys().collect();
    assert_eq!(item["required"].as_array().unwrap().len(), props.len());
    assert_eq!(item["required"][0], "quote");
    assert_eq!(item["properties"]["op"]["enum"].as_array().unwrap().len(), ops::MODEL_OPS.len());
    // Every op the model may write is one the prompt names and the code applies.
    for o in ops::MODEL_OPS { assert!(super::shadow::SYSTEM.contains(&format!("`{o}`")) && ops::OPS.contains(&o), "{o} in goals.md") }
    // The public edition's prompt too (review 2 Nov: it still taught asks and no reports).
    let public = include_str!("../../prompt/public/goals.md");
    for o in ops::MODEL_OPS { assert!(public.contains(&format!("`{o}`")), "{o} in public/goals.md") }
    for k in ["`reported`", "<heimdall_reports>", "`checked`"] { assert!(public.contains(k) && super::shadow::SYSTEM.contains(k), "{k}") }
    assert!(!public.contains("User") && !public.contains("`ask`"));
    assert!(ops::parse("```json\n{\"ops\": []}\n```").unwrap().is_empty());
    assert!(ops::parse("{\"todo\": []}").is_err());
}

#[test]
#[ignore = "needs the lab/ folder (goals mock runner), which is not in the public repository"]
fn goals_lab_replays_a_window_through_a_runner_into_its_own_folder() {
    let d = tempfile::tempdir().unwrap();
    let session = d.path().join("s.jsonl");
    write_session(&session, &[
        owner("p1", None, T0 - 60 * S, "Before the window."),
        owner("e1", Some("p1"), T0 + 10 * S, "Heimdall's privacy scan passed, finally."),
        heimdall("e2", "e1", T0 + 20 * S, "Great."),
        owner("e3", Some("e2"), T0 + 4000 * S, "After the window."),
    ]);
    let answers = d.path().join("answers.json");
    std::fs::write(&answers, json!({"privacy scan passed": {"ops":[
        {"quote":"privacy scan passed","op":"progress","goal":"g-heimdall-public","task":"","title":"Privacy scan","state":"done","now":"Privacy scan passed","kind":"","detail":"","ask":"","icon":""},
        {"quote":"not said anywhere","op":"add_goal","goal":"","task":"","title":"Invented","state":"","now":"","kind":"","detail":"","ask":"","icon":"star"}]}}).to_string()).unwrap();
    let runner = format!("GOALS_MOCK_ANSWERS={} python3 -I {}/../../lab/goals/mock-runner.py", answers.display(), env!("CARGO_MANIFEST_DIR"));
    let out = d.path().join("out");
    let summary = super::lab::replay(&super::lab::Replay {
        session, from: T0, to: T0 + 3600 * S, seed: "default".into(), runner, out: out.clone(), effort: "low".into(), compact_tokens: None, seed_settled: None,
    }).unwrap();
    assert_eq!(summary["calls"], 1);
    let turns: Vec<Value> = std::fs::read_to_string(out.join("turns.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(turns.len(), 1);
    let t = &turns[0];
    assert_eq!((t["seg"].as_str().is_some(), t["turn"].as_i64(), t["trigger"].as_str()), (true, Some(0), Some("owner")));
    assert_eq!(t["new_lines"].as_array().unwrap().len(), 2, "the window's lines, not the past nor the future");
    assert!(t["user"].as_str().unwrap().contains("Before the window."), "the past opens the header");
    assert_eq!(t["ops"].as_array().unwrap().iter().map(|o| o["kept"].as_bool().unwrap()).collect::<Vec<_>>(), [true, false]);
    assert_eq!(t["ops"][1]["reason"], "quote not in the new lines");
    assert_eq!(t["goals_after"]["goals"][2]["tasks"][0]["title"], "Privacy scan");
    assert!(t["usage"]["cache_read"].as_u64().unwrap() > 0 && t["segment_tokens"].as_u64().unwrap() > 0);
    assert_eq!(summary["goals"]["goals"][2]["now"], "Privacy scan passed");
    assert!(out.join("final.json").exists() && out.join("islands-root/islands/goals.json").exists());
    let lines = super::lab::stream_lines(&d.path().join("s.jsonl"), T0, T0 + 3600 * S).unwrap();
    assert_eq!(lines.len(), 2);
}

/// Review 31 Oct: an app that deletes a closed list, then a writer (the shadow, plan_edit, a restart's seeding) used to
/// start from nothing, overwrite both copies, and lose every goal.
#[test]
fn a_list_closed_by_deleting_its_file_is_written_on_from_its_copy_and_comes_back_whole() {
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let q = "pick the cut now";
    ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-arsenal","title":"Homebrew tap","state":"now","quote":q}))], &ops::Evidence::text(q), 2).unwrap();
    l.root.remove("goals").unwrap();
    crate::islands::plan::keep_closed(&l.root, "goals", None).unwrap();
    // A restart does not seed over it; the shadow's next op lands in the closed list, which stays closed.
    assert!(ops::ensure(&l.root, 3, true).unwrap().is_empty());
    // An ask (plan_edit's) lands in the closed list too.
    crate::islands::plan::edit_list(&l.root, "goals", |its| { its.push(json!({"id": "a-1", "title": "Pick the cut", "goal": ""}).as_object().unwrap().clone()); Ok(()) }).unwrap();
    let f = goals(&l.root);
    assert_eq!(f["hidden"], true);
    assert_eq!((f["goals"].as_array().unwrap().len(), f["goals"][1]["tasks"][0]["title"].as_str(), f["items"].as_array().unwrap().len()), (4, Some("Homebrew tap"), 1));
    assert_eq!(crate::islands::plan::reopen_closed(&l.root).unwrap(), ["goals"]);
    let f = goals(&l.root);
    assert!(f.get("hidden").is_none() && f["goals"][1]["tasks"][0]["title"] == "Homebrew tap" && f["items"][0]["title"] == "Pick the cut");
    // A file that no longer parses is written on from the copy of its last write, not from nothing.
    std::fs::write(l.root.file("goals"), "{ broken").unwrap();
    ops::apply_file(&l.root, &[op(json!({"op":"answered","ask":f["items"][0]["id"].as_str().unwrap(),"quote":q}))], &ops::Evidence::text(q), 5).unwrap();
    assert_eq!(goals(&l.root)["goals"].as_array().unwrap().len(), 4);
    // An answered op naming no ask removes nothing.
    assert_eq!(ops::apply(&mut goals(&l.root), &[op(json!({"op":"answered","ask":"","quote":q}))], &ops::Evidence::text(q), 6)[0].reason, "no ask");
}

#[test]
fn a_run_refused_or_paid_without_an_answer_sets_its_lines_aside_and_starts_the_context_over() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    s.ingest(&[owner("e1", None, T0, "first")], T0, None).unwrap();
    let r = s.request(T0 + 60 * S).unwrap();
    s.settle(&r, Answer::Text("{\"ops\":[]}".into()), T0 + 61 * S).unwrap();
    let key = s.segment.key();
    s.ingest(&[owner("e2", Some("e1"), T0 + 200 * S, "second")], T0, None).unwrap();
    let r = s.request(T0 + 260 * S).unwrap();
    let done = s.settle(&r, Answer::Settled("over the $0.50 limit".into()), T0 + 261 * S).unwrap().unwrap();
    assert_eq!(done.ended.as_deref(), Some(key.as_str()));
    assert!(s.pending.is_empty() && s.segment.turns.is_empty() && s.segment.header.contains("second"));
}

#[test]
fn owners_words_delivered_with_a_notice_are_his() {
    let mixed = json!({"type":"message","id":"m","timestamp":iso(T0),"message":{"role":"user","content":
        "[Heimdall notice: terminal:th-2]\nTask finished.\n[Mic · User] Great, now the signing."}});
    assert_eq!(render(&mixed, 0).unwrap().kind, Kind::Owner);
    let catchup = json!({"type":"custom_message","id":"c","timestamp":iso(T0),"content":"Heimdall catch-up: what was said since 10:00 …\n[Mic · User] earlier"});
    assert_eq!(render(&catchup, 0).unwrap().kind, Kind::Notice);
}

/// Review 31 Oct, item 1: islands_remove on Goals, then an app's due move writing a fresh goals.json holding only the
/// ask, then a reopen, a shadow write and a restart: every goal and finished task survives, with the ask.
#[test]
fn removing_goals_then_a_fresh_asks_only_file_loses_no_goal_or_finished_task() {
    let l = lab();
    let h = crate::islands::Host::new(l.root.clone(), None);
    ops::ensure(&l.root, 1, true).unwrap();
    let q = "the homebrew tap is published now";
    ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-arsenal","title":"Homebrew tap","state":"done","quote":q}))], &ops::Evidence::text(q), 2).unwrap();
    let line = crate::islands::tools::remove(&h, &json!({"ids": ["goals"]})).unwrap();
    assert!(line.starts_with("Hid 'Goals'"), "{line}");
    assert_eq!(goals(&l.root)["hidden"], true, "hidden, not deleted");
    let check = |f: &Map<String, Value>| {
        assert_eq!(f["goals"].as_array().unwrap().len(), 4, "{f:?}");
        assert_eq!((f["goals"][1]["tasks"][0]["title"].as_str(), f["goals"][1]["tasks"][0]["state"].as_str()), (Some("Homebrew tap"), Some("done")));
        assert!(f["items"].as_array().unwrap().iter().any(|i| i["id"] == "due-1"));
    };
    // The app's due move finds no visible list and writes a fresh one with only the ask.
    crate::islands::files::write_atomic(&l.root.file("goals"), &json!({"id":"goals","preset":"todo","list":"goals","items":[{"id":"due-1","title":"Review the night pass","goal":""}]})).unwrap();
    assert_eq!(crate::islands::plan::reopen_closed(&l.root).unwrap(), ["goals"]);
    check(&goals(&l.root));
    // The same with the file deleted under it and no close reported: the kept copy carries the goals.
    l.root.remove("goals").unwrap();
    crate::islands::files::write_atomic(&l.root.file("goals"), &json!({"list":"goals","items":[{"id":"due-1","title":"Review the night pass"}]})).unwrap();
    ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-job","title":"CV draft","state":"now","quote":q}))], &ops::Evidence::text(q), 3).unwrap();
    check(&goals(&l.root));
    // A restart with the file gone again reseeds nothing: the copies show goals existed.
    l.root.remove("goals").unwrap();
    assert!(!ops::ensure(&l.root, 4, true).unwrap().iter().any(|d| d.contains("seeded")));
    let f = ops::current(&l.root, "goals").unwrap();
    check(&f);
    assert!(f["goals"].as_array().unwrap().iter().all(|g| g["tasks"].as_array().unwrap().len() <= 1), "no reseed added anything");
}

/// Item 2: removing ops need a three-word quote; an answer is the user's (his words or his click); an ask the shadow did
/// not raise leaves only on his word.
#[test]
fn taking_something_away_needs_a_real_quote_and_owner_for_answers() {
    let mut f = ops::fresh();
    f.insert("goals".into(), Value::Array(ops::seed_goals()));
    f.insert("items".into(), json!([{"id":"a-mine","title":"Pick the cut","by":"goals"},{"id":"a-due","title":"Review the night pass"}]));
    let heimdall = "Heimdall: I published the cut and the review is not needed any more, ok";
    let ev = ops::Evidence { all: format!("{heimdall}\nNotice: User chose Yes on the approval 'Pick the cut'."), owner: "Notice: User chose Yes on the approval 'Pick the cut'.".into(), ..Default::default() };
    let r = ops::apply(&mut f.clone(), &[op(json!({"op":"finish_goal","goal":"g-arsenal","quote":"published the"}))], &ev, 1);
    assert!(r[0].reason.contains("three words") || r[0].reason.contains("3 words"), "{:?}", r[0]);
    let r = ops::apply(&mut f.clone(), &[op(json!({"op":"answered","ask":"a-mine","quote":"I published the cut"}))], &ev, 1);
    assert!(!r[0].kept && r[0].reason.contains("User"), "Heimdall's words are no answer: {:?}", r[0]);
    let r = ops::apply(&mut f.clone(), &[op(json!({"op":"answered","ask":"a-mine","quote":"User chose Yes on the approval"}))], &ev, 1);
    assert!(r[0].kept, "his click answers: {:?}", r[0]);
    let r = ops::apply(&mut f.clone(), &[op(json!({"op":"drop_ask","ask":"a-due","quote":"the review is not needed any more"}))], &ev, 1);
    assert!(!r[0].kept, "a Scheduled ask leaves on User's word only: {:?}", r[0]);
    // His plain short yes answers (climb v3: "Yeah, of course." stayed open under the 15-character rule).
    let yes = ops::Evidence { all: "Incoming: [19:58:39 Mic · User] Yeah, of course.".into(), owner: "Incoming: [19:58:39 Mic · User] Yeah, of course.".into(), ..Default::default() };
    assert!(ops::apply(&mut f.clone(), &[op(json!({"op":"answered","ask":"a-mine","quote":"Yeah, of course."}))], &yes, 1)[0].kept);
    assert!(!ops::apply(&mut f.clone(), &[op(json!({"op":"answered","ask":"a-mine","quote":"Yeah,"}))], &yes, 1)[0].kept, "one word is not enough");
    assert!(!ops::apply(&mut f.clone(), &[op(json!({"op":"drop_ask","ask":"a-mine","quote":"Yeah, of course."}))], &yes, 1)[0].kept, "other removals keep the longer rule");
    let r = ops::apply(&mut f, &[op(json!({"op":"drop_ask","ask":"a-mine","quote":"I published the cut"}))], &ev, 1);
    assert!(r[0].kept, "the shadow's own ask may go on the evidence: {:?}", r[0]);
}

/// Item 3: a turn carries the newest lines only; failures back off; a gate refusal waits without backoff.
#[test]
fn a_turn_carries_the_newest_lines_and_failures_back_off() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    let mut entries = vec![owner("e0", None, T0, "start")];
    for n in 1..500 { entries.push(heimdall(&format!("e{n}"), &format!("e{}", n - 1), T0 + n as i64, &format!("line {n}"))); }
    s.ingest(&entries, T0, None).unwrap();
    let r = s.request(T0 + 60 * S).unwrap();
    assert_eq!(r.lines.len(), super::shadow::TURN_LINES);
    assert_eq!(r.lines[0].id, "e100");
    assert_eq!(s.segment.sent.as_deref(), Some("e99"), "the lines set aside are not news after a restart");
    s.settle(&r, Answer::Retry("down".into()), T0 + 60 * S).unwrap();
    assert_eq!(s.due(), Some(T0 + 180 * S));
    let r = s.request(T0 + 180 * S).unwrap();
    s.settle(&r, Answer::Retry("down".into()), T0 + 180 * S).unwrap();
    assert_eq!(s.due(), Some(T0 + 420 * S), "twice the wait");
    let r = s.request(T0 + 420 * S).unwrap();
    s.settle(&r, Answer::Paused, T0 + 420 * S).unwrap();
    assert_eq!(s.due(), Some(T0 + 465 * S), "a gate refusal goes again as soon as calls may (the 45 s gap), no backoff");
    assert_eq!(s.failures, 2);
}

/// Item 4: the stream rotates and what it set aside never comes back.
#[test]
fn the_stream_rotates_and_never_repeats_what_it_set_aside() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    let mut entries = vec![owner("e0", None, T0, "start")];
    for n in 1..5_100 { entries.push(heimdall(&format!("e{n}"), &format!("e{}", n - 1), T0 + n as i64, "x")); }
    s.ingest(&entries, T0, None).unwrap();
    s.forget_pending();
    let s2 = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    let kept = std::fs::read_to_string(l.dir.join("stream.jsonl")).unwrap().lines().count();
    assert!(kept <= super::segment::TAIL_LINES, "rotated to the tail: {kept}");
    let mut s2 = s2;
    assert_eq!(s2.ingest(&entries, T0, None).unwrap(), 0, "nothing repeats after a rotation");
}

/// Item 5: the T3 session ends when the job is turned off or its model leaves T3.
#[test]
fn the_t3_session_ends_when_goals_is_off_or_off_t3() {
    let mut held = Some("goals-s1".to_owned());
    assert_eq!(super::session_to_end(&mut held, true, Some(true)), None);
    assert_eq!(super::session_to_end(&mut held, true, None), None);
    assert_eq!(super::session_to_end(&mut held, true, Some(false)).as_deref(), Some("goals-s1"));
    held = Some("goals-s2".into());
    assert_eq!(super::session_to_end(&mut held, false, None).as_deref(), Some("goals-s2"));
    assert_eq!(held, None);
}

/// Item 6: the cursor resumes from where it stopped, starts over on a replaced file, and reads the whole branch when
/// Pi branched back onto entries an earlier read passed over.
#[test]
fn the_session_cursor_handles_replaced_files_and_branches_back() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");
    std::fs::create_dir_all(data.join("agent")).unwrap();
    let session = data.join("s.jsonl");
    std::fs::write(data.join("agent/active-session.json"), serde_json::to_vec(&session).unwrap()).unwrap();
    let root = Root(d.path().join("fi"));
    let mut s = Shadow::open(&data.join("goals"), &root, T0, true).unwrap();
    s.fresh = false;
    let mut cur = super::Cursor::default();
    let a = owner("a", None, T0, "root words");
    let x = owner("x", Some("a"), T0 + S, "abandoned for now");
    let b = owner("b", Some("a"), T0 + 2 * S, "the branch then");
    write_session(&session, &[a.clone(), x.clone(), b.clone()]);
    assert_eq!(super::read_new(&mut s, &mut cur, &data, T0).unwrap(), 2, "a and b; x is off the branch");
    assert_eq!(super::Cursor::load(&data.join("goals")), cur, "kept for the next start");
    // Pi goes back to x's branch: the new entry's parent was passed over; the whole file is read.
    let y = owner("y", Some("x"), T0 + 3 * S, "back on x");
    let mut f = std::fs::OpenOptions::new().append(true).open(&session).unwrap();
    writeln!(f, "{y}").unwrap();
    drop(f);
    assert_eq!(super::read_new(&mut s, &mut cur, &data, T0).unwrap(), 2, "x and y");
    // The file is replaced by another (a new inode) that grew past the old offset: read from its start.
    let tmp = data.join("s.tmp");
    write_session(&tmp, &[owner("n1", None, T0 + 4 * S, &"new file ".repeat(80)), owner("n2", Some("n1"), T0 + 5 * S, "more")]);
    std::fs::rename(&tmp, &session).unwrap();
    assert_eq!(super::read_new(&mut s, &mut cur, &data, T0).unwrap(), 2);
}

/// Item 8: every goal says what work serves it; the model reads it.
#[test]
fn goals_carry_an_about_line_the_model_reads() {
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let f = goals(&l.root);
    assert!(f["goals"].as_array().unwrap().iter().all(|g| g["about"].as_str().is_some_and(|a| !a.is_empty())));
    assert!(!f["goals"][3]["about"].as_str().unwrap().contains("prompt-cache"));
    assert!(ops::compact(&f, false).contains("· about: CV, job search, applications, interviews."));
    let q = "my goal this month is the garden";
    let mut g = f.clone();
    ops::apply(&mut g, &[op(json!({"op":"add_goal","title":"Grow a garden","about":"a".repeat(200),"quote":q}))], &ops::Evidence::text(q), 2);
    assert!(crate::islands::proposals::queue(&g)[0]["about"].as_str().unwrap().chars().count() <= ops::ABOUT_CHARS);
    // An older Goals file without about gets the seed's, once; one the user emptied stays empty.
    let mut old = f.clone();
    for x in old["goals"].as_array_mut().unwrap() { x.as_object_mut().unwrap().remove("about"); }
    old["goals"][0]["about"] = json!("");
    l.root.put("goals", &old).unwrap();
    ops::ensure(&l.root, 3, true).unwrap();
    let f = goals(&l.root);
    assert_eq!((f["goals"][0]["about"].as_str(), f["goals"][1]["about"].as_str().map(|a| a.starts_with("Arsenal"))), (Some(""), Some(true)));
}

/// no To do list. The migration sends To do items with a time to Scheduled, waiting ones to Goals
/// as asks, and keeps the rest in the migrated copy; nothing is dropped.
#[test]
fn to_do_items_with_a_time_go_to_scheduled_waiting_ones_become_asks_the_rest_stay_kept() {
    let l = lab();
    l.root.put("scheduled", json!({"preset":"todo","list":"scheduled","title":"Scheduled","items":[{"id":"s0","title":"Night pass"}]}).as_object().unwrap()).unwrap();
    let todo = json!({"preset":"todo","list":"todo","items":[
        {"id":"t1","title":"Send the invoice","group":"waiting","due":1_900_000_000},
        {"id":"t2","title":"Call the bank","group":"discussed","when":"Friday"},
        {"id":"w1","title":"Approve the voice","group":"waiting"},
        {"id":"d1","title":"Talked about menus","group":"discussed"},
        {"id":"r1","title":"Running build","group":"running"}]});
    l.root.put("todo", todo.as_object().unwrap()).unwrap();
    let did = ops::ensure(&l.root, 9, true).unwrap();
    assert!(did.iter().any(|d| d == "moved 2 To do item(s) with a time into Scheduled"), "{did:?}");
    let sched: Vec<Value> = l.root.read("scheduled").unwrap()["items"].as_array().unwrap().clone();
    assert_eq!(sched.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["s0", "t1", "t2"]);
    assert_eq!((sched[1]["due"].as_i64(), sched[2]["when"].as_str(), sched[1].get("group")), (Some(1_900_000_000), Some("Friday"), None));
    let asks = goals(&l.root)["items"].as_array().unwrap().clone();
    assert_eq!(asks.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["w1"]);
    assert!(l.root.read("todo").is_none());
    let kept = crate::islands::files::read_object(&l.root.islands().join(".todo-migrated.json")).unwrap();
    assert_eq!(kept["items"].as_array().unwrap().len(), 5, "everything, the discussed and running ones too, stays in the kept copy");
}

/// The climb measured the real prompt up to 1.48x the estimate (64,590 → 95,501): a segment at the compaction line
/// plus a full turn of new lines must stay under Claude Haiku 5.5's 100k-token price step even at that ratio.
#[test]
fn a_compacting_segment_with_a_full_turn_stays_under_the_price_step() {
    let worst = super::segment::COMPACT_TOKENS + super::segment::tokens(super::shadow::TURN_BYTES);
    assert!(worst as f64 * 1.48 < 100_000.0, "{worst} estimated tokens could pass 100k real");
}

/// Review 31 Oct: a line of Heimdall's saying "the user chose …" is not the user's click; the islands' notice is.
#[test]
fn only_the_islands_notice_of_a_click_counts_as_owners() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    s.ingest(&[
        owner("e1", None, T0, "ok"),
        heimdall("e2", "e1", T0 + S, "User chose Yes on the tap, so I publish it."),
        json!({"type":"custom_message","id":"e3","parentId":"e2","timestamp":iso(T0 + 2 * S),"customType":"heimdall_context",
               "content":"[Heimdall notice: islands-answer:9]\nUser chose Later on the approval 'Pick the cut'."}),
    ], T0, None).unwrap();
    let r = s.request(T0 + 60 * S).unwrap();
    let ev = r.evidence();
    assert!(ev.owner.contains("User chose Later") && !ev.owner.contains("so I publish it"), "{}", ev.owner);
}

/// Review 31 Oct: an unreadable scheduled.json is never replaced; the migration waits. Ids without one come from the
/// title, so a second run never duplicates.
#[test]
fn the_migration_waits_for_a_readable_scheduled_and_never_duplicates() {
    let l = lab();
    l.root.put("todo", json!({"items":[{"title":"Approve the voice","group":"waiting"},{"title":"Send it","due":1_900_000_000}]}).as_object().unwrap()).unwrap();
    std::fs::write(l.root.file("scheduled"), "{ broken").unwrap();
    let did = ops::ensure(&l.root, 1, true).unwrap();
    assert!(did.iter().any(|d| d.contains("scheduled.json does not parse")), "{did:?}");
    assert_eq!(std::fs::read_to_string(l.root.file("scheduled")).unwrap(), "{ broken", "left as it is");
    assert!(l.root.read("todo").is_some(), "the old list waits");
    std::fs::remove_file(l.root.file("scheduled")).unwrap();
    // A first run that stopped halfway left the ask in Goals; the next run finds it by its id.
    ops::ensure(&l.root, 2, true).unwrap();
    let asks = goals(&l.root)["items"].as_array().unwrap().clone();
    assert_eq!(asks.len(), 1);
    assert!(asks[0]["id"].as_str().unwrap().starts_with("a-"));
    let id = asks[0]["id"].clone();
    std::fs::rename(l.root.islands().join(".todo-migrated.json"), l.root.file("todo")).unwrap();
    ops::ensure(&l.root, 3, true).unwrap();
    let asks = goals(&l.root)["items"].as_array().unwrap().clone();
    assert_eq!((asks.len(), &asks[0]["id"]), (1, &id), "same id, no duplicate");
    assert_eq!(l.root.read("scheduled").unwrap()["items"].as_array().unwrap().len(), 1);
}

/// Climb 1 Nov: every completed task notice is numbered and must be accounted for; a `finished` entry becomes a done
/// task under its goal, quoted from the notice itself; each open ask is checked every turn and an answered one
/// leaves on his words. What the answer leaves out is logged, and an op of the model's own for the same task wins.
#[test]
fn the_ledger_turns_finished_notices_and_checked_asks_into_ops() {
    let l = lab();
    let mut s = Shadow::open(&l.dir, &l.root, T0, true).unwrap();
    s.fresh = false;
    let mut g = goals(&l.root);
    g.insert("items".into(), json!([{"id":"a-sign","title":"Approve the signing key","by":"goals"}]));
    crate::islands::files::write_atomic(&l.root.file("goals"), &Value::Object(g)).unwrap();
    let done = json!({"type":"custom_message","id":"e2","parentId":"e1","timestamp":iso(T0 + S),"customType":"heimdall_context",
        "content":"[Heimdall notice: terminal:mcp:x:run:thread:mcp%3Ax:ordinal:1:completed]\nSplash port done: the Rust engine replaces the Python one, parity tests pass."});
    s.ingest(&[owner("e1", None, T0, "Yeah, of course, sign it."), done], T0, None).unwrap();
    let r = s.request(T0 + 61 * S).unwrap();
    assert_eq!(r.ledger.done.len(), 1);
    assert_eq!(r.ledger.asks, [("a-sign".to_string(), "Approve the signing key".to_string())]);
    assert!(r.volatile.contains("#1 Splash port done:") && r.volatile.contains("a-sign · Approve the signing key"));
    let answer = json!({"finished":[{"n":1,"goal":"g-heimdall-public","task":"","title":"Rust Splash engine","now":"Rust Splash engine merged"}],
        "checked":[{"ask":"a-sign","status":"answered","quote":"Yeah, of course, sign it."}],"ops":[]}).to_string();
    let d = s.settle(&r, Answer::Text(answer), T0 + 62 * S).unwrap().unwrap();
    assert!(d.outcomes.iter().all(|o| o.kept), "{:?}", d.outcomes);
    let g = goals(&l.root);
    let heim = g["goals"].as_array().unwrap().iter().find(|x| x["id"] == "g-heimdall-public").unwrap();
    assert_eq!((heim["tasks"][0]["title"].as_str(), heim["tasks"][0]["state"].as_str()), (Some("Rust Splash engine"), Some("done")));
    assert!(g["items"].as_array().unwrap().is_empty(), "the answered ask left");
    // Left out: logged, nothing invented; the model's own op for the same task is not doubled.
    let (extra, missing) = super::ledger::ops(&json!({"finished":[],"checked":[]}), &r.ledger);
    assert!(extra.is_empty() && missing == ["finished #1", "checked a-sign"]);
    // The model's own op for the same task gives way to the ledger's (which carries the notice's quote).
    let own = op(json!({"op":"progress","goal":"g-heimdall-public","title":"rust splash engine","state":"now","quote":"x"}));
    let (extra, _) = super::ledger::ops(&json!({"finished":[{"n":1,"goal":"g-heimdall-public","task":"","title":"Rust Splash engine","now":""}],"checked":[]}), &r.ledger);
    assert_eq!(extra.len(), 1);
    assert!(super::ledger::drop_duplicates(vec![own], &extra).is_empty());
    assert!(ops::schema()["required"].as_array().unwrap().iter().any(|k| k == "finished"));
}

/// Night 1 Nov: a question card's answer is the user's own (it clears the ask it answers); Heimdall's line that only says
/// so is not.
#[test]
fn a_question_cards_click_is_owners_answer() {
    let card = json!({"type":"custom_message","id":"q1","parentId":null,"timestamp":iso(T0),"customType":"heimdall_context",
        "content":"[Heimdall notice: ask-5407d321]\nQuestion ask-5407d321: User clicked Install both. Question: Install the combined update now?"});
    let l = render(&card, 0).unwrap();
    assert!(super::stream::owners_click(&l), "{:?}", l);
    let said = render(&heimdall("h1", "q1", T0, "User clicked Install both earlier."), 0).unwrap();
    assert!(!super::stream::owners_click(&said));
}

/// Night 1 Nov: one notice can report several finished pieces; each entry with its number becomes a task, a repeat
/// does not.
#[test]
fn one_notice_may_finish_several_pieces() {
    let l = super::ledger::Ledger { reported: Vec::new(), done: vec![super::ledger::Notice { n: 1, quote: "Lane done: clean export built, privacy scan clean".into(), show: String::new() }], asks: vec![] };
    let a = json!({"finished":[{"n":1,"goal":"g-h","task":"","title":"Clean export built","now":""},{"n":1,"goal":"g-h","task":"","title":"Privacy scan clean","now":""},{"n":1,"goal":"g-h","task":"","title":"clean export built","now":""}],"checked":[]});
    let (ops, missing) = super::ledger::ops(&a, &l);
    assert_eq!(ops.iter().map(|o| o.title.as_str()).collect::<Vec<_>>(), ["Clean export built", "Privacy scan clean"]);
    assert!(missing.is_empty());
}

/// Night 1 Nov: a lane's notice shows its name and the start of its result, past the framing every such notice repeats.
#[test]
fn a_lanes_notice_shows_what_finished() {
    let n = json!({"type":"custom_message","id":"n1","parentId":null,"timestamp":iso(T0),"customType":"heimdall_context",
        "content":"[Heimdall notice: terminal:mcp:x:run:thread:mcp%3Ax:ordinal:15:completed]\nTask 'Public alpha: build' (mcp:x) completed. It still has background work running: cargo build. Final reply:\nEarlier turn:\nThe clean public export is built and the privacy scan is clean."});
    let l = render(&n, 0).unwrap();
    let led = super::ledger::build(&[l], &Map::new());
    assert_eq!(led.done[0].quote, "The clean public export is built and the privacy scan is clean.");
    assert!(led.done[0].show.starts_with("[Public alpha: build] (the task is still running) The clean public export"), "{}", led.done[0].show);
}

/// Review 1 Nov: a catch-up of spooled lines is never the user's click, even when it holds a card's notice; a notice whose
/// reply opens with a short line is still numbered.
#[test]
fn a_catch_up_is_no_click_and_a_short_first_line_is_skipped() {
    let c = json!({"type":"custom_message","id":"c1","parentId":null,"timestamp":iso(T0),"customType":"heimdall_context",
        "content":"Heimdall catch-up: what was said since 01:00 (history only).\n[Heimdall notice: ask-1]\nQuestion ask-1: User clicked Install both."});
    assert!(!super::stream::owners_click(&render(&c, 0).unwrap()));
    let n = json!({"type":"custom_message","id":"n2","parentId":null,"timestamp":iso(T0),"customType":"heimdall_context",
        "content":"[Heimdall notice: terminal:mcp:y:run:thread:mcp%3Ay:ordinal:2:completed]\nDone.\nThe updater is signed and the feed is live."});
    let led = super::ledger::build(&[render(&n, 0).unwrap()], &Map::new());
    assert_eq!(led.done[0].quote, "The updater is signed and the feed is live.");
}

// ---------------------------------------------------------------------------------------------------------
// Settled: the shadow writes it, dated by the line, once per piece of work, never what he removed.

fn settled(root: &Root) -> Map<String, Value> { root.read("settled").unwrap_or_default() }
fn settled_items(root: &Root) -> Vec<Value> { settled(root).get("items").and_then(Value::as_array).cloned().unwrap_or_default() }
fn timed(lines: &[(&str, i64)]) -> ops::Evidence {
    let all = lines.iter().map(|(t, _)| *t).collect::<Vec<_>>().join("\n");
    ops::Evidence { all: all.clone(), owner: all, times: lines.iter().map(|(t, at)| (t.to_string(), *at)).collect(), removed: Vec::new(), recorded: Vec::new() }
}

#[test]
fn a_finished_task_is_settled_under_its_goal_dated_by_its_line_not_the_call() {
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let ev = timed(&[("Notice: Arsenal tap: formula audit passes.", 1_000), ("Heimdall: anything else?", 5_000)]);
    let (_, f) = ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-arsenal","title":"Homebrew tap passes audit","state":"done","quote":"formula audit passes"}))], &ev, 9_000).unwrap();
    let items = settled_items(&l.root);
    assert_eq!(items.len(), 1);
    assert_eq!((items[0]["title"].as_str(), items[0]["kind"].as_str(), items[0]["goal"].as_str(), items[0]["at"].as_i64()), (Some("Homebrew tap passes audit"), Some("shipped"), Some("g-arsenal"), Some(1_000)));
    assert!(items[0].get("when").is_none(), "a time, never a word like Today");
    // The Goals task carries the same true time.
    assert_eq!(f["goals"][1]["tasks"][0]["at"], 1_000);
    // Told again in other words (Heimdall to the user): the same task, not a second item.
    let ev2 = timed(&[("Heimdall: The Homebrew tap passes its audit now.", 6_000)]);
    ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-arsenal","title":"Homebrew tap audit passes","state":"done","quote":"Homebrew tap passes its audit"}))], &ev2, 9_500).unwrap();
    assert_eq!(settled_items(&l.root).len(), 1);
    assert_eq!(goals(&l.root)["goals"][1]["tasks"].as_array().unwrap().len(), 1, "never two tasks for one thing");
}

#[test]
fn work_that_serves_no_goal_and_asks_never_reach_settled_or_goals() {
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let q = "Restarted the speech server";
    let out = ops::apply(&mut goals(&l.root), &[op(json!({"op":"ask","goal":"g-arsenal","title":"Pick the icon","quote":q}))], &ops::Evidence::text(q), 2);
    assert!(!out[0].kept && out[0].reason.contains("card"), "{out:?}");
    assert!(ops::MODEL_OPS.iter().all(|o| *o != "ask"));
    // A finished entry with no goal (a chore) is no op at all.
    let ledger = super::ledger::Ledger { done: vec![super::ledger::Notice { n: 1, quote: q.into(), show: q.into() }], ..Default::default() };
    let (o, missing) = super::ledger::ops(&json!({"finished": [{"n": 1, "goal": "", "task": "", "title": "Speech restart", "now": ""}]}), &ledger);
    assert!(o.is_empty() && missing.is_empty());
}

#[test]
fn what_owner_removed_never_comes_back_in_other_words() {
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let mut s = super::settled::fresh();
    super::settled::remember_removed(&mut s, "Website work complete", "Website pass complete, all sections done", 1);
    ops::write_list(&l.root, "settled", &s).unwrap();
    let ev = timed(&[("Heimdall: The website work is complete.", 100), ("Notice: Website pass complete, all sections done.", 90)]);
    let out = ops::apply_file(&l.root, &[
        op(json!({"op":"progress","goal":"g-heimdall-public","title":"Heimdall website work complete","state":"done","quote":"website work is complete"})),
        op(json!({"op":"progress","goal":"g-heimdall-public","title":"Site pass finished","state":"done","quote":"Website pass complete, all sections done"})),
    ], &ev, 200).unwrap().0;
    assert!(out.iter().all(|o| !o.kept && o.reason == "User removed it"), "{out:?}");
    assert!(settled_items(&l.root).is_empty());
    assert!(goals(&l.root)["goals"][2]["tasks"].as_array().unwrap().is_empty());
}

#[test]
fn a_decision_is_settled_only_on_owners_words() {
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let heard = "Incoming: [Mic · User] From now on the public build wakes by name only.";
    let said = "Heimdall: I'll make it wake by name only.";
    let ev = ops::Evidence { all: format!("{heard}\n{said}"), owner: heard.into(), times: vec![(heard.into(), 50), (said.into(), 60)], removed: Vec::new(), recorded: Vec::new() };
    let out = ops::apply_file(&l.root, &[
        op(json!({"op":"decided","goal":"g-heimdall-public","title":"Wake by name only","quote":"I'll make it wake by name only"})),
        op(json!({"op":"decided","goal":"g-heimdall-public","title":"Wake by name only","quote":"public build wakes by name only"})),
    ], &ev, 99).unwrap().0;
    assert!(!out[0].kept && out[1].kept, "{out:?}");
    let items = settled_items(&l.root);
    assert_eq!((items.len(), items[0]["kind"].as_str(), items[0]["at"].as_i64()), (1, Some("decided"), Some(50)));
}

#[test]
fn heimdalls_report_of_finished_work_must_be_accounted_for() {
    use super::stream::Line;
    let lines = vec![
        Line { id: "a".into(), at: 1, lines: vec!["Heimdall: Looking into it.".into()], kind: Kind::Heimdall },
        Line { id: "b".into(), at: 2, lines: vec!["Heimdall: Good news. The Arsenal site is built and open in Ego Lite.".into()], kind: Kind::Heimdall },
    ];
    let l = super::ledger::build(&lines, &Map::new());
    assert_eq!(l.reported.len(), 1);
    assert_eq!(l.reported[0].quote, "The Arsenal site is built and open in Ego Lite.");
    assert!(super::ledger::text(&l).contains("<heimdall_reports"));
    let (o, missing) = super::ledger::ops(&json!({"reported": []}), &l);
    assert!(o.is_empty() && missing == ["reported #1"]);
    let (o, _) = super::ledger::ops(&json!({"reported": [{"n": 1, "goal": "g-arsenal", "task": "", "title": "Arsenal site built", "now": ""}]}), &l);
    assert_eq!((o[0].op.as_str(), o[0].state.as_str(), o[0].quote.as_str()), ("progress", "done", "The Arsenal site is built and open in Ego Lite."));
}

#[test]
fn plan_edit_remove_remembers_the_title_for_the_shadow() {
    let d = tempfile::tempdir().unwrap();
    let h = crate::islands::Host::new(Root(d.path().join("fi")), None);
    h.root.ensure().unwrap();
    let mut s = super::settled::fresh();
    s.insert("items".into(), json!([{"id": "s-1", "title": "Website work complete", "done": true, "at": 5, "quote": "website work complete"}]));
    ops::write_list(&h.root, "settled", &s).unwrap();
    crate::islands::tools::plan_edit(&h, &json!({"action": "remove", "item": "Website work complete"})).unwrap();
    let f = settled(&h.root);
    assert!(f["items"].as_array().unwrap().is_empty());
    assert!(super::settled::removed(&f, "Heimdall website work complete"));
}

/// Bug note 2026-11-02T02-29-09: every Settled row read Today because a write stamped the list, not the items. A write
/// (here a removal, which also migrates a legacy plain-string item) keeps each item's own `at` and invents none, so the
/// app (which draws only items with `at`, on their own day: `islands::model::onesys::settled_items`) shows true days.
#[test]
fn a_settled_write_keeps_each_items_own_date_and_invents_none() {
    let d = tempfile::tempdir().unwrap();
    let h = crate::islands::Host::new(Root(d.path().join("fi")), None);
    h.root.ensure().unwrap();
    let mut s = super::settled::fresh();
    s.insert("updated".into(), json!(1_000));
    s.insert("items".into(), json!([
        {"id": "s-1", "title": "Menu fixed", "done": true, "at": 1_793_000_000_i64, "quote": "menu fixed"},
        {"id": "s-2", "title": "Voice installed", "done": true, "at": 1_793_200_000_i64, "quote": "voice installed"},
        {"id": "s-3", "title": "Old row", "done": true, "at": 1_793_300_000_i64},
        "Heimdall: website work complete"]));
    ops::write_list(&h.root, "settled", &s).unwrap();
    crate::islands::tools::plan_edit(&h, &json!({"action": "remove", "item": "Old row"})).unwrap();
    let items = settled_items(&h.root);
    let at = |id: &str| items.iter().find(|i| i["id"] == id).map(|i| i["at"].clone());
    assert_eq!((at("s-1"), at("s-2"), at("s-3")), (Some(json!(1_793_000_000_i64)), Some(json!(1_793_200_000_i64)), None));
    let legacy = items.iter().find(|i| i["title"] == "Heimdall: website work complete").unwrap();
    assert!(legacy.get("at").is_none() && legacy.get("when").is_none(), "no date is made up: {legacy}");
}

/// Review 2 Nov: a removal is not a ban on its project (one shared word, the "Arsenal:" prefix), it ends after two
/// weeks, a starting task never reopens a done one, and the decision cap counts only decisions written.
#[test]
fn removals_are_narrow_and_brief_and_new_work_is_never_set_back() {
    let rem = vec![json!({"title": "Arsenal: tap", "quote": "tests pass", "at": 1_000}), json!({"title": "Vella: build merged", "quote": "", "at": 1_000})];
    let r = |t: &str, q: &str, now: i64| super::settled::removed_in(&rem, t, q, now);
    assert!(!r("Arsenal: icon fixed", "the icon is fixed and tests pass now", 2_000), "the project prefix and a short quote are not the removal");
    assert!(!r("Vella: icon fix merged", "", 2_000), "one word of the body shared is not the removal");
    assert!(r("Vella build merged today", "", 2_000));
    assert!(!r("Vella build merged today", "", 1_000 + 15 * 86_400), "a removal ends after two weeks");
    let l = lab();
    ops::ensure(&l.root, 1, true).unwrap();
    let q1 = "Vella icon export is done";
    ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-heimdall-public","title":"Vella icon export","state":"done","quote":q1}))], &ops::Evidence::text(q1), 2).unwrap();
    let q2 = "starting the Vella icon export for iPad";
    ops::apply_file(&l.root, &[op(json!({"op":"progress","goal":"g-heimdall-public","title":"Vella icon export for iPad","state":"now","quote":q2}))], &ops::Evidence::text(q2), 3).unwrap();
    let tasks = goals(&l.root)["goals"][2]["tasks"].as_array().unwrap().clone();
    assert_eq!(tasks.iter().map(|t| t["state"].as_str().unwrap()).collect::<Vec<_>>(), ["done", "now"], "{tasks:?}");
    // Three decisions, the first a repeat of one already settled: the two new ones are written.
    let heard = "Incoming: [Mic · User] We use Kokoro. We train on MLX always. Arsenal goes public as it is.";
    let ev = ops::Evidence { all: heard.into(), owner: heard.into(), times: vec![(heard.into(), 10)], removed: Vec::new(), recorded: Vec::new() };
    let d = |t: &str, q: &str| op(json!({"op":"decided","goal":"g-jarvis","title":t,"quote":q}));
    ops::apply_file(&l.root, &[d("Kokoro for tiny Jarvis", "We use Kokoro")], &ev, 11).unwrap();
    ops::apply_file(&l.root, &[d("Kokoro for tiny Jarvis", "We use Kokoro"), d("Always train on MLX", "We train on MLX always"), d("Arsenal public as it is", "Arsenal goes public as it is")], &ev, 12).unwrap();
    let titles: Vec<String> = settled_items(&l.root).iter().filter(|i| i["kind"] == "decided").map(|i| i["title"].as_str().unwrap().to_owned()).collect();
    assert_eq!(titles.len(), 3, "{titles:?}");
}

/// Held-out 2 Nov: a thread's notice framing ("Thread '…' (…), which you sent a message, completed.") is not the result.
#[test]
fn a_threads_notice_framing_is_never_the_quote() {
    let n = json!({"type":"custom_message","id":"n2","parentId":null,"timestamp":iso(T0),"customType":"heimdall_context",
        "content":"[Heimdall notice: terminal:thread:x:completed]\nThread 'Heimdall website: first build' (thread:delegated-task:abc%3A1), which you sent a message, completed. Final reply:\nThe first build is live on localhost and the review passed."});
    let led = super::ledger::build(&[render(&n, 0).unwrap()], &Map::new());
    assert_eq!(led.done[0].quote, "The first build is live on localhost and the review passed.");
    assert!(led.done[0].show.starts_with("[Heimdall website: first build] The first build"), "{}", led.done[0].show);
}
