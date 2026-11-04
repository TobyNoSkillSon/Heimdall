//! Vella's dictation state (docs/vella.md, Channel 1). While the user dictates to Vella, Heimdall keeps capturing and
//! archiving, but nothing heard on his microphone wakes the agent; those lines reach its context marked as dictated.
//!
//! `Link` is the pure state: dictation intervals on Heimdall's host clock, the current session, Vella's pid and its
//! keybind. `observe` (macOS) listens for `dev.vella.dictation.state` on the main run loop and asks Vella for a
//! `hello` once; each notification reaches the live loop as a `Heard`, stamped at receipt. Vella absent, nothing
//! arrives and nothing changes.
use std::collections::VecDeque;

pub const NOTIFICATION: &str = "dev.vella.dictation.state";
pub const QUERY: &str = "dev.heimdall.vella.query";
pub const QUERY_OBJECT: &str = "dev.heimdall";
/// Dictation counts as on only while a start or heartbeat arrived this recently (Vella beats every 15 s).
pub const TIMEOUT_NS: u64 = 45_000_000_000;
/// A notification's posting delay is subtracted from its receipt, at most this much.
pub const MAX_DELAY_NS: u64 = 2_000_000_000;
/// Closed intervals older than this are forgotten (no utterance waits that long for its final).
pub const KEEP_NS: u64 = 600_000_000_000;
/// The overlap that makes an utterance dictated: half its length.
pub const DICTATED_SHARE: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Start,
    Stop,
    Heartbeat,
    Hello,
    Quit,
}

/// One `dev.vella.dictation.state` notification's userInfo, validated (protocol version 1 only).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Notice {
    pub on: bool,
    pub reason: Reason,
    pub session: String,
    pub pid: i32,
    /// Unix seconds when Vella posted it.
    pub sent: f64,
    pub mode: String,
    /// The active binding as JSON, Vella's `shortcuts.json` form.
    pub shortcut: String,
    /// Its display form, e.g. `⌃⌘N · Toggle`.
    pub shortcut_label: String,
}

impl Notice {
    /// The userInfo as JSON (`observe` converts the dictionary). `None` for anything malformed or another version.
    pub fn parse(info: &serde_json::Value) -> Option<Notice> {
        let int = |key: &str| {
            let n = info.get(key)?;
            n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e15).map(|f| f as i64))
        };
        let text = |key: &str| info.get(key).and_then(|v| v.as_str());
        if int("v")? != 1 {
            return None;
        }
        let on = match text("state")? {
            "on" => true,
            "off" => false,
            _ => return None,
        };
        let reason = match text("reason")? {
            "start" => Reason::Start,
            "stop" => Reason::Stop,
            "heartbeat" => Reason::Heartbeat,
            "hello" => Reason::Hello,
            "quit" => Reason::Quit,
            _ => return None,
        };
        let pid = i32::try_from(int("pid")?).ok().filter(|p| *p > 0)?;
        let sent = info.get("sent")?.as_f64().filter(|s| s.is_finite() && *s > 0.0)?;
        // A string field of the wrong type is malformed; a missing one is empty (the protocol always sends them).
        let optional = |key: &str| match info.get(key) {
            None => Some(String::new()),
            Some(v) => v.as_str().map(str::to_owned),
        };
        Some(Notice {
            on,
            reason,
            session: optional("session")?,
            pid,
            sent,
            mode: optional("mode")?,
            shortcut: optional("shortcut")?,
            shortcut_label: optional("shortcutLabel")?,
        })
    }

    /// The host time it stands for: received at `at_ns` (host) / `at_unix` (wall), less the posting delay, 0–2 s.
    pub fn stamp(&self, at_ns: u64, at_unix: f64) -> u64 {
        let delay = ((at_unix - self.sent) * 1e9).clamp(0.0, MAX_DELAY_NS as f64);
        at_ns.saturating_sub(if delay.is_finite() { delay as u64 } else { 0 })
    }
}

/// One notification as the observer received it.
#[derive(Clone, Debug)]
pub struct Heard {
    pub info: serde_json::Value,
    /// Heimdall's host clock at receipt (`capture::host_now_ns`).
    pub at_ns: u64,
    /// Unix seconds at receipt.
    pub at_unix: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct Open {
    start: u64,
    session: String,
}

/// What a notice or the clock changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub started: bool,
    pub stopped: bool,
    pub shortcut: bool,
}

