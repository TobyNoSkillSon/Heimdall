//! Opt-in replay eval on Heimdall's real session up to 20:45 on 23 Oct and the lists as they stood then
//! (`tests/fixtures/plan-replay-1023`): one refresher pass on Claude Haiku 5.5. Until 31 Oct it scored To do (the
//! three Vella items the user had set aside, Morgan's chore); To do is Goals' now (`docs/goals.md`), so it scores what
//! the refresher still keeps: nothing lost from Scheduled and Settled, and To do untouched.
//!
//! Two cuts: "before" ends just before the user's 20:45 complaint; "with" adds his 20:45:39 line ("none of them are
//! good"). Scored per run: every open Scheduled item still on Scheduled or Settled (at most one may go, fairly: a
//! vague idea), Settled keeps at least 18 of its 20 items, and the fixture's `todo.json` is byte for byte the same
//! after the answer is applied to the islands copy. The night-pass note is left out (it kept changing after 20:45).
//!
//! The session is read-only from the user's data folder (only its prefix is copied); the model is reached with
//! HEIMDALL_ANTHROPIC_API_KEY when set, else through the `claude` CLI (`-p`, same model, system prompt, schema and
//! effort; no tools). About $0.10 a run. With the key each run goes through the shared background client and prints
//! its cache read and write and its cost: the second run of a cut reads the first one's cache (item 28).
//! HEIMDALL_PLAN_EVAL_OLD_SYSTEM=<an older system prompt> also runs it for a before/after table; the test asserts on
//! the current prompt only.
//! [HEIMDALL_PLAN_EVAL_SESSION=<session.jsonl>] [HEIMDALL_PLAN_EVAL_RUNS=2] [HEIMDALL_PLAN_EVAL_OUT=<dir>]
//! cargo test -p app --bin heimdall -- --ignored replay_23_oct_lists --nocapture
use super::*;
use std::io::Write;

/// UTC: the last entry kept for each cut (the user's complaint arrived at 18:45:39.149Z, Heimdall acted at 18:45:45).
const CUTS: [(&str, &str); 2] = [("before", "2026-10-23T18:45:39.000Z"), ("with", "2026-10-23T18:45:45.000Z")];
/// Settled items a pass may let go of (it keeps "the most recent 20 or so").
const SETTLED_SLACK: usize = 2;

struct Score { lost: Vec<String>, settled_kept: usize, settled_before: usize, added: Vec<String> }

fn fixture() -> PathBuf { agent::runtime::checkout_root().join("crates/app/tests/fixtures/plan-replay-1023") }

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() { std::fs::copy(e.path(), to.join(e.file_name())).unwrap(); }
}

/// The session's lines up to `cut` (file order; lines without a timestamp ride along).
fn session_prefix(session: &Path, cut: &str, data: &Path) {
    let text = std::fs::read_to_string(session).unwrap();
    let mut out = String::new();
    for line in text.lines() {
        let ts = serde_json::from_str::<Value>(line).ok().and_then(|v| v["timestamp"].as_str().map(str::to_owned));
        if ts.as_deref().is_some_and(|t| t > cut) { break }
        out.push_str(line);
        out.push('\n');
    }
    std::fs::create_dir_all(data).unwrap();
    std::fs::write(data.join("pi-session.jsonl"), out).unwrap();
}

