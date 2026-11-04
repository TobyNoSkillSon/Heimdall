//! Instant acknowledgement: when the user's words go to the agent as a spoken wake (d1's send, his name,
//! "send it" or the follow-up window), Heimdall says a short recorded line in Jarvis's voice ("On it.", "Sprawdzam.")
//! once he has been quiet `Settings::ack_delay_ms` since his last voiced audio (2.5 s, "1.5 is way
//! too short"; A setting, live from settings.json; 21:44: 2 s, and the island's send arrow and
//! thinking dots wait for the same moment, `hold_island`; 22:25: no added wait, 0 by default, so both come as the wake
//! goes). The wake normally reaches this point about 1.6 s after his last word (the speech detector's 1.5 s pause),
//! so a longer delay waits out the rest on its own thread (`voice_bank::play_ack_after`) while the wake goes to the
//! agent at once: the agent is never held back for it.
//! - Before the reply: the agent's first sentence comes seconds after the wake and waits out the same pause, so the
//!   ack sounds first. If agent speech, the user talking again, a barge-in or Speaking off comes first, the ack is skipped
//!   (never played after the reply has begun). The wake's line says so: `[Heimdall is saying: "On it." at 16:04:11,
//!   unless the user talks first]`.
//!
//! - The line: `voicebank::acks` picks by the turn's language and words, varying lines; takes are Jarvis bank clips.
//! - Precise ("too aggressive and not very precise"): decider v3's line passes
//!   `voicebank::ack_gate`, so "Stopping now." needs a stop order and "On it." a request; a line that claims too much
//!   becomes "Got it.", a remark or greeting gets nothing. Fewer: when the recent replies' first speech came within
//!   `REPLY_SOON_MS` of the ack's moment (`latency::expected_reply_ms`), the reply answers by itself and no ack plays.
//!   No take in that language: nothing is said (never answer Polish in English).
//! - The agent is told: its wake carries `[Heimdall already said: "On it." at 16:04:10]` (`Tools::take_ack_line`),
//!   so it does not repeat it or open with a second acknowledgement (the user: "the agent should receive that the
//!   decider decided to do a quick acknowledgement, so the agent knows what's happening").
//! - Not the user: while the ack sounds (and its tail), a microphone utterance lying wholly inside it is Heimdall's own
//!   voice, archived only (bridge/speech_policy.rs `own_sound`), and it counts as Heimdall audible for the barge-in
//!   and the reply hold like any bank line. Its text is noted as spoken (the barge-in's own-voice check).
//! - Talked over: the user starting to talk stops it at once where his microphone cannot hear Heimdall (headphones, or
//!   echo cancellation active). Through plain speakers his onset cannot be told from the ack's own echo, so there it
//!   plays to its end (about a second) unless the barge-in stops it.
//! - Quiet: Speaking or Listening off, Agent off (dry run), another voice than Jarvis, or Heimdall already talking.
//!   On a call too: within `ACK_CALL_NS` of a call line, unless the user addressed Heimdall by name (bug note 22 Oct
//!   20:17: two false wakes while he talked with Discord friends, 3 minutes after their last line, each got an ack). The wake sound does not also play when an ack does.
use super::Pipeline;
use crate::voice_bank;
use serde_json::json;
use std::sync::atomic::Ordering;

/// How long after a call line acks stay quiet unless the user says Heimdall's name: a call has long pauses.
pub(super) const ACK_CALL_NS: u64 = 10 * 60 * 1_000_000_000;
/// A reply expected within this long after the ack's moment skips the ack ("fewer acks"): an ack
/// followed at once by the reply only adds noise. The ack's moment is the later of the delay and now (with no delay,
/// the wake's moment, about 1.6 s after his last word).
const REPLY_SOON_MS: u64 = 2_000;
/// How many recent acks the rule avoids repeating.
const RECENT: usize = 3;

// ---- Decider v3 (night 23 Oct): the decider picks the line.

/// Which acknowledgement a wake gets. With decider v3 (`decide::send_v3`) the decider chooses: the line it picked
/// with its send, or nothing (its plain send). His name, "send it" and the follow-up window wake before d1 answers;
/// d1 still scores the same words just after, and its verdict picks their line or nothing (`Later`, `late_ack`;
/// "sometimes you acknowledge, sometimes you don't"). With v2 the rule picks a line for every spoken
/// wake, as before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AckPlan {
    Rule,
    /// A voicebank ack id (`en_on_it`).
    Line(String),
    /// v3, a wake sent before d1 answered: the wake goes now without an ack or its sound, and d1's verdict on the
    /// same words decides (`Pipeline::late_ack`).
    Later,
    /// Nothing is said; the reason is journalled.
    Silent(&'static str),
}
impl Pipeline {
    /// Whether the loaded decider picks the ack line (v3 and later).
    pub(super) fn decider_picks_acks(&self) -> bool {
        #[cfg(test)]
        if let Some(v3) = self.hooks.decider_v3 {
            return v3;
        }
        decide::send_v3::Generation::shipped().picks_acks()
    }
    /// The plan for a wake: `decided` is the decider's verdict on these words (None: sent before it answered), and
    /// its line when it picked one.
    pub(super) fn ack_plan(&self, decided: Option<Option<&str>>) -> AckPlan {
        if !self.decider_picks_acks() {
            return AckPlan::Rule;
        }
        match decided {
            Some(Some(id)) => AckPlan::Line(id.to_owned()),
            Some(None) => AckPlan::Silent("decider_plain_send"),
            None => AckPlan::Later,
        }
    }
}

// ---- Decider v3, wakes sent before d1 answered.
//
// The wake goes at once with no ack and no wake sound. d1's verdict on the same words usually comes about 0.4 s later,
// well before the ack's moment (2 s of quiet; the wake leaves about 1.6 s after his last word). Its line then
// plays at that moment like any ack; its plain send, wait or not-for-Heimdall says nothing and the wake sound plays
// instead. The wake has already gone, so once the line actually plays the agent is told by a context note (no turn):
// `[Heimdall already said: "On it." at 16:04:11]`. The user talking again, a newer wake, or no verdict by the ack's
// moment (`LATE_GRACE_NS` at least): no line; the wake sound stands in unless he is talking.

/// The least time d1 gets after the wake before the wake sound stands in for its line.
const LATE_GRACE_NS: u64 = 500_000_000;
/// The context note's id prefix (`agent::notice::NoticeKind::classify`: context, no turn).
pub(crate) const LATE_NOTE: &str = "ack-said";