/// Vella as Heimdall knows it.
#[derive(Clone, Debug)]
pub struct Link {
    closed: VecDeque<(u64, u64)>,
    open: Option<Open>,
    /// Receipt of the newest notice that said "on" (start, heartbeat, an "on" hello).
    last_seen: u64,
    pub pid: Option<i32>,
    pub mode: String,
    pub shortcut: String,
    pub shortcut_label: String,
    /// Whether a process is alive (`kill(pid, 0)`); tests replace it.
    pub alive: fn(i32) -> bool,
    /// Why the newest interval ended (`stop`, `quit`, `timeout`, `pid_gone`, `new_session`, `new_pid`).
    pub ended_by: Option<&'static str>,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            closed: VecDeque::new(),
            open: None,
            last_seen: 0,
            pid: None,
            mode: String::new(),
            shortcut: String::new(),
            shortcut_label: String::new(),
            alive: pid_alive,
            ended_by: None,
        }
    }
}

/// `kill(pid, 0)`: the process exists (EPERM still means it does).
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

impl Link {
    /// Applies one valid notice received at `at_ns` / `at_unix`.
    pub fn apply(&mut self, n: &Notice, at_ns: u64, at_unix: f64) -> Change {
        let mut change = Change::default();
        let stamp = n.stamp(at_ns, at_unix);
        if (!n.shortcut.is_empty() && n.shortcut != self.shortcut) || (!n.shortcut_label.is_empty() && n.shortcut_label != self.shortcut_label) {
            change.shortcut = true;
            self.shortcut = n.shortcut.clone();
            self.shortcut_label = n.shortcut_label.clone();
        }
        if !n.mode.is_empty() {
            self.mode = n.mode.clone();
        }
        // A new Vella process: whatever the old one had open ended when it was last heard.
        if self.pid.is_some_and(|p| p != n.pid) && self.open.is_some() {
            self.close(self.last_seen, "new_pid");
            change.stopped = true;
        }
        self.pid = Some(n.pid);
        if n.on {
            match &self.open {
                Some(o) if o.session == n.session => {}
                Some(_) => {
                    self.close(stamp, "new_session");
                    self.open = Some(Open { start: stamp, session: n.session.clone() });
                }
                None => {
                    self.open = Some(Open { start: stamp, session: n.session.clone() });
                    change.started = true;
                }
            }
            self.last_seen = self.last_seen.max(at_ns);
        } else if self.open.is_some() {
            self.close(stamp, if n.reason == Reason::Quit { "quit" } else { "stop" });
            change.stopped = true;
        }
        change
    }

    fn close(&mut self, at: u64, why: &'static str) {
        if let Some(o) = self.open.take() {
            self.closed.push_back((o.start, at.max(o.start)));
            self.ended_by = Some(why);
        }
    }

    /// Ends an open dictation that has not been heard from within `TIMEOUT_NS` of `now`, or whose Vella is gone: it
    /// ended when last heard. Also forgets intervals older than `KEEP_NS`. True when it ended one.
    pub fn expire(&mut self, now: u64) -> bool {
        let mut ended = false;
        if self.open.is_some() {
            let silent = now.saturating_sub(self.last_seen) > TIMEOUT_NS;
            let gone = !self.pid.is_some_and(self.alive);
            if silent || gone {
                self.close(self.last_seen, if silent { "timeout" } else { "pid_gone" });
                ended = true;
            }
        }
        while self.closed.front().is_some_and(|(_, end)| end.saturating_add(KEEP_NS) < now) {
            self.closed.pop_front();
        }
        ended
    }

    /// Whether the user is dictating to Vella now (call `expire` first).
    pub fn on(&self) -> bool {
        self.open.is_some()
    }

    /// When the open dictation times out unless heard from again (host ns).
    pub fn deadline(&self) -> Option<u64> {
        self.open.as_ref().map(|_| self.last_seen.saturating_add(TIMEOUT_NS + 1))
    }

    /// Whether audio from `start` to `end` (host ns) was dictated: dictation covers at least half of it. An open
    /// dictation counts up to `now`. Call `expire(now)` first.
    pub fn dictated(&self, start: u64, end: u64, now: u64) -> bool {
        let spans = self.closed.iter().copied().chain(self.open.as_ref().map(|o| (o.start, now.max(o.start))));
        if end <= start {
            return spans.into_iter().any(|(s, e)| s <= start && start <= e);
        }
        let covered: u64 = spans.map(|(s, e)| end.min(e).saturating_sub(start.max(s))).sum();
        covered as f64 >= DICTATED_SHARE * (end - start) as f64
    }

