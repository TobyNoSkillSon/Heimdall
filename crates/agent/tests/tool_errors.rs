//! Errors that carry the fix, and limits that fit themselves (night 1111, lane T; tool-call study 10 Nov, proposal 6:
//! Assistant/Research/tool-calls-2026-11-10/notes.md). Every input below is the exact one that failed in the window
//! 27 Oct - 10 Nov, copied from the transcripts.
use agent::mcp_contract::{fit_args_in, first_words, shorten, validate_tool, wall_ns};
use chrono::FixedOffset;
use serde_json::{json, Value};

fn cet() -> FixedOffset { FixedOffset::east_opt(3600).unwrap() }

fn fitted(name: &str, args: Value) -> (Value, Vec<String>) {
    let (a, notes) = fit_args_in(name, &args, &cet());
    validate_tool(name, &a).unwrap_or_else(|e| panic!("{name} {a}: {e}"));
    (a, notes)
}

/// `ask` (9 length failures of 11): "string length outside bounds; follow tools/list" named neither length nor limit.
#[test]
fn ask_bounds_errors_name_the_field_its_length_and_the_limit() {
    for (args, field) in [
        (json!({"question": "Tonight's film redo: which model per film?", "options": ["FlashVSR for both", "SeedVR2 for both", "FlashVSR Vernon, SeedVR2 loco"]}), "arguments.options: 29 characters; the limit is 1 to 24"),
        (json!({"question": "Rename the \"Vibe coding\" heading? It costs points with every judge and recruiter.", "options": ["Programowanie i narz\u{0119}dzia", "Keep Vibe coding"]}), "arguments.options: 25 characters; the limit is 1 to 24"),
        (json!({"questions": [{"question": "Release Heimdall on GitHub now with the recommended picks (alpha, source only, 8 personal features hidden, beta ones flagged)?", "header": "Release", "options": ["Go, recommended picks", "Let me pick first"]}, {"question": "Also put the Heimdall and Arsenal sites live on tobynoskillson.com for the CV links?", "header": "Sites", "options": ["Yes, deploy both", "Not yet"]}]}), "arguments.questions.question: "),
        (json!({"questions": [{"question": "The app is signed with your personal certificate (shows your email). Make a separate Heimdall signing certificate and re-sign?", "header": "Signing", "options": ["Yes, re-sign", "Keep as is"]}, {"question": "The app is 4.8 GB, too big for GitHub. Host the zip on your Hugging Face for downloads and Homebrew?", "header": "Download", "options": ["Host on Hugging Face", "Source only for now"]}]}), "arguments.questions.question: "),
        (json!({"question": "The hook forbids keychain changes outright, even with a click; lifting it needs a Heimdall code change and reinstall. Sign the alpha ad-hoc?", "options": ["Ad-hoc for alpha", "Change the hook"]}), "arguments.question: "),
        (json!({"question": "Look into a Discord integration so I can send files or messages as you? (User-token self-bots break Discord's rules and risk a ban, so I'd check safe routes first.)", "options": ["Research it", "Later", "Ignore"]}), "arguments.question: "),
        (json!({"question": "Give the test virtual Mac your microphone and speakers, so you can finish setup in it and hear the greeting? (One macOS mic prompt.)", "options": ["Yes, connect them", "No"]}), "arguments.question: "),
    ] {
        let e = validate_tool("ask", &args).unwrap_err().to_string();
        assert!(e.starts_with(field) && e.contains(" characters; the limit is "), "{args}\n{e}");
        assert!(!e.contains("outside bounds"), "{e}");
    }
}

/// Every other bounds error from the schema validator names the value and the limit too.
#[test]
fn number_and_list_bounds_name_the_value_and_the_limit() {
    let e = validate_tool("screen_frames", &json!({"at": "2026-11-03T23:39:58+01:00", "count": 9})).unwrap_err().to_string();
    assert_eq!(e, "arguments.count: 9; the limit is 1 to 4");
    let e = validate_tool("islands_remove", &json!({"ids": []})).unwrap_err().to_string();
    assert_eq!(e, "arguments.ids: 0 items; the limit is 1 to 20");
}

