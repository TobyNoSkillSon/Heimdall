//! Barge-in (decision 31, confirmed 20:34): while Heimdall's voice plays, speech that speaker ID
//! confidently matches to the user stops it at once. Other voices and Heimdall's own echo never do. No side
//! transcript and no decider question: "it would need a transcript; too slow". There is no "stop speaking" command.
//!
//! - **Arming.** the user's speech starts on the primary microphone while Heimdall is speaking (agent speech queued,
//!   synthesizing or playing, or a voice-bank line), or Heimdall starts speaking over his speech. Heimdall's own app
//!   audio and other microphones never arm it; neither does dictating to Vella.
//! - **The check.** From the onset, the microphone's last `heimdall_voice::WINDOW_NS` (1.5 s, the 300 ms before the
//!   onset included) is embedded by the speaker encoder every `heimdall_voice::STEP_NS` and scored against the user's
//!   profile and Heimdall's own voices (`heimdall_voice::user_verdict`). A window quieter than
//!   `heimdall_voice::SPEECH_RMS` is not scored. His voice in `heimdall_voice::USER_WINDOWS` windows in a row (two:
//!   about a second of his speech) stops Heimdall; anything
//!   else (Heimdall's echo, another person, no confident speaker, a failed check) keeps it talking, and the next window
//!   is checked while the speech goes on, so his voice starting over steady background talk still stops it.
//! - **Echo guard through speakers.** The microphone hears Heimdall's voice, so a window that scores as Heimdall's
//!   own voice is never his, and his score must beat Heimdall's by `heimdall_voice::USER_MARGIN`. Through headphones
//!   the same rule runs (there is just no echo to tell apart).
//! - **His own headset** (bug note 2026-11-13T13-23-33): on AirPods or Beats with Heimdall in the headphones, his bar
//!   is `heimdall_voice::HEADSET_USER_MIN` (0.50) instead of `USER_MIN`; nothing but him reaches that microphone, and
//!   the headset's voice processing lowers his short windows. Every check also scores him against the microphone's
//!   own set, when his print needed one there (`UserVoice::similarity_on`).
//! - **Before his voice is known** (fewer than `heimdall_voice::USER_MIN_SAMPLES` of his wakes): a window confidently
//!   not Heimdall's voice counts as his, so he can always stop it.
//! - **After his speech ends** the checks go on for `END_TAIL_NS` of audio past his last voice, so a short "stop, wait"
//!   is still judged (the first check is due 1.2 s after the onset, the second 0.5 s later). Until 11 Nov the speech
//!   detector reported his pause 1.5 s after his last voice and so gave every barge-in this tail; it now reports it
//!   from 300 ms (Smart Turn ends his turns early, crates/listen/README.md), and the tail keeps the old 1.5 s. A sound
//!   too short for two checks in a row (a click, under 0.2 s) never stops Heimdall. Speech starting again within the tail keeps the same barge-in. Speaking off and Listening off
//!   still stop Heimdall at once; they do not come through here.
//!
//! - **For the hold** (bug note 2026-11-07T04-31-47): while agent speech waits on an onset of his the hold has not
//!   confirmed (bridge/hold.rs `Heard::Unverified`), the same checks run (`armed_by` "hold_check" or the onset's) and
//!   each verdict goes to the hold: his voice skips the waiting line, another voice or Heimdall's echo lets it
//!   play (a quiet or unsure window decides nothing). Once the line is audible the checks go on as a barge-in.
//!
//! - **The phone's live microphone** (13 Nov, the user: "using an iPhone should feel exactly as using just a different
//!   microphone on the Mac"): an open phone source arms and checks exactly as the primary microphone does
//!   (`talk_mic`), against Heimdall's voice on the Mac or on the phone (`remote::phone_speaking`: the audio sent to
//!   the phone, its own `playback_start`/`playback_end`). Its echo: with the phone on its speaker the bridge zeroes
//!   the frames that carry Heimdall's voice and phone_mic.rs zeroes them again, so they are quiet windows, never
//!   scored; the Mac's speakers heard on the phone score as Heimdall's voice. A stop while the phone owns the output
//!   also stops the phone (`remote::talk_over_stop`: `speak_stop {reason:"talk_over"}`, the rest of the line unsent).
//!   It is not the Mac's primary microphone for anything else (the hold's speaker check, the voice print, health).
//!
//! Every checked window is journalled as `barge_in` evidence with its scores. The island shows nothing: the voice
//! stopping is its own feedback.
use super::{heimdall_voice, Pipeline};
use anyhow::Result;
use serde_json::json;
use std::time::Instant;

