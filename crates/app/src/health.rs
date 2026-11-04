//! Heimdall's health as the island shows it (docs/resilience.md): which conditions are held, so each change is
//! sent once; the recovery ladder's attempt counts; and the microphone switch cue. Pure state, no I/O: the
//! pipeline sends the cues this returns and journals them.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use ui::{HealthControl, HealthCue, Part};

#[derive(Clone, Debug, PartialEq)]
enum Held {
    Restarting(String),
    Busy(String),
    Problem(String, String),
    Drop(String),
}

/// Held conditions by key. Each method returns the cue to send, or `None` when nothing changed.
#[derive(Default, Debug)]
pub struct Health {
    held: BTreeMap<String, Held>,
}

impl Health {
    pub fn restarting(&mut self, key: &str, part: Part, label: &str) -> Option<HealthCue> {
        self.hold(key, Held::Restarting(label.into()))
            .then(|| HealthCue::Restarting { key: key.into(), part, label: label.into() })
    }

    /// Heimdall behind on its own work ("Catching up · 6,489 notes to save"): calm, it clears by itself. A new label for
    /// the same key updates the words in place.
    pub fn busy(&mut self, key: &str, part: Part, label: &str) -> Option<HealthCue> {
        self.hold(key, Held::Busy(label.into()))
            .then(|| HealthCue::Busy { key: key.into(), part, label: label.into() })
    }

    /// "… OK", only for a condition that was shown: a part that never failed says nothing.
    pub fn recovered(&mut self, key: &str, part: Part, label: &str) -> Option<HealthCue> {
        self.held.remove(key).map(|_| HealthCue::Recovered { key: key.into(), part, label: label.into() })
    }

    pub fn problem(&mut self, key: &str, part: Part, label: &str, action: &str, control: HealthControl) -> Option<HealthCue> {
        self.hold(key, Held::Problem(label.into(), action.into())).then(|| HealthCue::Problem {
            key: key.into(),
            part,
            label: label.into(),
            action: action.into(),
            control,
        })
    }

    pub fn dropped(&mut self, key: &str, part: Part, label: &str) -> Option<HealthCue> {
        self.hold(key, Held::Drop(label.into())).then(|| HealthCue::Drop { key: key.into(), part, label: label.into() })
    }

    /// Ends `key` without a message (the user turned the part off; it no longer applies).
    pub fn clear(&mut self, key: &str) -> Option<HealthCue> {
        self.held.remove(key).map(|_| HealthCue::Clear { key: key.into() })
    }

    pub fn holds(&self, key: &str) -> bool {
        self.held.contains_key(key)
    }

    /// Keys held now that start with `prefix` ("mic:" for every extra microphone's pill).
    pub fn keys_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.held.keys().filter(|k| k.starts_with(prefix)).cloned().collect()
    }

    fn hold(&mut self, key: &str, held: Held) -> bool {
        if self.held.get(key) == Some(&held) {
            return false;
        }
        self.held.insert(key.into(), held);
        true
    }
}

/// Recovery attempts for one kind of failure within a window: the count decides the rung
/// (restart the part, then restart the live pipeline, then relaunch Heimdall). Never a loop: once the attempts in
/// the window are used up the ladder says so, and the caller shows a problem instead.
#[derive(Debug)]
pub struct Ladder {
    window: Duration,
    attempts: VecDeque<Instant>,
}

/// A rung of the recovery ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    /// Restart the failed part alone.
    Part,
    /// Turn Listening off and on in-process: capture, screen, decisions and agent restart; models stay loaded.
    Pipeline,
    /// Relaunch Heimdall (bounded separately, across launches).
    Relaunch,
}

impl Ladder {
    pub fn new(window: Duration) -> Self {
        Ladder { window, attempts: VecDeque::new() }
    }

    /// The rung for a failure at `now`: the first in the window restarts the part, the second the pipeline, any
    /// later one relaunches.
    pub fn climb(&mut self, now: Instant) -> Rung {
        while self.attempts.front().is_some_and(|t| now.duration_since(*t) > self.window) {
            self.attempts.pop_front();
        }
        let rung = match self.attempts.len() {
            0 => Rung::Part,
            1 => Rung::Pipeline,
            _ => Rung::Relaunch,
        };
        self.attempts.push_back(now);
        rung
    }
}

