//! Getting ready's last card: once every required row is green, the Start button gives way to the
//! phrase the user says, "Heimdall, show me what you can do", with a small level indicator that moves only while
//! sound comes in. Setup closes when the app hears it (`UiUpdate::SetupHeard`), and those words are the agent's first
//! message. Nothing to click: after ~8 s of silence a short hint, and listening goes on. Only a microphone that has
//! truly failed (no input, a capture that did not start) shows its reason with its fix and Start without speaking.

use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSAutoresizingMaskOptions as Mask, NSTextAlignment, NSTextField};
use objc2_foundation::MainThreadMarker;

use super::kit::*;
use super::super::util::{ns, rect};
use crate::model::*;

/// The words the card asks for, with the assistant's name (its wake word).
pub(crate) fn phrase(name: &str) -> String {
    format!("{name}, show me what you can do")
}

/// How long the card waits in silence before its hint.
pub(crate) const HINT_AFTER: Duration = Duration::from_secs(8);
/// A level (the meter's share, 0 to 1) that counts as sound arriving.
pub(crate) const HEARD: f64 = 0.2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CardState {
    /// Listening, nothing heard yet (or a moment ago).
    Waiting,
    /// Sound arriving now: the indicator moves.
    Hearing,
    /// Silence for `HINT_AFTER`: one short line, listening goes on.
    Hint,
    /// The microphone failed for real: its reason, its fix, and Start without speaking.
    Failed { why: String, fix: Option<ReadyFix> },
}

/// The Microphone row's failure that ends listening for now: no input, or a capture that did not start. Silence ("No
/// sound from …") is not one: the card keeps listening and hints.
pub(crate) fn mic_failure(readiness: &SetupReadiness) -> Option<(String, Option<ReadyFix>)> {
    let row = readiness.rows.iter().find(|r| r.key == "microphone")?;
    match &row.state {
        ReadyState::Problem { why, fix } if !why.starts_with("No sound from") => Some((why.clone(), fix.clone())),
        _ => None,
    }
}

/// Whether the card stands in for Start: every row green (Start's own rule), or every row but the microphone's and
/// listening's (which follows it) green while the microphone has failed.
pub(crate) fn card_shown(downloads: &ModelDownloads, readiness: &SetupReadiness) -> bool {
    if super::start_allowed(downloads, readiness) { return true; }
    if mic_failure(readiness).is_none() { return false; }
    let rest = SetupReadiness { rows: readiness.rows.iter().filter(|r| r.key != "microphone" && r.key != "listening").cloned().collect() };
    downloads.finished() && !rest.rows.is_empty() && rest.ready()
}

/// The card's state: a failed microphone first; then sound now; then the hint once silent long enough.
pub(crate) fn card_state(readiness: &SetupReadiness, hearing: bool, silent_for: Duration) -> CardState {
    if let Some((why, fix)) = mic_failure(readiness) { return CardState::Failed { why, fix }; }
    if hearing { CardState::Hearing } else if silent_for >= HINT_AFTER { CardState::Hint } else { CardState::Waiting }
}

/// The hint's line.
pub(crate) const HINT: &str = "Say it a little louder";
/// The button the failed state adds.
pub(crate) const START_SILENT: &str = "Start without speaking";

const CARD_H: f64 = 84.0;
/// The level indicator: five bars, 4 pt wide, up to 24 pt tall, before the phrase.
const BARS: usize = 5;
const BAR: (f64, f64) = (4.0, 24.0);
const BARS_W: f64 = BARS as f64 * BAR.0 + (BARS - 1) as f64 * 4.0;

pub(super) struct PhraseCard {
    pub band: Retained<Band>,
    pub card: Retained<Plate>,
    pub words: Retained<NSTextField>,
    pub line: Retained<NSTextField>,
    pub bars: Vec<Retained<Plate>>,
    pub fix: Retained<Pad>,
    pub silent: Retained<Pad>,
    /// The level shown (0 to 1), and the state shown.
    pub level: f64,
    pub state: Option<CardState>,
}

