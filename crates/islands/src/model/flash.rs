//! Hidden islands come back for news ("just flashing, like oh, something's happened, and then they
//! disappear again"). While every island is hidden (`visibility.json`), an island with news fades in at its own place
//! with one soft pulse, stays `HOLD` seconds after its latest news, then fades out again. Pure: the app feeds it news,
//! the pointer, openness and attention with the time, and carries out the `Cmd`s it returns; one one-shot timer at
//! `next_wake` drives it (nothing runs at rest).
//! - News (`news`): an island arrives (a notice, a reply pulled out, a new job); its state becomes done, failed or
//!   waiting; a question or waiting item appears (`Attention`); a viewer or note shows new content (Heimdall replies).
//!   A stage or progress update is not news.
//! - Held up while the pointer is over it, while it is open (a click acts as usual) and while it needs the user (`sticky`:
//!   its question or bell not seen yet). Released, it lingers `LINGER` seconds. Held while fading out (clicked or
//!   pointed at in the last moment), it comes back instead of leaving.
//! - Grouped: islands up on their timers leave together, at the latest deadline among them, so news in a row keeps them
//!   all up instead of one going as the next comes; news for an island already up extends it and pulses at most once
//!   per `PULSE_GAP`; news for an island fading out brings it back from where the fade is.

use std::collections::BTreeMap;

use super::island::Island;
use super::plan::{Attention, Plan};

/// Seconds an island stays after its latest news.
pub const HOLD: f64 = 4.0;
/// Seconds it stays after the pointer leaves, it closes or its question is seen.
pub const LINGER: f64 = 1.5;
pub const FADE_IN: f64 = 0.3;
pub const FADE_OUT: f64 = 0.6;
/// No second pulse within this many seconds of the last one.
pub const PULSE_GAP: f64 = 1.5;
/// While the pointer is over a revealed island, it is looked at again this often (no tracking otherwise).
pub const HOVER_CHECK: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum News {
    Arrived,
    Result,
    Waiting,
    Ask,
    Reply,
}

/// What changed between two versions of an island that the user should see, if anything.
pub fn news(old: Option<&Island>, new: &Island) -> Option<News> {
    let Some(old) = old else { return Some(News::Arrived) };
    if old.state() != new.state() {
        match new.state() {
            "done" | "failed" => return Some(News::Result),
            "waiting" => return Some(News::Waiting),
            _ => {}
        }
    }
    if asks_more(old, new) {
        return Some(News::Ask);
    }
    let changed = |k: &str| old.raw.get(k) != new.raw.get(k);
    let reply = match new.preset() {
        "viewer" => old.path() != new.path() || changed("updated"),
        "note" | "result" => ["summary", "value", "parts", "image"].iter().any(|k| changed(k)),
        _ => false,
    };
    reply.then_some(News::Reply)
}

/// A new kind of attention, a new waiting item or new question buttons; one going away is not news.
fn asks_more(old: &Island, new: &Island) -> bool {
    let Some(kind) = Attention::of(new) else { return false };
    if Attention::of(old).as_deref() != Some(kind.as_str()) {
        return true;
    }
    let asks = |i: &Island| {
        let mut v: Vec<String> = Plan::items(&i.raw).into_iter().filter(|t| t.group.as_deref() == Some("waiting") && !t.done).map(|t| t.key).collect();
        v.extend(i.parts().iter().filter_map(|p| p.get("action").and_then(|a| a.as_str()).map(str::to_owned)));
        v
    };
    let before = asks(old);
    asks(new).iter().any(|a| !before.contains(a))
}

/// What the app does to an island's window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Onto the screen (if it is not) and fade in from where it is.
    Reveal(String),
    /// One soft pulse.
    Pulse(String),
    /// Fade out (`FADE_OUT`).
    Conceal(String),
    /// The fade is over: off the screen.
    Gone(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Up,
    Leaving { until: f64 },
}

#[derive(Clone, Debug)]
struct Entry {
    phase: Phase,
    deadline: f64,
    sticky: bool,
    open: bool,
    hover: bool,
    pulsed: f64,
}

impl Entry {
    fn held(&self) -> bool {
        self.sticky || self.open || self.hover
    }
}

#[derive(Default, Debug)]
pub struct Flash {
    entries: BTreeMap<String, Entry>,
}

impl Flash {
    pub fn new() -> Flash {
        Flash::default()
    }

