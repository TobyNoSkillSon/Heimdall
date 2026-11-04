use super::*;
use crate::background_call::t3 as bt3;
use agent::accounts::{Kind, Window};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

const ACCT1: &str = "t3:claudeAgent";
const ACCT2: &str = "t3:claudeAgent_claude_code_2";
const HAIKU: &str = "claude-haiku-5-5";

fn m(p: &str, id: &str) -> BackgroundModel { BackgroundModel { provider: p.into(), id: id.into() } }
fn luna() -> BackgroundModel { m("openai", "gpt-6-luna") }
fn world() -> World {
    World { claude: CLAUDE_ACCOUNTS.iter().map(|s| s.to_string()).collect(), quota: None, shared: luna(), opencode: Some("kimi-k3".into()), engine: true, paid: true, now_ms: 1_000 }
}
fn names(steps: &[Step]) -> Vec<String> { steps.iter().map(|s| format!("{}/{}", s.model.provider, s.model.id)).collect() }
/// An endpoint for a table: configured unless its provider is in `missing`.
fn table_endpoint(missing: &'static [&'static str]) -> impl Fn(&BackgroundModel) -> Endpoint {
    move |b: &BackgroundModel| {
        let provider: &'static str = Box::leak(b.provider.clone().into_boxed_str());
        Endpoint { provider, model: Some(b.id.clone()), key: (!missing.contains(&provider)).then(|| "k".into()), ..Default::default() }
    }
}
fn pick_of(steps: &[Step], w: &World, missing: &'static [&'static str], spent: &[&str]) -> Option<String> {
    let spent: Vec<String> = spent.iter().map(|s| s.to_string()).collect();
    choose(steps, w, &table_endpoint(missing), &|p: &str| !spent.iter().any(|s| s == p)).map(|(s, _)| format!("{}/{}", s.model.provider, s.model.id))
}

/// The user's order (30 Oct 07:37): the other Claude Code account, Codex, OpenCode Go, the local engine, API keys last.
#[test]
fn the_fallback_order_table() {
    let w = world();
    let steps = chain(&m(ACCT2, HAIKU), false, &w);
    assert_eq!(names(&steps), [format!("{ACCT2}/{HAIKU}"), format!("{ACCT1}/{HAIKU}"), "openai/gpt-6-luna".into(), "opencode-go/kimi-k3".into(), "local/".into(), format!("anthropic/{HAIKU}")]);
    assert_eq!(pick_of(&steps, &w, &[], &[]), Some(format!("{ACCT2}/{HAIKU}")), "its pick while usable");
    assert_eq!(pick_of(&steps, &w, &[], &[ACCT2]), Some(format!("{ACCT1}/{HAIKU}")), "account 2 spent: account 1");
    assert_eq!(pick_of(&steps, &w, &[], &[ACCT2, ACCT1]).as_deref(), Some("openai/gpt-6-luna"), "both spent: Codex");
    assert_eq!(pick_of(&steps, &w, &[], &[ACCT2, ACCT1, "openai"]).as_deref(), Some("opencode-go/kimi-k3"), "Codex spent: OpenCode Go");
    assert_eq!(pick_of(&steps, &w, &[], &[ACCT2, ACCT1, "openai", "opencode-go"]).as_deref(), Some("local/"), "then the local engine");
    assert_eq!(pick_of(&steps, &w, &[], &[ACCT2, ACCT1, "openai", "opencode-go", "local"]), Some(format!("anthropic/{HAIKU}")), "the API key last");
    assert_eq!(pick_of(&steps, &w, &[], &[ACCT2, ACCT1, "openai", "opencode-go", "local", "anthropic"]), None, "nothing left: no call");
    // Not configured is skipped like spent; nothing configured: no call.
    assert_eq!(pick_of(&steps, &w, &["openai", "opencode-go"], &[ACCT2, ACCT1]).as_deref(), Some("local/"));
    let none: &'static [&'static str] = &[ACCT1, ACCT2, "openai", "opencode-go", "local", "anthropic"];
    assert_eq!(pick_of(&steps, &w, none, &[]), None);
    // Without OpenCode Go's catalog or the engine, those steps are not there.
    let bare = World { opencode: None, engine: false, ..world() };
    assert_eq!(names(&chain(&m(ACCT2, HAIKU), false, &bare)).len(), 4);
    // Codex runs the shared model when that is a Codex model; a Claude pick's own model goes to the other account and
    // the API key.
    let sol = World { shared: m("openai", "gpt-6.1-sol"), ..world() };
    assert!(names(&chain(&m(ACCT1, "claude-sonnet-5-5"), false, &sol)).contains(&"openai/gpt-6.1-sol".to_string()));
    assert_eq!(names(&chain(&m(ACCT1, "claude-sonnet-5-5"), false, &sol))[1], format!("{ACCT2}/claude-sonnet-5-5"));
    // A Codex pick (Luna) falls to the Claude accounts on Haiku first; "t3:" is the default instance.
    assert_eq!(names(&chain(&luna(), false, &w))[..3], ["openai/gpt-6-luna".to_string(), format!("{ACCT1}/{HAIKU}"), format!("{ACCT2}/{HAIKU}")]);
    assert_eq!(names(&chain(&m("t3:", HAIKU), false, &w))[1], format!("{ACCT1}/{HAIKU}"));
}

