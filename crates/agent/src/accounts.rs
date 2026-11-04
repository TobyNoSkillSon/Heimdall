//! Subscription accounts and their quota: which Claude Code account (a T3 instance) or the Codex sign-in has quota
//! left, and which renews soonest. One interface for everything that chooses an account: the
//! background jobs' fallback (`app::background_fallback`) and the relay's choice of a Claude account
//! (docs/background-call.md, "Merging with the relay's account selection").
//!
//! The numbers come from `Tools/subscription_quota.py` in Heimdall's home (read-only: it asks each CLI for its rate
//! limits, sends no prompt, prints no token): its JSON `accounts[]`, each with `provider` ("claude", "codex"), `account`
//! (a Claude account is its `CLAUDE_CONFIG_DIR`, the default one "default (~/.claude)"), `windows[]` (`window` "5-hour"
//! or "weekly", `used_percent`, `resets_at` as local ISO time to the minute), and `error` when the read failed. T3 names
//! the same Claude accounts by instance; `default_config_dirs` maps one to the other (overridable).
//!
//! - `usable`: no window used up (100% or more) before its reset, and Codex not over its limit. An account whose read
//!   failed is unknown, not spent: it stays usable.
//! - `earliest_reset_first`: the usable accounts, the one whose next window renews soonest first (its quota would
//!   otherwise go unused), accounts without a known reset after them.
//! - `Board`: the last reading, at most one script run per `MIN_TTL` (5 minutes), run on its own thread and never
//!   waited for by a caller (`snapshot`), with a timeout.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Which subscription an account is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
}

/// One quota window ("5-hour", "weekly").
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub name: String,
    pub used_percent: Option<f64>,
    pub resets_at_ms: Option<u64>,
}

/// One account's quota as the script read it.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountQuota {
    /// The T3 instance for a Claude account (`default_config_dirs`; the script's account name when none maps), "codex"
    /// for the Codex sign-in.
    pub instance: String,
    pub kind: Kind,
    pub windows: Vec<Window>,
    /// Codex: the limit it says is reached (`rateLimitReachedType`), if any.
    pub reached: Option<String>,
    /// Why the read failed (unknown quota, not spent).
    pub error: Option<String>,
}

/// The Codex sign-in's instance name in `AccountQuota`.
pub const CODEX: &str = "codex";

/// T3's Claude instances and their `CLAUDE_CONFIG_DIR`: the user's first account in `~/.claude`, the second in
/// `~/.claude-account2`. A caller with more accounts passes its own list.
pub fn default_config_dirs(home: &Path) -> Vec<(String, PathBuf)> {
    vec![("claudeAgent".into(), home.join(".claude")), ("claudeAgent_claude_code_2".into(), home.join(".claude-account2"))]
}

/// T3's Claude instances and their config folders as T3's settings name them (`providerInstances.*.config.homePath`,
/// enabled ones only), the default instance in `~/.claude`; `default_config_dirs` when the file says nothing more. The
/// relay's accounts are T3's, so a third account needs no code (merged from the relay's own reader, 30 Oct).
pub fn config_dirs_from_t3(t3_settings: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    let settings: Value = std::fs::read(t3_settings).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let mut dirs = vec![("claudeAgent".to_string(), home.join(".claude"))];
    for (instance, p) in settings["providerInstances"].as_object().into_iter().flatten() {
        if !p["driver"].as_str().unwrap_or_default().contains("claude") || p["enabled"] == false { continue }
        let Some(dir) = p["config"]["homePath"].as_str() else { continue };
        dirs.retain(|(i, _)| i != instance);
        dirs.push((instance.clone(), expand(dir, home)));
    }
    if dirs.len() == 1 { return default_config_dirs(home) }
    dirs
}

fn expand(p: &str, home: &Path) -> PathBuf {
    let p = p.trim().trim_end_matches('/');
    match p.strip_prefix("~/") { Some(rest) => home.join(rest), None if p == "~" => home.to_owned(), None => PathBuf::from(p) }
}