/// What the live loop must do for a failure the pipeline found (main.rs owns capture and the screen stream).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    RestartCapture(String),
    RestartScreen(String),
    RestartPipeline(String),
    /// Relaunch Heimdall; past the relaunch bound, `problem` shows red under `key` instead.
    Relaunch { key: String, part: Part, problem: String },
}

/// A microphone change worth a cue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicCue {
    /// "AirPods Pro → MacBook Pro Microphone", and back.
    Switched(String),
    /// No microphone can be opened (none chosen is connected and there is no built-in one, e.g. the lid is closed).
    Missing,
    /// A microphone is in use again after `Missing`.
    Found,
    /// An extra microphone the user switched on disconnected (uid, name): a pill until it returns.
    ExtraGone(String, String),
    ExtraBack(String, String),
}

/// How long the chosen microphone must stay in use before "back" is announced: a retry that stalls again within
/// capture's 3-second check never flickers the island.
pub const MIC_BACK_SECONDS: u64 = 5;

/// Tracks capture's microphone reports for the island: the switch to the built-in microphone when the chosen one
/// goes (unplugged, Bluetooth dropped, removed while recording, or failing), the switch back once it is stable,
/// and no microphone at all. The system default input never matters: Heimdall uses the user's choice, else built-in.
#[derive(Default, Debug)]
pub struct MicWatch {
    names: HashMap<String, String>,
    /// (chosen, fallback) once the switch to the fallback was announced.
    announced: Option<(String, String)>,
    back_since: Option<Instant>,
    missing: bool,
    /// Switched-on extra microphones that are not connected.
    extras_gone: std::collections::BTreeSet<String>,
}

impl MicWatch {
    pub fn update(&mut self, state: &capture::MicrophoneState, chosen_name: Option<&str>, now: Instant) -> Vec<MicCue> {
        for d in &state.devices {
            self.names.insert(d.uid.clone(), d.name.clone());
        }
        if let (Some(uid), Some(name)) = (&state.chosen, chosen_name) {
            self.names.entry(uid.clone()).or_insert_with(|| name.into());
        }
        let mut cues = vec![];
        // Extras the user switched on: gone while not connected, back when they return (or are switched off).
        let connected = |uid: &String| state.devices.iter().any(|d| &d.uid == uid);
        for uid in state.extras.iter().filter(|u| !connected(u) && state.chosen.as_ref() != Some(*u)) {
            if self.extras_gone.insert(uid.clone()) {
                cues.push(MicCue::ExtraGone(uid.clone(), self.name(uid)));
            }
        }
        let back: Vec<String> = self.extras_gone.iter().filter(|u| connected(u) || !state.extras.contains(u)).cloned().collect();
        for uid in back {
            self.extras_gone.remove(&uid);
            cues.push(MicCue::ExtraBack(uid.clone(), self.name(&uid)));
        }
        let Some(in_use) = state.in_use.clone() else {
            if !std::mem::replace(&mut self.missing, true) {
                cues.push(MicCue::Missing);
            }
            return cues;
        };
        if std::mem::replace(&mut self.missing, false) {
            cues.push(MicCue::Found);
        }
        match &state.chosen {
            // The built-in microphone is the choice: nothing to fall back from.
            None => {
                self.announced = None;
                self.back_since = None;
            }
            Some(chosen) if *chosen != in_use => {
                // A different choice than the one announced (the user picked another mic) starts over.
                if self.announced.as_ref().is_none_or(|(c, _)| c != chosen) {
                    cues.push(MicCue::Switched(format!("{} → {}", self.name(chosen), self.name(&in_use))));
                    self.announced = Some((chosen.clone(), in_use));
                }
                self.back_since = None;
            }
            Some(chosen) => {
                if self.announced.as_ref().is_some_and(|(c, _)| c == chosen) {
                    self.back_since.get_or_insert(now);
                } else {
                    self.announced = None;
                    self.back_since = None;
                }
            }
        }
        cues
    }