/// The policy each record names.
pub(super) const POLICY: &str = "user-voice-v1;no-transcript;no-decider";
/// Audio kept before the detected onset (the speech detector reports onsets slightly late).
const PREROLL_NS: u64 = 300_000_000;
/// After a stop, his continuing speech arms nothing for this long (Heimdall has stopped; a new reply may start).
const REST_NS: u64 = 1_000_000_000;
/// How long the checks go on after his last voice: the speech detector's old 1.5 s pause (see the module docs).
const END_TAIL_NS: u64 = 1_500_000_000;
/// How long a check may stay unanswered, in audio time (the helper's own deadline is 1.5 s).
const VOICE_ANSWER_NS: u64 = 3_000_000_000;

#[derive(Clone)]
pub(super) struct BargeIn {
    source: String,
    onset_ns: u64,
    /// Audio time of the next check.
    due_ns: u64,
    checks: u32,
    pending: Option<Pending>,
    armed_by: &'static str,
    /// Heimdall's voice is coming out of speakers (`output_route`), read when it armed.
    speakers: bool,
    /// The microphone's name: his score uses its own set (`heimdall_voice::UserVoice::similarity_on`).
    device: String,
    /// His bar for one window: `HEADSET_USER_MIN` on his own headset with Heimdall in the headphones, else `USER_MIN`.
    user_min: f32,
    /// The voice bank's epoch when it armed: an answer from before a newer reply is dropped.
    epoch: u64,
    /// Checks in a row that heard his voice (`heimdall_voice::USER_WINDOWS` stop Heimdall).
    streak: u32,
    /// Audio time the checks stop: `END_TAIL_NS` past his last voice once his speech ended; u64::MAX while he talks.
    ends_ns: u64,
}
#[derive(Clone, Copy)]
struct Pending {
    id: u64,
    start: u64,
    end: u64,
}

