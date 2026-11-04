//! The one check every background model request passes just before it goes out: with Agent off (or while quitting)
//! none does. Background jobs are the plan refresher, the day timeline, the voice namer and the suggester (all through
//! `background_call::post`), the bug scout (`heimdall_night::bug_scout`) and the friends guest (`friends::gated`).
//! The app registers its Agent switch at launch (`set`); lab runs, replays and tests that register none are allowed.
use std::sync::Arc;

use agent::delivery::DeliveryGate;

#[cfg(not(test))]
static GATE: std::sync::RwLock<Option<Arc<DeliveryGate>>> = std::sync::RwLock::new(None);
#[cfg(not(test))]
pub fn set(gate: Arc<DeliveryGate>) { *GATE.write().unwrap() = Some(gate) }
#[cfg(not(test))]
fn gate() -> Option<Arc<DeliveryGate>> { GATE.read().unwrap().clone() }
// Tests: per thread, so one test's Agent off never stops another's calls.
#[cfg(test)]
thread_local! { static GATE: std::cell::RefCell<Option<Arc<DeliveryGate>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub fn set(gate: Arc<DeliveryGate>) { GATE.with(|c| *c.borrow_mut() = Some(gate)) }
#[cfg(test)]
fn gate() -> Option<Arc<DeliveryGate>> { GATE.with(|c| c.borrow().clone()) }

/// True unless the registered Agent switch is off or emergency mode paused background work.
pub fn allowed() -> bool {
    gate().is_none_or(|g| g.check().is_ok()) && emergency_paused().is_none()
}

/// Why a background request did not go out.
#[derive(Debug)]
pub struct AgentOff;
impl std::fmt::Display for AgentOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Agent is off: no background model call") }
}
impl std::error::Error for AgentOff {}

pub fn check() -> anyhow::Result<()> {
    if !gate().is_none_or(|g| g.check().is_ok()) { return Err(AgentOff.into()) }
    if let Some(reset_ms) = emergency_paused() { return Err(Paused { reset_ms }.into()) }
    Ok(())
}

/// A refusal that is not a failure (Agent off, emergency mode's pause): the job skips quietly, no log, no cue.
pub fn quiet(e: &anyhow::Error) -> bool {
    e.downcast_ref::<AgentOff>().is_some() || e.downcast_ref::<Paused>().is_some()
}

// ---------------------------------------------------------------------------------------------------------
// Emergency mode's pause (docs/claude-relay.md; The day timeline still ran and logged "naming
// failed … usage limit" while emergency mode covered). The relay cannot carry background jobs, so while emergency mode
// is on none runs: no request, no failure line, no cue. Resume wakes the loops (`resumed`), so a pass that came due
// runs soon after. Chosen relay mode does not pause: the jobs keep Background Tasks ▸ Model.

/// Paused: `Some(reset)` with the quota's reset when the provider stated it.
type Pause = Option<Option<u64>>;
#[cfg(not(test))]
static PAUSE: std::sync::Mutex<Pause> = std::sync::Mutex::new(None);
#[cfg(not(test))]
fn pause<T>(f: impl FnOnce(&mut Pause) -> T) -> T { f(&mut PAUSE.lock().unwrap_or_else(|e| e.into_inner())) }
#[cfg(test)]
thread_local! { static PAUSE: std::cell::RefCell<Pause> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn pause<T>(f: impl FnOnce(&mut Pause) -> T) -> T { PAUSE.with(|p| f(&mut p.borrow_mut())) }

static RESUMED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Emergency mode on (`Some(reset_ms)`) pauses every background model job; `None` resumes them.
pub fn emergency_pause(reset_ms: Option<Option<u64>>) {
    let was = pause(|p| std::mem::replace(p, reset_ms));
    match (was, reset_ms) {
        (None, Some(_)) => eprintln!("Background tasks: paused while emergency mode covers the agent"),
        (Some(_), None) => { eprintln!("Background tasks: emergency mode ended; background tasks resume"); RESUMED.notify_waiters(); }
        _ => {}
    }
}

/// `Some(reset_ms)` while emergency mode pauses background jobs.
pub fn emergency_paused() -> Option<Option<u64>> { pause(|p| *p) }

/// Completes when emergency mode's pause ends (a loop's extra wake; never while nothing is paused).
pub async fn resumed() { RESUMED.notified().await }

/// Why a background request did not go out: emergency mode's pause.
#[derive(Debug)]
pub struct Paused { pub reset_ms: Option<u64> }
impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("emergency mode: background tasks paused") }
}
impl std::error::Error for Paused {}

