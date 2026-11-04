//! The island's "catching up" (31 Oct 22:20-22:24: the archive's write lock was held for minutes, thousands of writes
//! queued behind it, and nothing on screen said so). On the live loop's settings tick.
//!
//! (bug note 2026-11-03T23-08-14): "I don't want to see them... it's okay when we're catching up as
//! long as we are keeping up over the span of a couple of hours." A backlog that drains within minutes is routine and
//! shows nothing, no island and no warning: the island shows only when the oldest unsaved note has waited `SHOW_AFTER`
//! (the writer is really falling behind), or once writes were dropped (notes lost). It clears by itself once they
//! are saved. A drained backlog still restarts a screen stream that gave up meanwhile, shown or not.
use super::Pipeline;
use std::time::{Duration, Instant};

/// The island's key for it.
pub(crate) const KEY: &str = "archive-catch-up";
/// The oldest unsaved note waiting this long shows: about a fifth of what the writer's queue holds before it drops
/// (`live::MAX_PENDING` at ~124 notes a second), and far past the routine stalls of seconds to two minutes.
pub(crate) const SHOW_AFTER: Duration = Duration::from_secs(10 * 60);
/// A backlog this old counts as one the screen recorder may have given up in (its restarts span ~30 s).
const SCREEN_AFTER: Duration = Duration::from_secs(3);
/// The words change at most this often (the count falls every tick while it drains).
const WORDS_EVERY: Duration = Duration::from_secs(4);

/// What the island says, or `None` when nothing has fallen behind for real. `waiting`: writes queued for the
/// archive's writer; `behind`: how long the oldest has waited; `lost`: writes dropped since the island last cleared.
pub(crate) fn words(waiting: usize, behind: Duration, lost: usize) -> Option<String> {
    if waiting == 0 || (behind < SHOW_AFTER && lost == 0) {
        return None;
    }
    Some(format!("Catching up · {} {} to save", about(waiting), if waiting == 1 { "note" } else { "notes" }))
}

/// A count as read at a glance: exact below 1,000, then two significant figures ("6,500"), with thousands separators.
fn about(n: usize) -> String {
    let rounded = if n < 1000 {
        n
    } else {
        let step = 10usize.pow((n as f64).log10().floor() as u32 - 1);
        (n + step / 2) / step * step
    };
    let digits = rounded.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Pipeline {
    /// The settings tick's look at the archive's backlog (see the module notes).
    pub fn catch_up_tick(&mut self) {
        let queued = heimdall_archive::live::backlog();
        let waiting = queued.map_or(0, |(n, _)| n);
        let behind = queued.map(|(_, at)| at.elapsed()).unwrap_or_default();
        let dropped = heimdall_archive::live::dropped_writes();
        let lost = dropped.saturating_sub(*self.catch_up_dropped.get_or_insert(dropped));
        if behind >= SCREEN_AFTER {
            self.catch_up_backlog = true;
        }
        let shown = self.health.holds(KEY);
        match words(waiting, behind, lost) {
            Some(label) => {
                if shown && self.catch_up_shown.is_some_and(|at| at.elapsed() < WORDS_EVERY) {
                    return;
                }
                let cue = self.health.busy(KEY, ui::Part::Archive, &label);
                if cue.is_some() {
                    self.catch_up_shown = Some(Instant::now());
                }
                self.health_cue(cue);
            }
            None if waiting == 0 && (shown || self.catch_up_backlog) => self.archive_caught_up(),
            None => {}
        }
    }

    /// The archive saved what waited: the island says so if it showed, and a screen stream that gave up meanwhile
    /// starts again now, not at the next wake or display change (2 Nov 15:52-15:57: the screen stayed off after the
    /// archive caught up).
    pub(crate) fn archive_caught_up(&mut self) {
        let cue = self.health.recovered(KEY, ui::Part::Archive, "Caught up");
        self.catch_up_shown = None;
        self.catch_up_backlog = false;
        self.catch_up_dropped = Some(heimdall_archive::live::dropped_writes());
        self.health_cue(cue);
        if self.screen_gave_up {
            self.request_recovery(crate::health::Recovery::RestartScreen("the archive caught up".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_routine_wait_says_nothing_and_a_real_fall_behind_says_what_and_how_much() {
        assert_eq!(words(0, Duration::from_secs(3600), 0), None);
        assert_eq!(words(12, Duration::from_secs(2), 0), None, "an ordinary wait");
        // Bug note 2026-11-03T23-08-14: 3,601 queued, 36 s behind, caught up within seconds; and 2 Nov's 110 s stall.
        assert_eq!(words(3601, Duration::from_secs(36), 0), None, "routine catch-up shows nothing");
        assert_eq!(words(14_478, Duration::from_secs(110), 0), None);
        assert_eq!(words(6489, SHOW_AFTER, 0).as_deref(), Some("Catching up · 6,500 notes to save"));
        assert_eq!(words(1, SHOW_AFTER + Duration::from_secs(1), 0).as_deref(), Some("Catching up · 1 note to save"));
        assert_eq!(words(900, Duration::from_secs(5), 3).as_deref(), Some("Catching up · 900 notes to save"), "notes lost show at once");
    }
    #[test]
    fn counts_read_at_a_glance() {
        assert_eq!(about(7), "7");
        assert_eq!(about(999), "999");
        assert_eq!(about(1049), "1,000");
        assert_eq!(about(6489), "6,500");
        assert_eq!(about(12_345), "12,000");
        assert_eq!(about(65_536), "66,000");
    }
}