/// "2026-10-30T07:50+01:00" (to the minute, as the script writes it) or RFC 3339 with seconds.
fn time_ms(v: &Value) -> Option<u64> {
    let s = v.as_str()?;
    let t = chrono::DateTime::parse_from_rfc3339(s).or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M%:z")).ok()?;
    u64::try_from(t.timestamp_millis()).ok()
}

/// The script's JSON, read: each account with its instance (`dirs` maps a Claude account's config folder to it).
pub fn parse(json: &Value, dirs: &[(String, PathBuf)], home: &Path) -> Vec<AccountQuota> {
    json["accounts"].as_array().into_iter().flatten().filter_map(|a| {
        let kind = match a["provider"].as_str()? { "claude" => Kind::Claude, "codex" => Kind::Codex, _ => return None };
        let instance = match kind {
            Kind::Codex => CODEX.to_owned(),
            Kind::Claude => {
                let name = a["account"].as_str().unwrap_or("");
                // "default (~/.claude)" is the default folder.
                let dir = name.strip_prefix("default (").and_then(|r| r.strip_suffix(')')).unwrap_or(name);
                let dir = expand(dir, home);
                dirs.iter().find(|(_, d)| expand(&d.to_string_lossy(), home) == dir).map_or_else(|| name.to_owned(), |(i, _)| i.clone())
            }
        };
        let windows = a["windows"].as_array().into_iter().flatten().map(|w| Window {
            name: w["window"].as_str().unwrap_or("").to_owned(),
            used_percent: w["used_percent"].as_f64(),
            resets_at_ms: time_ms(&w["resets_at"]),
        }).collect();
        Some(AccountQuota { instance, kind, windows, reached: a["reached"].as_str().map(str::to_owned), error: a["error"].as_str().map(str::to_owned) })
    }).collect()
}

/// When the account can be used again: `None` when it can now, else the latest reset among its used-up windows
/// (`Some(u64::MAX)` when a used-up window names no reset, or Codex says a limit is reached without one).
pub fn spent_until(a: &AccountQuota, now_ms: u64) -> Option<u64> {
    let mut until: Option<u64> = None;
    for w in &a.windows {
        if w.used_percent.is_some_and(|u| u >= 100.0) {
            match w.resets_at_ms {
                Some(at) if at <= now_ms => {}
                Some(at) => until = Some(until.map_or(at, |u| u.max(at))),
                None => until = Some(u64::MAX),
            }
        }
    }
    if until.is_none() && a.reached.as_deref().is_some_and(|r| !r.is_empty()) {
        // Codex says a limit is reached: its windows' resets bound it, else unknown.
        until = Some(a.windows.iter().filter_map(|w| w.resets_at_ms).filter(|t| *t > now_ms).min().unwrap_or(u64::MAX));
    }
    until
}

/// Whether the account has quota now (an account whose read failed counts as usable: unknown is not spent).
pub fn usable(a: &AccountQuota, now_ms: u64) -> bool { spent_until(a, now_ms).is_none() }

/// The soonest reset of any of its windows still ahead.
pub fn next_reset(a: &AccountQuota, now_ms: u64) -> Option<u64> {
    a.windows.iter().filter_map(|w| w.resets_at_ms).filter(|t| *t > now_ms).min()
}

/// The usable accounts' instances, the one renewing soonest first (its quota would go unused otherwise); those with no
/// known reset after them, in the given order. Spent accounts are left out.
pub fn earliest_reset_first(accounts: &[AccountQuota], now_ms: u64) -> Vec<String> {
    let mut ok: Vec<(usize, &AccountQuota)> = accounts.iter().enumerate().filter(|(_, a)| usable(a, now_ms)).collect();
    ok.sort_by_key(|(i, a)| (next_reset(a, now_ms).unwrap_or(u64::MAX), *i));
    ok.into_iter().map(|(_, a)| a.instance.clone()).collect()
}

// ---------------------------------------------------------------------------------------------------------
// Reading.