    /// "Back" once the chosen microphone has stayed in use for `MIC_BACK_SECONDS`.
    pub fn tick(&mut self, now: Instant) -> Option<MicCue> {
        let since = self.back_since?;
        if now.duration_since(since) < Duration::from_secs(MIC_BACK_SECONDS) {
            return None;
        }
        self.back_since = None;
        let (chosen, fallback) = self.announced.take()?;
        Some(MicCue::Switched(format!("{} → {}", self.name(&fallback), self.name(&chosen))))
    }

    /// Listening off: a later On starts fresh (the menu footer still says which mic is in use).
    pub fn reset(&mut self) {
        self.announced = None;
        self.back_since = None;
        self.missing = false;
        self.extras_gone.clear();
    }

    fn name(&self, uid: &str) -> String {
        self.names.get(uid).cloned().unwrap_or_else(|| "Microphone".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(uid: &str, name: &str, built_in: bool) -> capture::InputDevice {
        capture::InputDevice { uid: uid.into(), name: name.into(), built_in }
    }
    fn state(devices: Vec<capture::InputDevice>, chosen: Option<&str>, in_use: Option<&str>) -> capture::MicrophoneState {
        capture::MicrophoneState {
            devices,
            chosen: chosen.map(Into::into),
            in_use: in_use.map(Into::into),
            extras: vec![],
            open: in_use.into_iter().map(Into::into).collect(),
        }
    }

    #[test]
    fn each_change_is_sent_once_and_ok_only_follows_a_shown_failure() {
        let mut h = Health::default();
        assert!(h.recovered("listen", Part::Transcription, "Transcription OK").is_none(), "nothing failed");
        assert!(h.restarting("listen", Part::Transcription, "Restarting transcription…").is_some());
        assert!(h.restarting("listen", Part::Transcription, "Restarting transcription…").is_none());
        assert!(h.problem("listen", Part::Transcription, "Transcription stopped", "Click to restart Heimdall", HealthControl::Relaunch).is_some());
        assert!(h.problem("listen", Part::Transcription, "Transcription stopped", "Click to restart Heimdall", HealthControl::Relaunch).is_none());
        assert!(matches!(h.recovered("listen", Part::Transcription, "Transcription OK"), Some(HealthCue::Recovered { .. })));
        assert!(!h.holds("listen"));
        assert!(h.dropped("mic", Part::Microphone, "x").is_some());
        assert_eq!(h.clear("mic"), Some(HealthCue::Clear { key: "mic".into() }));
        assert_eq!(h.clear("mic"), None);
    }

    #[test]
    fn the_ladder_climbs_within_its_window_and_resets_after_it() {
        let mut l = Ladder::new(Duration::from_secs(600));
        let t = Instant::now();
        assert_eq!(l.climb(t), Rung::Part);
        assert_eq!(l.climb(t + Duration::from_secs(60)), Rung::Pipeline);
        assert_eq!(l.climb(t + Duration::from_secs(120)), Rung::Relaunch);
        assert_eq!(l.climb(t + Duration::from_secs(2000)), Rung::Part, "an old failure no longer counts");
    }

    /// Addendum: every disconnect path shows the move to the built-in microphone and back.
    #[test]
    fn mic_disconnect_paths_show_the_switch_and_the_way_back() {
        let builtin = || dev("BuiltIn", "MacBook Pro Microphone", true);
        let airpods = || dev("AirPods", "AirPods Pro", false);
        let t = Instant::now();
        let s = |n: u64| t + Duration::from_secs(n);
        let mut m = MicWatch::default();
        assert!(m.update(&state(vec![builtin(), airpods()], Some("AirPods"), Some("AirPods")), Some("AirPods Pro"), s(0)).is_empty());
        // Unplugged / Bluetooth dropped / removed while recording: the device leaves the list, capture uses built-in.
        let gone = m.update(&state(vec![builtin()], Some("AirPods"), Some("BuiltIn")), Some("AirPods Pro"), s(1));
        assert_eq!(gone, [MicCue::Switched("AirPods Pro → MacBook Pro Microphone".into())]);
        // The default input changing, or more reports, say nothing new.
        assert!(m.update(&state(vec![builtin(), dev("Zoom", "ZoomAudioDevice", false)], Some("AirPods"), Some("BuiltIn")), None, s(2)).is_empty());
        // Back: announced only once it stays.
        assert!(m.update(&state(vec![builtin(), airpods()], Some("AirPods"), Some("AirPods")), None, s(10)).is_empty());
        assert_eq!(m.tick(s(12)), None);
        assert_eq!(m.tick(s(10 + MIC_BACK_SECONDS)), Some(MicCue::Switched("MacBook Pro Microphone → AirPods Pro".into())));
        assert_eq!(m.tick(s(30)), None);
    }

    #[test]
    fn a_failing_mic_retried_by_capture_does_not_flicker_the_island() {
        let builtin = || dev("BuiltIn", "MacBook Pro Microphone", true);
        let airpods = || dev("AirPods", "AirPods Pro", false);
        let t = Instant::now();
        let s = |n: u64| t + Duration::from_secs(n);
        let mut m = MicWatch::default();
        m.update(&state(vec![builtin(), airpods()], Some("AirPods"), Some("AirPods")), None, s(0));
        let mut cues = vec![];
        // Stalled, retried at 2 s, stalled again after 3 s, retried at 4 s...: in use flips between the two.
        for (at, in_use) in [(3, "BuiltIn"), (5, "AirPods"), (8, "BuiltIn"), (12, "AirPods"), (15, "BuiltIn")] {
            cues.extend(m.update(&state(vec![builtin(), airpods()], Some("AirPods"), Some(in_use)), None, s(at)));
            cues.extend(m.tick(s(at + 1)));
        }
        assert_eq!(cues, [MicCue::Switched("AirPods Pro → MacBook Pro Microphone".into())]);
    }

    #[test]
    fn a_switched_on_extra_mic_that_disconnects_is_a_pill_until_it_returns() {
        let t = Instant::now();
        let builtin = || dev("BuiltIn", "MacBook Pro Microphone", true);
        let usb = || dev("USB", "Shure MV7", false);
        let with_extra = |devices: Vec<capture::InputDevice>| capture::MicrophoneState {
            devices,
            chosen: None,
            in_use: Some("BuiltIn".into()),
            extras: vec!["USB".into()],
            open: vec!["BuiltIn".into()],
        };
        let mut m = MicWatch::default();
        assert!(m.update(&with_extra(vec![builtin(), usb()]), None, t).is_empty());
        assert_eq!(m.update(&with_extra(vec![builtin()]), None, t), [MicCue::ExtraGone("USB".into(), "Shure MV7".into())]);
        assert!(m.update(&with_extra(vec![builtin()]), None, t).is_empty(), "said once");
        assert_eq!(m.update(&with_extra(vec![builtin(), usb()]), None, t), [MicCue::ExtraBack("USB".into(), "Shure MV7".into())]);
    }

    #[test]
    fn no_microphone_at_all_is_a_problem_until_one_returns_and_a_new_choice_starts_over() {
        let t = Instant::now();
        let mut m = MicWatch::default();
        // Built-in chosen, lid closed in clamshell mode: no built-in, nothing opened.
        assert_eq!(m.update(&state(vec![dev("Zoom", "ZoomAudioDevice", false)], None, None), None, t), [MicCue::Missing]);
        assert!(m.update(&state(vec![], None, None), None, t).is_empty());
        assert_eq!(m.update(&state(vec![dev("BuiltIn", "MacBook Pro Microphone", true)], None, Some("BuiltIn")), None, t), [MicCue::Found]);
        // The user chooses a mic that is not connected: the switch is announced once for that choice.
        let usb = m.update(&state(vec![dev("BuiltIn", "MacBook Pro Microphone", true)], Some("USB"), Some("BuiltIn")), Some("Shure MV7"), t);
        assert_eq!(usb, [MicCue::Switched("Shure MV7 → MacBook Pro Microphone".into())]);
    }
}
