//! The harness's one clock. Production reads the system clock; the simulator (`sim`) advances a virtual one, so a
//! fifteen-minute session replays in milliseconds. Harness policy (notice pacing, the 30-minute context window, the
//! two-minute resend limit, start times) reads time only through a `ClockHandle`; `clock_lint` keeps the clock-clean
//! files free of direct wall-clock calls.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Wall-clock milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// A virtual clock shared by every part of one simulated scenario. Time moves only when the runner advances it.
#[derive(Debug, Clone)]
pub struct SimClock(Arc<AtomicU64>);
impl SimClock {
    pub fn at(ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(ms)))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}
impl Clock for SimClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub type ClockHandle = Arc<dyn Clock>;
pub fn system() -> ClockHandle {
    Arc::new(SystemClock)
}
/// The system clock's time, for callers that have no clock handle (outside the harness policy).
pub fn now_ms() -> u64 {
    SystemClock.now_ms()
}

fn local(ms: u64) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms as i64).single()
}
/// Local time of day `HH:MM:SS`, the stamp every transcript line the agent receives carries (`[17:52:38 Mic · User]`).
pub fn local_hms(ms: u64) -> String {
    local(ms).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_else(|| "--:--:--".into())
}
/// Local time of day `HH:MM`.
pub fn local_hm(ms: u64) -> String {
    local(ms).map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "--:--".into())
}

/// Night pass 20→21 Oct (P1): deliveries carried only `[HH:MM:SS]` and restart lines only `HH:MM`, so on 20 Oct the
/// agent believed it was 19 Oct (its compaction summary was built from 19 Oct) and wrote that date into bug notes and
/// worker briefs. Every turn-starting delivery now opens with this line, right under the delivery marker. It lives in
/// the user message, never the system prompt, so the cached prefix stays the same all day.
pub const NOW_PREFIX: &str = "[Now: ";
/// `[Now: Tuesday 20 Oct 2026, 19:22]`, local time.
pub fn now_line(ms: u64) -> String {
    let at = local(ms).map(|t| t.format("%A %-d %b %Y, %H:%M").to_string()).unwrap_or_else(|| "unknown".into());
    format!("{NOW_PREFIX}{at}]")
}
/// `text` without a leading `now_line` (display and edit paths show what was said, not the stamp).
pub fn strip_now_line(text: &str) -> &str {
    match text.strip_prefix(NOW_PREFIX).and_then(|rest| rest.split_once("]\n")) {
        Some((stamp, body)) if !stamp.contains('\n') => body,
        _ => text,
    }
}

#[cfg(test)]
mod clock_lint {
    /// Files whose time comes only from a `ClockHandle`. Add a file here once its wall-clock calls are routed.
    const CLOCK_CLEAN: &[&str] = &["src/lib.rs", "src/notice.rs", "src/reply.rs", "src/sim/mod.rs", "src/sim/fake_pi.rs", "src/sim/scenarios.rs", "src/sim/reply_scenarios.rs"];
    #[test]
    fn clock_clean_files_read_time_only_through_the_clock() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for file in CLOCK_CLEAN {
            let text = std::fs::read_to_string(root.join(file)).unwrap_or_default();
            for (n, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                for banned in ["SystemTime::now", "Instant::now", "crate::now_ms()", "agent::now_ms()", "clock::now_ms()"] {
                    assert!(!code.contains(banned), "{file}:{}: {banned} outside the clock: {line}", n + 1);
                }
            }
        }
    }
    #[test]
    fn now_line_names_the_weekday_date_and_time_and_strips_back_off() {
        use chrono::TimeZone;
        let ms = chrono::Local.with_ymd_and_hms(2026, 10, 20, 19, 22, 5).single().unwrap().timestamp_millis() as u64;
        assert_eq!(super::now_line(ms), "[Now: Tuesday 20 Oct 2026, 19:22]");
        // Just after midnight the date moves on (the 19→20 Oct confusion).
        let after = chrono::Local.with_ymd_and_hms(2026, 10, 21, 0, 3, 0).single().unwrap().timestamp_millis() as u64;
        assert_eq!(super::now_line(after), "[Now: Wednesday 21 Oct 2026, 00:03]");
        let body = "[Heimdall typed message]\nhello";
        assert_eq!(super::strip_now_line(&format!("{}\n{body}", super::now_line(ms))), body);
        // Only a whole leading stamp line goes; the user's own words that start alike stay.
        assert_eq!(super::strip_now_line(body), body);
        assert_eq!(super::strip_now_line("[Now: what\nis this]\nx"), "[Now: what\nis this]\nx");
        assert_eq!(super::strip_now_line("[Now: no newline]"), "[Now: no newline]");
    }
    #[test]
    fn sim_clock_moves_only_when_advanced() {
        use super::Clock;
        let c = super::SimClock::at(1_000);
        assert_eq!(c.now_ms(), 1_000);
        c.advance(250);
        assert_eq!(c.clone().now_ms(), 1_250);
        c.set(5);
        assert_eq!(c.now_ms(), 5);
    }
}
