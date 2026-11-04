//! When a drop onto the notch reaches the agent ("While I'm talking to you I can just drop it …
//! it will actually be sent to you at the end of my speech, so that you don't randomly get some image").
//!
//! - Dropped while his utterance is open (speech started on one of his microphones, its final not yet in, and not
//!   `END_WAIT` of quiet since it stopped), or his speech starts within `GRACE` after the drop: the drop is held and
//!   joins the turn made from that utterance — his words, then the drop's lines, its images attached — as one wake
//!   (`admit_agent_final`). It never arrives as a turn of its own in the middle of his speech: speech starting again
//!   re-arms a drop whose last utterance made no turn.
//! - No speech within `GRACE`: it goes alone, as his typed message ("the user dropped onto the notch: …"). The timer waits
//!   `DETECT_LAG` longer, as speech detection reports a start a little after it happened; starts are compared on the
//!   capture clock.
//! - A held drop whose utterance makes no turn for the agent (Heimdall's own echo, a final that stays archive-only)
//!   goes alone `AFTER_FINAL` after that final; speech that stops without a final (a cough, a noise with no words)
//!   releases it `END_WAIT` after it stopped; speech with no end at all, `SPEECH_LIMIT` after its last sign.
//! - A drop whose turn the agent never gets (a stop phrase, Agent off) stays as context with that turn's words, like a
//!   screenshot taken with them.
//!
//! The island's animation plays at the drop either way. Describing a drop (files read, images converted) runs on its
//! own thread; a turn that needs it first waits at most `DESCRIBE_WAIT`, then sends it by its paths alone.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::drops::Message;

/// How long a drop waits for the user to start speaking before it goes alone.
pub(crate) const GRACE: Duration = Duration::from_millis(3000);
/// Speech detection reports a start this much after it happened, at most: the alone timer waits this much longer.
pub(crate) const DETECT_LAG: Duration = Duration::from_millis(600);
/// A held drop whose utterance ended without becoming the agent's turn goes alone this long after its final.
pub(crate) const AFTER_FINAL: Duration = Duration::from_millis(2500);
/// Speech that stopped without a final (no words: a cough, typing) releases its drops this long after it stopped.
pub(crate) const END_WAIT: Duration = Duration::from_secs(4);
/// A held drop with speech that never ends goes alone this long after the last sign of speech.
pub(crate) const SPEECH_LIMIT: Duration = Duration::from_secs(30);
/// A turn or the timer waits at most this long for a drop's message to be written.
const DESCRIBE_WAIT: Duration = Duration::from_secs(3);

/// A drop's message: ready, or still being written on its own thread (with its paths-only stand-in).
#[derive(Debug)]
enum Body {
    Ready(Message),
    Pending(std::sync::mpsc::Receiver<Message>, Message),
}