    /// Whether the island is on screen for news now (up or fading out).
    pub fn is_up(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// Islands on screen for news, in id order.
    pub fn up(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Up, not fading out, and not held: these leave on the timer.
    pub fn timed(&self) -> Vec<String> {
        self.entries.iter().filter(|(_, e)| e.phase == Phase::Up && !e.held()).map(|(k, _)| k.clone()).collect()
    }

    /// News for `id`; `sticky`: it needs the user (an unseen question or bell).
    pub fn news(&mut self, id: &str, now: f64, sticky: bool) -> Vec<Cmd> {
        let mut out = vec![];
        match self.entries.get_mut(id) {
            Some(e) => {
                e.sticky = sticky;
                e.deadline = e.deadline.max(now + HOLD);
                if matches!(e.phase, Phase::Leaving { .. }) {
                    e.phase = Phase::Up;
                    out.push(Cmd::Reveal(id.into()));
                }
                if now - e.pulsed >= PULSE_GAP {
                    e.pulsed = now;
                    out.push(Cmd::Pulse(id.into()));
                }
            }
            None => {
                self.entries.insert(id.into(), Entry { phase: Phase::Up, deadline: now + HOLD, sticky, open: false, hover: false, pulsed: now });
                out.push(Cmd::Reveal(id.into()));
                out.push(Cmd::Pulse(id.into()));
            }
        }
        out
    }

    fn set(&mut self, id: &str, now: f64, f: impl FnOnce(&mut Entry)) -> Vec<Cmd> {
        let Some(e) = self.entries.get_mut(id) else { return vec![] };
        let was = e.held();
        f(e);
        if was && !e.held() {
            e.deadline = e.deadline.max(now + LINGER);
        }
        if e.held() && matches!(e.phase, Phase::Leaving { .. }) {
            e.phase = Phase::Up;
            return vec![Cmd::Reveal(id.into())];
        }
        vec![]
    }

    pub fn set_sticky(&mut self, id: &str, sticky: bool, now: f64) -> Vec<Cmd> {
        self.set(id, now, |e| e.sticky = sticky)
    }

    pub fn set_open(&mut self, id: &str, open: bool, now: f64) -> Vec<Cmd> {
        self.set(id, now, |e| e.open = open)
    }

    pub fn set_hover(&mut self, id: &str, hover: bool, now: f64) -> Vec<Cmd> {
        self.set(id, now, |e| e.hover = hover)
    }

    /// Its file went away (or the island was hidden on its own): nothing to do any more.
    pub fn removed(&mut self, id: &str) {
        self.entries.remove(id);
    }

    /// Shown or hidden all at once: news no longer decides; returns the islands that were up.
    pub fn clear(&mut self) -> Vec<String> {
        std::mem::take(&mut self.entries).into_keys().collect()
    }

    /// Run the clock to `now`: fades that ended go off screen; held islands that start leaving fade out together.
    pub fn tick(&mut self, now: f64) -> Vec<Cmd> {
        let mut out = vec![];
        let gone: Vec<String> = self.entries.iter().filter(|(_, e)| matches!(e.phase, Phase::Leaving { until } if until <= now)).map(|(k, _)| k.clone()).collect();
        for id in gone {
            self.entries.remove(&id);
            out.push(Cmd::Gone(id));
        }
        let timed = self.timed();
        let last = timed.iter().filter_map(|k| self.entries.get(k)).map(|e| e.deadline).fold(f64::MIN, f64::max);
        if !timed.is_empty() && last <= now {
            for id in timed {
                if let Some(e) = self.entries.get_mut(&id) {
                    e.phase = Phase::Leaving { until: now + FADE_OUT };
                }
                out.push(Cmd::Conceal(id));
            }
        }
        out
    }

    /// When `tick` has something to do next (or the pointer should be looked at again); `None`: nothing left.
    pub fn next_wake(&self, now: f64) -> Option<f64> {
        let mut t = f64::MAX;
        let timed = self.timed();
        if !timed.is_empty() {
            t = t.min(timed.iter().filter_map(|k| self.entries.get(k)).map(|e| e.deadline).fold(f64::MIN, f64::max));
        }
        for e in self.entries.values() {
            match e.phase {
                Phase::Leaving { until } => t = t.min(until),
                Phase::Up if e.hover => t = t.min(now + HOVER_CHECK),
                Phase::Up => {}
            }
        }
        // A held island only waits for its release (an event), except the pointer, which is looked at.
        (t < f64::MAX).then_some(t)
    }
}