/// A wake sent before d1 answered, waiting for its verdict on the same words.
pub(crate) struct LateAck {
    through: u64,
    source: String,
    utterance: String,
    text: String,
    /// His last voice when the wake went (`quiet_since`): words since then make the line stale.
    voice: u64,
    /// Taken by whichever comes first: the verdict, or the wake sound's deadline.
    claimed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Pipeline {
    /// The wake for `through` went with `AckPlan::Later`: hold its sound until d1's verdict or the ack's moment.
    /// False when nothing is held (he is talking: the sound plays now, as before).
    pub(super) fn defer_ack(&mut self, source: &str, through: u64, utterance: &str, text: &str) -> bool {
        let Some(&voice) = self.quiet_since.get(source) else { return false };
        let now = self.ack_now();
        let wait = self.ack_quiet_ns().saturating_sub(now.saturating_sub(voice)).max(LATE_GRACE_NS);
        let claimed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (claim, tools) = (claimed.clone(), self.tools.clone());
        let spawned = std::thread::Builder::new().name("heimdall-ack-late".into()).spawn(move || {
            std::thread::sleep(std::time::Duration::from_nanos(wait));
            if !claim.swap(true, Ordering::AcqRel) && !tools.is_speaking() {
                tools.speech_policy.wake_sent(capture::host_now_ns());
            }
        });
        if spawned.is_err() {
            return false;
        }
        self.late_ack = Some(LateAck { through, source: source.into(), utterance: utterance.into(), text: text.into(), voice, claimed });
        true
    }
    /// d1's verdict on words already sent as a wake (`explicit`): `decided` as in `ack_plan`, None without a verdict.
    pub(super) fn late_ack(&mut self, through: u64, decided: Option<Option<&str>>) {
        let Some(late) = self.late_ack.take_if(|l| l.through <= through) else { return };
        if late.through != through {
            return; // an older wake's: its deadline has the sound
        }
        if late.claimed.swap(true, Ordering::AcqRel) {
            let _ = self.index.evidence.event("voice_ack", &late.source, std::slice::from_ref(&late.utterance), self.ack_now(),
                json!({"schema":1,"utterance_id":late.utterance,"source":late.source,"through":through,"late":true,
                    "outcome":"nothing","reason":"decider_too_late"}));
            return;
        }
        let talked = self.tools.is_speaking() || self.quiet_since.get(&late.source) != Some(&late.voice);
        let plan = if talked {
            AckPlan::Silent("owner_talked_again")
        } else if self.dispatched_through > through {
            AckPlan::Silent("newer_wake")
        } else {
            match decided {
                Some(Some(id)) => AckPlan::Line(id.to_owned()),
                Some(None) => AckPlan::Silent("decider_no_line"),
                None => AckPlan::Silent("no_decision"),
            }
        };
        let started = self.ack_with(&late.source, through, &late.utterance, &late.text, plan, true);
        // No line: the wake sound it held plays now, unless he is talking again.
        if !started && !talked && !self.settings.dry_run {
            self.tools.speech_policy.wake_sent(capture::host_now_ns());
        }
    }
}

impl Pipeline {
    /// Called as the user's words are handed to the agent as a wake, before the wake is sent. Whether an ack started.
    #[cfg(test)]
    pub(super) fn instant_ack(&mut self, source: &str, through: u64, utterance: &str, text: &str) -> bool {
        self.instant_ack_with(source, through, utterance, text, AckPlan::Rule)
    }
    /// `instant_ack` with the wake's plan (`AckPlan`).
    pub(super) fn instant_ack_with(&mut self, source: &str, through: u64, utterance: &str, text: &str, plan: AckPlan) -> bool {
        self.ack_with(source, through, utterance, text, plan, false)
    }
    /// `late`: the wake has gone (`late_ack`), so the agent learns of the line by a context note once it plays.
    fn ack_with(&mut self, source: &str, through: u64, utterance: &str, text: &str, plan: AckPlan, late: bool) -> bool {
        let now = self.ack_now();
        // Precision: the decider's line plays only when his words ask for what it claims; else the
        // neutral line or nothing (`voicebank::ack_gate`).
        let mut gated_from = None;
        let plan = match plan {
            AckPlan::Line(id) => match voicebank::ack_gate::gate(&id, text) {
                voicebank::ack_gate::Gated::Keep => AckPlan::Line(id),
                voicebank::ack_gate::Gated::Neutral(neutral) => {
                    gated_from = Some(id);
                    AckPlan::Line(neutral.to_owned())
                }
                voicebank::ack_gate::Gated::Skip(reason) => {
                    gated_from = Some(id);
                    AckPlan::Silent(reason)
                }
            },
            plan => plan,
        };
        // Fewer acks: when replies have been coming close behind the ack's moment, the reply answers by itself.
        let plan = match plan {
            AckPlan::Line(_) | AckPlan::Rule if self.reply_expected_soon(source) => AckPlan::Silent("reply_expected_soon"),
            plan => plan,
        };
        // A retired line the decider still knows plays the line that replaced it.
        let line = match &plan { AckPlan::Line(id) => voicebank::acks::resolve(id), _ => None };
        let lang = line.map_or_else(|| voicebank::acks::language(text), |a| a.lang.as_str());
        let pool = line.map_or_else(|| voicebank::acks::pool_for(text), |a| a.pool);
        let mut record = json!({"schema":1,"utterance_id":utterance,"source":source,"through":through,"language":lang,"pool":pool,
            "outcome":"nothing","reason":null});
        if late { record["late"] = json!(true); }
        if let Some(id) = gated_from { record["gated_from"] = json!(id); }
        let named = decide::send::addressed_by_name(text);
        let outcome = match &plan {
            AckPlan::Silent(reason) => Err(*reason),
            AckPlan::Later => Err(if self.settings.dry_run { "agent_off" } else { "awaiting_decider" }),
            AckPlan::Line(id) => {
                record["decider_line"] = json!(id);
                if line.is_some() { self.try_ack(source, through, utterance, lang, pool, now, named, &mut record, line, late) } else { Err("unknown_line") }
            }
            AckPlan::Rule => self.try_ack(source, through, utterance, lang, pool, now, named, &mut record, None, late),
        };
        match outcome {
            Ok(()) => record["outcome"] = json!(if record["wait_ms"].as_f64().unwrap_or(0.) > 0. { "waiting" } else { "playing" }),
            Err(reason) => record["reason"] = json!(reason),
        }
        if let Err(e) = self.index.evidence.event("voice_ack", source, &[utterance.to_owned()], now, record) {
            eprintln!("Ack evidence failed: {e:#}");
        }
        outcome.is_ok()
    }
    #[allow(clippy::too_many_arguments)]
    fn try_ack(&mut self, source: &str, through: u64, utterance: &str, lang: &str, pool: voicebank::acks::Pool, now: u64,
        named: bool, record: &mut serde_json::Value, line: Option<&'static voicebank::acks::Ack>, late: bool) -> Result<(), &'static str> {
        let tools = self.tools.clone();
        if self.settings.dry_run {
            return Err("agent_off");
        }
        if self.settings.paused || !self.settings.speak_replies || !tools.enabled.load(Ordering::Acquire) {
            return Err("speaking_off");
        }
        if !self.primary_mic(source) {
            return Err("not_primary_microphone");
        }
        if !named && tools.speech_policy.call_heard_within(now, ACK_CALL_NS) {
            return Err("in_conversation");
        }
        // Nobody talking now; the ack starts once he has been quiet `ack_delay_ms` (0 by default: now) since his last voiced audio.
        let Some(&last_voice) = self.quiet_since.get(source) else { return Err("owner_talking") };
        record["quiet_ms"] = json!(now.saturating_sub(last_voice) as f64 / 1e6);
        if tools.is_speaking() {
            return Err("owner_talking");
        }
        let wait = self.ack_quiet_ns().saturating_sub(now.saturating_sub(last_voice));
        record["wait_ms"] = json!(wait as f64 / 1e6);
        let (voice, generator) = tools.bank_identity();
        let bank = &tools.voice_bank;
        let takes = bank.store().and_then(|s| s.takes(&voice, generator)).map_err(|e| {
            eprintln!("Ack bank unavailable: {e:#}");
            "bank_unavailable"
        })?;
        let recent: Vec<String> = bank.recent_acks.lock().unwrap().iter().cloned().collect();
        let chosen = match line {
            // Decider v3's line: one of its takes in the turn's language, or nothing.
            Some(ack) => {
                let id = record["decider_line"].as_str().unwrap_or(&ack.id).to_owned();
                voicebank::acks::decider_takes(&id, &takes)
                    .map(|(ack, clips)| (ack, clips[((through / 7) % clips.len() as u64) as usize]))
            }
            None => voicebank::acks::choose(lang, pool, &takes, &recent, through),
        };
        let Some((ack, clip)) = chosen else {
            return Err("no_clip_for_language");
        };
        record["ack"] = json!(ack.id);
        record["text"] = json!(ack.text);
        record["clip_id"] = json!(clip);
        // Played now, the wake waits for it to start (tens of ms), so it only ever says what the user actually hears. Still
        // waiting, the wake says when it will be said and that the user talking first skips it (the wake usually takes its
        // line before the ack's moment); a skip withdraws the line while the wake has not taken it.
        let speakers = !self.ack_onset_is_owner();
        let at = agent::clock::local_hms(agent::now_ms() + wait / 1_000_000);
        let line = if wait == 0 {
            format!("[Heimdall already said: \"{}\" at {at}]", ack.text)
        } else {
            format!("[Heimdall is saying: \"{}\" at {at}, unless User talks first]", ack.text)
        };
        if !late {
            *tools.ack_said.lock().unwrap() = Some((through, line));
        }
        let said = tools.clone();
        let spoken = ack.text.clone();
        let withdraw = move |outcome: Result<(), &'static str>| {
            let reason = match outcome {
                Ok(()) if late => return said.context_note(LATE_NOTE, said_note(&spoken)),
                Ok(()) => return,
                Err(reason) => reason,
            };
            {
                let mut line = said.ack_said.lock().unwrap();
                if line.as_ref().is_some_and(|(seq, _)| *seq == through) {
                    line.take();
                }
            }
            // The wake went without its sound for this ack (review 23 Oct): the sound stands in, unless the user is
            // talking again, Heimdall is already speaking or was interrupted, or Speaking is off.
            if !matches!(reason, "owner_speaking" | "interrupted" | "speaking_off" | "agent_speech" | "line_playing") {
                said.speech_policy.wake_sent(capture::host_now_ns());
            }
        };
        match voice_bank::play_ack_after(&tools, clip, utterance, speakers, std::time::Duration::from_nanos(wait), withdraw) {
            Ok(true) => {
                if late && wait == 0 {
                    tools.context_note(LATE_NOTE, said_note(&ack.text));
                }
            }
            started => {
                // A late ack never set the line: the one there is a newer wake's.
                if !late {
                    tools.ack_said.lock().unwrap().take();
                }
                return Err(started.err().unwrap_or("not_started"));
            }
        }
        {
            let mut recent = bank.recent_acks.lock().unwrap();
            recent.push_back(ack.id.clone());
            while recent.len() > RECENT {
                recent.pop_front();
            }
        }
        Ok(())
    }
    /// The user started talking on `source`: an ack playing stops when his microphone cannot be hearing the ack itself.
    pub(super) fn ack_speech_start(&mut self, source: &str) {
        if !self.primary_mic(source) || !self.tools.voice_bank.ack_playing() {
            return;
        }
        if self.ack_onset_is_owner() && self.tools.voice_bank.stop_ack() {
            let _ = self.index.evidence.event("voice_ack", source, &[], self.ack_now(),
                json!({"schema":1,"outcome":"stopped","reason":"owner_talked_over"}));
        }
    }
    /// Through headphones (or with echo cancellation on) a speech onset during the ack is the user, not the ack.
    fn ack_onset_is_owner(&self) -> bool {
        #[cfg(test)]
        return !self.hooks.speakers;
        #[cfg(not(test))]
        return !self.replay && super::output_route::current() != super::output_route::Route::Speakers;
    }
    /// Words on the user's microphone that lie wholly inside Heimdall's own sound (the wake sound, an acknowledgement and
    /// its tail): that sound, not the user. Judged by the words' own times: the speech detector dates an onset up to a
    /// second early (its pre-roll), which would put Heimdall's echo before the sound that made it. Without words, the
    /// detector's span `from_ns..to_ns` (0: none).
    pub(super) fn heard_own_sound(&self, source: &str, words: &[super::Word], from_ns: u64, to_ns: u64) -> bool {
        let (from, to) = match (words.first(), words.last()) {
            (Some(first), Some(last)) => (first.start_ns, last.end_ns),
            _ if from_ns != 0 => (from_ns, to_ns),
            _ => return false,
        };
        self.owner(source) && (self.tools.speech_policy.cue_covers(from, to) || self.echo_verdict(source, words).is_some_and(|v| v.own()))
    }
    /// Words with Heimdall's own sound left out (each lying wholly inside it), for command matching: an echo of
    /// "Let me take a look." merged with the user's next words never becomes a screenshot.
    pub(super) fn without_own_sound(&self, source: &str, words: &[super::Word]) -> Vec<super::Word> {
        if !self.owner(source) {
            return words.to_vec();
        }
        let words = self.echo_verdict(source, words).map_or_else(|| words.to_vec(), |v| v.kept(words));
        words.into_iter().filter(|w| !self.tools.speech_policy.cue_covers(w.start_ns, w.end_ns)).collect()
    }
    /// Whether the next reply is likely to start within `REPLY_SOON_MS` of the ack's moment (the median of the last
    /// five replies' first speech, `latency::expected_reply_ms`; none before three replies). Both count from his last
    /// voice; the moment is the delay's end, or now when that has passed (with 0, the wake's own moment).
    fn reply_expected_soon(&self, source: &str) -> bool {
        #[cfg(test)]
        let expected = self.hooks.expected_reply_ms;
        #[cfg(not(test))]
        let expected = crate::latency::expected_reply_ms();
        let quiet_ms = self.quiet_since.get(source).map_or(0, |&voice| self.ack_now().saturating_sub(voice) / 1_000_000);
        let moment = u64::from(self.settings.ack_delay_ms).max(quiet_ms);
        expected.is_some_and(|ms| ms < moment + REPLY_SOON_MS)
    }
    /// The user's quiet before the ack and the island's arrow and dots (2.5 s; 1.5 s before, "way too
    /// short"; 26 Oct: a setting; 2 s; 22:25: 0 by default).
    fn ack_quiet_ns(&self) -> u64 {
        u64::from(self.settings.ack_delay_ms) * 1_000_000
    }
    fn ack_now(&self) -> u64 {
        #[cfg(test)]
        if let Some(now) = self.hooks.ack_now {
            return now;
        }
        capture::host_now_ns()
    }
}

// ---- The island waits for the same moment ("That is an animation, but also speech").
//
// A spoken wake's send arrow and thinking dots show when the ack would sound: once the user has been quiet `ack_delay_ms`
// since his last voiced audio (`ack_quiet_ns`, the one setting for both). He talks again before then: the island waits
// for his quiet again (the ack itself is skipped for that turn; the agent is still working, so the dots come back once
// he has been quiet the same delay). Once shown, his talking no longer hides them.

impl Pipeline {
    /// A spoken wake from the user on `source` is being handed over: hold the island until the ack's moment.
    pub(super) fn hold_island(&mut self, source: &str) {
        let until = match self.quiet_since.get(source) {
            Some(&voice) if !self.tools.is_speaking() => voice.saturating_add(self.ack_quiet_ns()),
            _ => u64::MAX,
        };
        self.island_hold = Some((source.into(), until));
        self.send_island_hold();
    }
    /// The user's voice on `source` started (`talking`) or ended with his last voice at `voice`: a hold not yet over
    /// waits for his quiet again.
    pub(super) fn island_hold_voice(&mut self, source: &str, talking: bool, voice: u64) {
        let now = self.ack_now();
        let quiet = self.ack_quiet_ns();
        let Some((held, until)) = &mut self.island_hold else { return };
        if *until <= now {
            self.island_hold = None;
            return;
        }
        if held != source {
            return;
        }
        *until = if talking { u64::MAX } else { voice.saturating_add(quiet) };
        self.send_island_hold();
    }
    fn send_island_hold(&mut self) {
        let Some((_, until)) = self.island_hold else { return };
        let now = self.ack_now();
        let ms = (until != u64::MAX).then(|| (until.saturating_sub(now) / 1_000_000).min(u64::from(u32::MAX)) as u32);
        if ms == Some(0) {
            self.island_hold = None;
        }
        self.ui(ui::UiUpdate::IslandHold(ms));
    }
}

/// The note for an ack the wake could not carry (`late_ack`), at the moment it started.
fn said_note(text: &str) -> String {
    format!("[Heimdall already said: \"{text}\" at {}]", agent::clock::local_hms(agent::now_ms()))
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::voice_bank::tests::{probe, wait_idle};
    use std::time::{Duration, Instant};

    fn rig() -> (tempfile::TempDir, Pipeline, String) {
        let t = tempfile::tempdir().unwrap();
        let mut a = Pipeline::new(t.path().join("data"), t.path().join("missing-models"), Settings::default(), None, true).unwrap();
        let mic = crate::fake_source("mic", 0, 1);
        let id = mic.id.0.clone();
        a.source(mic);
        a.audio.insert(id.clone(), AudioRing { start: 0, samples: vec![0.01; 16000 * 120].into() });
        a.tools.set_agent_enabled(true);
        a.settings.dry_run = false;
        a.settings.speak_replies = true;
        // The waits these scenarios drive; the default is no added wait (`the_ack_comes_with_the_wake_by_default`).
        a.settings.ack_delay_ms = 2_000;
        (t, a, id)
    }
    /// A Jarvis bank take for `line` of `samples` at 44.1 kHz.
    fn jarvis(a: &Pipeline, line: &str, text: &str, lang: &str, samples: usize) -> i64 {
        let mut settings = speak::drama3::settings();
        settings["asr"] = serde_json::json!({"passed":true});
        a.tools.voice_bank.store().unwrap().insert(&voicebank::ClipMeta { origin: voicebank::Origin::Bank, line_id: Some(line.into()),
            text: text.into(), language: lang.into(), voice: speak::drama3::VOICE.into(), generator: speak::drama3::GENERATOR.into(), settings },
            &speak::Audio { samples: vec![0.1; samples], sample_rate: 44100, first_audio_ms: 0., synthesis_ms: 0. }).unwrap().clip_id
    }
    fn acks(a: &Pipeline) -> Vec<serde_json::Value> {
        a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "voice_ack").map(|r| r.payload).collect()
    }
    /// The user's line on the microphone ended (last voice at `end`), and its wake is being handed over at `now`.
    fn quiet(a: &mut Pipeline, mic: &str, end: u64, now: u64) {
        a.quiet_since.insert(mic.into(), end);
        a.hooks.ack_now = Some(now);
    }