impl Body {
    fn get(self) -> Message {
        match self {
            Body::Ready(m) => m,
            Body::Pending(rx, quick) => rx.recv_timeout(DESCRIBE_WAIT).unwrap_or(quick),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Wait {
    /// No speech yet: alone at `GRACE` (+ `DETECT_LAG`) after the drop.
    Grace,
    /// His utterance is open; `last` is its newest sign of speech.
    Speech { last: Instant },
    /// His utterance's final came at `at`; the turn it makes takes the drop.
    Final { at: Instant },
}

#[derive(Debug)]
struct Held {
    message: Body,
    at: Instant,
    wait: Wait,
}

/// One of his microphones: speaking now, and its last sign of speech.
#[derive(Clone, Copy, Debug)]
struct Mic {
    speaking: bool,
    last: Instant,
}

/// The drops waiting for the user's words, and his microphones' open utterances.
#[derive(Debug, Default)]
pub(crate) struct DropHold {
    held: Vec<Held>,
    open: HashMap<String, Mic>,
}

impl DropHold {
    /// A drop arrived.
    #[cfg(test)]
    pub(crate) fn dropped(&mut self, message: Message, now: Instant) {
        self.arrived(Body::Ready(message), now);
    }

    /// A drop arrived; its message comes on `rx` when written (`quick` if it never does).
    pub(crate) fn dropped_pending(&mut self, rx: std::sync::mpsc::Receiver<Message>, quick: Message, now: Instant) {
        self.arrived(Body::Pending(rx, quick), now);
    }

    fn arrived(&mut self, message: Body, now: Instant) {
        let wait = if self.is_open(now) { Wait::Speech { last: now } } else { Wait::Grace };
        self.held.push(Held { message, at: now, wait });
    }

    /// Whether an utterance of his is open at `now`.
    pub(crate) fn is_open(&self, now: Instant) -> bool {
        self.open.values().any(|m| {
            let quiet = now.saturating_duration_since(m.last);
            if m.speaking { quiet < SPEECH_LIMIT } else { quiet < END_WAIT }
        })
    }

    fn speaking(&self) -> bool {
        self.open.values().any(|m| m.speaking)
    }

    /// The user's speech started on `source` (one of his microphones) at `started` (capture time, on this clock).
    pub(crate) fn speech_start(&mut self, source: &str, started: Instant, now: Instant) {
        self.open.insert(source.to_owned(), Mic { speaking: true, last: now });
        for h in &mut self.held {
            match h.wait {
                // He starts within the grace after the drop.
                Wait::Grace if started >= h.at && started.saturating_duration_since(h.at) <= GRACE => h.wait = Wait::Speech { last: now },
                Wait::Speech { .. } => h.wait = Wait::Speech { last: now },
                // He speaks again before the turn of his last words arrived (or after they made none): his new words
                // take it. A start from before that final (a replayed event) changes nothing.
                Wait::Final { at } if started >= at => h.wait = Wait::Speech { last: now },
                _ => {}
            }
        }
    }

    /// More of his words (a partial, a segment): the held drops keep waiting.
    pub(crate) fn speech_continues(&mut self, source: &str, now: Instant) {
        let Some(m) = self.open.get_mut(source) else { return };
        m.last = now;
        for h in &mut self.held {
            if let Wait::Speech { .. } = h.wait {
                h.wait = Wait::Speech { last: now };
            }
        }
    }

    /// His speech stopped on `source` (its final may still come).
    pub(crate) fn speech_end(&mut self, source: &str, now: Instant) {
        if let Some(m) = self.open.get_mut(source) {
            *m = Mic { speaking: false, last: now };
        }
        for h in &mut self.held {
            if let Wait::Speech { .. } = h.wait {
                h.wait = Wait::Speech { last: now };
            }
        }
    }

    /// His utterance on `source` ended (its final). Drops waiting on speech now wait for the turn it makes.
    pub(crate) fn speech_final(&mut self, source: &str, now: Instant) {
        self.open.remove(source);
        if self.speaking() {
            return;
        }
        for h in &mut self.held {
            if let Wait::Speech { .. } = h.wait {
                h.wait = Wait::Final { at: now };
            }
        }
    }

    /// A microphone closed or lost audio: its utterance is over.
    pub(crate) fn forget(&mut self, source: &str) {
        self.open.remove(source);
    }

    /// The turn made from the user's utterance takes every drop that waited on his speech.
    pub(crate) fn take_for_turn(&mut self) -> Vec<Message> {
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held).into_iter().partition(|h| !matches!(h.wait, Wait::Grace));
        self.held = kept;
        taken.into_iter().map(|h| h.message.get()).collect()
    }

    /// The drops due to go alone at `now`.
    pub(crate) fn due(&mut self, now: Instant) -> Vec<Message> {
        let speaking = self.speaking();
        let (due, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held).into_iter().partition(|h| now >= Self::until(h, speaking));
        self.held = kept;
        due.into_iter().map(|h| h.message.get()).collect()
    }

    fn until(h: &Held, speaking: bool) -> Instant {
        match h.wait {
            Wait::Grace => h.at + GRACE + DETECT_LAG,
            Wait::Speech { last } => last + if speaking { SPEECH_LIMIT } else { END_WAIT },
            Wait::Final { at } => at + AFTER_FINAL,
        }
    }

    /// When the next drop is due to go alone.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let speaking = self.speaking();
        self.held.iter().map(|h| Self::until(h, speaking)).min()
    }

    /// Any drop waiting.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

impl super::Pipeline {
    /// The user dropped something onto the notch. Its time and his speech are taken now; its message (files read, images
    /// converted) is written on a thread of its own, so the conductor never waits on `sips` or a large screenshot.
    pub(super) fn dropped(&mut self, d: ui::drop::Dropped) -> Result<()> {
        let (kind, label) = ui::drop::summary(&d.items);
        let speaking = self.drops.is_open(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel();
        let quick = crate::drops::paths_only(&d);
        let items = d.items.len();
        std::thread::Builder::new().name("notch-drop".into()).spawn(move || {
            let _ = tx.send(crate::drops::describe(&d));
        })?;
        self.drops.dropped_pending(rx, quick, Instant::now());
        self.index.evidence.event("notch_drop", "notch", &[], capture::host_now_ns(), serde_json::json!({
            "label": label, "kind": kind.tag(), "items": items, "owner_speaking": speaking,
        }))?;
        Ok(())
    }

    /// When a held drop is next due (the live loop's timer).
    pub fn drop_deadline(&self) -> Option<Instant> {
        self.drops.deadline()
    }

    /// The timer: drops nobody's words took go alone.
    pub fn drop_tick(&mut self) -> Result<()> {
        self.drop_tick_at(Instant::now())
    }

    pub(super) fn drop_tick_at(&mut self, now: Instant) -> Result<()> {
        // Each is sent even when an earlier one failed; the first failure is reported.
        let mut first = Ok(());
        for m in self.drops.due(now) {
            let sent = self.send_drop_alone(m);
            if first.is_ok() {
                first = sent;
            }
        }
        first
    }

    /// His microphones' speech, for the drops waiting on it. Starts are placed at their capture time.
    pub(super) fn drop_listen(&mut self, e: &listen::ListenEvent) {
        use listen::ListenEvent as E;
        let source = match e {
            E::SpeechStart { source, .. } | E::SpeechEnd { source, .. } | E::PartialTranscript { source, .. } | E::TranscriptSegment { source, .. } | E::FinalUtterance { source, .. } | E::Discontinuity { source, .. } => source,
        };
        if !self.owner(source) {
            return;
        }
        let now = Instant::now();
        match e {
            E::SpeechStart { host_time_ns, .. } => {
                // Detection lags a start by well under a second; a larger gap is a clock this one cannot be compared
                // with (a replay's), and the start counts as now.
                let ago = Duration::from_nanos(capture::host_now_ns().saturating_sub(*host_time_ns));
                let ago = if ago <= DETECT_LAG * 3 { ago } else { Duration::ZERO };
                self.drops.speech_start(source, now.checked_sub(ago).unwrap_or(now), now)
            }
            E::SpeechEnd { .. } => self.drops.speech_end(source, now),
            E::FinalUtterance { .. } => self.drops.speech_final(source, now),
            E::Discontinuity { .. } => self.drops.forget(source),
            _ => self.drops.speech_continues(source, now),
        }
    }

    /// The user sent the open pull-down's composer with what he dropped onto it (30 Oct 03:00): his typed words with the
    /// drop's lines under them and its images, at once (he chose when by sending). The drop is described on its own
    /// thread as a notch drop is, and this waits for it as a turn waits for a held drop (at most `DESCRIBE_WAIT`, then
    /// by its paths alone), so the message keeps its place before anything he types next.
    pub(super) fn typed_with_drop(&mut self, text: String, d: ui::drop::Dropped) -> Result<()> {
        let (kind, label) = ui::drop::summary(&d.items);
        self.index.evidence.event("notch_drop", "notch", &[], capture::host_now_ns(), serde_json::json!({
            "label": label, "kind": kind.tag(), "items": d.items.len(), "composer": true, "typed": !text.trim().is_empty(),
        }))?;
        let (tx, rx) = std::sync::mpsc::channel();
        let quick = crate::drops::paths_only(&d);
        std::thread::Builder::new().name("composer-drop".into()).spawn(move || {
            let _ = tx.send(crate::drops::describe(&d));
        })?;
        self.send_typed_drop(&text, Body::Pending(rx, quick).get())
    }

    /// His typed message with a drop: one typed message with the drop's images, the path a notch drop alone takes (the
    /// relay holds and relays it the same way). His own words may change who Heimdall listens to, as typing does; the
    /// drop's never do.
    pub(super) fn send_typed_drop(&mut self, text: &str, m: Message) -> Result<()> {
        self.index.evidence.event("notch_drop_delivery", "notch", &[], capture::host_now_ns(), serde_json::json!({"label":m.label,"lines":m.lines,"images":m.images.len(),"joined":false,"typed":!text.trim().is_empty()}))?;
        self.ensure_agent()?;
        let Some(agent) = self.agent.as_ref() else { return Ok(()) };
        if !text.trim().is_empty() {
            self.tools.speech_policy.owner_woke(text.trim(), capture::host_now_ns());
        }
        agent.send(crate::bridge::Command::Dropped { text: m.with_typed(text), images: m.images })
    }

    /// A drop on its own: the user's typed message, through the same path as his typing, with its images.
    pub(super) fn send_drop_alone(&mut self, m: Message) -> Result<()> {
        self.index.evidence.event("notch_drop_delivery", "notch", &[], capture::host_now_ns(), serde_json::json!({"label":m.label,"lines":m.lines,"images":m.images.len(),"joined":false}))?;
        self.ensure_agent()?;
        let Some(agent) = self.agent.as_ref() else { return Ok(()) };
        agent.send(crate::bridge::Command::Dropped { text: m.alone(), images: m.images })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(label: &str) -> Message {
        Message { lines: vec![format!("- link: https://{label}")], images: vec![], label: label.into() }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn dropped_during_speech_joins_the_turn_at_its_end_and_never_goes_alone_meanwhile() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:built-in", t0, t0);
        h.dropped(msg("a"), t0 + ms(800));
        // He keeps talking well past the grace period: nothing is due.
        for k in 1..20 {
            h.speech_continues("mic:built-in", t0 + ms(800 + k * 500));
            assert!(h.due(t0 + ms(800 + k * 500)).is_empty(), "never alone mid-speech");
        }
        h.speech_final("mic:built-in", t0 + ms(11_000));
        assert!(h.due(t0 + ms(11_100)).is_empty());
        let taken = h.take_for_turn();
        assert_eq!(taken.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(h.is_empty() && h.deadline().is_none());
    }

    #[test]
    fn dropped_then_speaking_within_the_grace_joins_his_turn() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.dropped(msg("a"), t0);
        assert_eq!(h.deadline(), Some(t0 + GRACE + DETECT_LAG));
        h.speech_start("mic:built-in", t0 + ms(2_400), t0 + ms(2_400));
        // Past the grace: held for his words now.
        assert!(h.due(t0 + ms(3_500)).is_empty());
        h.speech_final("mic:built-in", t0 + ms(6_000));
        assert_eq!(h.take_for_turn().len(), 1);
    }

    #[test]
    fn dropped_with_no_speech_goes_alone_after_the_grace() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.dropped(msg("a"), t0);
        assert!(h.due(t0 + GRACE + DETECT_LAG - ms(1)).is_empty());
        // Speech starting after the grace is a new matter: the drop has gone.
        let due = h.due(t0 + GRACE + DETECT_LAG);
        assert_eq!(due.len(), 1);
        h.speech_start("mic:built-in", t0 + GRACE + DETECT_LAG + ms(100), t0 + GRACE + DETECT_LAG + ms(100));
        h.speech_final("mic:built-in", t0 + GRACE + DETECT_LAG + ms(900));
        assert!(h.take_for_turn().is_empty());
    }

    #[test]
    fn a_turn_takes_only_the_drops_that_waited_on_speech() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:a", t0, t0);
        h.dropped(msg("during"), t0 + ms(100));
        h.speech_final("mic:a", t0 + ms(1_000));
        h.dropped(msg("after"), t0 + ms(1_200));
        let taken = h.take_for_turn();
        assert_eq!(taken.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(), ["during"]);
        assert_eq!(h.due(t0 + ms(1_200) + GRACE + DETECT_LAG).len(), 1, "the later drop still goes alone");
    }

    #[test]
    fn an_utterance_that_makes_no_turn_releases_its_drop_and_endless_speech_too() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:a", t0, t0);
        h.dropped(msg("echo"), t0 + ms(100));
        h.speech_final("mic:a", t0 + ms(900));
        assert!(h.due(t0 + ms(900) + AFTER_FINAL - ms(1)).is_empty());
        assert_eq!(h.due(t0 + ms(900) + AFTER_FINAL).len(), 1);
        // A speech start whose final never comes.
        h.speech_start("mic:a", t0 + ms(5_000), t0 + ms(5_000));
        h.dropped(msg("stuck"), t0 + ms(5_100));
        assert!(h.due(t0 + ms(5_100) + SPEECH_LIMIT - ms(200)).is_empty());
        assert_eq!(h.due(t0 + ms(5_100) + SPEECH_LIMIT).len(), 1);
    }

    #[test]
    fn two_microphones_close_together() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:a", t0, t0);
        h.speech_start("mic:b", t0 + ms(20), t0 + ms(20));
        h.dropped(msg("x"), t0 + ms(100));
        h.speech_final("mic:a", t0 + ms(900));
        // The other microphone still hears him: still his open utterance.
        assert!(h.due(t0 + ms(900) + AFTER_FINAL).is_empty());
        h.speech_final("mic:b", t0 + ms(950));
        assert_eq!(h.take_for_turn().len(), 1);
    }