/// Where readings come from: the script (`Script`), a fixed one in tests.
pub trait QuotaSource: Send + Sync {
    /// Blocking: never call it on an async runtime's thread (`Board` runs it on its own).
    fn read(&self) -> Result<Vec<AccountQuota>>;
}

/// `Tools/subscription_quota.py`, run with `python3 -I`, one `--claude-dir` per account besides `~/.claude`, in a
/// process group of its own: after `timeout` the whole group is killed, the `claude` and `codex` processes it started
/// included (review 30 Oct: killing python alone left them running). The CLIs it starts are found in the usual install folders even when the app's PATH is launchd's.
pub struct Script {
    pub script: PathBuf,
    pub dirs: Vec<(String, PathBuf)>,
    /// T3's settings: when set, `dirs` is read from them at every run (`config_dirs_from_t3`), so an account added in
    /// T3 later is read under its own instance without a restart.
    pub t3_settings: Option<PathBuf>,
    pub home: PathBuf,
    pub timeout: Duration,
}

impl Script {
    /// The script in `heimdall_home` (`Tools/subscription_quota.py`), with the default accounts; `None` when it is not
    /// there (the public edition, another home).
    pub fn in_home(heimdall_home: &Path, user_home: &Path) -> Option<Script> {
        let script = heimdall_home.join("Tools/subscription_quota.py");
        script.is_file().then(|| Script { script, dirs: default_config_dirs(user_home), t3_settings: None, home: user_home.to_owned(), timeout: Duration::from_secs(300) })
    }
}