// ---------------------------------------------------------------------------------------------------------
// The quota episode (audit 27 Oct, F8): 26-27 Oct 00:40-03:01 the plan refresher failed with ChatGPT's usage-limit text
// on every 5-minute pass (28 passes) and the day timeline every 15 minutes (11), while the agent's quota watch had
// already said the limit was spent. A provider whose plan limit or prepaid credit is spent gets no background request
// until it is back:
// - spent: the agent's quota frame (`runtime/quota.mjs`, with its reset when the provider states one), or a background
//   request's own usage-limit or credits answer;
// - back: a request on that provider succeeds (the agent's reply, the emergency probe's hand-back turn, a background
//   answer), or the stated reset passes.
// Background jobs fall back meanwhile (`background_fallback`): a spent or unavailable backend is
// skipped when a job's next call picks its backend (`usable`, read between calls only, never mid-call), until its stated
// reset, else until a probe is due: 10 minutes after it failed, doubling with each failed probe up to an hour. The first
// call after that goes to it again; its success ends the episode, its failure starts the next wait. A backend that is
// down (T3 not running, a refused connection, a 5xx: `backend_down`) is skipped the same way, but for routing only: a
// call that reaches it anyway (nowhere else to go) is let through, so the job retries its pick on its next pass as
// before; only a spent plan refuses calls (`quota_check`). Every T3 instance also waits on the "t3:*" episode (T3
// itself down).

/// Without a stated reset, the first probe of a spent or unavailable backend comes this long after it failed…
pub const FIRST_PROBE_MS: u64 = 10 * 60_000;
/// …and each failed probe doubles the wait, up to this.
pub const MAX_PROBE_MS: u64 = 60 * 60_000;
/// The episode of T3 itself (not running, not connected): every "t3:<instance>" waits on it. Not a provider id any
/// choice can name (a bare "t3:" is the default instance, `key`), so marking one instance never blocks them all.
pub const T3_ALL: &str = "t3:*";

/// The episode key of a provider: a bare "t3:" is `ui::T3_DEFAULT_INSTANCE`'s.
fn key(provider: &str) -> String {
    if provider == ui::T3_PROVIDER_PREFIX { format!("{}{}", ui::T3_PROVIDER_PREFIX, ui::T3_DEFAULT_INSTANCE) } else { provider.to_owned() }
}
/// A T3 instance's key (not T3 itself).
fn t3_instance(provider: &str) -> bool { provider.starts_with(ui::T3_PROVIDER_PREFIX) && provider != T3_ALL }

/// `kind`: "spent" (a plan limit or credit), else what went wrong ("down", "unreachable", "overloaded").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Episode { at_ms: u64, until_ms: Option<u64>, backoff_ms: u64, kind: &'static str }
const SPENT: &str = "spent";

type Episodes = std::collections::BTreeMap<String, Episode>;
#[cfg(not(test))]
static QUOTA: std::sync::Mutex<Episodes> = std::sync::Mutex::new(Episodes::new());
#[cfg(not(test))]
fn episodes<T>(f: impl FnOnce(&mut Episodes) -> T) -> T { f(&mut QUOTA.lock().unwrap()) }
// Tests: per thread, like the Agent switch.
#[cfg(test)]
thread_local! { static QUOTA: std::cell::RefCell<Episodes> = const { std::cell::RefCell::new(Episodes::new()) }; }
#[cfg(test)]
fn episodes<T>(f: impl FnOnce(&mut Episodes) -> T) -> T { QUOTA.with(|q| f(&mut q.borrow_mut())) }

