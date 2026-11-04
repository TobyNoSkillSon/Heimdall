use agent::{
    actions::{ActionSink, Actions, AskItem, Notice, Question},
    delivery::{DeliveryGate, DeliveryPermit},
    mcp_contract::{tools, validate_tool, TEXT_BYTES},
};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::{atomic::Ordering, Arc, Mutex};
#[test]
fn full_catalog_schema_snapshot() {
    // `MCP_SCHEMA_WRITE=1` writes the fixture from the catalog instead of checking it (then read the diff).
    if std::env::var_os("MCP_SCHEMA_WRITE").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-v2-schema.json");
        std::fs::write(path, serde_json::to_string_pretty(&tools()).unwrap() + "\n").unwrap();
        return;
    }
    let mut expected: Value =
        serde_json::from_str(include_str!("fixtures/mcp-v2-schema.json")).unwrap();
    // The public edition's catalog is the same without `bug_note` (docs/bugs.md).
    if cfg!(feature = "public") { expected.as_array_mut().unwrap().retain(|t| t["name"] != "bug_note"); }
    // Nor a grant on an ask option: the user's clicks grant nothing there (`heimdall_emergency::grants::ENABLED`).
    if !heimdall_emergency::grants::ENABLED {
        let ask = expected.as_array_mut().unwrap().iter_mut().find(|t| t["name"] == "ask").unwrap();
        ask["inputSchema"]["properties"]["questions"]["items"]["properties"]["options"]["items"]["oneOf"][1]["properties"].as_object_mut().unwrap().remove("grant").expect("the fixture's grant");
    }
    assert_eq!(tools(), expected);
    let list = tools();
    let list = list.as_array().unwrap();
    // 23, plus refresh_plan and the four Floating Islands tools (night 1022), plus name_voice (23 Oct), plus
    // archive_clip and listen_to, taste_note and taste_search (24 Oct), personalise and settings (26 Oct).
    assert_eq!(list.len(), if cfg!(feature = "public") { 34 } else { 35 });
    let mut names = std::collections::HashSet::new();
    for t in list {
        assert!(names.insert(t["name"].as_str().unwrap()));
        assert!(t.to_string().len() < 16384);
        assert_eq!(t["inputSchema"]["additionalProperties"], false);
    }
}
/// The settings tool's contract (26 Oct 16:43): an action always, a key for get and set, a value only with set, an item for
/// the per-device, per-app and per-task keys, owner_asked only with set; keys only from the catalog.
#[test]
fn settings_contract() {
    for (args, ok) in [
        (json!({"action":"list"}), true),
        (json!({}), false),
        (json!({"action":"delete","key":"name"}), false),
        (json!({"action":"get"}), false),
        (json!({"action":"get","key":"wake_words"}), false),
        (json!({"action":"get","key":"name","value":"Ivy"}), false),
        (json!({"action":"list","key":"name"}), false),
        (json!({"action":"set","key":"name"}), false),
        (json!({"action":"set","key":"name","value":"Ivy"}), true),
        (json!({"action":"set","key":"app_audio","value":true}), false),
        (json!({"action":"set","key":"app_audio","item":"Ego","value":true}), true),
        (json!({"action":"set","key":"listening","value":[true]}), false),
        (json!({"action":"set","key":"recording_fps","value":-3}), false),
        (json!({"action":"get","key":"agent","owner_asked":"off"}), false),
    ] {
        assert_eq!(validate_tool("settings", &args).is_ok(), ok, "{args}");
    }
    let t = tools();
    let settings = t.as_array().unwrap().iter().find(|t| t["name"] == "settings").unwrap();
    assert_eq!(settings["inputSchema"]["properties"]["key"]["enum"].as_array().unwrap().len(), agent::mcp_contract::SETTINGS_KEYS.len());
    assert!(settings["description"].as_str().unwrap().contains("owner_asked"));
}
#[test]
fn personalise_refuses_an_icon_and_a_drawing_together_and_relative_paths() {
    assert!(validate_tool("personalise", &json!({"icon":"leaf","icon_svg":"<svg/>"})).is_err());
    assert!(validate_tool("personalise", &json!({"icon":"marks/ivy.svg"})).is_err());
    assert!(validate_tool("personalise", &json!({"icon":"/tmp/ivy.gif"})).is_err());
    assert!(validate_tool("personalise", &json!({"icon":"/tmp/ivy.svg"})).is_ok());
    assert!(validate_tool("personalise", &json!({"icon_svg":"a circle"})).is_err());
    assert!(validate_tool("personalise", &json!({"voice":"Bryony"})).is_err(), "a voice names its model");
    assert!(validate_tool("personalise", &json!({"speaking_model":"drama4"})).is_err());
}
#[test]
fn validates_every_tool_and_rejects_unknown_keys_wrong_types_and_blank_text() {
    let cases = [
        ("speak", json!({"text":"Cześć","language":"pl"})),
        ("screen_frames", json!({})),
        ("timeline_search", json!({"query":"hello"})),
        ("archive_clip", json!({"start":"2026-10-23T22:40:00Z","end":"2026-10-23T22:47:00Z","display_id":"1","width":1280,"fps":5,"path":"/tmp/clip.mp4"})),
        ("island_notice", json!({"text":"Answer","detail":{"type":"text","text":"Long answer"}})),
        ("ask", json!({"question":"Install the new build?","options":["Yes","No"]})),
        ("ask_close", json!({"request_id":"ask-1","answer":"yes"})),
        ("fast_read", json!({"text":"The build failed because tests timed out.","title":"Build"})),
        ("reload_me", json!({})),
        (
            "delegate_task",
            json!({"task":"Do it","title":"Work","target":{"model":"m","providerInstanceId":"p","options":{"effort":"high"}},"path":"/tmp","isolation":"worktree","branch":"b"}),
        ),
        ("task_status", json!({})),
        ("task_status", json!({"taskId":"t"})),
        ("task_cancel", json!({"taskId":"t","reason":"dropped"})),
        ("t3_thread_list", json!({"query":"Mobius","limit":5})),
        ("t3_thread_read", json!({"threadId":"t","last":3,"view":"activity","details":true})),
        ("t3_thread_read", json!({"threadIds":["a","b","c"]})),
        ("t3_thread_read", json!({"threadId":"t","itemId":"i","textOffset":1500})),
        ("t3_thread_send", json!({"threadId":"t","message":"continue","mode":"steer"})),
        ("t3_tools", json!({"query":"worktree"})),
        ("t3_call", json!({"tool":"t3_thread_organize","args":{"threadId":"t","action":"archive"}})),
        ("point", json!({"spec":"arrow 640 400 Save button"})),
        ("look", json!({"app":"Safari"})),
        ("look", json!({})),
        ("drawing_mode", json!({"open":true})),
        ("bug_note", json!({"text":"t3_workers returned a wall of JSON at 12:32."})),
        ("bug_note", json!({"text":"Island stuck on Working","evidence":"log: turn ended 12:40"})),
        ("schedule_wake", json!({"at":"07:00","message":"Tell User whether the build passed"})),
        ("schedule_wake", json!({"in_minutes":45,"message":"Check the TTS research task"})),
        ("list_wakes", json!({})),
        ("cancel_wake", json!({"id":"wake-3f2a1c"})),
        ("refresh_plan", json!({})),
        ("name_voice", json!({"voice":"Speaker Fri 23 Oct 21h #3","name":"John"})),
        ("listen_to", json!({"speaker":"Vim","on":true})),
        ("taste_note", json!({"subject":"benchmark report","kind":"report","verdict":"like","words":"really well made","why":"rich, varied visuals with a clear hierarchy","at":"2026-10-24T01:16:00+02:00","lines":["49:text:utterance:session-1-a:12"],"screen_at":"2026-10-24T01:15:50+02:00","files":["/tmp/benchmark.html"],"follows":"taste-1-abc"})),
        ("taste_search", json!({"query":"reports","verdict":"dislike","since":"2026-10-17T00:00:00+02:00","limit":5})),
        ("personalise", json!({})),
        ("personalise", json!({"name":"Ivy","user_name":"Evelyn","wake_words":["Hey Ivy","Ivy"],"icon":"leaf","speaking_model":"phonon","voice":"Bryony"})),
        ("settings", json!({"action":"list"})),
        ("settings", json!({"action":"get","key":"microphone"})),
        ("settings", json!({"action":"set","key":"app_audio","item":"Ego Browser","value":true})),
        ("settings", json!({"action":"set","key":"agent","value":false,"owner_asked":"turn the agent off for tonight"})),
        ("settings", json!({"action":"set","key":"voice_speed","value":120})),
        ("personalise", json!({"icon_svg":"<svg viewBox=\"0 0 16 16\"><circle cx=\"8\" cy=\"8\" r=\"6\"/></svg>","wake_words":[]})),
        // Floating islands (night 1022).
        ("islands_put", json!({"id":"voices","title":"Male voices","state":"running","stage":"Rendering","summary":"Three voices, then a pick","steps":[{"title":"Render","state":"now"}],"items":[{"title":"Warm","group":"running","done":false}],"kv":[{"label":"Size","value":"20 GB"}],"image":"/tmp/a.png","open":"https://example.com","parts":[{"type":"button","label":"Go","action":"go"}],"side":"right","thread":"th-1","expanded":true})),
        ("islands_view", json!({"path":"/tmp/notes.md","title":"Notes","actions":[{"label":"Do all of it","action":"do-all"}]})),
        ("islands_remove", json!({"ids":["voices"]})),
        ("plan_edit", json!({"action":"add","item":"Arsenal: push branch","list":"goals","kind":"approve","goal":"g-arsenal","detail":"after tests","proposal":{"text":"git push","actions":[{"label":"Yes","action":"yes"}]}})),
    ];
    for (name, args) in cases {
        // The public edition has no bug notes: its contract refuses the tool (docs/bugs.md).
        if cfg!(feature = "public") && name == "bug_note" {
            assert!(validate_tool(name, &args).is_err());
            continue;
        }
        validate_tool(name, &args).unwrap();
        let mut unknown = args.clone();
        unknown["unknown"] = json!(true);
        assert!(validate_tool(name, &unknown).is_err(), "{name}");
        assert!(validate_tool(name, &json!(null)).is_err());
        assert!(validate_tool(name, &json!([])).is_err());
        for (key, value) in args.as_object().unwrap() {
            if value.is_string() {
                let mut wrong = args.clone();
                wrong[key] = json!(7);
                assert!(validate_tool(name, &wrong).is_err());
                wrong[key] = json!("   ");
                assert!(validate_tool(name, &wrong).is_err());
            }
        }
    }
    // Non-string fields and required keys of the newer tools.
    assert!(validate_tool("drawing_mode", &json!({"open":"yes"})).is_err());
    assert!(validate_tool("drawing_mode", &json!({})).is_err());
    assert!(validate_tool("point", &json!({})).is_err());
    assert!(validate_tool("point", &json!({"spec":"x".repeat(agent::mcp_contract::SPEC_CHARS + 1)})).is_err());
    // Floating islands (night 1022): required keys, enums and nested shapes.
    assert!(validate_tool("islands_put", &json!({"title":"Tests"})).is_err());
    assert!(validate_tool("islands_put", &json!({"id":"t","state":"paused"})).is_err());
    assert!(validate_tool("islands_put", &json!({"id":"t","steps":[{"state":"now"}]})).is_err());
    assert!(validate_tool("islands_put", &json!({"id":"t","parts":["text"]})).is_err());
    assert!(validate_tool("islands_remove", &json!({"ids":[]})).is_err());
    assert!(validate_tool("plan_edit", &json!({"action":"rename","item":"x"})).is_err());
    assert!(validate_tool("plan_edit", &json!({"action":"add","item":"x","list":"later"})).is_err());
    assert!(validate_tool("plan_edit", &json!({"action":"add","item":"x","proposal":{"actions":[]}})).is_err());
}
#[test]
fn cross_field_bounds_dates_tags_and_unicode() {
    for (name, args) in [
        ("speak", json!({"text":"ok","language":42})),
        ("speak", json!({"text":"😀".repeat(4097)})),
        ("screen_frames", json!({"count":2})),
        ("archive_clip", json!({"start":"2026-10-23T22:47:00Z","end":"2026-10-23T22:40:00Z"})),
        ("archive_clip", json!({"start":"2026-10-23T20:00:00Z","end":"2026-10-23T22:00:01Z"})),
        ("archive_clip", json!({"start":"2026-10-23T22:40:00Z","end":"2026-10-23T22:47:00Z","path":"clip.mp4"})),
        ("archive_clip", json!({"start":"2026-10-23T22:40:00Z","end":"2026-10-23T22:47:00Z","path":"/tmp/clip.gif"})),
        ("archive_clip", json!({"start":"2026-10-23T22:40:00Z","end":"2026-10-23T22:47:00Z","fps":0})),
        ("archive_clip", json!({"start":"2026-10-23 22:40","end":"2026-10-23T22:47:00Z"})),
        ("screen_frames", json!({"at":"now","chunk_id":"x"})),
        ("screen_frames", json!({"quality":"ultra"})),
        ("timeline_search", json!({})),
        ("timeline_search", json!({"query":"x","limit":"10"})),
        ("timeline_search", json!({"query":"x","limit":0})),
        ("timeline_search", json!({"start":"2026-10-14"})),
        (
            "timeline_search",
            json!({"start":"2026-10-14T00:00:01Z","end":"2026-10-14T00:00:00Z"}),
        ),
        ("timeline_search", json!({"id":"x","query":"x"})),
        ("timeline_search", json!({"cursor":"x","limit":5})),
        ("timeline_search", json!({"query":"x","output":"pictures"})),
        ("timeline_search", json!({"query":"x","output":"summary"})),
        (
            "timeline_search",
            json!({"tags":[{"key":"bogus","value":"x"}]}),
        ),
        (
            "timeline_search",
            json!({"tags":[{"key":"speaker","value":"x","extra":true}]}),
        ),
        ("delegate_task", json!({"task":"y","title":"x","target":{"model":"m"},"path":"relative"})),
        ("delegate_task", json!({"task":"y","title":"x","target":{"model":"m"},"branch":"b"})),
        ("delegate_task", json!({"task":"y","title":"x","target":{"model":"m","options":{"effort":2}}})),
        ("delegate_task", json!({"task":"y","title":"x","target":{"provider":"p","model":"m"}})),
        ("delegate_task", json!({"task":"y","title":"x"})),
        ("t3_thread_read", json!({})),
        ("t3_thread_read", json!({"threadId":"a","threadIds":["b"]})),
        ("t3_thread_read", json!({"threadIds":["a","b","c","d","e","f"]})),
        ("t3_thread_read", json!({"threadIds":["a"],"itemId":"i"})),
        ("t3_thread_read", json!({"threadId":"a","textOffset":5})),
        ("t3_thread_read", json!({"threadId":"a","last":0})),
        ("t3_thread_send", json!({"threadId":"a","message":"x","mode":"later"})),
    ] {
        assert!(validate_tool(name, &args).is_err(), "{name}: {args}");
    }
    validate_tool("speak", &json!({"text":"😀".repeat(4096)})).unwrap();
    validate_tool(
        "timeline_search",
        &json!({"tags":[{"key":"window_title","value":"Editor"}]}),
    )
    .unwrap();
    // Token diet (26 Oct): output.
    for args in [json!({"query":"x","output":"frames"}), json!({"vector":{"query":"x"},"output":"full"})] {
        validate_tool("timeline_search", &args).unwrap();
    }
    assert_eq!(TEXT_BYTES, 8192);
}
#[test]
fn island_notice_detail_and_ask_bounds() {
    for args in [
        json!({"text":"Table ready","detail":{"type":"table","headers":["Model","Speed"],"rows":[["A","42 tok/s"],["B",""]]}}),
        json!({"text":"Table ready","notice_id":"notice-1","detail":{"type":"table","headers":["Model"],"rows":[["A"]]}}),
        json!({"text":"Answer","detail":{"type":"text","text":"Long answer\nwith lines"}}),
        json!({"text":"Chart","detail":{"type":"svg","svg":"<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'/>"}}),
        json!({"text":"This one","detail":{"type":"image","archive_id":"frame-123"}}),
    ] {
        validate_tool("island_notice", &args).unwrap_or_else(|e| panic!("{args}: {e}"));
    }
    // Review 19 Oct: the retired `kind` is accepted and ignored, not refused; it is still not advertised, and the rest
    // of the call is checked as usual.
    validate_tool("island_notice", &json!({"text":"Done","kind":"success","detail":{"type":"text","text":"a"}})).unwrap();
    assert!(tools().as_array().unwrap().iter().find(|t| t["name"] == "island_notice").unwrap()["inputSchema"]["properties"].get("kind").is_none());
    assert!(validate_tool("island_notice", &json!({"text":"Working","kind":"working"})).unwrap_err().to_string().contains("detail: required"));
    // 19 Oct: no agent status icons. A status line ("Working", kind) is refused; content needs its detail.
    for (args, why) in [
        (json!({"text":"x".repeat(41),"detail":{"type":"text","text":"a"}}), "string length"),
        (json!({"text":"Working"}), "detail: required"),
        (json!({"text":"Table","detail":{"type":"table","headers":["A"],"rows":[["1","2"]]}}), "more cells than headers"),
        (json!({"text":"Table","detail":{"type":"table","headers":[],"rows":[["1"]]}}), "too many items"),
        (json!({"text":"Chart","detail":{"type":"svg","svg":"not svg"}}), "one <svg>"),
        (json!({"text":"Pic","detail":{"type":"image"}}), "archive_id: required"),
        (json!({"text":"Pic","detail":{"type":"video","archive_id":"x"}}), "wrong type"),
        (json!({"text":"Text","detail":{"type":"text","text":"a","extra":1}}), "unknown field"),
    ] {
        let e = validate_tool("island_notice", &args).unwrap_err().to_string();
        assert!(e.contains(why), "{args}: {e}");
    }
    for args in [
        json!({"question":"Install?","options":["Yes"]}),
        json!({"question":"Install?","options":["A","B","C","D","E"]}),
        json!({"question":"Install?","options":["Yes","yes"]}),
        json!({"question":"Install?","options":["Yes","x".repeat(25)]}),
        json!({"question":"x".repeat(121),"options":["Yes","No"]}),
        json!({"question":"Install?"}),
    ] {
        assert!(validate_tool("ask", &args).is_err(), "{args}");
    }
    validate_tool("ask", &json!({"question":"Install?","options":["Today","Tomorrow","Friday","Never"]})).unwrap();
    assert!(validate_tool("ask_close", &json!({"request_id":"ask-1"})).is_err());
    assert!(validate_tool("ask_to_confirm", &json!({"question":"Continue?"})).is_err(), "ask replaces ask_to_confirm");
}
/// A one-question card: the legacy `ask` call (question and options), as its items.
fn card(question: &str, labels: &[&str]) -> Vec<AskItem> {
    vec![AskItem::one(question, labels)]
}
#[derive(Default)]
struct Sink {
    events: Mutex<Vec<String>>,
    ids: Mutex<std::collections::HashSet<String>>,
}
impl ActionSink for Sink {
    fn notice(&self, n: &Notice, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.events.lock().unwrap().push(n.id.clone());
            Ok(())
        })
    }
    fn question(&self, c: &Question, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.events.lock().unwrap().push(c.id.clone());
            Ok(())
        })
    }
    fn question_closed(&self, id: &str, answer: &str, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.events.lock().unwrap().push(format!("closed {id}: {answer}"));
            Ok(())
        })
    }
    fn deliver(&self, id: &str, text: &str, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            if self.ids.lock().unwrap().insert(id.into()) {
                self.events.lock().unwrap().push(text.into());
            }
            Ok(())
        })
    }
    fn reload(&self, id: &str, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.events.lock().unwrap().push(id.into());
            Ok(())
        })
    }
}
#[test]
fn durable_receipts_answers_replay_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.db");
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let sink = Arc::new(Sink::default());
    let a = Actions::open(&path).unwrap();
    a.attach(sink.clone());
    let notice = a.notice("n", "Testing", "info", None, &p).unwrap();
    assert_eq!(a.notice("n", "Testing", "info", None, &p).unwrap(), notice);
    assert!(a.notice("n", "Different", "info", None, &p).is_err());
    let yes_no = card("Ship?", &["Yes", "No"]);
    let question = a.ask("q", &yes_no, &p).unwrap();
    assert_eq!(a.ask("q", &yes_no, &p).unwrap(), question, "a replay is the same question");
    assert!(a.answer(&question, "Maybe", &p).is_err(), "only a listed option answers");
    a.answer(&question, "No", &p).unwrap();
    a.answer(&question, "No", &p).unwrap();
    assert_eq!(a.state(&question).unwrap(), "answered");
    assert!(sink.events.lock().unwrap().last().unwrap().contains("User clicked No."));
    assert_eq!(sink.events.lock().unwrap().iter().filter(|e| e.contains("User clicked")).count(), 1, "answered once");
    // Answered by voice: the agent closes the card; nothing is delivered back and a later click does nothing.
    let spoken = a.ask("q2", &card("When should I install it?", &["Today", "Tomorrow", "Friday", "Never"]), &p).unwrap();
    assert_eq!(a.close_question(&spoken, "after lunch", &p).unwrap(), "closed");
    assert_eq!(a.close_question(&spoken, "after lunch", &p).unwrap(), "closed");
    assert_eq!(a.state(&spoken).unwrap(), "closed");
    assert!(sink.events.lock().unwrap().iter().any(|e| e == &format!("closed {spoken}: after lunch")));
    a.answer(&spoken, "Today", &p).unwrap();
    assert!(!sink.events.lock().unwrap().iter().any(|e| e.contains("Today")), "a click after a spoken close is not delivered");
    assert_eq!(a.close_question(&question, "no", &p).unwrap(), "already answered by click");
    assert!(a.close_question("ask-unknown", "no", &p).is_err());
    let reload = a.reload("r", &p).unwrap();
    drop(a);
    let a = Actions::open(&path).unwrap();
    a.attach(sink.clone());
    assert_eq!(a.reload("r", &p).unwrap(), reload);
    assert_eq!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == reload)
            .count(),
        1
    );
    a.drain(&p).unwrap();
}
#[test]
fn agent_off_revokes_action_commit_and_outbox_delivery() {
    let d = tempfile::tempdir().unwrap();
    let a = Actions::open(&d.path().join("db")).unwrap();
    let sink = Arc::new(Sink::default());
    a.attach(sink.clone());
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    a.enqueue("x", "private worker result", &p).unwrap();
    gate.store(false, Ordering::Release);
    gate.store(true, Ordering::Release);
    assert!(a.notice("n", "private", "info", None, &p).is_err());
    assert!(a.ask("q", &card("private?", &["Yes", "No"]), &p).is_err());
    assert!(a.reload("r", &p).is_err());
    assert!(a.drain(&p).is_err());
    assert!(sink.events.lock().unwrap().is_empty());
    a.drain(&gate.permit().unwrap()).unwrap();
    // A fresh On permit cannot revive an outbox entry accepted before Off.
    assert!(sink.events.lock().unwrap().is_empty());
}