/// Review 30 Oct: the paid step (the Anthropic API key) only when the user allowed it; a custom provider (maybe on this Mac
/// or the private network) never falls back; the quota is read only when the choice depends on it.
#[test]
fn the_paid_step_is_opt_in_and_custom_picks_stay_put() {
    let free = World { paid: false, ..world() };
    let steps = chain(&m(ACCT2, HAIKU), false, &free);
    assert!(!names(&steps).iter().any(|n| n.starts_with("anthropic/")), "{:?}", names(&steps));
    assert_eq!(pick_of(&steps, &free, &[], &[ACCT2, ACCT1, "openai", "opencode-go", "local"]), None, "no paid step: no call");
    for p in ["openai-compatible", "anthropic-compatible", ui::LOCAL_ENGINE] {
        assert_eq!(chain(&m(p, "qwen"), false, &world()).len(), 1, "{p}");
    }
    assert!(falls_back(&m(ACCT2, HAIKU)) && falls_back(&luna()) && falls_back(&m("anthropic", HAIKU)) && falls_back(&m("opencode-go", "kimi-k3")));
    let steps = chain(&m(ACCT2, HAIKU), false, &world());
    let step = |i: usize| steps[i].clone();
    assert!(!quota_matters(&steps, &step(4)), "the local engine: no reading");
    assert!(quota_matters(&steps, &step(2)), "Codex: its quota decides");
    assert!(!quota_matters(&steps, &step(1)), "one other Claude account: nothing to rank");
    let three = World { claude: vec!["a".into(), "b".into(), "c".into()], ..world() };
    let steps = chain(&m("t3:a", HAIKU), false, &three);
    assert!(quota_matters(&steps, &steps[1]), "two Claude accounts left: the soonest reset decides");
}

/// Suggestions runs a tool loop: no T3 step, even when its pick or the shared model is on T3.
#[test]
fn a_tool_loop_skips_every_t3_backend() {
    let w = World { shared: m(ACCT2, HAIKU), ..world() };
    let steps = chain(&m(ACCT2, HAIKU), true, &w);
    assert!(steps.iter().all(|s| !s.model.provider.starts_with("t3:")), "{:?}", names(&steps));
    assert_eq!(names(&steps)[0], "openai/gpt-6-luna");
    assert_eq!(names(&chain(&luna(), true, &world())), ["openai/gpt-6-luna", "opencode-go/kimi-k3", "local/", &format!("anthropic/{HAIKU}")]);
}

/// A local pick never falls back to the cloud.
#[test]
fn a_local_pick_stays_local() {
    assert_eq!(names(&chain(&m(ui::LOCAL_ENGINE, ""), false, &world())), ["local/"]);
}