    #[test]
    fn ack_plays_after_quiet_in_the_turns_language_varies_and_tells_the_agent() {
        let (_t, mut a, mic) = rig();
        let on_it = jarvis(&a, "on_it", "On it.", "en", 900);
        jarvis(&a, "will_do", "Will do.", "en", 900);
        let pl = jarvis(&a, "pl_sprawdzam", "Sprawdzam.", "pl", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        quiet(&mut a, &mic, 10_000_000_000, 12_600_000_000);
        assert!(a.instant_ack(&mic, 3, "s:1", "Open Safari and start the timer."));
        wait_idle(&a.tools);
        let r = acks(&a);
        assert_eq!(r[0]["outcome"], "playing", "{r:#?}");
        assert_eq!(r[0]["language"], "en");
        let first = r[0]["clip_id"].as_i64().unwrap();
        // The wake for these words carries the line, once.
        let line = a.tools.take_ack_line(Some(3)).unwrap();
        assert!(line.starts_with("[Heimdall already said: \"") && line.contains(" at ") && line.ends_with(']'), "{line}");
        assert!(a.tools.take_ack_line(Some(3)).is_none(), "said once");
        assert!(a.tools.recent_speech().contains(r[0]["text"].as_str().unwrap()), "noted as Heimdall's words");
        // The next action ack is a different line.
        assert!(a.instant_ack(&mic, 4, "s:2", "Build it."));
        wait_idle(&a.tools);
        let second = acks(&a)[1]["clip_id"].as_i64().unwrap();
        assert_ne!(first, second, "varied");
        assert!([first, second].contains(&on_it));
        // Polish words: no ack (English-only acks), not even last night's Polish take, nor an English one.
        assert!(!a.instant_ack(&mic, 5, "s:3", "Czy build już przeszedł?"));
        wait_idle(&a.tools);
        let r = &acks(&a)[2];
        assert_eq!((r["outcome"].as_str(), r["reason"].as_str(), r["language"].as_str()), (Some("nothing"), Some("no_clip_for_language"), Some("pl")), "{r:#}");
        assert!(r["clip_id"].is_null() && r["clip_id"].as_i64() != Some(pl));
        assert_eq!(log.lock().unwrap().len(), 2, "two clips played, once each");
        let played: Vec<_> = a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "voice_ack_playback").collect();
        let _ = played; // journalled through the player's own connection when a session is attached
    }

