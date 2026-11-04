//! Who speaks on the user's microphone (after Heimdall's own reply came back through the
//! speakers labelled as him and woke the agent): "the mic should label speech as the user only when it actually matches my
//! voice signature, not everything from the Shure mic", and Heimdall's own voice "shouldn't be sent to you".
//!
//! - Each utterance on his microphone is embedded whole (at most six seconds) by the same
//!   encoder the barge-in uses, asked at its speech end so the answer is usually there when the final arrives; the
//!   final waits at most `WAIT` for it.
//! - The scores against his voice print (`UserVoice`) and Heimdall's own voices give the verdict
//!   (`heimdall_voice::mic_verdict`): `User`, `Heimdall` (archived as Heimdall's, never the decider's or the agent's),
//!   or `Unknown` (labelled "Unknown", context only: it never wakes, steers, sends early or grants `listen_to`).
//!   `Unknown` needs a safe call: a reliable utterance (1.5 s of speech, at speaking level) clearly unlike his print,
//!   or any line not his while Heimdall was audible through speakers. A short or unclear line stays his.
//! - A long utterance is judged on the six seconds holding most of its words, not its centre (a pause).
//! - When the echo guard kept some words of an utterance over Heimdall's echo, only those words' audio is judged, so
//!   his voice over the echo is still his; not confidently his while Heimdall spoke is Unknown (never a wake).
//! - Only the primary microphone is judged: his print and the thresholds were measured on it.
//! - No verdict (`Unverified`: no profile of his yet, the encoder down or late, too little audio) leaves his
//!   microphone his, as before; the text echo guard still applies. Every verdict is a `mic_voice` evidence record.
use super::heimdall_voice::{self, MicVoice, Scores};
use super::*;
use speakers::embedding::Embedding;
use std::time::Duration;

/// How long a final waits for its utterance's voice before it goes on unverified.
const WAIT: Duration = Duration::from_millis(700);
/// Around each kept word over the echo, so its edges are in the clip.
const KEPT_PAD_NS: u64 = 50_000_000;
/// How far the judged stretch may reach past the prefetched clip and still use its answer.
const PREFETCH_SLACK_NS: u64 = 400_000_000;
/// The encoder reads six seconds at most.
const WINDOW_NS: u64 = 6_000_000_000;
/// Verdicts and line voices kept (the early send and the final read the same verdict; the decider reads it later).
const KEPT: usize = 256;

/// Whose line an admitted turn is, for the decider's gate (send_worker.rs) and the wake triggers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Line {
    /// His microphone, with his voice (or no verdict).
    Owner,
    /// His microphone, another voice.
    Unknown,
    /// A call guest the user listens to (listen_to.rs): counts like his line.
    Guest(String),
}

/// One utterance's verdict.
#[derive(Clone, Debug)]
pub(super) struct Heard {
    pub(super) voice: MicVoice,
    pub(super) scores: Option<Scores>,
    pub(super) reason: &'static str,
}
impl Heard {
    fn unverified(reason: &'static str) -> Self {
        Self { voice: MicVoice::Unverified, scores: None, reason }
    }
}

#[derive(Default)]
pub(super) struct MicVoices {
    /// Asked at speech end, per source: the utterance's start, the clip's end and the encoder's answer.
    pending: HashMap<String, (u64, u64, crossbeam_channel::Receiver<Result<Embedding>>)>,
    /// Verdicts by (source, utterance start, judged stretch).
    verdicts: VecDeque<((String, u64, u64, u64), Heard)>,
    /// Admitted lines by observation id.
    lines: VecDeque<(String, Line)>,
}

/// The stretch of `words` to judge: all of them, or the six seconds holding the most of their time (a long line's
/// centre can be a pause: a 96-second wake on 30 Oct scored 0.04 against his print on its centre).
fn spoken_window(words: &[Word]) -> Option<(u64, u64)> {
    let (first, last) = (words.first()?, words.last()?);
    if last.end_ns.saturating_sub(first.start_ns) <= WINDOW_NS {
        return Some((first.start_ns, last.end_ns));
    }
    let covered = |from: u64| -> u64 { words.iter().map(|w| w.end_ns.min(from + WINDOW_NS).saturating_sub(w.start_ns.max(from))).sum() };
    let from = words.iter().map(|w| w.start_ns).max_by_key(|f| (covered(*f), std::cmp::Reverse(*f)))?;
    Some((from, from + WINDOW_NS))
}