fn acct(i: &str, used: f64, reset: u64) -> AccountQuota {
    AccountQuota { instance: i.into(), kind: if i == "codex" { Kind::Codex } else { Kind::Claude },
        windows: vec![Window { name: "5-hour".into(), used_percent: Some(used), resets_at_ms: Some(reset) }], reached: None, error: None }
}

/// With three Claude accounts, the earliest reset among those not spent goes first; a spent Codex account is skipped.
#[test]
fn the_account_that_renews_soonest_goes_first() {
    let now = 1_000;
    let w = World { claude: vec!["a".into(), "b".into(), "c".into(), "d".into()],
        quota: Some(vec![acct("a", 100.0, now + 60_000), acct("b", 40.0, now + 7_200_000), acct("c", 70.0, now + 900_000), acct("codex", 100.0, now + 3_600_000)]), ..world() };
    let steps = chain(&m("t3:b", HAIKU), false, &w);
    assert_eq!(names(&steps)[..3], [format!("t3:b/{HAIKU}"), format!("t3:c/{HAIKU}"), format!("t3:d/{HAIKU}")], "c renews before d (unknown), a is spent");
    assert!(!names(&steps).iter().any(|n| n.starts_with("openai/")), "Codex spent");
    assert_eq!(pick_of(&steps, &w, &[], &["t3:b"]).as_deref(), Some(&*format!("t3:c/{HAIKU}")));
    // The pick's own account spent by its quota: the next one, without a failed call first (the others unread: T3's order).
    let spent_pick = World { quota: Some(vec![acct("b", 100.0, now + 60_000)]), ..w.clone() };
    assert_eq!(pick_of(&chain(&m("t3:b", HAIKU), false, &spent_pick), &spent_pick, &[], &[]).as_deref(), Some(&*format!("t3:a/{HAIKU}")));
}

/// A T3 that never answers (each test that calls it registers its own).
struct Quiet;
#[async_trait::async_trait]
impl bt3::Calls for Quiet {
    async fn call(&self, name: &str, _: Value, _: Duration) -> anyhow::Result<Value> { anyhow::bail!("{name}: not expected") }
}

fn app_state(t3: Arc<dyn bt3::Calls>) -> Arc<Mutex<Vec<Vec<(Task, String, String)>>>> {
    bt3::set_calls(t3, std::env::temp_dir().join("heimdall-fallback-test"), bt3::Limits::default());
    crate::background_model::publish(&luna(), &[]);
    crate::background_model::publish_tasks(&crate::background_model::with_task_model(&[], Task::Islands, Some(m(ACCT2, HAIKU))));
    crate::background_model::publish_engine(true);
    let seen = Arc::new(Mutex::new(vec![]));
    let keep = seen.clone();
    set_routes_sink(Arc::new(move |r| keep.lock().unwrap().push(r)));
    seen
}

/// The app's path: a spent account moves the task, the sign lists it, and the first call after the reset is on its
/// pick again (the sign sent again, empty).
#[test]
fn a_spent_pick_moves_the_task_and_it_returns_after_the_reset() {
    let seen = app_state(Arc::new(Quiet));
    let now = agent::now_ms();
    let e = crate::background_model::task_endpoint(Task::Islands);
    assert_eq!((e.provider, e.key.is_some()), (ACCT2, true));
    assert!(routes().is_empty() && seen.lock().unwrap().is_empty());
    crate::background_gate::quota_spent(ACCT2, Some(now + 300), now);
    let e = crate::background_model::task_endpoint(Task::Islands);
    assert_eq!((e.provider, e.model.as_deref()), (ACCT1, Some(HAIKU)), "the other Claude Code account");
    assert_eq!(routes(), [(Task::Islands, ACCT1.to_string(), HAIKU.to_string())]);
    // T3 itself down: both accounts skipped; Codex is not signed in here, so the local engine.
    crate::background_gate::backend_down(crate::background_gate::T3_ALL, "test", now);
    let e = crate::background_model::task_endpoint(Task::Islands);
    assert_eq!(e.provider, ui::LOCAL_ENGINE);
    assert_eq!(seen.lock().unwrap().last().unwrap(), &vec![(Task::Islands, ui::LOCAL_ENGINE.to_string(), String::new())]);
    // The other tasks keep their own pick (Luna, not signed in here: no call, and no move).
    assert!(crate::background_model::task_endpoint(Task::Plan).key.is_none());
    assert!(!routes().iter().any(|(t, ..)| *t == Task::Plan));
    // The reset passes and T3 answers again: back on its pick, the sign cleared.
    crate::background_gate::quota_back(crate::background_gate::T3_ALL);
    std::thread::sleep(Duration::from_millis(350));
    let e = crate::background_model::task_endpoint(Task::Islands);
    assert_eq!(e.provider, ACCT2);
    assert!(routes().is_empty());
    assert_eq!(seen.lock().unwrap().last().unwrap(), &Vec::<(Task, String, String)>::new());
}

