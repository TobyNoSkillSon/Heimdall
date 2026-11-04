//! The fast reader inside the island: a `crates/rsvp` plan played on the island's clock. Each unit shows for its own
//! `duration_ms`, then its `pause_after_ms` blank; the next change is always one deadline (one one-shot timer per
//! unit in the app, nothing in between). Times are the island's seconds; nothing here reads a clock.

use crate::reader::{on_grid, FastRead, ReadOutcome, ReadResult, LEAD_IN_MS};

/// The reader closes itself this long after the end ("now it doesn't disappear I have to click on
/// it"; DESIGN.md §3 lists it as the one line to confirm with him).
pub const AUTO_CLOSE_SECONDS: f64 = 1.2;
/// Pulled up, the reader waits at the notch this long, then counts as dismissed.
pub const TOKEN_SECONDS: f64 = 600.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    /// Open, waiting for play.
    Ready,
    /// The red focus point alone, since this time.
    LeadIn(f64),
    /// Playing: the current unit (or its blank) began at `since`.
    Playing { since: f64, blank: bool },
    /// Stopped on a unit; the whole sentence shows.
    Paused,
    /// Past the last unit, since this time: the face, then the reader closes itself.
    Ended(f64),
}

/// What the reader's centre shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Face {
    Title(String),
    Focus,
    Unit { text: String, focus: usize },
    Blank,
    Sentence { read: String, rest: String },
    End,
}

#[derive(Clone, Debug)]
pub struct Reader {
    pub message: FastRead,
    pub plan: rsvp::Plan,
    pub phase: Phase,
    pub index: usize,
    pub start_wpm: u32,
    pub rewinds: u32,
    pub reached: usize,
    /// Pulled up into the notch until this time (a token; a click opens it again, paused).
    pub pulled: Option<f64>,
    weighted: f64,
    weight: f64,
}

impl Reader {
    pub fn new(message: FastRead) -> Reader {
        let wpm = on_grid(message.wpm.clamp(1, 2000) as f64);
        let plan = rsvp::plan(&message.text, &rsvp::Config { wpm, ..Default::default() });
        Reader { message, plan, phase: Phase::Ready, index: 0, start_wpm: wpm, rewinds: 0, reached: 0, pulled: None, weighted: 0.0, weight: 0.0 }
    }

    pub fn wpm(&self) -> u32 {
        self.plan.wpm
    }

    pub fn started(&self) -> bool {
        self.phase != Phase::Ready
    }

    pub fn playing(&self) -> bool {
        matches!(self.phase, Phase::LeadIn(_) | Phase::Playing { .. })
    }

    /// The next change on its own, or none (waiting, paused).
    pub fn deadline(&self) -> Option<f64> {
        if let Some(until) = self.pulled {
            return Some(until);
        }
        match self.phase {
            Phase::LeadIn(since) => Some(since + LEAD_IN_MS / 1000.0),
            Phase::Playing { since, blank } => {
                let u = self.plan.units.get(self.index)?;
                Some(since + (if blank { u.pause_after_ms } else { u.duration_ms.max(1) }) as f64 / 1000.0)
            }
            Phase::Ended(at) => Some(at + AUTO_CLOSE_SECONDS),
            _ => None,
        }
    }

    /// Advances through every change due by `t` (a late or skipped tick catches up, never replays). Returns true
    /// when the reader should close (auto-close after the end, or the token's time ran out).
    pub fn tick(&mut self, t: f64) -> bool {
        for _ in 0..100_000 {
            let Some(due) = self.deadline() else { return false };
            if due > t {
                return false;
            }
            if self.pulled.is_some() {
                return true;
            }
            match self.phase {
                Phase::LeadIn(_) => {
                    self.phase = if self.plan.units.is_empty() { Phase::Ended(due) } else { Phase::Playing { since: due, blank: false } };
                }
                Phase::Playing { blank, .. } => {
                    let Some(u) = self.plan.units.get(self.index) else {
                        self.phase = Phase::Ended(due);
                        continue;
                    };
                    let spent = if blank { u.pause_after_ms } else { u.duration_ms } as f64;
                    if 3 * self.index >= self.plan.units.len() {
                        self.weighted += spent * self.plan.wpm as f64;
                        self.weight += spent;
                    }
                    if !blank && u.pause_after_ms > 0 {
                        self.phase = Phase::Playing { since: due, blank: true };
                    } else if self.index + 1 >= self.plan.units.len() {
                        self.phase = Phase::Ended(due);
                    } else {
                        self.index += 1;
                        self.reached = self.reached.max(self.index);
                        self.phase = Phase::Playing { since: due, blank: false };
                    }
                }
                Phase::Ended(_) => return true,
                _ => return false,
            }
        }
        false
    }