/// `islands_view` and `islands_put` titles of 4-5 words (9 failures) are shortened to the first three and the result says so.
#[test]
fn island_titles_over_three_words_are_shortened_not_refused() {
    for (tool, args, want) in [
        ("islands_view", json!({"path": "/Users/user/Work/Projects/Heimdall-heimdall-goals-scheduled-settled-build-t/islands/lab/one-system-shots/sheet.png", "title": "New Goals, every state"}), "New Goals"),
        ("islands_view", json!({"path": "/Users/user/Work/Assistant/Research/2026-11-04-deep-thought-infinite-space.html", "title": "Deep thought and infinite space"}), "Deep thought"),
        ("islands_view", json!({"path": "/Users/user/Work/Assistant/Temp/music/minimax-test-golden-waves.wav", "title": "MiniMax Music 3 test"}), "MiniMax Music 3"),
        ("islands_view", json!({"path": "/Users/user/Work/Assistant/Research/phone-look-2026-11-10/before-after.png", "title": "Phone app: before / after"}), "Phone app"),
        ("islands_view", json!({"path": "/Users/user/Work/Assistant/Research/rpms-2026-11-10/data-rules.html", "title": "Law firm data rules"}), "Law firm data"),
        ("islands_put", json!({"id": "task-qwen-3.8-flash", "title": "Flash on 128 GB", "icon": "memorychip"}), "Flash on 128"),
    ] {
        let (a, notes) = fitted(tool, args);
        assert_eq!(a["title"], want);
        assert_eq!(notes, vec![format!("Title shortened to \"{want}\" (1-3 words); set it again to name it otherwise.")]);
    }
    // Three words or fewer pass untouched, with no note.
    let (a, notes) = fitted("islands_view", json!({"path": "/tmp/a.png", "title": "Phone app"}));
    assert_eq!((a["title"].as_str(), notes.len()), (Some("Phone app"), 0));
}

/// An action label of 25 characters (the limit is 24) is shortened at a word break.
#[test]
fn action_labels_over_24_characters_are_shortened() {
    let (a, notes) = fitted("islands_view", json!({"path": "/tmp/a.html", "title": "Report", "actions": [{"label": "Approve and install it now", "action": "go"}, {"label": "Later", "action": "later"}]}));
    assert_eq!(a["actions"][0]["label"], "Approve and install it");
    assert_eq!(a["actions"][1]["label"], "Later");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(shorten("Abcdefghijklmnopqrstuvwxyz0", 24).chars().count(), 24, "one long word is cut hard");
}

/// Titles refused for length elsewhere are shortened too: a new plan item (3 Nov 22:55, 67 characters, limit 64),
/// delegate_task, phone_show. A reference to an existing item is never shortened.
#[test]
fn every_title_refused_for_length_is_shortened() {
    let (a, notes) = fitted("plan_edit", json!({"action": "propose", "list": "scheduled", "item": "Vella: proper Nemotron 3.5 Transcribe run, then add it to the picker", "due": "2026-11-04T23:00", "why": "User 22:54: \"we need to schedule some time where we can do a proper Vella run on the new Nemotron model so we can add it to the selection... after we're done with Vella we can delete the model\""}));
    assert_eq!(a["item"], "Vella: proper Nemotron 3.5 Transcribe run");
    assert_eq!(notes.len(), 1);
    let long = "Heimdall bug round: fix all open bug notes plus log findings, then rebuild, retest every path User uses daily, and install the result on his Mac tonight after a fresh review";
    let (a, _) = fitted("delegate_task", json!({"title": long, "task": "x", "target": {"model": "claude-opus-5-5", "providerInstanceId": "claudeAgent"}}));
    let t = a["title"].as_str().unwrap();
    assert!(t.chars().count() <= 160 && long.starts_with(t), "{t}");
    let (a, notes) = fit_args_in("plan_edit", &json!({"action": "remove", "item": "x".repeat(90)}), &cet());
    assert_eq!((a["item"].as_str().unwrap().len(), notes.len()), (90, 0));
    if !cfg!(feature = "public") {
        let (a, _) = fitted("phone_show", json!({"path": "/tmp/r.html", "title": "A report title that runs on and on well past the sixty character limit"}));
        assert!(a["title"].as_str().unwrap().chars().count() <= 60);
    }
}

/// `screen_frames` at a time without an offset (3 Nov 23:40: "time: use RFC3339 with an explicit offset") is the user's
/// local time; the same for timeline_search and archive_clip windows.
#[test]
fn a_time_without_offset_is_owners_local_time() {
    let (a, notes) = fitted("screen_frames", json!({"at": "2026-11-03T23:39:58"}));
    assert_eq!(a["at"], "2026-11-03T23:39:58+01:00");
    assert_eq!(notes, vec!["at had no offset; read as the user's local time, 2026-11-03T23:39:58+01:00.".to_string()]);
    let (a, _) = fitted("timeline_search", json!({"start": "2026-11-06T12:10", "end": "2026-11-06T12:40:00+01:00", "limit": 5}));
    assert_eq!((a["start"].as_str(), a["end"].as_str()), (Some("2026-11-06T12:10:00+01:00"), Some("2026-11-06T12:40:00+01:00")));
    let (a, notes) = fit_args_in("screen_frames", &json!({"at": "now"}), &cet());
    assert_eq!((a["at"].as_str(), notes.len()), (Some("now"), 0));
    // wall_ns reads the same input (in the Mac's zone) rather than refusing it.
    assert!(wall_ns("2026-11-03T23:39:58").is_ok());
    assert!(wall_ns("yesterday").unwrap_err().to_string().contains("local date-time"));
}

#[test]
fn first_words_keeps_the_name() {
    assert_eq!(first_words("Qwen-Image download progress now", 3), "Qwen-Image download progress");
    assert_eq!(first_words("Arsenal: show what each model's memory is for", 3), "Arsenal:".trim_end_matches(':'));
}