/// Suggestions with the shared model on T3: its calls never go to T3 (a tool loop), and a T3 pick is refused.
#[test]
fn suggestions_never_run_on_t3() {
    app_state(Arc::new(Quiet));
    crate::background_model::publish(&m(ACCT2, HAIKU), &[]);
    let e = crate::background_model::task_endpoint(Task::Suggestions);
    assert_eq!(e.provider, ui::LOCAL_ENGINE);
    assert_eq!(routes(), [(Task::Suggestions, ui::LOCAL_ENGINE.to_string(), String::new())]);
    assert!(!crate::background_model::task_accepts(Task::Suggestions, &m(ACCT1, HAIKU)));
    assert!(crate::background_model::task_accepts(Task::Suggestions, &luna()) && crate::background_model::task_accepts(Task::Islands, &m(ACCT1, HAIKU)));
}

/// No switch in the middle of a call: a call that started on account 2 and met its usage limit fails there (nothing is
/// launched on another account during it); only the next call moves.
#[test]
fn a_call_never_switches_backend_midway() {
    static LAUNCHES: Mutex<Vec<String>> = Mutex::new(Vec::new());
    struct Spent;
    #[async_trait::async_trait]
    impl bt3::Calls for Spent {
        async fn call(&self, name: &str, args: Value, _: Duration) -> anyhow::Result<Value> {
            let reset = agent::now_ms() / 1000 + 3600;
            Ok(match name {
                "t3_project_list" => json!({"projects": [{"id": "p", "workspaceRoot": std::env::temp_dir().join("heimdall-fallback-midcall")}]}),
                "t3_thread_launch" => { LAUNCHES.lock().unwrap().push(args["modelSelection"]["instanceId"].as_str().unwrap().into()); json!({"threadId": "th", "runId": "r"}) }
                "t3_thread_wait" => json!({"status": "completed", "timedOut": false}),
                "t3_thread_read" => json!({"thread": {"status": "idle"}, "items": [{"position": 1, "itemId": "i", "type": "assistant_message", "status": "completed", "text": format!("Claude AI usage limit reached|{reset}")}], "hasMore": false}),
                "t3_thread_organize" | "t3_thread_interrupt" => json!({}),
                other => panic!("{other}"),
            })
        }
    }
    app_state(Arc::new(Spent));
    bt3::set_calls(Arc::new(Spent), std::env::temp_dir().join("heimdall-fallback-midcall"), bt3::Limits::default());
    let e = crate::background_model::task_endpoint(Task::Islands);
    assert_eq!(e.provider, ACCT2);
    let c = crate::background_call::Call { job: "island agent", model: HAIKU.into(), system: "s", schema: json!({"type": "object"}), effort: "low", prompt: "x".into(), history: Vec::new(), session: None };
    assert!(crate::background_call::ask_blocking(&e, &c).is_err(), "the call fails on account 2");
    assert_eq!(*LAUNCHES.lock().unwrap(), ["claudeAgent_claude_code_2"], "nothing launched elsewhere during the call");
    assert_eq!(crate::background_model::task_endpoint(Task::Islands).provider, ACCT1, "the next call moves");
}