/// The key a usage warning's reset is kept under, beside the provider's episode.
fn warned_key(provider: &str) -> String { format!("warned:{provider}") }
/// A usage warning named `provider`'s reset: a limit that follows without one waits for it, as emergency mode does
/// (`agent::emergency::Detector::quota_frame`), instead of probing the spent account (night review).
pub fn quota_warned(provider: &str, resets_at_ms: Option<u64>, now_ms: u64) {
    let Some(at) = resets_at_ms.filter(|t| *t > now_ms) else { return };
    episodes(|q| q.insert(warned_key(provider), Episode { at_ms: now_ms, until_ms: Some(at), backoff_ms: FIRST_PROBE_MS, kind: SPENT }));
}
/// `provider`'s plan limit or credit is spent (`resets_at_ms`: when the provider says it resets; else a reset an
/// earlier warning named, if it is still ahead).
pub fn quota_spent(provider: &str, resets_at_ms: Option<u64>, now_ms: u64) {
    let warned = episodes(|q| q.get(&warned_key(provider)).and_then(|e| e.until_ms));
    let resets_at_ms = resets_at_ms.filter(|t| *t > now_ms).or(warned.filter(|t| *t > now_ms));
    let provider = &key(provider);
    if mark(provider, resets_at_ms, now_ms, SPENT) { eprintln!("Background tasks: {provider}'s usage is spent; its background calls fall back until it is back"); }
}
/// `provider` did not answer (T3 not running, a refused connection, a 5xx): skipped like a spent one, probed again
/// after 10 minutes, doubling to an hour.
pub fn backend_down(provider: &str, why: &str, now_ms: u64) {
    let provider = &key(provider);
    let kind = down_kind(why);
    if mark(provider, None, now_ms, kind) { eprintln!("Background tasks: {provider} is {kind} ({why}); its background calls go elsewhere until it answers"); }
}
/// What a down backend's failure says: "overloaded" (overloaded, a 529), "unreachable" (no connection, T3 not
/// running), else "down".
fn down_kind(why: &str) -> &'static str {
    let w = why.to_ascii_lowercase();
    if w.contains("overloaded") || w.contains("529") { "overloaded" }
    else if ["connection", "not running", "not connected", "unreachable", "refused"].iter().any(|x| w.contains(x)) { "unreachable" }
    else { "down" }
}
/// A new or renewed episode; true when it is new. Without a stated reset, a failed probe (a failure after the current
/// wait ran out) waits twice as long as the last, up to `MAX_PROBE_MS`; failures inside the current wait (other calls
/// that were already running, review 30 Oct) leave it as it is.
fn mark(provider: &str, until_ms: Option<u64>, now_ms: u64, kind: &'static str) -> bool {
    episodes(|q| {
        let next = match q.get(provider).copied() {
            // A spent plan found inside a down wait makes it a spent one.
            Some(e) if until_ms.is_none() && e.until_ms.is_none() && now_ms < e.at_ms + e.backoff_ms => Episode { kind: if kind == SPENT { SPENT } else { e.kind }, ..e },
            Some(e) if until_ms.is_none() && e.until_ms.is_none() => Episode { at_ms: now_ms, until_ms: None, backoff_ms: (e.backoff_ms * 2).min(MAX_PROBE_MS), kind },
            _ => Episode { at_ms: now_ms, until_ms, backoff_ms: FIRST_PROBE_MS, kind },
        };
        q.insert(provider.to_owned(), next).is_none()
    })
}

/// A request on `provider` succeeded: its background jobs run again (a T3 instance's success is T3's too).
pub fn quota_back(provider: &str) {
    let provider = &key(provider);
    episodes(|q| q.remove(&warned_key(provider)));
    let t3 = t3_instance(provider) && episodes(|q| q.remove(T3_ALL)).is_some();
    let own = episodes(|q| q.remove(provider.as_str())).is_some();
    if own { eprintln!("Background tasks: {provider} answers again; background requests resume"); }
    // A task that fell back may go home now (`background_fallback::refresh_routes`).
    if own || t3 { crate::background_fallback::refresh_routes(); }
}

/// When `provider` (or, for a T3 instance, T3 itself) can be used again: `None` now. No side effect: a job's next call
/// reads it to pick its backend (`background_fallback`).
pub fn blocked_until(provider: &str, now_ms: u64) -> Option<u64> { episode(provider, now_ms).map(|(until, _)| until) }