    /// Whether the instant `t` lies inside a dictation (an open one counts up to `now`): a word is dictated when its
    /// middle is. Call `expire(now)` first.
    pub fn covers(&self, t: u64, now: u64) -> bool {
        self.closed.iter().copied().chain(self.open.as_ref().map(|o| (o.start, now.max(o.start)))).any(|(s, e)| s <= t && t <= e)
    }

    /// The shortcut's keys alone, e.g. `⌃⌘N` from `⌃⌘N · Toggle`.
    pub fn keys(&self) -> Option<String> {
        let keys = self.shortcut_label.split(" · ").next().unwrap_or("").trim();
        (!keys.is_empty()).then(|| keys.to_owned())
    }

    /// For the journal and diagnostics.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "on": self.on(), "session": self.open.as_ref().map(|o| o.session.clone()), "since_ns": self.open.as_ref().map(|o| o.start),
            "last_seen_ns": self.last_seen, "pid": self.pid, "mode": self.mode, "shortcut": self.shortcut,
            "shortcut_label": self.shortcut_label, "ended_by": self.ended_by, "intervals": self.closed.len(),
        })
    }
}

/// Listens for Vella on the main run loop for the rest of the process and asks it for a `hello`. Every notification
/// goes to `tx`, stamped at receipt. Call on the main thread; a second call only asks again.
#[cfg(target_os = "macos")]
pub fn observe(tx: crossbeam_channel::Sender<Heard>) {
    use block2::RcBlock;
    use objc2_foundation::{NSDistributedNotificationCenter, NSNotification, NSString};
    use std::ptr::NonNull;
    thread_local! {
        static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let center = NSDistributedNotificationCenter::defaultCenter();
    if !INSTALLED.get() {
        INSTALLED.set(true);
        let block = RcBlock::new(move |n: NonNull<NSNotification>| {
            let at_ns = capture::host_now_ns();
            let at_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
            let info = unsafe { n.as_ref() }.userInfo().map_or(serde_json::Value::Null, |d| dictionary(&d));
            let _ = tx.try_send(Heard { info, at_ns, at_unix });
        });
        let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(&NSString::from_str(NOTIFICATION)), None, None, &block) };
        // Observed for the process's lifetime: the token is never removed.
        std::mem::forget(token);
    }
    unsafe {
        center.postNotificationName_object_userInfo_deliverImmediately(
            &NSString::from_str(QUERY),
            Some(&NSString::from_str(QUERY_OBJECT)),
            None,
            true,
        );
    }
}

