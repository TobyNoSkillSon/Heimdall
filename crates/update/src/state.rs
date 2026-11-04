//! The app's update flow as a pure state machine, ported from Vella's UpdateMachine. The menu row follows `phase`.
//!
//! ```text
//! idle --checked(newer)--> available --confirmed--> downloading --verified(idle)--> installing --> (the app quits)
//!   ^                        |  ^                      |  `--verified(busy)--> waiting-for-idle --became-idle--'
//!   `--checked(none)---------'  `----failed(reason): the reason is kept, the offer stays-----------------'
//! ```
//!
//! An update is refused when confirmed while Heimdall is busy, and a verified download waits until it is idle. A check
//! never interrupts a download or install; a failed check keeps what was known.

use crate::Release;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Available(Release),
    Downloading(Release),
    WaitingForIdle(Release),
    Installing(Release),
}

impl Phase {
    pub fn release(&self) -> Option<&Release> {
        match self {
            Phase::Idle => None,
            Phase::Available(r) | Phase::Downloading(r) | Phase::WaitingForIdle(r) | Phase::Installing(r) => Some(r),
        }
    }

    /// Downloading or installing.
    pub fn busy(&self) -> bool {
        matches!(self, Phase::Downloading(_) | Phase::WaitingForIdle(_) | Phase::Installing(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A check finished: the release to offer, or None when this version is current.
    Checked(Option<Release>),
    CheckFailed(String),
    /// The user said Update.
    Confirmed,
    /// The download is verified; whether Heimdall is busy right now.
    Verified { busy: bool },
    BecameIdle,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub phase: Phase,
    /// The last failure or refusal; cleared when an update starts.
    pub last_error: Option<String>,
}

impl Default for Machine {
    fn default() -> Self {
        Machine { phase: Phase::Idle, last_error: None }
    }
}

impl Machine {
    /// Applies an event; returns false (and changes nothing) when it does not apply in the current phase.
    pub fn handle(&mut self, event: Event) -> bool {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let (next, applied) = match (phase, event) {
            (Phase::Idle | Phase::Available(_), Event::Checked(release)) => {
                (release.map(Phase::Available).unwrap_or(Phase::Idle), true)
            }
            (phase, Event::CheckFailed(_)) => (phase, true),
            (Phase::Available(r), Event::Confirmed) => {
                self.last_error = None;
                (Phase::Downloading(r), true)
            }
            (Phase::Downloading(r), Event::Verified { busy }) => {
                (if busy { Phase::WaitingForIdle(r) } else { Phase::Installing(r) }, true)
            }
            (Phase::WaitingForIdle(r), Event::BecameIdle) => (Phase::Installing(r), true),
            (Phase::Available(r), Event::Failed(reason)) => {
                self.last_error = Some(reason);
                (Phase::Available(r), true)
            }
            (Phase::Downloading(r) | Phase::WaitingForIdle(r) | Phase::Installing(r), Event::Failed(reason)) => {
                self.last_error = Some(reason);
                (Phase::Available(r), true)
            }
            (phase, _) => (phase, false),
        };
        self.phase = next;
        applied
    }
}

/// The periodic check is due: never checked, 24 hours since the last one, or the clock went backwards.
pub fn check_due(last: Option<u64>, now: u64, interval: u64) -> bool {
    match last {
        None => true,
        Some(last) => now < last || now - last >= interval,
    }
}

pub const DAY: u64 = 24 * 60 * 60;