impl Pipeline {
    /// The user's speech began: arm a barge-in if Heimdall is speaking.
    pub(super) fn barge_speech_start(&mut self, source: &str, onset_ns: u64) -> Result<()> {
        // He talks again within the tail: the same barge-in goes on.
        if let Some(b) = self.barge.as_mut().filter(|b| b.source == source && b.due_ns != u64::MAX) {
            b.ends_ns = u64::MAX;
        }
        if !self.talk_mic(source) || self.barge.as_ref().is_some_and(|b| b.due_ns != u64::MAX) || !self.barge_wanted(source) {
            return Ok(());
        }
        // Talking to Vella is not talking over Heimdall (pipeline/vella.rs).
        if self.vella_dictating(source) {
            return Ok(());
        }
        self.arm_barge(source, onset_ns, "speech_start");
        self.barge_tick(source)
    }
    /// Heimdall is speaking (a barge-in), or agent speech waits on his unverified speech on `source` (the hold's
    /// speaker check, bug note 2026-11-07T04-31-47).
    fn barge_wanted(&self, source: &str) -> bool {
        self.speaking_for(source) || (self.tools.agent_speech_busy() && self.tools.owner.unverified(source))
    }
    /// The microphones the user talks over Heimdall on: the primary Mac microphone, and the phone's live microphone while
    /// its source is open (13 Nov).
    pub(super) fn talk_mic(&self, source: &str) -> bool {
        self.primary_mic(source) || self.sources.get(source).is_some_and(|i| matches!(i.kind, capture::SourceKind::Phone { .. }))
    }
    /// Heimdall's voice can be heard on `source` now: on the Mac, or, for the phone's live microphone, also on the phone
    /// while it owns the output (`remote::phone_speaking`).
    fn speaking_for(&self, source: &str) -> bool {
        self.tools.heimdall_speaking() || (self.phone_source(source) && crate::remote::phone_speaking())
    }
    /// Every live-loop tick: tell the hold which microphones the speaker check judges, so the listen worker's onset
    /// (reported before the pipeline's) is already unverified on them, not only from the pipeline's first onset on.
    pub(super) fn hold_verify_tick(&self) {
        let ready = self.voice_check_ready();
        for id in self.sources.keys().filter(|id| self.user_source(id)) {
            self.tools.owner.verify(id, ready && self.primary_mic(id));
        }
    }
    /// Tell the hold what a window said about the speech on `source`.
    fn barge_hold_verdict(&self, source: &str, who: heimdall_voice::Who) {
        use heimdall_voice::Who;
        let (owner, why) = match who {
            Who::User => (Some(true), "his voice by speaker check"),
            Who::Heimdall => (Some(false), "Heimdall's own voice by speaker check"),
            Who::Other => (Some(false), "another voice by speaker check"),
            // Unsure or no profile; a quiet window (pre-roll silence before a soft first word) is never asked.
            _ => (None, ""),
        };
        self.tools.owner.heard(source, owner, why);
    }
    fn arm_barge(&mut self, source: &str, onset_ns: u64, armed_by: &'static str) {
        let speakers = self.output_speakers();
        let device = self.sources.get(source).map(|i| i.label.clone()).unwrap_or_default();
        let user_min = if !speakers && heimdall_voice::headset_device(&device) { heimdall_voice::HEADSET_USER_MIN } else { heimdall_voice::USER_MIN };
        self.barge = Some(BargeIn {
            source: source.into(),
            onset_ns,
            due_ns: onset_ns + heimdall_voice::WINDOW_NS - PREROLL_NS,
            checks: 0,
            pending: None,
            armed_by,
            speakers,
            device,
            user_min,
            epoch: self.tools.voice_bank.epoch(),
            streak: 0,
            ends_ns: u64::MAX,
        });
    }
    /// Whether Heimdall's voice is coming out of speakers now (never in a replay: the route then is not the one
    /// recorded). Tests set it through their hooks.
    fn output_speakers(&self) -> bool {
        #[cfg(test)]
        return self.hooks.speakers;
        #[cfg(not(test))]
        return !self.replay && super::output_route::current() == super::output_route::Route::Speakers;
    }
    /// His speech ended at `end_ns` (his last voice): the checks go on through the tail (`barge_tick` ends it).
    pub(super) fn barge_speech_end(&mut self, source: &str, end_ns: u64) -> Result<()> {
        let Some(b) = self.barge.as_mut().filter(|b| b.source == source) else { return Ok(()) };
        b.ends_ns = end_ns.saturating_add(END_TAIL_NS);
        self.barge_tick(source)
    }
    /// The tail after his speech is over.
    fn barge_ended(&mut self, source: &str, end_ns: u64) -> Result<()> {
        let Some(b) = self.barge.as_ref().filter(|b| b.source == source) else { return Ok(()) };
        if b.checks == 0 && b.pending.is_none() {
            let b = self.barge.take().unwrap();
            return self.barge_record(&b, json!({"outcome":"too_short","speech_ms":(end_ns.saturating_sub(b.onset_ns)) as f64 / 1e6}));
        }
        // A check already asked still answers (his last words may be the ones that stop Heimdall); none follows it.
        if b.pending.is_none() {
            self.barge = None;
        } else {
            self.barge.as_mut().unwrap().due_ns = u64::MAX;
        }
        Ok(())
    }
    /// After each audio chunk: arm when Heimdall starts speaking over speech already going on, and check the newest
    /// window when it is due.
    pub(super) fn barge_audio(&mut self, source: &str) -> Result<()> {
        if self.barge.is_none() {
            if self.voice_warmed.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(60))
                && self.talk_mic(source)
                && self.speaking_for(source)
                && self.voice_check_ready()
            {
                // Heimdall is speaking: the encoder starts ahead of the first check (at most once a minute).
                self.voice_warmed = Some(Instant::now());
                self.warm_voice_check();
            }
            // Speech in progress as this thread has seen it (onset reported, no end yet), so a chunk that
            // arrives before its SpeechStart event never arms with a late onset.
            let talking = self.active.contains_key(source) && !self.quiet_since.contains_key(source);
            if !talking || !self.talk_mic(source) || !self.barge_wanted(source) {
                return Ok(());
            }
            let Some(now) = self.audio.get(source).map(|r| r.end()) else { return Ok(()) };
            if now < self.barge_rest_until || self.vella_dictating(source) {
                return Ok(());
            }
            let armed_by = if self.speaking_for(source) { "heimdall_started_over_speech" } else { "hold_check" };
            self.arm_barge(source, now, armed_by);
        }
        self.barge_tick(source)
    }
    fn barge_tick(&mut self, source: &str) -> Result<()> {
        // He started dictating to Vella after it armed: his words are Vella's, Heimdall talks on (pipeline/vella.rs).
        if self.barge.as_ref().is_some_and(|b| b.source == source) && self.vella_dictating(source) {
            let b = self.barge.take().unwrap();
            return self.barge_record(&b, json!({"outcome":"continued","reason":"dictated_to_vella"}));
        }
        // An answer that never came (the encoder's thread gone) must not leave the barge-in waiting for good: after
        // `VOICE_ANSWER_NS` of audio the window counts as unchecked and the next one is asked.
        if let Some(p) = self.barge.as_ref().filter(|b| b.source == source).and_then(|b| b.pending) {
            if self.audio.get(source).is_some_and(|r| r.end() >= p.end + VOICE_ANSWER_NS) {
                return self.barge_voice(p.id, Err("no answer".into()));
            }
        }
        if let Some(ends) = self.barge.as_ref().filter(|b| b.source == source && b.ends_ns != u64::MAX && b.due_ns != u64::MAX).map(|b| b.ends_ns) {
            if self.audio.get(source).is_some_and(|r| r.end() >= ends) {
                return self.barge_ended(source, ends - END_TAIL_NS);
            }
        }
        let Some(b) = self.barge.as_ref().filter(|b| b.source == source && b.pending.is_none()) else { return Ok(()) };
        let Some(ring) = self.audio.get(source) else { return Ok(()) };
        if ring.end() < b.due_ns {
            return Ok(());
        }
        if !self.barge_wanted(source) {
            let b = self.barge.take().unwrap();
            return if b.checks == 0 { self.barge_record(&b, json!({"outcome":"heimdall_finished"})) } else { Ok(()) };
        }
        let end = b.due_ns;
        let device = b.device.clone();
        let start = end.saturating_sub(heimdall_voice::WINDOW_NS).max(ring.start);
        let clip = ring.clip(start, end).ok().filter(|c| c.len() >= 8000);
        let id = self.next_side_id;
        self.next_side_id += 1;
        self.barge.as_mut().unwrap().pending = Some(Pending { id, start, end });
        if let Some(level) = clip.as_deref().map(heimdall_voice::rms).filter(|l| *l < heimdall_voice::SPEECH_RMS) {
            // Not speech: a quiet window scores like him on his own microphone, so it is never scored.
            return self.barge_quiet(id, level);
        }
        let asked = self.voice_check_ready() && clip.is_some_and(|samples| self.request_voice_check(id, samples, device));
        if !asked {
            // No check could run (no profile yet, the encoder down or its queue full): Heimdall keeps talking.
            let reason = if self.voice_check_ready() { "check not run" } else { "no profile or encoder" };
            return self.barge_voice(id, Err(reason.into()));
        }
        #[cfg(test)]
        while let Some((id, answer)) = self.hooks.voice_answers.pop_front() {
            self.barge_voice(id, answer)?;
        }
        Ok(())
    }
    /// A window too quiet to be speech: no score, Heimdall talks on, the streak restarts.
    fn barge_quiet(&mut self, id: u64, level: f32) -> Result<()> {
        let mut b = self.barge.take().unwrap();
        let p = b.pending.take().filter(|p| p.id == id).unwrap();
        b.checks += 1;
        b.streak = 0;
        let record = json!({"check":b.checks,"outcome":"continued","reason":"quiet","rms":level,
            "voice":{"window":{"start_ns":p.start,"end_ns":p.end}},"speech_ms":(p.end.saturating_sub(b.onset_ns)) as f64 / 1e6});
        self.barge_record(&b, record)?;
        if b.due_ns != u64::MAX {
            let newest = self.audio.get(&b.source).map_or(0, |r| r.end());
            b.due_ns = (p.end + heimdall_voice::STEP_NS).max(newest);
            self.barge = Some(b);
        }
        Ok(())
    }
    /// A check answered (`Ok(scores)`): the user's voice stops Heimdall now; anything else keeps it talking and the
    /// next window is checked half a second later while his speech goes on.
    pub(super) fn barge_voice(&mut self, id: u64, result: std::result::Result<heimdall_voice::Scores, String>) -> Result<()> {
        let Some(b) = self.barge.as_mut() else { return Ok(()) };
        let Some(p) = b.pending.filter(|p| p.id == id) else { return Ok(()) };
        if self.tools.voice_bank.epoch() != b.epoch {
            // A newer reply (or a stop) owns a different lifetime: this barge-in is over.
            self.barge = None;
            return Ok(());
        }
        b.pending = None;
        b.checks += 1;
        let (who, scores, error) = match &result {
            Ok(s) => (heimdall_voice::user_verdict_at(s, b.user_min), Some(s), None),
            Err(e) => (heimdall_voice::Who::Unsure, None, Some(e.as_str())),
        };
        let voice = json!({
            "verdict":who,"user_cosine":scores.and_then(|s| s.user),
            "heimdall_cosine":scores.and_then(|s| s.heimdall.as_ref().map(|h| h.1)),
            "closest_voice":scores.and_then(|s| s.heimdall.as_ref().map(|h| h.0.clone())),"error":error,
            "window":{"start_ns":p.start,"end_ns":p.end},
            "user_min":b.user_min,"user_margin":heimdall_voice::USER_MARGIN,
            "heimdall_min":heimdall_voice::HEIMDALL_MIN,"other_max":heimdall_voice::OTHER_MAX,
        });
        let mut b = self.barge.take().unwrap();
        b.streak = if who == heimdall_voice::Who::User { b.streak + 1 } else { 0 };
        let mut record = json!({"check":b.checks,"voice":voice,"streak":b.streak,"speech_ms":(p.end.saturating_sub(b.onset_ns)) as f64 / 1e6});
        // Read before the hold hears the verdict: a line his voice skips is not Heimdall speaking either.
        let speaking = self.speaking_for(&b.source);
        let holding = !speaking && self.barge_wanted(&b.source);
        if holding {
            if error.is_none() {
                self.barge_hold_verdict(&b.source, who);
            }
            // The hold's check: a waiting line, not Heimdall's voice, so nothing is stopped here.
            record["outcome"] = json!("hold_check");
            if self.barge_wanted(&b.source) && b.due_ns != u64::MAX {
                let newest = self.audio.get(&b.source).map_or(0, |r| r.end());
                b.due_ns = (p.end + heimdall_voice::STEP_NS).max(newest);
                self.barge_record(&b, record)?;
                self.barge = Some(b);
                return Ok(());
            }
            return self.barge_record(&b, record);
        }
        if !speaking {
            record["outcome"] = json!("heimdall_finished");
            return self.barge_record(&b, record);
        }
        if who == heimdall_voice::Who::User && b.streak >= heimdall_voice::USER_WINDOWS {
            // A verdict that arrives once he is dictating to Vella never stops Heimdall (pipeline/vella.rs).
            if self.vella_dictating(&b.source) {
                record["outcome"] = json!("continued");
                record["reason"] = json!("dictated_to_vella");
                return self.barge_record(&b, record);
            }
            record["outcome"] = json!("stopped");
            record["reason"] = json!("user_voice");
            // The user is talking to Heimdall: speech stops, and what he did not hear is kept for the agent (night pass V1).
            self.tools.talked_over();
            // 13 Nov: Heimdall's voice on the phone stops too (`speak_stop`), and the rest of its line is not sent.
            if crate::remote::phone_owns_output() {
                crate::remote::talk_over_stop();
                record["phone"] = json!("speak_stop");
            }
            self.barge_rest_until = p.end + REST_NS;
            if !self.replay {
                record["onset_to_stop_ms"] = json!(capture::host_now_ns().saturating_sub(b.onset_ns) as f64 / 1e6);
            }
            return self.barge_record(&b, record);
        }
        record["outcome"] = json!("continued");
        record["reason"] = json!(match (who, error) {
            (_, Some(_)) => "voice_check_failed",
            (heimdall_voice::Who::User, _) => "user_voice_once",
            (heimdall_voice::Who::Heimdall, _) => "voice_heimdall",
            (heimdall_voice::Who::Other, _) => "voice_other",
            (heimdall_voice::Who::NoProfile, _) => "no_profile",
            _ => "voice_unsure",
        });
        self.barge_record(&b, record)?;
        if b.due_ns != u64::MAX {
            // Never behind the newest audio: a slow or lost answer does not leave later checks reading the past.
            let newest = self.audio.get(&b.source).map_or(0, |r| r.end());
            b.due_ns = (p.end + heimdall_voice::STEP_NS).max(newest);
            self.barge = Some(b);
        }
        Ok(())
    }
    fn barge_record(&mut self, b: &BargeIn, mut record: serde_json::Value) -> Result<()> {
        record["schema"] = json!(2);
        record["policy"] = json!(POLICY);
        record["onset_ns"] = json!(b.onset_ns);
        record["output"] = json!(if b.speakers { "speakers" } else { "headphones" });
        record["armed_by"] = json!(b.armed_by);
        let ids = self.active.get(&b.source).map(|u| vec![format!("{}:{}", self.session.file_name().unwrap().to_string_lossy(), u.id)]).unwrap_or_default();
        self.index.evidence.event("barge_in", &b.source, &ids, b.onset_ns, record)
    }
    /// A gap, a closed source or a new primary microphone: forget an undecided barge-in on it.
    pub(super) fn reset_barge(&mut self, source: &str) {
        if self.barge.as_ref().is_some_and(|b| b.source == source) {
            self.barge = None;
        }
    }
}