#[test]
fn notice_updates_return_the_presentation_id_and_retired_receipts_still_deduplicate() {
    let d=tempfile::tempdir().unwrap();let path=d.path().join("db");let a=Actions::open(&path).unwrap();
    a.attach(Arc::new(Sink::default()));let gate=Arc::new(DeliveryGate::new(true));let p=gate.permit().unwrap();
    let first=a.notice("first","First","info",None,&p).unwrap();
    assert_eq!(a.notice("second","Second","success",Some(&first),&p).unwrap(),first);
    let archive=Arc::new(Mutex::new(vec![]));let store=archive.clone();
    a.attach_archive(Arc::new(move|v|{store.lock().unwrap().push(v.clone());Ok("reference".into())}));
    let unresolved=a.ask("unresolved",&card("Keep this question",&["Yes","No"]),&p).unwrap();
    a.retire().unwrap();
    assert_eq!(a.notice("second","Second","success",Some(&first),&p).unwrap(),first);
    assert!(a.notice("second","changed","success",Some(&first),&p).is_err());
    let db=rusqlite::Connection::open(path).unwrap();
    // The body is the card's items (24 Oct); the receipt still reads as the question it asked.
    let body: Value = serde_json::from_str(&db.query_row("SELECT body FROM receipts WHERE id=?",[&unresolved],|r|r.get::<_,String>(0)).unwrap()).unwrap();
    assert_eq!(body, json!({"questions":[{"allowOther":true,"id":"q1","multiSelect":false,"options":[{"label":"Yes"},{"label":"No"}],"question":"Keep this question"}]}));
    assert_eq!(archive.lock().unwrap().len(),2);
}