    #[test]
    fn speech_again_after_a_final_that_made_no_turn_keeps_the_drop_for_his_new_words() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:a", t0, t0);
        h.dropped(msg("x"), t0 + ms(100));
        // Heimdall's own echo: a final that makes no turn.
        h.speech_final("mic:a", t0 + ms(800));
        h.speech_start("mic:a", t0 + ms(1_500), t0 + ms(1_500));
        assert!(h.due(t0 + ms(800) + AFTER_FINAL + ms(100)).is_empty(), "never alone while he speaks again");
        h.speech_final("mic:a", t0 + ms(6_000));
        assert_eq!(h.take_for_turn().len(), 1);
        // A replayed start from before the final re-arms nothing.
        h.speech_start("mic:a", t0, t0);
        h.dropped(msg("y"), t0 + ms(7_000));
        h.speech_final("mic:a", t0 + ms(7_100));
        h.speech_start("mic:a", t0 + ms(6_900), t0 + ms(7_200));
        assert_eq!(h.due(t0 + ms(7_100) + AFTER_FINAL).len(), 1);
    }

    #[test]
    fn a_cough_without_words_releases_the_drop_soon() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:a", t0, t0);
        h.speech_end("mic:a", t0 + ms(300));
        // No final ever comes (no words). A drop now still counts it as open for a moment, then goes.
        h.dropped(msg("x"), t0 + ms(500));
        assert!(h.due(t0 + ms(500) + END_WAIT - ms(1)).is_empty());
        assert_eq!(h.due(t0 + ms(500) + END_WAIT).len(), 1);
        // Long after the cough, a new drop is not held for it.
        h.dropped(msg("y"), t0 + ms(10_000));
        assert_eq!(h.deadline(), Some(t0 + ms(10_000) + GRACE + DETECT_LAG));
    }

    #[test]
    fn a_start_reported_late_still_counts_from_when_he_started() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.dropped(msg("x"), t0);
        // He started at 2.8 s; detection reported it at 3.3 s, before the alone timer (3.6 s).
        h.speech_start("mic:a", t0 + ms(2_800), t0 + ms(3_300));
        assert!(h.due(t0 + GRACE + DETECT_LAG).is_empty());
        h.speech_final("mic:a", t0 + ms(5_000));
        assert_eq!(h.take_for_turn().len(), 1);
        // A start before the drop (speech already over) does not claim it.
        h.dropped(msg("y"), t0 + ms(20_000));
        h.speech_start("mic:a", t0 + ms(19_000), t0 + ms(20_100));
        assert_eq!(h.deadline(), Some(t0 + ms(20_000) + GRACE + DETECT_LAG));
    }

    #[test]
    fn a_microphone_that_closes_ends_its_utterance() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        h.speech_start("mic:a", t0, t0);
        h.forget("mic:a");
        h.dropped(msg("x"), t0 + ms(100));
        assert_eq!(h.deadline(), Some(t0 + ms(100) + GRACE + DETECT_LAG));
    }

    #[test]
    fn a_message_not_written_in_time_goes_by_its_paths() {
        let t0 = Instant::now();
        let mut h = DropHold::default();
        // Its thread ended without writing it.
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        h.dropped_pending(rx, msg("quick"), t0);
        let due = h.due(t0 + GRACE + DETECT_LAG);
        assert_eq!(due[0].label, "quick");
        // Written in time: the full message.
        let (tx, rx) = std::sync::mpsc::channel();
        h.dropped_pending(rx, msg("quick"), t0);
        tx.send(msg("full")).unwrap();
        assert_eq!(h.due(t0 + GRACE + DETECT_LAG)[0].label, "full");
    }
}