impl PhraseCard {
    pub(super) fn build(doc: &super::super::util::FlippedView, width: f64, target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let card = Plate::card(rect(0.0, 0.0, width, CARD_H), mtm);
        let bars: Vec<Retained<Plate>> = (0..BARS).map(|_| {
            let bar = Plate::new(rect(0.0, 0.0, BAR.0, BAR.0), 2.0, mtm);
            bar.set_fill(Some(hex(GREEN, 1.0)));
            card.addSubview(&bar);
            bar
        }).collect();
        let words = text(&card, "", &semibold(19.0), &t1(), 0.0, 0.0, 0.0, NSTextAlignment::Left, mtm);
        let line = text(&card, "", &regular(13.0), &t3(), 0.0, 0.0, 0.0, NSTextAlignment::Left, mtm);
        let fix = Pad::button("Retry", false, true, 0.0, target, sel!(setupPhraseFix:), mtm);
        let silent = Pad::button(START_SILENT, false, true, 0.0, target, sel!(setupStartSilent:), mtm);
        card.addSubview(&fix);
        card.addSubview(&silent);
        let band = Band::wrap(doc, &card, 0.0, width, super::SIDE, mtm);
        band.setHidden(true);
        Self { band, card, words, line, bars, fix, silent, level: 0.0, state: None }
    }

    /// The card at `y` in its page, saying `phrase` in `state`. Laid out by hand, centred in the card: the bars and the
    /// phrase on one line, the hint under them; a failed microphone's reason, then its fix and Start without speaking.
    pub(super) fn sync(&mut self, y: f64, phrase: &str, state: &CardState) {
        let f = self.band.frame();
        self.band.setFrameOrigin(objc2_foundation::NSPoint::new(f.origin.x, y));
        self.band.setHidden(false);
        let w = self.card.frame().size.width;
        let failed = matches!(state, CardState::Failed { .. });
        let set = |field: &NSTextField, s: &str| { field.setStringValue(&ns(s)); field.sizeToFit(); field.frame().size };
        let words = set(&self.words, &format!("\u{201c}{phrase}\u{201d}"));
        self.words.setHidden(failed);
        for b in &self.bars { b.setHidden(failed); }
        let (line, color) = match state {
            CardState::Hint => (HINT.to_string(), t2()),
            CardState::Failed { why, .. } => (why.clone(), hex(ORANGE, 1.0)),
            _ => (String::new(), t3()),
        };
        let lsize = set(&self.line, &line);
        self.line.setTextColor(Some(&*color));
        self.line.setHidden(line.is_empty());
        let fix = match state { CardState::Failed { fix, .. } => fix.clone(), _ => None };
        self.fix.setHidden(fix.is_none());
        if let Some(f) = &fix { self.fix.retitle(f.title(), 0.0); }
        self.silent.setHidden(!failed);
        if failed {
            // The reason, then the buttons under it, centred together.
            self.line.setFrameOrigin(objc2_foundation::NSPoint::new(((w - lsize.width) / 2.0).round(), 12.0));
            let shown: Vec<&Retained<Pad>> = [&self.fix, &self.silent].into_iter().filter(|b| !b.isHidden()).collect();
            let total: f64 = shown.iter().map(|b| b.frame().size.width).sum::<f64>() + 10.0 * (shown.len().saturating_sub(1)) as f64;
            let mut x = ((w - total) / 2.0).round();
            for b in shown { b.setFrameOrigin(objc2_foundation::NSPoint::new(x, 42.0)); x += b.frame().size.width + 10.0; }
        } else {
            let hint = !line.is_empty();
            let top = if hint { 14.0 } else { ((CARD_H - words.height) / 2.0).round() };
            let left = ((w - (BARS_W + 14.0 + words.width)) / 2.0).round();
            self.words.setFrameOrigin(objc2_foundation::NSPoint::new(left + BARS_W + 14.0, top));
            self.bars_at(left, top + (words.height / 2.0).round());
            self.line.setFrameOrigin(objc2_foundation::NSPoint::new(((w - lsize.width) / 2.0).round(), top + words.height + 8.0));
        }
        self.state = Some(state.clone());
        if !matches!(state, CardState::Hearing) { self.level = -1.0; self.set_level(0.0); }
    }