/// Old `declined` confirmations (from ask_to_confirm) still retire to the archive; a fast read still waiting for
/// the user keeps its body (the restart report names it).
#[test]
fn retire_archives_declined_receipts_and_keeps_waiting_fast_reads() {
    let d=tempfile::tempdir().unwrap();let path=d.path().join("db");let a=Actions::open(&path).unwrap();
    a.attach(Arc::new(Sink::default()));let gate=Arc::new(DeliveryGate::new(true));let p=gate.permit().unwrap();
    let declined=a.receipt("old-confirm","confirm","Publish?",None,&p).unwrap();a.set_state(&declined,"declined",&p).unwrap();
    let reading=a.receipt("read","fastread",r#"["text",null]"#,None,&p).unwrap();a.set_state(&reading,"shown",&p).unwrap();
    let archive=Arc::new(Mutex::new(vec![]));let store=archive.clone();
    a.attach_archive(Arc::new(move|v|{store.lock().unwrap().push(v.clone());Ok("reference".into())}));
    a.retire().unwrap();
    let archived=archive.lock().unwrap().clone();
    assert!(archived.iter().any(|v|v["id"]==declined.as_str()&&v["state"]=="declined"),"{archived:?}");
    assert!(!archived.iter().any(|v|v["id"]==reading.as_str()),"{archived:?}");
    assert_eq!(a.receipts_in("fastread","shown").unwrap(),vec![(reading,r#"["text",null]"#.to_string())]);
}

/// Bug notes (bugnote worker, 19 Oct): plain text, optional evidence, nothing else. Personal edition only.
#[cfg(not(feature = "public"))]
#[test]
fn bug_note_takes_plain_text_and_optional_evidence() {
    assert!(validate_tool("bug_note", &json!({})).is_err());
    assert!(validate_tool("bug_note", &json!({"text":"   "})).is_err());
    assert!(validate_tool("bug_note", &json!({"text":"x".repeat(8001)})).is_err());
    assert!(validate_tool("bug_note", &json!({"text":"Zażółć gęślą ✓ ".repeat(500)})).is_ok());
    assert!(validate_tool("bug_note", &json!({"text":"x","evidence":"e".repeat(32_001)})).is_err());
    assert!(validate_tool("bug_note", &json!({"text":"x","evidence":"e".repeat(32_000)})).is_ok());
    assert!(validate_tool("bug_note", &json!({"text":"x","path":"/tmp/note.md"})).is_err());
    let list = tools();
    let tool = list.as_array().unwrap().iter().find(|t| t["name"] == "bug_note").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    assert_eq!(tool["annotations"]["openWorldHint"], false);
}

/// Scheduled wakes (selfwake, night 21 Oct): a local time or a delay, never both, and a note of 1–2000 characters.
#[test]
fn schedule_wake_takes_a_time_or_a_delay_and_a_bounded_note() {
    assert!(validate_tool("schedule_wake", &json!({"at":"2026-10-23 07:00","message":"x"})).is_ok());
    assert!(validate_tool("schedule_wake", &json!({"message":"x"})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"at":"07:00","in_minutes":5,"message":"x"})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"at":"7pm","message":"x"})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"at":"07:00"})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"in_minutes":0,"message":"x"})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"in_minutes":43_201,"message":"x"})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"in_minutes":5,"message":"x".repeat(2001)})).is_err());
    assert!(validate_tool("schedule_wake", &json!({"in_minutes":5,"message":"ż".repeat(2000)})).is_ok());
    assert!(validate_tool("cancel_wake", &json!({})).is_err());
    let list = tools();
    let read_only = |name: &str| list.as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"].clone();
    assert_eq!(read_only("list_wakes"), true);
    assert_eq!(read_only("schedule_wake"), false);
    assert_eq!(read_only("cancel_wake"), false);
}