    pub fn face(&self) -> Face {
        match self.phase {
            Phase::Ready => Face::Title(self.message.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Fast read".into())),
            Phase::LeadIn(_) => Face::Focus,
            // The pause after a unit holds that unit (a blank flickered at every clause and sentence end).
            Phase::Playing { blank: true, .. } => match self.plan.units.get(self.index) {
                Some(u) => Face::Unit { text: u.text.clone(), focus: u.focus_index.min(u.text.chars().count().saturating_sub(1)) },
                None => Face::Blank,
            },
            Phase::Playing { blank: false, .. } => match self.plan.units.get(self.index) {
                Some(u) => Face::Unit { text: u.text.clone(), focus: u.focus_index.min(u.text.chars().count().saturating_sub(1)) },
                None => Face::Blank,
            },
            Phase::Paused => {
                let Some(u) = self.plan.units.get(self.index) else { return Face::Blank };
                let range = self.plan.sentence_range(u.sentence_index);
                let read: Vec<&str> = self.plan.units[range.start..self.index.max(range.start)].iter().map(|u| u.text.as_str()).collect();
                let rest: Vec<&str> = self.plan.units[self.index.max(range.start)..range.end].iter().map(|u| u.text.as_str()).collect();
                let read = read.join(" ");
                Face::Sentence { read: if read.is_empty() { read } else { read + " " }, rest: rest.join(" ") }
            }
            Phase::Ended(_) => Face::End,
        }
    }

    /// Time left at the current speed, as the ear shows it ("0:42").
    pub fn left(&self) -> String {
        let ms = match self.phase {
            Phase::Ended(_) => 0,
            Phase::Ready | Phase::LeadIn(_) => self.plan.total_ms,
            _ => self.plan.remaining_ms(self.index),
        };
        let s = (ms as f64 / 1000.0).ceil() as u64;
        format!("{}:{:02}", s / 60, s % 60)
    }

    /// Play or pause (the play button, or a click on the words).
    pub fn toggle(&mut self, t: f64) {
        match self.phase {
            Phase::Ready => self.phase = Phase::LeadIn(t),
            Phase::LeadIn(_) | Phase::Playing { .. } => self.phase = Phase::Paused,
            Phase::Paused => self.phase = Phase::Playing { since: t, blank: false },
            Phase::Ended(_) => self.repeat(t),
        }
    }

    /// Back to the start of this sentence (the previous one when already at its start), playing.
    pub fn back(&mut self, t: f64) {
        if !self.started() || self.plan.units.is_empty() {
            return;
        }
        let i = self.index.min(self.plan.units.len() - 1);
        self.index = if matches!(self.phase, Phase::Ended(_)) { self.plan.sentence_start(i) } else { self.plan.back(i) };
        self.rewinds += 1;
        self.phase = Phase::Playing { since: t, blank: false };
    }

    /// The whole message again, from the lead-in.
    pub fn repeat(&mut self, t: f64) {
        if !self.started() {
            return;
        }
        self.index = 0;
        self.rewinds += 1;
        self.phase = Phase::LeadIn(t);
    }

    /// One step on the dial; the current unit restarts at the new speed.
    pub fn nudge(&mut self, faster: bool, t: f64) {
        let dial = rsvp::dial();
        let wpm = self.plan.wpm;
        let next = if faster { dial.iter().copied().find(|w| *w > wpm) } else { dial.iter().rev().copied().find(|w| *w < wpm) }.unwrap_or(wpm);
        if next == wpm {
            return;
        }
        self.plan.retime(next);
        if let Phase::Playing { .. } = self.phase {
            self.phase = Phase::Playing { since: t, blank: false };
        }
    }

    /// A better starting speed arrived before the user started.
    pub fn start_wpm(&mut self, wpm: u32) {
        if self.phase == Phase::Ready {
            let wpm = on_grid(wpm.clamp(1, 2000) as f64);
            self.plan.retime(wpm);
            self.start_wpm = wpm;
        }
    }

    /// Pulled up: paused, resting at the notch as a token.
    pub fn pull_up(&mut self, t: f64) {
        if self.playing() {
            self.phase = Phase::Paused;
        }
        self.pulled = Some(t + TOKEN_SECONDS);
    }

    pub fn reopen(&mut self) {
        self.pulled = None;
    }

    fn progress_pct(&self) -> u8 {
        let n = self.plan.units.len();
        match self.phase {
            Phase::Ended(_) => 100,
            Phase::Ready => 0,
            _ if n == 0 => 100,
            _ => (((self.reached + 1) as f64 / n as f64) * 100.0).round().min(100.0) as u8,
        }
    }

    /// The result the app hears once: read when the end (or its last tenth) was reached.
    pub fn result(&self) -> ReadResult {
        let pct = self.progress_pct();
        let outcome = if pct >= 90 { ReadOutcome::Read } else { ReadOutcome::Dismissed };
        self.result_as(outcome)
    }

    pub fn result_as(&self, outcome: ReadOutcome) -> ReadResult {
        let settled = if self.weight > 0.0 { on_grid(self.weighted / self.weight) } else { self.plan.wpm };
        ReadResult { id: self.message.id.clone(), outcome, start_wpm: self.start_wpm, settled_wpm: settled, rewinds: self.rewinds, progress_pct: self.progress_pct() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> FastRead {
        FastRead { id: "r1".into(), text: text.into(), title: Some("Build report".into()), wpm: 240 }
    }

    #[test]
    fn units_play_for_their_own_durations_and_the_reader_closes_after_the_end() {
        let mut r = Reader::new(msg("The build failed because tests timed out. Three things matter."));
        assert_eq!(r.face(), Face::Title("Build report".into()));
        assert_eq!(r.deadline(), None, "waiting for play: no timer");
        r.toggle(0.0);
        assert_eq!(r.face(), Face::Focus);
        assert_eq!(r.deadline(), Some(0.6));
        r.tick(0.6);
        let first = r.plan.units[0].clone();
        assert_eq!(r.face(), Face::Unit { text: first.text.clone(), focus: first.focus_index });
        assert_eq!(r.deadline(), Some(0.6 + first.duration_ms as f64 / 1000.0));
        // Different units have different durations (not one fixed interval: the 17:50 complaint).
        let durations: std::collections::BTreeSet<u32> = r.plan.units.iter().map(|u| u.duration_ms).collect();
        assert!(durations.len() > 2, "{durations:?}");
        let end = 0.6 + r.plan.total_ms as f64 / 1000.0;
        assert!(!r.tick(end - 0.001));
        r.tick(end + 0.0001);
        assert_eq!(r.face(), Face::End);
        assert!(!r.tick(end + 1.0));
        assert!(r.tick(end + AUTO_CLOSE_SECONDS + 0.001), "closes itself");
        assert_eq!(r.result().outcome, ReadOutcome::Read);
    }

    #[test]
    fn empty_and_huge_texts_never_panic() {
        let mut r = Reader::new(msg(""));
        r.toggle(0.0);
        r.tick(1.0);
        assert_eq!(r.face(), Face::End);
        r.back(1.0);
        r.repeat(1.1);
        r.nudge(true, 1.2);
        let _ = r.left();
        let mut big = Reader::new(msg(&"word ".repeat(20_000)));
        big.toggle(0.0);
        big.tick(30_000.0);
        assert!(matches!(big.phase, Phase::Ended(_)));
    }

    #[test]
    fn pause_shows_the_sentence_with_the_read_part() {
        let mut r = Reader::new(msg("First, the limit is two minutes, and the tests need longer."));
        r.toggle(0.0);
        r.tick(0.6);
        let t2 = 0.6 + (r.plan.units[0].duration_ms + r.plan.units[0].pause_after_ms) as f64 / 1000.0 + 0.0001;
        r.tick(t2);
        r.toggle(t2);
        match r.face() {
            Face::Sentence { read, rest } => {
                assert_eq!(read, "First, ");
                assert!(rest.starts_with("the limit"));
            }
            f => panic!("{f:?}"),
        }
        assert_eq!(r.deadline(), None);
    }
}