    #[test]
    fn no_ack_before_quiet_while_talking_off_or_without_a_take_in_that_language() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        // He talks again.
        quiet(&mut a, &mic, 10_000_000_000, 12_600_000_000);
        a.tools.voice(&mic, true);
        assert!(!a.instant_ack(&mic, 1, "s:1", "Build it."));
        a.tools.voice(&mic, false);
        // Polish turn, only English takes: nothing.
        assert!(!a.instant_ack(&mic, 2, "s:2", "Zrób zrzut ekranu."));
        // Speaking off.
        a.settings.speak_replies = false;
        assert!(!a.instant_ack(&mic, 3, "s:3", "Build it."));
        a.settings.speak_replies = true;
        // Agent off (dry run).
        a.settings.dry_run = true;
        assert!(!a.instant_ack(&mic, 4, "s:4", "Build it."));
        let reasons: Vec<_> = acks(&a).iter().map(|r| r["reason"].as_str().unwrap().to_owned()).collect();
        assert_eq!(reasons, ["owner_talking", "no_clip_for_language", "speaking_off", "agent_off"]);
        assert!(log.lock().unwrap().is_empty(), "nothing played");
        assert!(a.tools.take_ack_line(Some(9)).is_none(), "and nothing told to the agent");
    }

    /// The ack waits 2.5 s of quiet, not 1.5 s (2 s). The wake reaches the ack
    /// about 1.6 s after his last word (measured on 22 Oct: 80 of 90 acks at 1.6–1.9 s), so it waits out the rest on its
    /// own; the wake is not held.
    #[test]
    fn the_ack_waits_out_its_quiet_without_holding_the_wake() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        // 1.7 s after his last voice: 300 ms to go.
        quiet(&mut a, &mic, 10_000_000_000, 11_700_000_000);
        let asked = Instant::now();
        assert!(a.instant_ack(&mic, 1, "s:1", "Build it."));
        assert!(asked.elapsed() < Duration::from_millis(250), "the wake goes at once: {:?}", asked.elapsed());
        assert!(log.lock().unwrap().is_empty() && a.tools.voice_bank.ack_waiting(), "not yet played");
        let r = acks(&a);
        assert_eq!(r[0]["outcome"], "waiting", "{r:#?}");
        assert!((r[0]["wait_ms"].as_f64().unwrap() - 300.).abs() < 1., "{r:#?}");
        let line = a.tools.ack_said.lock().unwrap().clone().unwrap().1;
        assert!(line.starts_with("[Heimdall is saying: \"On it.\" at "), "{line}");
        wait_idle(&a.tools);
        assert!(asked.elapsed() >= Duration::from_millis(300), "played at 2 s of quiet, not before");
        assert_eq!(log.lock().unwrap().len(), 1, "then played once");
        assert!(a.tools.take_ack_line(Some(1)).is_some(), "and the wake keeps its line");

        // 1.6 s: he talks again while it waits: never played, and the line is withdrawn while the wake has not taken it.
        quiet(&mut a, &mic, 20_000_000_000, 21_600_000_000);
        assert!(a.instant_ack(&mic, 2, "s:2", "Build it."));
        a.tools.voice(&mic, true);
        wait_idle(&a.tools);
        a.tools.voice(&mic, false);
        assert_eq!(log.lock().unwrap().len(), 1, "skipped");
        assert!(a.tools.take_ack_line(Some(2)).is_none(), "an ack that never played is not claimed");
    }

    /// The ack waits `Settings::ack_delay_ms` (2 s by default since; it was 2.5 s) after his last voiced audio, never
    /// less; his voice inside the wait cancels it for that turn; a reply that starts first wins. `quiet` is the clock.
    #[test]
    fn the_ack_delay_is_a_setting_and_his_voice_or_a_reply_inside_it_cancels_the_ack() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let reply = jarvis(&a, "reply", "The build passed.", "en", 900);
        // Each line sounds 50 ms, so the waiting ack sees the reply going (it looks every 10 ms).
        let log = probe(&a.tools, Duration::from_millis(50));
        assert_eq!(Settings::default().ack_delay_ms, 0, "no added wait by default");
        // 2 s of quiet already: it plays now.
        quiet(&mut a, &mic, 10_000_000_000, 12_000_000_000);
        assert!(a.instant_ack(&mic, 1, "s:1", "Build it."));
        wait_idle(&a.tools);
        let r = acks(&a);
        assert_eq!((r[0]["outcome"].as_str(), r[0]["wait_ms"].as_f64()), (Some("playing"), Some(0.)), "{r:#?}");
        // A millisecond short of 2 s: it waits that millisecond, never plays early.
        quiet(&mut a, &mic, 20_000_000_000, 21_999_000_000);
        assert!(a.instant_ack(&mic, 2, "s:2", "Build it."));
        let r = acks(&a);
        assert_eq!((r[1]["outcome"].as_str(), r[1]["wait_ms"].as_f64()), (Some("waiting"), Some(1.)), "{r:#?}");
        wait_idle(&a.tools);
        assert_eq!(log.lock().unwrap().len(), 2);

        // Set to 4 s: at 2.6 s of quiet it still waits 1.4 s.
        a.settings.ack_delay_ms = 4_000;
        quiet(&mut a, &mic, 30_000_000_000, 32_600_000_000);
        assert!(a.instant_ack(&mic, 3, "s:3", "Build it."));
        let r = acks(&a);
        assert_eq!((r[2]["outcome"].as_str(), r[2]["wait_ms"].as_f64()), (Some("waiting"), Some(1_400.)), "{r:#?}");
        assert!(a.tools.voice_bank.ack_waiting() && log.lock().unwrap().len() == 2, "not at 2 s any more");
        // He talks again inside it: cancelled, and it never comes back for this turn once he stops.
        a.tools.voice(&mic, true);
        wait_idle(&a.tools);
        a.tools.voice(&mic, false);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!a.tools.voice_bank.ack_waiting());
        assert_eq!(log.lock().unwrap().len(), 2, "never played");
        assert!(a.tools.take_ack_line(Some(3)).is_none(), "and not claimed to the agent");

        // The reply is ready before the wait ends: the reply plays, the ack is skipped.
        quiet(&mut a, &mic, 40_000_000_000, 42_600_000_000);
        assert!(a.instant_ack(&mic, 4, "s:4", "Build it."));
        crate::voice_bank::play(&a.tools, reply, a.tools.voice_bank.epoch(), "s:4").unwrap();
        wait_idle(&a.tools);
        assert_eq!(log.lock().unwrap().len(), 3, "only the reply played");
        assert!(a.tools.take_ack_line(Some(4)).is_none());
    }

    /// ("That is an animation, but also speech"): the island's send arrow and thinking dots wait for
    /// the ack's own moment, one setting (`ack_delay_ms`) for both. His voice before it holds the island until he has
    /// been quiet that long again; once shown, his talking no longer hides it.
    #[test]
    fn the_island_and_the_ack_wait_for_the_same_moment() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let _log = probe(&a.tools, Duration::from_millis(1));
        let holds = |a: &Pipeline| -> Vec<Option<u32>> {
            a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::IslandHold(ms) => Some(*ms), _ => None }).collect()
        };
        for (through, delay) in [(1, 2_000), (2, 3_000)] {
            a.settings.ack_delay_ms = delay;
            a.hooks.ui.lock().unwrap().clear();
            // The wake leaves 1.6 s after his last voice.
            quiet(&mut a, &mic, through * 10_000_000_000, through * 10_000_000_000 + 1_600_000_000);
            a.hold_island(&mic);
            assert!(a.instant_ack(&mic, through, &format!("s:{through}"), "Build it."));
            assert_eq!(acks(&a).last().unwrap()["wait_ms"].as_f64(), Some(f64::from(delay - 1_600)));
            assert_eq!(holds(&a), [Some(delay - 1_600)], "the island waits exactly as long as the ack");
            wait_idle(&a.tools);
        }
        // He talks again before the moment (3 s now): held while he talks, then until he has been quiet 3 s again.
        a.hooks.ui.lock().unwrap().clear();
        quiet(&mut a, &mic, 30_000_000_000, 31_600_000_000);
        a.hold_island(&mic);
        a.hooks.ack_now = Some(31_800_000_000);
        a.island_hold_voice(&mic, true, 31_800_000_000);
        a.hooks.ack_now = Some(34_000_000_000);
        a.island_hold_voice(&mic, false, 33_000_000_000);
        assert_eq!(holds(&a), [Some(1_400), None, Some(2_000)]);
        // Shown at 36 s: his voice no longer hides it.
        a.hooks.ack_now = Some(36_000_000_000);
        a.island_hold_voice(&mic, true, 36_000_000_000);
        assert_eq!(holds(&a).len(), 3);
        // Talking when the wake goes: held until his quiet.
        a.hooks.ui.lock().unwrap().clear();
        a.quiet_since.remove(&mic);
        a.hold_island(&mic);
        assert_eq!(holds(&a), [None]);
    }

    /// No added wait. With the default delay (0) the ack and the island come as the wake goes, about
    /// 1.6 s after his last voice; however long the delay, handing over the wake never waits for either (both return at
    /// once and the wait runs on the ack's own thread); his voice inside a delay still cancels the ack.
    #[test]
    fn the_ack_comes_with_the_wake_by_default_and_never_holds_the_wake() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        let holds = |a: &Pipeline| -> Vec<Option<u32>> {
            a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::IslandHold(ms) => Some(*ms), _ => None }).collect()
        };
        a.settings.ack_delay_ms = Settings::default().ack_delay_ms;
        quiet(&mut a, &mic, 10_000_000_000, 11_600_000_000);
        a.hold_island(&mic);
        assert!(a.instant_ack(&mic, 1, "s:1", "Build it."));
        assert_eq!(acks(&a)[0]["wait_ms"].as_f64(), Some(0.), "at the wake: no added wait");
        assert_eq!(holds(&a), [Some(0)], "the island shows at the wake too");
        wait_idle(&a.tools);
        assert_eq!(log.lock().unwrap().len(), 1);
        // The longest delay: the wake's side still returns at once.
        a.settings.ack_delay_ms = crate::settings::MAX_ACK_DELAY_MS;
        quiet(&mut a, &mic, 20_000_000_000, 21_600_000_000);
        let asked = Instant::now();
        a.hold_island(&mic);
        assert!(a.instant_ack(&mic, 2, "s:2", "Build it."));
        assert!(asked.elapsed() < Duration::from_millis(100), "the wake is never held for the ack: {:?}", asked.elapsed());
        assert_eq!(acks(&a)[1]["wait_ms"].as_f64(), Some(8_400.));
        a.tools.voice(&mic, true);
        wait_idle(&a.tools);
        a.tools.voice(&mic, false);
        assert_eq!(log.lock().unwrap().len(), 1, "his voice inside the delay cancelled it");
    }

    /// He told Heimdall to go easy on Opus and heard "Stopping now." The decider's line passes the
    /// precision gate (`voicebank::ack_gate`): a stop line without a stop order becomes the neutral "Got it.", a remark
    /// gets nothing, and a fitting line plays as picked.
    #[test]
    fn the_deciders_line_plays_only_when_his_words_ask_for_it() {
        let (_t, mut a, mic) = rig();
        a.hooks.decider_v3 = Some(true);
        jarvis(&a, "en_stopping_now", "Stopping now.", "en", 900);
        jarvis(&a, "en_got_it", "Got it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        quiet(&mut a, &mic, 10_000_000_000, 12_500_000_000);
        let line = |id: &str| super::AckPlan::Line(id.into());
        assert!(a.instant_ack_with(&mic, 1, "s:1", "By the way, go easy on Opus, okay? Not everything needs it.", line("en_okay_stopping")));
        wait_idle(&a.tools);
        assert!(!a.instant_ack_with(&mic, 2, "s:2", "I closed the website already.", line("en_stopping_now")));
        assert!(a.instant_ack_with(&mic, 3, "s:3", "Stop the download now.", line("en_stopping_now")));
        wait_idle(&a.tools);
        let r = acks(&a);
        assert_eq!((r[0]["text"].as_str(), r[0]["gated_from"].as_str()), (Some("Got it."), Some("en_okay_stopping")), "{r:#?}");
        assert_eq!((r[1]["reason"].as_str(), r[1]["gated_from"].as_str()), (Some("no_request"), Some("en_stopping_now")), "{r:#?}");
        assert_eq!((r[2]["text"].as_str(), r[2]["gated_from"].as_str()), (Some("Stopping now."), None), "{r:#?}");
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    /// Fewer acks. When recent replies start within 2 s of the ack's moment (the delay's end, or
    /// the wake's moment once that has passed), the reply answers by itself and the ack is skipped.
    #[test]
    fn no_ack_when_the_reply_is_expected_right_behind_it() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        quiet(&mut a, &mic, 10_000_000_000, 12_500_000_000);
        a.hooks.expected_reply_ms = Some(3_500);
        assert!(!a.instant_ack(&mic, 1, "s:1", "Build it."));
        assert_eq!(acks(&a)[0]["reason"], "reply_expected_soon");
        // 2.5 s of quiet already, past the 2 s delay: the bar is 2.5 + 2 s. Replies that take longer: the ack plays.
        a.hooks.expected_reply_ms = Some(4_400);
        assert!(!a.instant_ack(&mic, 2, "s:2", "Build it."));
        a.hooks.expected_reply_ms = Some(4_500);
        assert!(a.instant_ack(&mic, 3, "s:3", "Build it."));
        wait_idle(&a.tools);
        // No delay (the default), the wake 1.6 s after his last voice: the bar is 3.6 s.
        a.settings.ack_delay_ms = 0;
        quiet(&mut a, &mic, 15_000_000_000, 16_600_000_000);
        a.hooks.expected_reply_ms = Some(3_500);
        assert!(!a.instant_ack(&mic, 4, "s:4", "Build it."));
        a.hooks.expected_reply_ms = Some(3_600);
        assert!(a.instant_ack(&mic, 5, "s:5", "Build it."));
        wait_idle(&a.tools);
        // A longer delay moves the bar with it.
        a.settings.ack_delay_ms = 4_000;
        a.hooks.expected_reply_ms = Some(5_500);
        quiet(&mut a, &mic, 20_000_000_000, 24_000_000_000);
        assert!(!a.instant_ack(&mic, 6, "s:6", "Build it."));
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    /// Bug note 22 Oct 20:17: false wakes 3 minutes after a Discord line each got an ack. On a call acks stay quiet for
    /// `ACK_CALL_NS` after its last line, unless the user says Heimdall's name.
    #[test]
    fn on_a_call_the_ack_stays_quiet_unless_heimdall_is_named() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        a.tools.speech_policy.other_line(400_000_000_000);
        quiet(&mut a, &mic, 590_000_000_000, 592_600_000_000);
        assert!(!a.instant_ack(&mic, 1, "s:1", "This is so funny, the agent rebuilds itself."));
        assert_eq!(acks(&a)[0]["reason"], "in_conversation");
        assert!(a.instant_ack(&mic, 2, "s:2", "Heimdall, build it."), "named: the ack plays");
        wait_idle(&a.tools);
        // Ten minutes after the call's last line: acks again.
        quiet(&mut a, &mic, 1_010_000_000_000, 1_012_600_000_000);
        assert!(a.instant_ack(&mic, 3, "s:3", "Build it."));
        wait_idle(&a.tools);
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    /// Review 23 Oct: a waiting ack that is skipped or fails on the player thread before its first sample is withdrawn
    /// too, with its reason.
    #[test]
    fn a_waiting_ack_that_never_starts_is_withdrawn() {
        let (_t, a, _mic) = rig();
        let _log = probe(&a.tools, Duration::from_millis(1));
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = crate::voice_bank::play_ack_after(&a.tools, 9_999, "s:1", true, Duration::from_millis(50), move |r| {
            let _ = tx.send(r);
        });
        assert_eq!(waiting, Ok(true));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Err("not_started"));
        wait_idle(&a.tools);
    }

    /// Decider v3: a wake sent before d1 answered holds its sound; d1's line on the same words then waits
    /// out the rest of the 2 s of quiet, plays, and only then is the agent told, by a context note (the wake has
    /// gone), never through a later wake's line.
    #[test]
    fn a_late_line_waits_out_the_quiet_then_plays_and_tells_the_agent() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "en_on_it", "On it.", "en", 900);
        let log = probe(&a.tools, Duration::from_millis(1));
        let cue = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = cue.clone();
        a.tools.speech_policy.set_player(std::sync::Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); }));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *a.tools.agent_commands.lock().unwrap() = Some(tx);
        // The wake left 1.6 s after his last word; d1 answers at 1.7 s: 300 ms to go.
        quiet(&mut a, &mic, 10_000_000_000, 11_600_000_000);
        assert!(a.defer_ack(&mic, 3, "s:3", "Heimdall, build it."));
        a.hooks.ack_now = Some(11_700_000_000);
        let asked = Instant::now();
        a.late_ack(3, Some(Some("en_on_it")));
        let r = acks(&a);
        assert_eq!((r[0]["outcome"].as_str(), r[0]["late"].as_bool()), (Some("waiting"), Some(true)), "{r:#?}");
        assert!((r[0]["wait_ms"].as_f64().unwrap() - 300.).abs() < 1., "{r:#?}");
        assert!(a.tools.ack_said.lock().unwrap().is_none(), "the wake is gone: no wake line");
        assert!(rx.try_recv().is_err(), "nothing told before it plays");
        wait_idle(&a.tools);
        assert!(asked.elapsed() >= Duration::from_millis(300), "at 2 s of quiet");
        assert_eq!(log.lock().unwrap().len(), 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        let told = loop {
            if let Ok(crate::bridge::Command::Notice { id, text, .. }) = rx.try_recv() { break (id, text) }
            assert!(Instant::now() < deadline, "the agent was never told");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(told.0.starts_with("ack-said:") && told.1.starts_with("[Heimdall already said: \"On it.\" at "), "{told:?}");
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(cue.load(Ordering::SeqCst), 0, "the line stood for the wake sound");
    }

    /// Agent speech that is already going when the ack's moment comes wins: the ack never plays after the reply began.
    #[test]
    fn a_reply_already_speaking_skips_the_waiting_ack() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 900);
        let reply = jarvis(&a, "reply", "The build passed.", "en", 44100);
        let log = probe(&a.tools, Duration::from_millis(200));
        quiet(&mut a, &mic, 10_000_000_000, 11_800_000_000);
        assert!(a.instant_ack(&mic, 1, "s:1", "Build it."));
        // A bank reply line takes the voice first (the same lock agent speech holds).
        crate::voice_bank::play(&a.tools, reply, a.tools.voice_bank.epoch(), "s:1").unwrap();
        wait_idle(&a.tools);
        assert_eq!(log.lock().unwrap().len(), 1, "only the reply played");
        assert!(a.tools.take_ack_line(Some(1)).is_none());
    }

    #[test]
    fn talking_over_stops_the_ack_through_headphones_but_not_its_own_echo_through_speakers() {
        let (_t, mut a, mic) = rig();
        jarvis(&a, "on_it", "On it.", "en", 44100 * 3);
        // A slow output: the ack is still playing when he starts.
        let log = probe(&a.tools, Duration::from_millis(400));
        quiet(&mut a, &mic, 10_000_000_000, 12_600_000_000);
        a.hooks.speakers = true;
        assert!(a.instant_ack(&mic, 1, "s:1", "Build it."), "it returns once the ack is playing");
        assert!(a.tools.voice_bank.ack_playing());
        assert!(a.tools.speech_policy.last_own_sound().is_some(), "through speakers its span is Heimdall's");
        a.ack_speech_start(&mic);
        assert!(a.tools.voice_bank.ack_playing(), "through speakers the onset may be the ack itself");
        wait_idle(&a.tools);
        a.hooks.speakers = false;
        a.hooks.ack_now = Some(20_000_000_000);
        let first = a.tools.speech_policy.last_own_sound();
        assert!(a.instant_ack(&mic, 2, "s:2", "Build it."));
        assert_eq!(a.tools.speech_policy.last_own_sound(), first,
            "through headphones the ack is not on his microphone: his words over it stay his (re-review 22 Oct)");
        std::thread::sleep(Duration::from_millis(20));
        a.ack_speech_start(&mic);
        wait_idle(&a.tools);
        assert!(!a.tools.voice_bank.ack_playing());
        assert!(acks(&a).iter().any(|r| r["outcome"] == "stopped"), "{:#?}", acks(&a));
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    /// Review 22 Oct: an ack that never reaches the speaker is not claimed to the agent (the wake goes without its
    /// line, and so with its sound); a line left by a wake that never went out is never told to a later wake.
    #[test]
    fn an_ack_that_never_played_is_not_claimed_and_stale_lines_are_dropped() {
        let (_t, a, _mic) = rig();
        let _log = probe(&a.tools, Duration::from_millis(1));
        // A clip the store cannot read fails on the player thread, before any sample: the caller hears so at once.
        let asked = Instant::now();
        assert_eq!(crate::voice_bank::play_ack(&a.tools, 9_999, "s:1", true), Ok(false));
        assert!(asked.elapsed() < crate::voice_bank::ACK_START_WAIT, "a failure does not wait out the start wait");
        wait_idle(&a.tools);
        assert!(a.tools.recent_speech().is_empty(), "its words were never noted as Heimdall's");
        assert!(a.tools.speech_policy.last_own_sound().is_none(), "nor its span");
        // Stale: a line for wake 3 is dropped by wake 5, never carried into it.
        *a.tools.ack_said.lock().unwrap() = Some((3, "[old]".into()));
        assert!(a.tools.take_ack_line(Some(5)).is_none());
        assert!(a.tools.ack_said.lock().unwrap().is_none());
        // Its own wake takes it once.
        *a.tools.ack_said.lock().unwrap() = Some((6, "[new]".into()));
        assert!(a.tools.take_ack_line(Some(4)).is_none(), "an older wake does not take a newer line");
        assert_eq!(a.tools.take_ack_line(Some(6)).as_deref(), Some("[new]"));
    }

    #[test]
    fn the_acks_own_voice_on_the_microphone_is_heimdalls_not_owner() {
        let (_t, a, _mic) = rig();
        let p = &a.tools.speech_policy;
        p.own_sound(5_000_000_000, 6_400_000_000);
        assert!(p.cue_covers(5_100_000_000, 6_000_000_000), "the ack's echo lies wholly inside");
        assert!(!p.cue_covers(5_100_000_000, 7_000_000_000), "User talking past it is his");
        assert!(!p.cue_covers(4_000_000_000, 5_500_000_000), "speech that began before it is his");
    }
}