/// One answer: the API with a key, else the `claude` CLI. Returns the answer's JSON text, its cost when known and,
/// on the API, its usage.
async fn ask(system: &str, prompt: &crate::background_call::Prompt, scratch: &Path) -> (String, Option<f64>, Option<crate::background_call::Usage>) {
    let e = Endpoint::anthropic();
    if e.key.is_some() {
        let a = call_model_with(&e, system, prompt).await.unwrap();
        return (a.text, Some(a.usd), Some(a.usage));
    }
    let sys = scratch.join("system.md");
    std::fs::write(&sys, system).unwrap();
    let mut child = std::process::Command::new("claude")
        .args(["-p", "--model", MODEL, "--effort", EFFORT, "--tools", "", "--no-session-persistence", "--setting-sources", "",
               "--output-format", "json", "--system-prompt-file"])
        .arg(&sys).arg("--json-schema").arg(schema().to_string())
        .current_dir(scratch)
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .spawn().expect("no API key and no `claude` CLI");
    child.stdin.take().unwrap().write_all(prompt.text().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("claude CLI: {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(v["is_error"], false, "claude CLI: {v}");
    (v["structured_output"].to_string(), v["total_cost_usd"].as_f64(), None)
}

fn score(plan: &Plan, before: &Lists) -> Score {
    let known: HashSet<&str> = before.iter().flatten().filter_map(|o| o.get("id").and_then(Value::as_str)).collect();
    let anywhere = |id: &str| plan.lists.iter().flatten().any(|i| i.id == id);
    let id_title = |o: &Map<String, Value>| Some((o.get("id")?.as_str()?.to_owned(), o.get("title")?.as_str()?.to_owned()));
    Score {
        lost: before[0].iter().filter(|o| o.get("done") != Some(&Value::Bool(true))).filter_map(id_title)
            .filter(|(id, _)| !anywhere(id)).map(|(_, t)| t).collect(),
        settled_kept: before[1].iter().filter_map(id_title).filter(|(id, _)| plan.lists[1].iter().any(|i| &i.id == id)).count(),
        settled_before: before[1].len(),
        added: plan.lists.iter().flatten().filter(|i| !known.contains(i.id.as_str())).map(|i| i.title.clone()).collect(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn replay_23_oct_lists() {
    let session = std::env::var("HEIMDALL_PLAN_EVAL_SESSION").map(PathBuf::from)
        .unwrap_or_else(|_| session_path(&heimdall_home::data_dir()));
    let runs: usize = std::env::var("HEIMDALL_PLAN_EVAL_RUNS").ok().and_then(|r| r.parse().ok()).unwrap_or(2);
    let keep = std::env::var("HEIMDALL_PLAN_EVAL_OUT").ok().map(PathBuf::from);
    let tmp = tempfile::tempdir().unwrap();
    let out = keep.clone().unwrap_or_else(|| tmp.path().join("out"));
    std::fs::create_dir_all(&out).unwrap();
    let mut prompts = vec![("new", SYSTEM.to_owned())];
    if let Ok(p) = std::env::var("HEIMDALL_PLAN_EVAL_OLD_SYSTEM") { prompts.insert(0, ("old", std::fs::read_to_string(p).unwrap())); }
    let now = chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 23, 20, 45, 0).unwrap();
    let (mut failures, mut total) = (Vec::new(), 0);
    let mut cost = 0.0;
    for (cut, until) in CUTS {
        let dir = tmp.path().join(cut);
        session_prefix(&session, until, &dir.join("data"));
        // No night note: the live one keeps changing after 20:45 (a run would see the future and not repeat).
        let cfg = |root: Root| Config { data: dir.join("data"), islands: root, notes: dir.join("notes"),
                                        endpoint: Arc::new(Endpoint::default), enabled: Arc::new(|| true) };
        let first = Root(dir.join("islands-root"));
        copy_dir(&fixture().join("islands"), &first.islands());
        let g = gather(&cfg(first), None).unwrap().unwrap();
        let prompt = user_prompt_at(&g, now);
        assert!(prompt.text().contains("hold on on uh Vella"), "the cut lost the 18:57 line");
        for (name, system) in &prompts {
            for run in 1..=runs {
                let (answer, usd, usage) = ask(system, &prompt, tmp.path()).await;
                cost += usd.unwrap_or(0.0);
                if let Some(u) = usage {
                    eprintln!("{cut:6} {name}  run {run}: {} prompt tokens, cache read {}, cache write {}, uncached {}, output {}, ${:.4}",
                        u.prompt(), u.cache_read, u.cache_write(), u.input, u.output, usd.unwrap_or(0.0));
                }
                std::fs::write(out.join(format!("{cut}-{name}-{run}.json")), &answer).unwrap();
                let plan = validate(&answer, &g.lists, &g.jobs, &g.pictures, &g.conversation.join("\n")).unwrap();
                // Applied to a fresh copy of the islands: To do comes out byte for byte as it went in.
                let root = Root(dir.join(format!("islands-{name}-{run}")));
                copy_dir(&fixture().join("islands"), &root.islands());
                let todo = std::fs::read(root.file("todo")).unwrap();
                apply(&root, &g.lists, &plan, &g.jobs).unwrap();
                let todo_kept = std::fs::read(root.file("todo")).unwrap() == todo;
                let s = score(&plan, &g.lists);
                eprintln!("{cut:6} {name}  run {run}: Scheduled lost {:?}; Settled kept {}/{}; added {:?}; todo.json untouched {todo_kept}",
                    s.lost, s.settled_kept, s.settled_before, s.added);
                total += usize::from(*name == "new");
                if *name == "new" && (s.lost.len() > 1 || s.settled_kept + SETTLED_SLACK < s.settled_before || !todo_kept) {
                    failures.push(format!("{cut} run {run}"));
                }
            }
        }
    }
    eprintln!("answers in {}; cost ${cost:.4} (list price)", out.display());
    // Haiku varies between runs: at most one run in three may miss.
    assert!(failures.len() * 3 <= total, "the current prompt missed {} of {total} runs: {failures:?}", failures.len());
}