/// The episode that keeps `provider` from routing now: until when, and what it is ("spent", "down", "unreachable",
/// "overloaded"); the longest when T3 itself is down too.
pub fn episode(provider: &str, now_ms: u64) -> Option<(u64, &'static str)> {
    let own = key(provider);
    let keys: Vec<&str> = if t3_instance(&own) { vec![own.as_str(), T3_ALL] } else { vec![own.as_str()] };
    episodes(|q| {
        keys.iter().filter_map(|k| q.get(*k)).map(|e| (e.until_ms.unwrap_or(e.at_ms + e.backoff_ms), e.kind)).filter(|(u, _)| *u > now_ms).max_by_key(|(u, _)| *u)
    })
}

/// When `provider`'s plan or credit is back, while it is spent: the one episode that refuses calls (`quota_check`).
pub fn spent_until(provider: &str, now_ms: u64) -> Option<u64> {
    let own = key(provider);
    episodes(|q| q.get(own.as_str()).filter(|e| e.kind == SPENT).map(|e| e.until_ms.unwrap_or(e.at_ms + e.backoff_ms)).filter(|u| *u > now_ms))
}
/// Whether a call may go to `provider` now.
pub fn usable(provider: &str, now_ms: u64) -> bool { blocked_until(provider, now_ms).is_none() }

/// Why a background request on a spent provider did not go out.
#[derive(Debug)]
pub struct QuotaOut { pub provider: String, pub until_ms: u64 }
impl std::fmt::Display for QuotaOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = agent::provider::provider(&self.provider).map_or(self.provider.as_str(), |p| p.name);
        let at = chrono::DateTime::from_timestamp_millis(self.until_ms as i64).map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default();
        write!(f, "{name}'s usage is spent: no background request until {at}")
    }
}
impl std::error::Error for QuotaOut {}

/// Err while `provider`'s plan or credit is spent at `now_ms` (`QuotaOut`). A backend that is only down is not refused
/// here: routing avoids it when there is somewhere else to go, and a call that reaches it anyway retries it.
pub fn quota_check(provider: &str, now_ms: u64) -> anyhow::Result<()> {
    match spent_until(provider, now_ms) { Some(until_ms) => Err(QuotaOut { provider: provider.into(), until_ms }.into()), None => Ok(()) }
}

/// What a failed call says about its backend (review 30 Oct): only these change backend state and move tasks on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A plan limit, a rate limit (429) or credit spent: skipped until its reset (`quota_spent`).
    Spent,
    /// Overloaded or a 5xx, no connection, T3 not running, the sign-in lost (401, "please run /login"): `backend_down`.
    Down,
}

/// The backend's fault in a failed call's HTTP status (if any) and text, or `None`: the job's own failure (a 400, a
/// prompt too long, a malformed answer, a short "Error: …" reply), which changes no backend and moves no task.
pub fn fault(status: Option<u16>, text: &str) -> Option<Fault> {
    let lower = text.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if status == Some(429) || says_spent(text) || has(&["usage limit", "rate limit", "rate_limit", "you've hit your", "you have hit your", "credit balance", "api error: 429"]) {
        return Some(Fault::Spent);
    }
    let server = status.is_some_and(|s| (500..600).contains(&s)) || has(&["overloaded", "api error: 5", "unexpected status 5", "status code 5"]);
    let auth = status == Some(401) || has(&["please run /login", "invalid api key", "invalid x-api-key", "authentication_error", "oauth token has expired"]);
    let gone = has(&["connection refused", "econnrefused", "unable to connect", "could not connect", "network is unreachable"]);
    (server || auth || gone).then_some(Fault::Down)
}

/// Marks `provider` by `fault` (nothing for a job's own failure).
pub fn mark_fault(provider: &str, fault: Option<Fault>, reset_ms: Option<u64>, why: &str, now_ms: u64) {
    match fault {
        Some(Fault::Spent) => quota_spent(provider, reset_ms, now_ms),
        Some(Fault::Down) => backend_down(provider, why, now_ms),
        None => {}
    }
}

