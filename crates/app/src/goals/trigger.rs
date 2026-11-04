//! When the shadow calls its model (plan, 31 Oct): on events, debounced, never per line. The user's words fire once he
//! has been quiet `QUIET_MS`, at most `MAX_WAIT_MS` after the first pending words (a long monologue still gets a call);
//! a finished task `TASK_MS` after its notice; Heimdall's reply and every other line ride on the next call. Calls are
//! at least `GAP_MS` apart: events meanwhile coalesce into one. Nothing new, no call. Every time is passed in (epoch
//! ms), so the lab replays a day in simulated time.

use crate::goals::stream::Kind;

pub const QUIET_MS: i64 = 60_000;
pub const MAX_WAIT_MS: i64 = 180_000;
pub const TASK_MS: i64 = 20_000;
pub const GAP_MS: i64 = 45_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Triggers {
    /// The user's first and latest words since the last call.
    owner: Option<(i64, i64)>,
    /// The earliest finished task since the last call.
    task: Option<i64>,
    /// When the last call went out.
    last_call: Option<i64>,
    /// A failed call's lines wait for this retry.
    retry: Option<i64>,
}

/// Why a call went out (the lab's `trigger`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why { Owner, Task, Both, Retry }

impl Why {
    pub fn as_str(self) -> &'static str {
        match self { Why::Owner => "owner", Why::Task => "task", Why::Both => "owner+task", Why::Retry => "retry" }
    }
}

impl Triggers {
    /// A new stream line at `at`.
    pub fn note(&mut self, kind: Kind, at: i64) {
        match kind {
            Kind::Owner => self.owner = Some(self.owner.map_or((at, at), |(first, last)| (first.min(at), last.max(at)))),
            Kind::Task => self.task = Some(self.task.map_or(at, |t| t.min(at))),
            Kind::Heimdall | Kind::Tool | Kind::Notice => {}
        }
    }

    /// When the next call may go out, if anything waits for one.
    pub fn due(&self) -> Option<i64> {
        let owner = self.owner.map(|(first, last)| (last + QUIET_MS).min(first + MAX_WAIT_MS));
        let task = self.task.map(|t| t + TASK_MS);
        let at = [owner, task, self.retry].into_iter().flatten().min()?;
        Some(self.last_call.map_or(at, |c| at.max(c + GAP_MS)))
    }

    /// The call due at `now`, if one is: the pending events are taken by it.
    pub fn fire(&mut self, now: i64) -> Option<Why> {
        if self.due()? > now { return None }
        let why = match (self.owner.is_some(), self.task.is_some()) {
            (true, true) => Why::Both,
            (true, false) => Why::Owner,
            (false, true) => Why::Task,
            (false, false) => Why::Retry,
        };
        *self = Triggers { last_call: Some(now), ..Triggers::default() };
        Some(why)
    }

    /// The call fired at its time failed without costing an answer (network, server): its lines go again after `after`.
    pub fn retry_in(&mut self, now: i64, after: i64) { self.retry = Some(now + after) }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: i64 = 1000;

    #[test]
    fn owner_fires_after_a_minute_of_quiet_at_most_three_after_his_first_words() {
        let mut t = Triggers::default();
        t.note(Kind::Owner, 0);
        assert_eq!(t.due(), Some(60 * S));
        assert_eq!(t.fire(59 * S), None);
        // He keeps talking: each line pushes the quiet minute, the three minutes from his first words cap it.
        for at in [50, 100, 150] { t.note(Kind::Owner, at * S) }
        assert_eq!(t.due(), Some(180 * S));
        assert_eq!(t.fire(180 * S), Some(Why::Owner));
        assert_eq!(t.due(), None, "nothing new, no call");
        assert_eq!(t.fire(10_000 * S), None);
    }

    #[test]
    fn heimdall_and_notices_ride_along_and_never_fire() {
        let mut t = Triggers::default();
        for (k, at) in [(Kind::Heimdall, 0), (Kind::Tool, 1), (Kind::Notice, 2)] { t.note(k, at * S) }
        assert_eq!(t.due(), None);
        t.note(Kind::Task, 10 * S);
        assert_eq!(t.due(), Some(30 * S));
        assert_eq!(t.fire(30 * S), Some(Why::Task));
    }

    #[test]
    fn calls_are_45_s_apart_and_events_meanwhile_coalesce() {
        let mut t = Triggers::default();
        t.note(Kind::Task, 0);
        assert_eq!(t.fire(20 * S), Some(Why::Task));
        t.note(Kind::Task, 21 * S);
        t.note(Kind::Owner, 22 * S);
        // The task would be due at 41 s, the gap holds it to 65 s; the user's words join that call.
        assert_eq!(t.due(), Some(65 * S));
        assert_eq!(t.fire(64 * S), None);
        assert_eq!(t.fire(65 * S), Some(Why::Both));
        assert_eq!(t.due(), None);
    }

    #[test]
    fn a_failed_call_retries_its_lines_later() {
        let mut t = Triggers::default();
        t.note(Kind::Owner, 0);
        assert_eq!(t.fire(60 * S), Some(Why::Owner));
        t.retry_in(60 * S, 120 * S);
        assert_eq!(t.due(), Some(180 * S));
        assert_eq!(t.fire(180 * S), Some(Why::Retry));
    }
}