    pub(super) fn hide(&mut self) {
        self.band.setHidden(true);
        self.state = None;
    }

    /// Where the bars stand: from `left`, centred on `mid`.
    fn bars_at(&self, left: f64, mid: f64) {
        for (i, bar) in self.bars.iter().enumerate() {
            let f = bar.frame();
            bar.setFrame(rect(left + i as f64 * (BAR.0 + 4.0), mid - (f.size.height / 2.0).round(), BAR.0, f.size.height));
        }
    }

    /// The bars at `share` (0 to 1): a centre-high shape scaled by the level, resting as dots; a change too small to
    /// see draws nothing.
    pub(super) fn set_level(&mut self, share: f64) {
        if (share - self.level).abs() < 0.03 { return; }
        self.level = share;
        const SHAPE: [f64; BARS] = [0.45, 0.75, 1.0, 0.75, 0.45];
        for (bar, k) in self.bars.iter().zip(SHAPE) {
            let h = (BAR.0 + (BAR.1 - BAR.0) * share * k).round();
            let f = bar.frame();
            let mid = f.origin.y + f.size.height / 2.0;
            bar.setFrame(rect(f.origin.x, (mid - h / 2.0).round(), BAR.0, h));
        }
    }

    pub(super) fn height() -> f64 { CARD_H }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, state: ReadyState) -> ReadyRow {
        ReadyRow { key: key.into(), title: key.into(), detail: None, symbol: String::new(), state }
    }
    fn done() -> ModelDownloads {
        ModelDownloads { rows: vec![ModelDownload { title: "Parakeet".into(), state: DownloadState::Done, total: 1, done: 1, ..Default::default() }] }
    }

    /// The card stands in for Start once every row is green; silence hints after 8 s and listening
    /// goes on; only a failed microphone shows its reason, its fix and Start without speaking.
    #[test]
    fn wizard_the_phrase_card_waits_hears_hints_and_fails_only_for_a_dead_microphone() {
        let green = SetupReadiness { rows: vec![row("microphone", ReadyState::Ready), row("listening", ReadyState::Ready), row("agent", ReadyState::Ready)] };
        assert!(card_shown(&done(), &green));
        assert_eq!(phrase("Heimdall"), "Heimdall, show me what you can do");
        assert_eq!(card_state(&green, false, Duration::from_secs(2)), CardState::Waiting);
        assert_eq!(card_state(&green, true, Duration::from_secs(20)), CardState::Hearing);
        assert_eq!(card_state(&green, false, HINT_AFTER), CardState::Hint);
        let agent_waiting = SetupReadiness { rows: vec![row("microphone", ReadyState::Ready), row("agent", ReadyState::Working { words: "Asking…".into(), progress: None })] };
        assert!(!card_shown(&done(), &agent_waiting), "Start's rows first");
        // Silence from the microphone: still the card, hinting.
        let quiet = SetupReadiness { rows: vec![row("microphone", ReadyState::Problem { why: "No sound from BlackHole 2ch".into(), fix: None }), row("agent", ReadyState::Ready)] };
        assert!(!card_shown(&done(), &quiet), "a silent input is not a failed one: the rows say so");
        assert_eq!(card_state(&quiet, false, HINT_AFTER), CardState::Hint);
        // A capture that did not start: the card, failed, with Retry and Start without speaking.
        let retry = ReadyFix::Action { title: "Retry".into(), action: Box::new(UiAction::RetrySystemAudio) };
        let dead = SetupReadiness { rows: vec![
            row("microphone", ReadyState::Problem { why: "Couldn't start listening: the audio system did not answer".into(), fix: Some(retry.clone()) }),
            row("listening", ReadyState::Blocked { words: "Waits for the microphone".into() }),
            row("agent", ReadyState::Ready),
        ] };
        assert!(card_shown(&done(), &dead));
        assert_eq!(card_state(&dead, true, Duration::ZERO), CardState::Failed { why: "Couldn't start listening: the audio system did not answer".into(), fix: Some(retry) });
        assert!(!card_shown(&ModelDownloads::default(), &dead) || !cfg!(feature = "public"), "models first");
    }
}