/// A userInfo dictionary as JSON: strings and numbers (property-list types); anything else is null.
#[cfg(target_os = "macos")]
fn dictionary(d: &objc2_foundation::NSDictionary) -> serde_json::Value {
    use objc2_foundation::{NSNumber, NSString};
    let mut map = serde_json::Map::new();
    for key in d.allKeys().iter() {
        let Some(name) = key.downcast_ref::<NSString>().map(|s| s.to_string()) else { continue };
        let Some(value) = d.objectForKey(&key) else { continue };
        let json = if let Some(s) = value.downcast_ref::<NSString>() {
            serde_json::Value::String(s.to_string())
        } else if let Some(n) = value.downcast_ref::<NSNumber>() {
            let f = n.doubleValue();
            if f.fract() == 0.0 && f.abs() < 9e15 { serde_json::json!(f as i64) } else { serde_json::json!(f) }
        } else {
            serde_json::Value::Null
        };
        map.insert(name, json);
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;
    fn info(state: &str, reason: &str, session: &str, sent: f64) -> serde_json::Value {
        serde_json::json!({"v":1,"state":state,"reason":reason,"session":session,"pid":4321,"sent":sent,"mode":"dictation",
            "shortcut":"{\"key\":\"n\",\"modifiers\":[\"control\",\"command\"],\"behavior\":\"toggle\"}","shortcutLabel":"⌃⌘N · Toggle"})
    }
    fn link() -> Link {
        Link { alive: |_| true, ..Default::default() }
    }
    /// Feeds a notice received at host `at` with Vella's clock `delay_s` behind Heimdall's wall clock.
    fn feed(l: &mut Link, state: &str, reason: &str, session: &str, at: u64, delay_s: f64) -> Change {
        let unix = 1_800_000_000.0 + at as f64 / 1e9;
        l.apply(&Notice::parse(&info(state, reason, session, unix - delay_s)).unwrap(), at, unix)
    }

    #[test]
    fn parses_version_one_and_rejects_everything_malformed() {
        let n = Notice::parse(&info("on", "start", "A", 5.0)).unwrap();
        assert!(n.on && n.reason == Reason::Start && n.pid == 4321 && n.shortcut_label == "⌃⌘N · Toggle");
        // A float-typed integer (NSNumber) is still version 1.
        let mut f = info("off", "hello", "", 5.0);
        f["v"] = serde_json::json!(1.0);
        assert!(Notice::parse(&f).is_some());
        for (key, bad) in [("v", serde_json::json!(2)), ("v", serde_json::json!("1")), ("state", serde_json::json!("maybe")),
            ("reason", serde_json::json!("pause")), ("pid", serde_json::json!(0)), ("pid", serde_json::json!("12")),
            ("sent", serde_json::json!("now")), ("sent", serde_json::json!(-1)), ("session", serde_json::json!(7)), ("shortcutLabel", serde_json::json!([]))] {
            let mut v = info("on", "start", "A", 5.0);
            v[key] = bad.clone();
            assert!(Notice::parse(&v).is_none(), "{key} = {bad}");
        }
        for key in ["v", "state", "reason", "pid", "sent"] {
            let mut v = info("on", "start", "A", 5.0);
            v.as_object_mut().unwrap().remove(key);
            assert!(Notice::parse(&v).is_none(), "missing {key}");
        }
        assert!(Notice::parse(&serde_json::Value::Null).is_none());
        assert!(Notice::parse(&serde_json::json!([1, 2])).is_none());
    }

    #[test]
    fn start_and_stop_are_stamped_less_their_posting_delay_clamped_to_two_seconds() {
        let mut l = link();
        assert!(feed(&mut l, "on", "start", "A", 10 * S, 0.3).started);
        assert!(feed(&mut l, "off", "stop", "A", 20 * S, 9.0).stopped, "a 9 s delay counts as 2 s");
        // [9.7, 18]: half of 9..11 is 1.3 s covered (9.7..11) -> dictated; 18..20 is not.
        assert!(l.dictated(9 * S, 11 * S, 30 * S));
        assert!(!l.dictated(18 * S, 20 * S, 30 * S));
        // A clock running ahead of Heimdall's (negative delay) subtracts nothing.
        let mut l = link();
        feed(&mut l, "on", "start", "B", 10 * S, -5.0);
        assert!(!l.dictated(8 * S, 10 * S - 1, 11 * S));
    }

    #[test]
    fn an_utterance_counts_when_at_least_half_of_it_overlaps() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        feed(&mut l, "off", "stop", "A", 20 * S, 0.0);
        assert!(l.dictated(12 * S, 18 * S, 30 * S), "inside");
        assert!(l.dictated(8 * S, 12 * S, 30 * S), "exactly half");
        assert!(!l.dictated(7 * S, 11 * S, 30 * S), "a quarter");
        assert!(l.dictated(19 * S, 21 * S, 30 * S), "half at the end");
        assert!(!l.dictated(19 * S + S / 2, 22 * S, 30 * S));
        assert!(l.dictated(15 * S, 15 * S, 30 * S) && !l.dictated(25 * S, 25 * S, 30 * S), "an instant inside or not");
        // Two sessions together cover an utterance that spans the gap between them.
        feed(&mut l, "on", "start", "B", 22 * S, 0.0);
        feed(&mut l, "off", "stop", "B", 30 * S, 0.0);
        assert!(l.dictated(18 * S, 24 * S, 31 * S));
    }

    #[test]
    fn an_open_dictation_counts_up_to_now_and_heartbeats_keep_it_open() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        for k in 1..=6 {
            assert!(!feed(&mut l, "on", "heartbeat", "A", 10 * S + k * 15 * S, 0.0).started);
            assert!(!l.expire(10 * S + k * 15 * S + 40 * S));
        }
        assert!(l.on());
        let now = 110 * S;
        assert!(l.dictated(100 * S, 108 * S, now), "still dictating at 100 s");
        assert!(!l.dictated(100 * S, 140 * S, now), "the future is not dictation");
        assert_eq!(l.deadline(), Some(100 * S + TIMEOUT_NS + 1));
    }

    #[test]
    fn a_missed_stop_ends_at_the_last_heartbeat_after_45_seconds() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        feed(&mut l, "on", "heartbeat", "A", 25 * S, 0.0);
        assert!(!l.expire(25 * S + TIMEOUT_NS), "exactly 45 s is still on");
        assert!(l.on());
        assert!(l.expire(25 * S + TIMEOUT_NS + 1));
        assert!(!l.on() && l.ended_by == Some("timeout"));
        assert!(l.dictated(20 * S, 24 * S, 80 * S));
        assert!(!l.dictated(30 * S, 60 * S, 80 * S), "after the last heartbeat it was not dictation");
        assert_eq!(l.deadline(), None);
    }

    #[test]
    fn a_dead_vella_ends_its_dictation_when_last_heard() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        l.alive = |_| false;
        assert!(l.expire(12 * S));
        assert_eq!(l.ended_by, Some("pid_gone"));
        assert!(!l.dictated(11 * S, 12 * S, 12 * S));
    }

    #[test]
    fn a_hello_updates_the_keybind_and_a_hello_while_on_opens_a_dictation() {
        let mut l = link();
        let mut hello = info("off", "hello", "", 1.0);
        assert!(l.apply(&Notice::parse(&hello).unwrap(), S, 1.0).shortcut);
        assert_eq!(l.keys().as_deref(), Some("⌃⌘N"));
        hello["shortcut"] = serde_json::json!("{\"key\":\"d\",\"modifiers\":[\"option\"],\"behavior\":\"hold\"}");
        hello["shortcutLabel"] = serde_json::json!("⌥D · Hold");
        let c = l.apply(&Notice::parse(&hello).unwrap(), 2 * S, 2.0);
        assert!(c.shortcut && !c.started && !l.on());
        assert_eq!((l.keys().as_deref(), l.shortcut.contains("option")), (Some("⌥D"), true));
        assert!(!l.apply(&Notice::parse(&hello).unwrap(), 3 * S, 3.0).shortcut, "the same keybind again changes nothing");
        // Heimdall started mid-dictation: Vella's answer says on.
        assert!(feed(&mut l, "on", "hello", "Z", 40 * S, 0.0).started);
    }

    #[test]
    fn a_new_session_or_a_new_vella_closes_the_old_interval() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        feed(&mut l, "on", "start", "B", 20 * S, 0.0);
        assert_eq!(l.ended_by, Some("new_session"));
        assert!(l.on());
        let mut other = info("on", "heartbeat", "B", 1_800_000_030.0);
        other["pid"] = serde_json::json!(999);
        let c = l.apply(&Notice::parse(&other).unwrap(), 30 * S, 1_800_000_030.0);
        assert!(c.stopped && l.on(), "the new process's own dictation opens");
        // Quit ends it.
        assert!(l.apply(&Notice::parse(&{ let mut q = info("off", "quit", "B", 1_800_000_031.0); q["pid"] = serde_json::json!(999); q }).unwrap(), 31 * S, 1_800_000_031.0).stopped);
        assert_eq!(l.ended_by, Some("quit"));
    }

    #[test]
    fn covers_marks_the_instants_inside_a_dictation() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        feed(&mut l, "off", "stop", "A", 20 * S, 0.0);
        assert!(!l.covers(9 * S, 30 * S) && l.covers(10 * S, 30 * S) && l.covers(20 * S, 30 * S) && !l.covers(20 * S + 1, 30 * S));
        feed(&mut l, "on", "start", "B", 40 * S, 0.0);
        assert!(l.covers(45 * S, 50 * S) && !l.covers(51 * S, 50 * S), "an open one counts up to now");
    }

    #[test]
    fn old_intervals_are_forgotten_after_ten_minutes() {
        let mut l = link();
        feed(&mut l, "on", "start", "A", 10 * S, 0.0);
        feed(&mut l, "off", "stop", "A", 20 * S, 0.0);
        l.expire(20 * S + KEEP_NS);
        assert!(l.dictated(12 * S, 14 * S, 20 * S + KEEP_NS));
        l.expire(20 * S + KEEP_NS + 1);
        assert!(!l.dictated(12 * S, 14 * S, 20 * S + KEEP_NS + 1));
    }

    #[test]
    fn the_real_pid_check_sees_this_process_and_not_a_free_pid() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0) && !pid_alive(-1));
    }
}
