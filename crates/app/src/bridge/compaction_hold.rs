//! When Heimdall may start a compaction of its own (idle maintenance, a start's catch-up, session rotation).
//!
//! Two reasons hold them back. A compaction Pi answered as failed (the provider cannot summarise) holds until a reply
//! succeeds, as before. A compaction that died with its Pi (the harness's silence limit, a crash) is the night audit's
//! H1: on 27 Oct 03:25-03:53 each restart compacted the same 230k-token history again, was killed 180 s later, and
//! restarted, six times, each attempt billed in full. A killed compaction counts as failed and backs off, 10 minutes
//! then 20, a successful reply does not lift it early, and from the third the hold lasts until a compaction completes
//! (Pi's own threshold compaction inside a turn) or Heimdall relaunches.
use std::time::{Duration, Instant};

pub(super) const KILL_BACKOFF: Duration = Duration::from_secs(10 * 60);
pub(super) const KILL_CAP: u32 = 3;

#[derive(Debug, Default)]
pub(super) struct CompactionHold {
    failed: bool,
    kills: u32,
    until: Option<Instant>,
    /// This process's kill is counted: a failed reopen leaves the dead Pi, still compacting, to be seen again.
    counted: bool,
}
impl CompactionHold {
    pub(super) fn held(&self) -> bool { self.held_at(Instant::now()) }
    pub(super) fn held_at(&self, now: Instant) -> bool {
        self.failed || self.kills >= KILL_CAP || self.until.is_some_and(|u| now < u)
    }
    /// Pi answered a compaction as failed.
    pub(super) fn failed(&mut self) { self.failed = true; }
    /// A compaction was running when its Pi died. Returns the kills so far and how long the hold lasts (`None`: until
    /// a compaction completes), or `None` when this process's kill is already counted.
    pub(super) fn killed_at(&mut self, now: Instant) -> Option<(u32, Option<Duration>)> {
        if std::mem::replace(&mut self.counted, true) { return None; }
        self.kills += 1;
        let wait = (self.kills < KILL_CAP).then(|| KILL_BACKOFF * (1 << (self.kills - 1)));
        self.until = wait.map(|w| now + w);
        Some((self.kills, wait))
    }
    /// A new Pi process started: its own compaction's death counts.
    pub(super) fn new_process(&mut self) { self.counted = false; }
    /// A reply succeeded: a provider failure no longer holds. True when that lifted the hold.
    pub(super) fn reply_succeeded(&mut self) -> bool {
        let was = self.held();
        self.failed = false;
        was && !self.held()
    }
    /// Pi completed a compaction: the history can be summarised, the kill count starts over.
    pub(super) fn compacted(&mut self) { *self = Self::default(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_killed_compaction_backs_off_then_caps_and_only_a_completed_one_clears_it() {
        let t = Instant::now();
        let mut h = CompactionHold::default();
        assert!(!h.held_at(t));
        assert_eq!(h.killed_at(t), Some((1, Some(KILL_BACKOFF))));
        assert_eq!(h.killed_at(t), None, "the same dead process again (a failed reopen): counted once");
        assert!(h.held_at(t + KILL_BACKOFF - Duration::from_secs(1)));
        assert!(!h.reply_succeeded() && h.held_at(t + Duration::from_secs(1)), "a reply does not lift a kill's backoff");
        assert!(!h.held_at(t + KILL_BACKOFF));
        let t2 = t + KILL_BACKOFF;
        h.new_process();
        assert_eq!(h.killed_at(t2), Some((2, Some(2 * KILL_BACKOFF))));
        assert!(h.held_at(t2 + 2 * KILL_BACKOFF - Duration::from_secs(1)) && !h.held_at(t2 + 2 * KILL_BACKOFF));
        h.new_process();
        assert_eq!(h.killed_at(t2 + 2 * KILL_BACKOFF), Some((3, None)));
        assert!(h.held_at(t2 + Duration::from_secs(24 * 3600)), "capped: held until a compaction completes");
        h.compacted();
        assert!(!h.held_at(t2));
    }
    #[test]
    fn a_failed_compaction_holds_until_a_reply_succeeds() {
        let mut h = CompactionHold::default();
        h.failed();
        assert!(h.held());
        assert!(h.reply_succeeded());
        assert!(!h.held());
    }
}