impl QuotaSource for Script {
    fn read(&self) -> Result<Vec<AccountQuota>> {
        let default = self.home.join(".claude");
        let dirs = self.t3_settings.as_ref().map_or_else(|| self.dirs.clone(), |t3| config_dirs_from_t3(t3, &self.home));
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-I").arg(&self.script);
        for (_, dir) in &dirs {
            if expand(&dir.to_string_lossy(), &self.home) != default { cmd.arg("--claude-dir").arg(dir); }
        }
        let path = std::env::var("PATH").unwrap_or_default();
        let extra = [self.home.join(".local/bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")];
        let path = extra.iter().map(|p| p.display().to_string()).chain(std::iter::once(path)).collect::<Vec<_>>().join(":");
        cmd.env("PATH", path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        if let Some(dir) = self.script.parent() { cmd.current_dir(dir); }
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().context("the quota script did not start")?;
        let mut out = child.stdout.take().context("no output pipe")?;
        let reader = std::thread::spawn(move || { let mut s = String::new(); let _ = std::io::Read::read_to_string(&mut out, &mut s); s });
        let started = std::time::Instant::now();
        loop {
            if child.try_wait()?.is_some() { break }
            if started.elapsed() > self.timeout {
                // The group's id is the script's pid (`process_group(0)`).
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
                let _ = child.wait();
                bail!("the quota script took longer than {} s", self.timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let text = reader.join().map_err(|_| anyhow::anyhow!("the quota output was not read"))?;
        let v: Value = serde_json::from_str(&text).context("the quota script printed no JSON")?;
        Ok(parse(&v, &dirs, &self.home))
    }
}

/// The least time between two script runs.
pub const MIN_TTL: Duration = Duration::from_secs(300);
/// How long a reading serves the background fallback (review 30 Oct: each run starts two `claude` and a `codex`
/// process, on battery).
pub const FALLBACK_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Default)]
struct Reading {
    /// The last attempt, read or failed (the TTL counts from it).
    at_ms: Option<u64>,
    /// The last successful read (`fresh` serves only a reading this young).
    good_ms: Option<u64>,
    accounts: Vec<AccountQuota>,
    refreshing: bool,
}

/// The last reading, refreshed on its own thread when older than its TTL (at least `MIN_TTL`); a caller never waits.
pub struct Board {
    source: Arc<dyn QuotaSource>,
    ttl_ms: u64,
    state: Mutex<Reading>,
}

impl Board {
    pub fn new(source: Arc<dyn QuotaSource>, ttl: Duration) -> Arc<Board> {
        Arc::new(Board { source, ttl_ms: ttl.max(MIN_TTL).as_millis() as u64, state: Mutex::new(Reading::default()) })
    }
    /// A board that already holds `accounts`, read at `at_ms` (tests, a caller that read them itself).
    pub fn with(source: Arc<dyn QuotaSource>, accounts: Vec<AccountQuota>, at_ms: u64) -> Arc<Board> {
        let b = Board::new(source, MIN_TTL);
        *b.state.lock().unwrap() = Reading { at_ms: Some(at_ms), good_ms: Some(at_ms), accounts, refreshing: false };
        b
    }
    /// The last reading (`None` before the first), and a refresh started in the background when it is older than the
    /// TTL and none runs. Never blocks on the script.
    pub fn snapshot(self: &Arc<Self>, now_ms: u64) -> Option<Vec<AccountQuota>> {
        let mut s = self.state.lock().unwrap();
        let stale = s.at_ms.is_none_or(|at| now_ms.saturating_sub(at) >= self.ttl_ms);
        if stale { self.refresh(&mut s); }
        s.at_ms.is_some().then(|| s.accounts.clone()).filter(|a| !a.is_empty())
    }
    /// A reading at most `max_age` old, or none: an older one is refreshed (unless an attempt within `max_age` failed)
    /// and waited for up to `wait`. Never an old reading passed off as new (review 30 Oct: the relay asked to go back
    /// to an account a five-minute-old reading still showed with quota). Blocking: for a caller whose choice depends on
    /// it now (the relay picking or checking an account), off any async runtime's thread.
    pub fn fresh(self: &Arc<Self>, max_age: Duration, wait: Duration) -> Option<Vec<AccountQuota>> {
        let max_age = max_age.as_millis() as u64;
        let young = |at: Option<u64>| at.is_some_and(|at| crate::now_ms().saturating_sub(at) < max_age);
        {
            let mut s = self.state.lock().unwrap();
            if young(s.good_ms) { return self.cached_locked(&s); }
            if !young(s.at_ms) { self.refresh(&mut s); }
        }
        let began = std::time::Instant::now();
        loop {
            {
                let s = self.state.lock().unwrap();
                if young(s.good_ms) { return self.cached_locked(&s); }
                if !s.refreshing || began.elapsed() >= wait { return None; }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    /// Starts a read on its own thread unless one runs.
    fn refresh(self: &Arc<Self>, s: &mut Reading) {
        if s.refreshing { return }
        s.refreshing = true;
        let me = self.clone();
        std::thread::Builder::new().name("quota-read".into()).spawn(move || {
            let read = me.source.read();
            let mut s = me.state.lock().unwrap();
            s.refreshing = false;
            match read {
                Ok(accounts) => { let now = crate::now_ms(); s.accounts = accounts; s.at_ms = Some(now); s.good_ms = Some(now); }
                // Tried: the next try waits a TTL too.
                Err(e) => { eprintln!("Account quota not read: {e:#}"); s.at_ms = Some(crate::now_ms()); }
            }
        }).ok();
    }
    fn cached_locked(&self, s: &Reading) -> Option<Vec<AccountQuota>> {
        s.at_ms.is_some().then(|| s.accounts.clone()).filter(|a| !a.is_empty())
    }
    /// The last reading as it is, starting no refresh (a caller whose choice does not depend on it).
    pub fn cached(&self) -> Option<Vec<AccountQuota>> { self.cached_locked(&self.state.lock().unwrap()) }
    /// Whether a refresh runs now (tests).
    pub fn refreshing(&self) -> bool { self.state.lock().unwrap().refreshing }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf { PathBuf::from("/Users/t") }
    /// Read live on 30 Oct 07:42 (`subscription_quota.py --claude-dir ~/.claude-account2`, read-only, no prompt).
    fn sample() -> Value { serde_json::from_str(include_str!("accounts_sample.json")).unwrap() }
    fn at(s: &str) -> u64 { time_ms(&Value::String(s.into())).unwrap() }

    #[test]
    fn the_scripts_json_is_read_with_t3_instances() {
        let a = parse(&sample(), &default_config_dirs(&home()), &home());
        assert_eq!(a.iter().map(|a| (a.instance.as_str(), a.kind)).collect::<Vec<_>>(),
            [("claudeAgent", Kind::Claude), ("claudeAgent_claude_code_2", Kind::Claude), ("codex", Kind::Codex)]);
        assert_eq!(a[0].windows[1], Window { name: "weekly".into(), used_percent: Some(94.0), resets_at_ms: Some(at("2026-11-02T13:00+01:00")) });
        assert_eq!(a[1].windows[0].resets_at_ms, Some(at("2026-10-30T12:10:00+01:00")));
        assert!(a.iter().all(|a| a.error.is_none()));
        // Another folder name maps through the caller's list; an unknown one keeps the script's name.
        let dirs = vec![("claudeAgent_work".to_string(), PathBuf::from("/Users/t/.claude-account2/"))];
        assert_eq!(parse(&sample(), &dirs, &home())[1].instance, "claudeAgent_work");
        assert_eq!(parse(&sample(), &dirs, &home())[0].instance, "default (~/.claude)");
    }

    #[test]
    fn spent_accounts_wait_for_their_reset_and_the_soonest_reset_goes_first() {
        let now = at("2026-10-30T08:00+01:00");
        let a = parse(&sample(), &default_config_dirs(&home()), &home());
        assert!(a.iter().all(|a| usable(a, now)), "94% is not spent");
        // Account 1's 5-hour window renewed at 07:50, so its next reset is the weekly one (2 Nov); account 2's is 09:00;
        // Codex's 08:13.
        assert_eq!(earliest_reset_first(&a, now), ["codex", "claudeAgent_claude_code_2", "claudeAgent"]);
        let mut spent = a.clone();
        spent[1].windows[1].used_percent = Some(100.0);
        assert_eq!(spent_until(&spent[1], now), Some(at("2026-10-30T09:00+01:00")));
        assert!(!usable(&spent[1], now) && usable(&spent[1], at("2026-10-30T09:00+01:00")));
        assert_eq!(earliest_reset_first(&spent, now), ["codex", "claudeAgent"]);
        // Codex says its limit is reached: spent until its next reset.
        let mut codex = a[2].clone();
        codex.reached = Some("primary".into());
        assert_eq!(spent_until(&codex, now), Some(at("2026-10-30T08:13+01:00")));
        // A failed read is unknown, not spent; a used-up window without a reset is spent for good.
        let unknown = AccountQuota { instance: "x".into(), kind: Kind::Claude, windows: vec![], reached: None, error: Some("no get_usage reply".into()) };
        assert!(usable(&unknown, now));
        let forever = AccountQuota { windows: vec![Window { name: "weekly".into(), used_percent: Some(100.0), resets_at_ms: None }], ..unknown.clone() };
        assert_eq!(spent_until(&forever, now), Some(u64::MAX));
        // Three Claude accounts: the earliest reset among those not spent.
        let acct = |i: &str, used: f64, reset: &str| AccountQuota { instance: i.into(), kind: Kind::Claude,
            windows: vec![Window { name: "5-hour".into(), used_percent: Some(used), resets_at_ms: Some(at(reset)) }], reached: None, error: None };
        let three = [acct("a", 100.0, "2026-10-30T08:30+01:00"), acct("b", 40.0, "2026-10-30T11:00+01:00"), acct("c", 70.0, "2026-10-30T09:15+01:00")];
        assert_eq!(earliest_reset_first(&three, now), ["c", "b"]);
    }

    /// A script that outlives its timeout is killed with everything it started (its process group).
    #[test]
    fn a_script_past_its_timeout_dies_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow.py");
        let pid_file = dir.path().join("child.pid");
        std::fs::write(&script, format!("import subprocess, time\np = subprocess.Popen(['sleep', '30'])\nopen({:?}, 'w').write(str(p.pid))\ntime.sleep(30)\n", pid_file.display().to_string())).unwrap();
        let s = Script { script, dirs: vec![], t3_settings: None, home: dir.path().into(), timeout: Duration::from_secs(2) };
        let started = std::time::Instant::now();
        let err = s.read().unwrap_err().to_string();
        assert!(err.contains("longer than") && started.elapsed() < Duration::from_secs(10), "{err}");
        let child: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let alive = unsafe { libc::kill(child, 0) } == 0;
        assert!(!alive, "the script's child was killed with it");
    }

    struct Counting(std::sync::atomic::AtomicUsize, Vec<AccountQuota>);
    impl QuotaSource for Counting {
        fn read(&self) -> Result<Vec<AccountQuota>> { self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst); Ok(self.1.clone()) }
    }

    #[test]
    fn t3_s_claude_instances_map_to_their_config_folders() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        std::fs::write(&f, serde_json::json!({"providerInstances":{
            "grok":{"driver":"grok","enabled":true,"config":{}},
            "claudeAgent_claude_code_2":{"driver":"claudeAgent","enabled":true,"config":{"homePath":"~/.claude-account2"}},
            "claudeAgent_3":{"driver":"claudeAgent","enabled":false,"config":{"homePath":"/x"}}
        }}).to_string()).unwrap();
        assert_eq!(config_dirs_from_t3(&f, &home()), [("claudeAgent".to_string(), home().join(".claude")),
            ("claudeAgent_claude_code_2".to_string(), home().join(".claude-account2"))]);
        assert_eq!(config_dirs_from_t3(&d.path().join("missing.json"), &home()), default_config_dirs(&home()));
    }

    #[test]
    fn fresh_waits_for_a_stale_reading_and_not_for_a_fresh_one() {
        let source = Arc::new(Counting(Default::default(), vec![]));
        let now = crate::now_ms();
        let b = Board::with(source.clone(), vec![], now);
        b.fresh(MIN_TTL, Duration::from_secs(5));
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0, "fresh enough: no read");
        let b = Board::with(source.clone(), vec![], now - MIN_TTL.as_millis() as u64 - 1);
        b.fresh(MIN_TTL, Duration::from_secs(5));
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 1, "stale: read and waited for");
        assert!(!b.refreshing());
    }

