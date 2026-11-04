//! A typed message arriving (the Agent window's Send, the island's composer, a drop on the notch): the user, 30-31 Oct,
//! bug notes 2026-10-30T18-54-31 ("the same small ring as a voice wake, so he knows it arrived"),
//! 2026-10-30T18-57-22 ("a short spoken acknowledgement, the same way voice wakes do") and 2026-10-31T02-16-40 ("a
//! typed message should interrupt Heimdall's speech, the same way talking over it does").
//!
//! In order: Heimdall's speech stops (as `Stop` does), the wake sound plays (Wake Sound and its Bluetooth rule as for
//! a spoken wake), and once it has sounded a bank acknowledgement says the message arrived (Speaking on, Jarvis's
//! bank; skipped like any ack when the user is talking or a line already plays).
use super::*;

/// The ack waits for the wake sound to finish (Tink is 0.56 s) so the two never overlap.
pub(crate) const TYPED_ACK_AFTER_CUE: std::time::Duration = std::time::Duration::from_millis(650);
/// A seed for the ack's take, so two typed messages in a row do not repeat a take.
static TYPED_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Tools {
    pub(crate) fn typed_arrived(self: &Arc<Self>, text: &str) {
        // Cut by the user, as talking over it is (`talked_over` without claiming the turn): a line that never played is
        // told to the agent as unheard, never taken as said (review 1 Nov).
        self.speech_status();
        let jobs: Vec<u64> = self.speech_text.lock().unwrap().keys().copied().collect();
        self.cut_by_owner.lock().unwrap().extend(jobs);
        // Held lines stay: his message releases the newest (`Tools::stop_speech`).
        self.stop_speech();
        let epoch = self.voice_bank.epoch();
        let cued = self.speech_policy.wake_sent(capture::host_now_ns());
        let Some(clip) = self.typed_ack_clip(text) else { return };
        let tools = self.clone();
        let delay = if cued { TYPED_ACK_AFTER_CUE } else { std::time::Duration::ZERO };
        let spawned = std::thread::Builder::new().name("heimdall-typed-ack".into()).spawn(move || {
            std::thread::sleep(delay);
            // A Stop, another interruption or a later typed message meanwhile: no ack.
            if tools.voice_bank.epoch() != epoch { return; }
            let speakers = crate::pipeline::output_route::current() == crate::pipeline::output_route::Route::Speakers;
            if let Err(reason) = crate::voice_bank::play_ack(&tools, clip, "typed", speakers) {
                if !matches!(reason, "speaking_off" | "other_voice") { eprintln!("voice: typed message ack skipped: {reason}"); }
            }
        });
        if let Err(e) = spawned { eprintln!("voice: typed message ack not started: {e}"); }
    }
    /// One of the bank's ack takes for `text`'s language, or none (Speaking off, no bank, another voice chosen).
    fn typed_ack_clip(&self, text: &str) -> Option<i64> {
        if !self.enabled.load(Ordering::Acquire) || !self.speech_voices.bank_matches() {
            return None;
        }
        let (voice, generator) = self.bank_identity();
        let takes = self.voice_bank.store().and_then(|s| s.takes(&voice, generator)).ok()?;
        let recent: Vec<String> = self.voice_bank.recent_acks.lock().unwrap().iter().cloned().collect();
        let seed = TYPED_SEED.fetch_add(1, Ordering::Relaxed);
        let (ack, clip) = voicebank::acks::choose(voicebank::acks::language(text), voicebank::acks::pool_for(text), &takes, &recent, seed)?;
        let mut recent = self.voice_bank.recent_acks.lock().unwrap();
        recent.push_back(ack.id.clone());
        while recent.len() > 3 { recent.pop_front(); }
        Some(clip)
    }
}