/// Whether a provider's answer says its plan limit or prepaid credit is spent.
pub fn says_spent(text: &str) -> bool {
    agent::notice::quota_exhausted(text) || agent::provider::says_credits_exhausted(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Night review: a limit without a reset after a warning that named one waits for that reset, not 30 minutes.
    #[test]
    fn a_limit_after_a_warning_waits_for_the_warned_reset() {
        let reset = 10 * 60 * 60_000;
        quota_warned("openai", Some(reset), 1_000);
        quota_spent("openai", None, 2_000);
        assert!(quota_check("openai", 2_000 + MAX_PROBE_MS + 1).is_err(), "no probe before the warned reset");
        assert!(quota_check("openai", reset).is_ok(), "the warned reset ends it");
        // That window is over: a warning naming a reset already past, and the old one, are not used.
        quota_warned("openai", Some(reset + 5), reset + 10);
        quota_spent("openai", None, reset + 20);
        assert!(quota_check("openai", reset + 20 + FIRST_PROBE_MS).is_ok(), "the first probe, not a stale reset");
    }
    use crate::background_call::{self as call, Call, Tool};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A local stand-in for Anthropic that only counts connections.
    fn counting_server() -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        std::thread::spawn(move || for stream in listener.incoming() { seen.fetch_add(1, Ordering::SeqCst); drop(stream); });
        (base, hits)
    }
    fn job() -> Call<'static> {
        Call { job: "test job", model: "claude-haiku-5-5".into(), system: "s", schema: serde_json::json!({"type": "object"}), effort: "low", prompt: "x".into(), history: Vec::new(), session: None }
    }

    /// Agent off: no background job's request leaves the Mac. The plan refresher (`ask`), the day timeline and the voice
    /// namer (`ask_blocking`) and the suggester (`ask_tools_blocking`) all post through `background_call::post`; the
    /// friends guest goes to the user instead of its model; the bug scout's case is `heimdall_night::bug_scout`'s test.
    #[test]
    fn with_agent_off_no_background_model_request_goes_out() {
        let (base, hits) = counting_server();
        let e = crate::plan_refresher::Endpoint { base, key: Some("sk-test".into()), workspace: None, timeout: std::time::Duration::from_secs(5), model: None, ..Default::default() };
        let gate = Arc::new(DeliveryGate::new(true));
        set(gate.clone());
        gate.intent(agent::delivery::StopReason::Agent, false);
        assert!(!allowed());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let off = |r: anyhow::Result<call::Answer>| r.unwrap_err().downcast_ref::<AgentOff>().is_some();
        assert!(off(rt.block_on(call::ask(&e, &job()))), "plan refresher");
        assert!(off(call::ask_blocking(&e, &job())), "day timeline, voice namer");
        let tool = Tool { name: "look", description: "d", schema: serde_json::json!({"type": "object"}) };
        assert!(off(call::ask_tools_blocking(&e, &job(), &[tool], "submit", 3, &mut |_, _| String::new())), "suggester");
        #[cfg(feature = "friends")]
        {
            use ::friends::guest::{Decision, HaikuGuest};
            let haiku = HaikuGuest::with_url("sk-test".into(), format!("{}/v1/messages", e.base)).unwrap();
            let guest = crate::friends::gated(Arc::new(haiku));
            assert_eq!(rt.block_on(guest.decide("Is User free Friday?", &[])).unwrap(), Decision::Ask, "friends guest asks User");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no connection reached the stand-in");
        // Agent on again: the same call goes out.
        gate.intent(agent::delivery::StopReason::Agent, true);
        let _ = call::ask_blocking(&e, &job());
        assert!(hits.load(Ordering::SeqCst) >= 1, "Agent on: the request is made");
    }

    /// Emergency mode (the day timeline still ran and logged "naming failed … usage limit"): while
    /// paused no background request leaves the Mac and the refusal is quiet, not a failure; resume lets it out again.
    #[test]
    fn emergency_mode_pauses_every_background_request_quietly_until_resume() {
        let (base, hits) = counting_server();
        let e = crate::plan_refresher::Endpoint { base, key: Some("sk-test".into()), workspace: None, timeout: std::time::Duration::from_secs(5), model: None, ..Default::default() };
        let reset = 1_900_000_000_000;
        emergency_pause(Some(Some(reset)));
        assert_eq!(emergency_paused(), Some(Some(reset)));
        assert!(!allowed(), "the bug scout, the suggester and the friends guest ask `allowed`");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let paused = |r: anyhow::Result<call::Answer>| { let e = r.unwrap_err(); quiet(&e) && e.downcast_ref::<Paused>().is_some_and(|p| p.reset_ms == Some(reset)) };
        assert!(paused(rt.block_on(call::ask(&e, &job()))), "plan refresher");
        assert!(paused(call::ask_blocking(&e, &job())), "day timeline, voice namer");
        let tool = Tool { name: "look", description: "d", schema: serde_json::json!({"type": "object"}) };
        assert!(paused(call::ask_tools_blocking(&e, &job(), &[tool], "submit", 3, &mut |_, _| String::new())), "suggester");
        // An unknown reset pauses the same way.
        emergency_pause(Some(None));
        assert_eq!(emergency_paused(), Some(None));
        assert!(paused_unknown(call::ask_blocking(&e, &job())));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no connection while paused");
        // Resume wakes a waiting loop and lets the request out.
        let woke = rt.block_on(async {
            let wait = tokio::spawn(resumed());
            tokio::task::yield_now().await;
            emergency_pause(None);
            tokio::time::timeout(std::time::Duration::from_secs(1), wait).await.is_ok()
        });
        assert!(woke, "a loop waiting on `resumed` runs its pass soon after");
        assert!(emergency_paused().is_none() && allowed());
        let _ = call::ask_blocking(&e, &job());
        assert!(hits.load(Ordering::SeqCst) >= 1, "resumed: the request is made");
    }
    fn paused_unknown(r: anyhow::Result<call::Answer>) -> bool { r.unwrap_err().downcast_ref::<Paused>().is_some_and(|p| p.reset_ms.is_none()) }

    /// F8: a spent provider gets no background request until its reset or a success; without a reset, one pass every 30
    /// minutes; another provider is not affected.
    #[test]
    fn a_spent_provider_waits_for_its_reset_or_a_success() {
        let t = 1_000_000_000;
        assert!(quota_check("openai", t).is_ok());
        quota_spent("openai", Some(t + 3_600_000), t);
        let e = quota_check("openai", t + 60_000).unwrap_err();
        assert!(e.downcast_ref::<QuotaOut>().is_some() && e.to_string().starts_with("ChatGPT's usage is spent"), "{e}");
        assert!(quota_check("anthropic", t).is_ok(), "only that provider");
        assert!(quota_check("openai", t + 3_600_000).is_ok(), "the stated reset passed");
        assert!(quota_check("openai", t + 3_600_001).is_ok());
        // No stated reset (the route answers 200 with the limit in the stream): a probe after 10 minutes, then 20, 40,
        // and an hour at most; reading it changes nothing (decided between calls).
        quota_spent("openai", None, t);
        assert!(quota_check("openai", t + FIRST_PROBE_MS - 1).is_err());
        assert!(usable("openai", t + FIRST_PROBE_MS) && usable("openai", t + FIRST_PROBE_MS), "the probe is due; reading it twice is the same");
        // Other calls that were already running fail within the wait: it stays 10 minutes (review 30 Oct).
        quota_spent("openai", None, t + 5_000);
        backend_down("openai", "HTTP 503", t + 9_000);
        assert!(!usable("openai", t + FIRST_PROBE_MS - 1) && usable("openai", t + FIRST_PROBE_MS), "one event, one wait");
        let mut at = t + FIRST_PROBE_MS;
        for wait in [2 * FIRST_PROBE_MS, 4 * FIRST_PROBE_MS, MAX_PROBE_MS, MAX_PROBE_MS] {
            quota_spent("openai", None, at); // the probe failed
            assert!(!usable("openai", at + wait - 1) && usable("openai", at + wait), "{wait}");
            at += wait;
        }
        quota_back("openai");
        assert!(usable("openai", at) && quota_check("openai", t + 1).is_ok(), "a success ends it");
        // Down is for routing only: never refused, and never told as spent (review 30 Oct).
        backend_down("anthropic", "HTTP 529", t);
        assert!(!usable("anthropic", t + 1) && quota_check("anthropic", t + 1).is_ok());
        assert_eq!(episode("anthropic", t + 1).map(|e| e.1), Some("overloaded"));
        backend_down("opencode-go", "no connection", t);
        assert_eq!(episode("opencode-go", t + 1).map(|e| e.1), Some("unreachable"));
        quota_spent("anthropic", None, t + 2);
        assert!(quota_check("anthropic", t + 3).unwrap_err().to_string().contains("usage is spent"), "a real limit inside the down wait");
        // T3 itself down: every instance waits; an instance's own success ends it.
        backend_down(T3_ALL, "T3 is not running", t);
        assert!(!usable("t3:claudeAgent", t + 1) && !usable("t3:claudeAgent_claude_code_2", t + 1) && usable("openai", t + 1));
        quota_back("t3:claudeAgent");
        assert!(usable("t3:claudeAgent_claude_code_2", t + 1));
        // A bare "t3:" is the default instance: marking it never blocks the other instances.
        quota_spent("t3:", Some(t + 60_000), t);
        assert!(!usable("t3:claudeAgent_claude_code_2", t + 1) && usable("t3:claudeAgent", t + 1));
        quota_back("t3:claudeAgent_claude_code_2");
        assert!(says_spent("The ChatGPT user has reached their Subscription Sharing usage limit.") && says_spent("Your credit balance is too low"));
        assert!(!says_spent("rate limit reached, retry in 20 s"));
    }

    /// Review 30 Oct: only a backend's own fault moves tasks; a job's failure (a 400, a prompt too long, a short error
    /// reply) changes no backend state.
    #[test]
    fn only_backend_faults_change_backend_state() {
        for (status, text, want) in [
            (None, "Claude AI usage limit reached|1793066649", Some(Fault::Spent)),
            (None, "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}", Some(Fault::Spent)),
            (Some(429), "", Some(Fault::Spent)),
            (Some(400), "Your credit balance is too low", Some(Fault::Spent)),
            (None, "API Error: 529 {\"type\":\"overloaded_error\"}", Some(Fault::Down)),
            (Some(503), "upstream", Some(Fault::Down)),
            (Some(401), "unauthorized", Some(Fault::Down)),
            (None, "Invalid API key · Please run /login", Some(Fault::Down)),
            (None, "Error: connect ECONNREFUSED 127.0.0.1:443", Some(Fault::Down)),
            (None, "API Error: 400 {\"error\":{\"message\":\"prompt is too long: 250000 tokens > 200000 maximum\"}}", None),
            (Some(400), "invalid_request_error: schema", None),
            (None, "Error: no JSON", None),
            (Some(404), "model not found", None),
        ] {
            assert_eq!(fault(status, text), want, "{status:?} {text}");
        }
        let t = 1_000;
        mark_fault("anthropic", fault(Some(400), "prompt is too long"), None, "x", t);
        assert!(usable("anthropic", t + 1), "a job's failure marks nothing");
    }

    /// Every background model request in the app goes through one of the gated paths above: no other file posts to a
    /// Messages endpoint, runs the scout's model, or makes a friends guest that skips the gate.
    #[test]
    fn no_background_model_request_bypasses_the_gate() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() { stack.push(path); continue; }
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if !name.ends_with(".rs") || name.contains("test") || name.ends_with("_replay.rs") || name.ends_with("_lab.rs") { continue; }
                let text = std::fs::read_to_string(&path).unwrap();
                let shipping = text.split("#[cfg(test)]").next().unwrap();
                if name != "background_call.rs" { assert!(!shipping.contains("/v1/messages\""), "{name} posts to a Messages endpoint outside background_call::post"); }
                assert!(!shipping.contains("ask_claude("), "{name} runs the scout's model outside its gated worker");
                for (at, _) in shipping.match_indices("HaikuGuest::new") {
                    assert!(shipping[at.saturating_sub(200)..at].contains("Gated("), "{name}: a friends guest without the gate");
                }
            }
        }
    }
}