/// 19 Oct 18:45–18:47: the user answered "Start a thread…?" by voice, then clicked Later on the card that was still up; the
/// click came back as a second, contradicting answer. Voice overrides the card: whichever lands first wins the card,
/// a click after a voice close is dropped (and says so), and the agent's own ask_close after the host's is harmless.
#[test]
fn voice_and_click_race_for_one_card_and_only_the_first_answer_counts() {
    use agent::actions::{Clicked, Closed};
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(DeliveryGate::new(true));
    let p = gate.permit().unwrap();
    let sink = Arc::new(Sink::default());
    let a = Actions::open(&dir.path().join("receipts.db")).unwrap();
    a.attach(sink.clone());
    // Voice first, then a click: the click is dropped, never delivered.
    let q = a.ask("q1", &card("Start a thread on one test desktop widget?", &["Yes", "Later", "No"]), &p).unwrap();
    assert_eq!(a.close(&q, "Yes, but let's keep it as a separate project", &p).unwrap(), Closed::Closed);
    assert_eq!(a.answer(&q, "Later", &p).unwrap(), Clicked::ClosedByVoice);
    assert!(!sink.events.lock().unwrap().iter().any(|e| e.contains("clicked Later")), "{:?}", sink.events.lock().unwrap());
    assert_eq!(a.ask_answer(&q).unwrap().unwrap().1, "Yes, but let's keep it as a separate project");
    assert_eq!(a.ask_click(&q).unwrap(), None, "a spoken answer never counts as a click");
    // The agent's own ask_close after the host closed it: no second card update.
    assert_eq!(a.close_question(&q, "Yes", &p).unwrap(), "closed");
    assert_eq!(sink.events.lock().unwrap().iter().filter(|e| e.starts_with(&format!("closed {q}"))).count(), 1);
    // Click first, then voice: the click stands, the voice close reports it.
    let q2 = a.ask("q2", &card("Stop the widget thread?", &["Keep going", "Stop it"]), &p).unwrap();
    assert_eq!(a.answer(&q2, "Keep going", &p).unwrap(), Clicked::Delivered);
    assert_eq!(a.answer(&q2, "Keep going", &p).unwrap(), Clicked::AlreadyAnswered);
    assert_eq!(a.close(&q2, "stop it", &p).unwrap(), Closed::AnsweredByClick);
    assert_eq!(sink.events.lock().unwrap().iter().filter(|e| e.contains("clicked Keep going")).count(), 1);
    // A cancelled question (Agent Off) is not reopened by a late voice close.
    let q3 = a.ask("q3", &card("Install?", &["Yes", "No"]), &p).unwrap();
    a.cancel_question(&q3).unwrap();
    assert_eq!(a.close(&q3, "yes", &p).unwrap(), Closed::NotOpen);
    // A guarded T3 confirmation is click-only, by its exact words; other questions are not.
    a.require_click("Delete project 'Vella'?");
    let guarded = a.ask("q4", &card("Delete project 'Vella'?", &["Yes", "No"]), &p).unwrap();
    assert!(a.needs_click(&guarded));
    assert!(!a.needs_click(&q2) && !a.needs_click("ask-unknown"));
}