    struct Failing;
    impl QuotaSource for Failing {
        fn read(&self) -> Result<Vec<AccountQuota>> { bail!("no reply") }
    }

    /// A failed read never passes the old reading off as new (review 30 Oct), and is not retried within `max_age`.
    #[test]
    fn fresh_gives_nothing_when_the_read_fails() {
        let old = AccountQuota { instance: "claudeAgent".into(), kind: Kind::Claude, windows: vec![], reached: None, error: None };
        let b = Board::with(Arc::new(Failing), vec![old], crate::now_ms() - 3_600_000);
        assert_eq!(b.fresh(Duration::from_secs(60), Duration::from_secs(5)), None);
        assert!(b.cached().is_some(), "the old reading stays for callers that take any");
        assert_eq!(b.fresh(Duration::from_secs(60), Duration::from_secs(5)), None, "failed just now: not run again");
    }

    #[test]
    fn the_board_never_blocks_and_reads_at_most_once_per_ttl() {
        let accounts = parse(&sample(), &default_config_dirs(&home()), &home());
        let source = Arc::new(Counting(Default::default(), accounts.clone()));
        let board = Board::new(source.clone(), Duration::from_secs(1));
        let now = crate::now_ms();
        assert_eq!(board.snapshot(now), None, "nothing read yet: no answer, a read starts");
        for _ in 0..200 { if !board.refreshing() { break } std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(board.snapshot(now).unwrap(), accounts);
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 1, "fresh: no second read (TTL at least 5 min)");
        let _ = board.snapshot(now + MIN_TTL.as_millis() as u64 + 1);
        for _ in 0..200 { if !board.refreshing() { break } std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