impl Pipeline {
    /// His microphone's speech started: the encoder's helper starts now if it was idle, so its answer is ready at the
    /// final (a cold start takes longer than the final waits).
    pub(super) fn mic_voice_warm(&self, source: &str) {
        if !self.replay && self.primary_mic(source) && self.heimdall_voice.user.ready() {
            self.warm_voice_check();
        }
    }
    /// His microphone's speech ended: ask the encoder for the utterance's voice now, ahead of its final (an
    /// utterance of at most six seconds: a longer one is judged on its most spoken six seconds, known from its words).
    pub(super) fn mic_voice_prefetch(&mut self, source: &str, end_ns: u64) {
        if self.replay || !self.primary_mic(source) {
            return;
        }
        let Some(start) = self.active.get(source).map(|u| u.start) else { return };
        if end_ns.saturating_sub(start) > WINDOW_NS {
            return;
        }
        let Some(encoder) = self.heimdall_voice.encoder.clone().filter(|e| e.available()) else { return };
        let Some(audio) = self.audio.get(source).and_then(|r| r.clip(start, end_ns).ok()) else { return };
        if audio.len() < heimdall_voice::MIC_MIN_SAMPLES {
            return;
        }
        if let Some(rx) = encoder.identify(audio) {
            self.mic_voices.pending.insert(source.to_owned(), (start, end_ns, rx));
        }
    }
    /// The voice of the utterance on `source` that began at `start` and ended at `end` (host clock), with its words
    /// `all`. When the echo guard left some echo out (`echo`), only the words it kept are judged. Asked once per
    /// utterance and stretch.
    pub(super) fn mic_voice(&mut self, source: &str, utterance: &str, start: u64, end: u64, all: &[Word], echo: Option<&echo_guard::Verdict>) -> Heard {
        let stretch = echo.is_some_and(|v| v.any());
        let judged = match echo.filter(|v| v.any()) { Some(v) => v.kept(all), None => all.to_vec() };
        let (from, to) = spoken_window(&judged).unwrap_or((start, end));
        let key = (source.to_owned(), start, from, to);
        if let Some((_, h)) = self.mic_voices.verdicts.iter().find(|(k, _)| *k == key) {
            return h.clone();
        }
        let voiced_ns: u64 = judged.iter().map(|w| w.end_ns.min(to).saturating_sub(w.start_ns.max(from))).sum();
        // Over the echo: only his kept words' own audio, joined (the echo between them would pull it to Heimdall's).
        let clip = match self.audio.get(source) {
            Some(r) if stretch => {
                let parts: Vec<f32> = judged.iter().filter(|w| w.end_ns > from && w.start_ns < to)
                    .filter_map(|w| r.clip(w.start_ns.saturating_sub(KEPT_PAD_NS).max(from), (w.end_ns + KEPT_PAD_NS).min(to)).ok()).flatten().collect();
                (!parts.is_empty()).then_some(parts)
            }
            Some(r) => r.clip(from, to).ok(),
            None => None,
        };
        let level = clip.as_deref().map_or(0., heimdall_voice::rms);
        let reliable = voiced_ns >= heimdall_voice::MIC_RELIABLE_NS && level >= heimdall_voice::SPEECH_RMS;
        // Heimdall audible through speakers around it: no benefit of the doubt (its echo can be quiet and short).
        let audible = !self.tools.speech_policy.played_around(from, to).is_empty();
        // The answer asked at speech end, only if its clip covers what is judged now (a later part of the utterance,
        // or a forced final before the last speech end, is asked again).
        let pending = self.mic_voices.pending.remove(source);
        let prefetched = pending.filter(|(s, e, _)| !stretch && *s == start && from.saturating_sub(*s) <= PREFETCH_SLACK_NS && to <= e.saturating_add(PREFETCH_SLACK_NS)).map(|(_, _, rx)| rx);
        let heard = match self.mic_voice_scores(clip, prefetched) {
            Ok(scores) => Heard { voice: heimdall_voice::mic_verdict(&scores, reliable, audible), scores: Some(scores), reason: "scored" },
            Err(reason) => Heard::unverified(reason),
        };
        let mut record = serde_json::json!({"schema":1,"voice":heard.voice,"reason":heard.reason,"start_ns":from,"end_ns":to,
            "stretch":stretch,"voiced_s":voiced_ns as f64 / 1e9,"rms":level,"reliable":reliable,"heimdall_audible":audible,
            "user_ready":self.heimdall_voice.user.ready()});
        if let Some(s) = &heard.scores {
            record["user"] = serde_json::json!(s.user);
            record["heimdall"] = serde_json::json!(s.heimdall);
        }
        let _ = self.index.evidence.event("mic_voice", source, &[utterance.to_owned()], to, record);
        if matches!(heard.voice, MicVoice::Heimdall | MicVoice::Unknown) {
            let s = heard.scores.as_ref();
            eprintln!("voice: {} on the microphone (User {:.2}, Heimdall {:.2})", if heard.voice == MicVoice::Heimdall { "Heimdall's own voice" } else { "not User's voice" },
                s.and_then(|s| s.user).unwrap_or(f32::NAN), s.and_then(|s| s.heimdall.as_ref().map(|h| h.1)).unwrap_or(f32::NAN));
        }
        self.mic_voices.verdicts.push_back((key, heard.clone()));
        while self.mic_voices.verdicts.len() > KEPT {
            self.mic_voices.verdicts.pop_front();
        }
        heard
    }
    /// The judged audio's scores, or why there are none.
    fn mic_voice_scores(&mut self, clip: Option<Vec<f32>>, prefetched: Option<crossbeam_channel::Receiver<Result<Embedding>>>) -> std::result::Result<Scores, &'static str> {
        #[cfg(test)]
        {
            let _ = (clip, prefetched);
            let (user, heimdall) = self.hooks.mic_voice.pop_front().ok_or("no_check")?;
            Ok(Scores { user, heimdall: heimdall.map(|c| ("test".to_string(), c)) })
        }
        #[cfg(not(test))]
        {
            let encoder = self.heimdall_voice.encoder.clone().filter(|e| e.available()).ok_or("encoder_unavailable")?;
            let rx = match prefetched {
                Some(rx) => rx,
                None => {
                    let audio = clip.ok_or("no_audio")?;
                    if audio.len() < heimdall_voice::MIC_MIN_SAMPLES {
                        return Err("too_short");
                    }
                    encoder.identify(audio).ok_or("encoder_busy")?
                }
            };
            match rx.recv_timeout(WAIT) {
                Ok(Ok(e)) => Ok(self.heimdall_voice.scores(&e)),
                Ok(Err(_)) => Err("encoder_error"),
                Err(_) => Err("late"),
            }
        }
    }
    pub(super) fn note_line_voice(&mut self, id: &str, line: Line) {
        self.mic_voices.lines.retain(|(k, _)| k != id);
        self.mic_voices.lines.push_back((id.to_owned(), line));
        while self.mic_voices.lines.len() > KEPT {
            self.mic_voices.lines.pop_front();
        }
    }
    pub(super) fn line_voice(&self, id: &str) -> Option<&Line> {
        self.mic_voices.lines.iter().rev().find(|(k, _)| k == id).map(|(_, l)| l)
    }
    /// The line `id` on `source` is the user's own: his microphone, and not a voice that is not his.
    pub(super) fn owner_line(&self, source: &str, id: &str) -> bool {
        self.owner(source) && self.line_voice(id) != Some(&Line::Unknown)
    }
    /// The line counts like his for waking Heimdall: his own, or a listened guest's (listen_to.rs).
    pub(super) fn like_owner(&self, source: &str, id: &str) -> bool {
        self.owner_line(source, id) || matches!(self.line_voice(id), Some(Line::Guest(_)))
    }
}