/// Review 30 Oct: the sign clears when it stops being true, not at the task's next call: the task turned off, its pick
/// changed, or its pick answering again; each change sends the list once.
#[test]
fn the_sign_clears_without_waiting_for_the_next_call() {
    let seen = app_state(Arc::new(Quiet));
    let now = agent::now_ms();
    crate::background_gate::quota_spent(ACCT2, Some(now + 3_600_000), now);
    assert_eq!(crate::background_model::task_endpoint(Task::Islands).provider, ACCT1);
    // Its pick answers again (another call's success): cleared at once.
    crate::background_gate::quota_back(ACCT2);
    assert!(routes().is_empty() && seen.lock().unwrap().last().unwrap().is_empty());
    // Turned off.
    crate::background_gate::quota_spent(ACCT2, Some(now + 3_600_000), now);
    crate::background_model::task_endpoint(Task::Islands);
    crate::background_model::publish(&luna(), &["islands".into()]);
    refresh_routes();
    assert!(routes().is_empty());
    // A new pick.
    crate::background_model::publish(&luna(), &[]);
    crate::background_model::task_endpoint(Task::Islands);
    assert_eq!(routes().len(), 1);
    crate::background_model::publish_tasks(&crate::background_model::with_task_model(&[], Task::Islands, Some(m(ACCT1, HAIKU))));
    refresh_routes();
    assert!(routes().is_empty());
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent.iter().filter(|r| r.is_empty()).count(), 3, "one empty list per clearing: {sent:?}");
    refresh_routes();
    assert_eq!(seen.lock().unwrap().len(), sent.len(), "nothing changed: nothing sent");
}

/// The accounts' quota reading at the app's path: account 1 shows spent, so a task off account 2 skips it too.
#[test]
fn the_quota_reading_skips_a_spent_account() {
    struct Never;
    impl agent::accounts::QuotaSource for Never {
        fn read(&self) -> anyhow::Result<Vec<AccountQuota>> { anyhow::bail!("not read in this test") }
    }
    app_state(Arc::new(Quiet));
    let now = agent::now_ms();
    set_quota_board(agent::accounts::Board::with(Arc::new(Never), vec![acct("claudeAgent", 100.0, now + 3_600_000)], now));
    crate::background_gate::quota_spent(ACCT2, Some(now + 3_600_000), now);
    assert_eq!(crate::background_model::task_endpoint(Task::Islands).provider, ui::LOCAL_ENGINE);
}

/// A pick that is only down (a 5xx, overloaded, unreachable) moves when there is somewhere else to go; with nowhere else
/// it is tried again on the next pass, as before the fallback. A spent pick with nowhere else makes no call.
#[test]
fn a_down_pick_is_retried_when_nothing_else_is_usable() {
    app_state(Arc::new(Quiet));
    let now = agent::now_ms();
    crate::background_gate::backend_down(ACCT2, "API Error: 529 overloaded", now);
    // With a second backend: it moves.
    assert_eq!(crate::background_model::task_endpoint(Task::Islands).provider, ACCT1);
    // Without one (the other account down too, no engine, nothing else connected here): its pick, tried again.
    crate::background_gate::backend_down(ACCT1, "HTTP 503", now);
    crate::background_model::publish_engine(false);
    let e = crate::background_model::task_endpoint(Task::Islands);
    assert_eq!((e.provider, e.key.is_some()), (ACCT2, true), "nowhere else: the pick again");
    assert!(routes().is_empty(), "on its pick: no sign");
    assert!(crate::background_gate::quota_check(ACCT2, now + 1).is_ok(), "a down backend is not refused");
    // Spent with nowhere else: no call.
    crate::background_gate::quota_spent(ACCT2, Some(now + 3_600_000), now);
    assert!(crate::background_model::task_endpoint(Task::Islands).key.is_none());
}
