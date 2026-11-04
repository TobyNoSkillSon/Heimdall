//! One utterance heard on two microphones is one utterance (bug note 21 Oct 00:10:14). With the AirPods as the primary
//! and the Shure switched on beside it, both turned "Okay, I have both enabled right now…" into a final (00:10:05.132
//! and .305, spans overlapping by 94%), each final went to the decider, and each woke the agent: two deliveries, the
//! second restarting the turn with the line twice. Every microphone is the user's (`Pipeline::owner`), so the first final
//! wins and a later final from another microphone that covers the same time with the same words is archived with the
//! first one's id and never reaches the decider or the agent.
use super::*;

/// How long a final stays comparable after it ended.
const KEEP_NS: u64 = 30_000_000_000;
/// The finals kept for comparison.
const KEEP: usize = 16;

struct MicFinal {
    source: String,
    id: String,
    start: u64,
    end: u64,
    words: Vec<String>,
}

#[derive(Default)]
pub(crate) struct HeardTwice {
    recent: VecDeque<MicFinal>,
    /// Finals archived as another microphone's copy: an early send's later final does nothing more for them.
    pub(crate) copies: std::collections::HashSet<String>,
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Same speech: the spans overlap by at least half of the shorter one, and at least 80% of the shorter word list is
/// in the other (an early send carries only the words up to its trigger).
fn same_speech(a: &MicFinal, start: u64, end: u64, words: &[String]) -> bool {
    let overlap = a.end.min(end).saturating_sub(a.start.max(start));
    let shorter = (a.end.saturating_sub(a.start)).min(end.saturating_sub(start)).max(1);
    if overlap * 2 < shorter || a.words.is_empty() || words.is_empty() {
        return false;
    }
    let (small, large) = if a.words.len() <= words.len() { (&a.words[..], words) } else { (words, &a.words[..]) };
    let mut pool: Vec<&String> = large.iter().collect();
    let shared = small.iter().filter(|w| pool.iter().position(|p| p == w).map(|i| pool.swap_remove(i)).is_some()).count();
    shared * 5 >= small.len() * 4
}

impl HeardTwice {
    /// The id of the earlier final from another microphone this one repeats; otherwise it is remembered and `None`.
    pub(crate) fn check(&mut self, source: &str, id: &str, start: u64, end: u64, text: &str) -> Option<String> {
        while self.recent.front().is_some_and(|f| f.end.saturating_add(KEEP_NS) < start) || self.recent.len() >= KEEP {
            self.recent.pop_front();
        }
        let heard = words(text);
        if let Some(first) = self.recent.iter().find(|f| f.source != source && same_speech(f, start, end, &heard)) {
            self.copies.insert(id.to_owned());
            return Some(first.id.clone());
        }
        self.recent.push_back(MicFinal { source: source.into(), id: id.into(), start, end, words: heard });
        None
    }
}

impl Pipeline {
    /// Archives a microphone final that repeats another microphone's (`HeardTwice`) and reports whether it did.
    pub(super) fn archive_heard_twice(&mut self, final_: &AgentFinal) -> Result<bool> {
        if !self.owner(&final_.source) {
            return Ok(false);
        }
        let Some(first) = self.heard_twice.check(&final_.source, &final_.observation.id, final_.start, final_.end, &final_.observation.text) else {
            return Ok(false);
        };
        let mut record = serde_json::json!({"status":"heard_on_another_microphone","same_as":first,"given_to_agent":false,"would_wake":false});
        if let Some(e) = &final_.audio_error { record["audio_error"] = serde_json::json!(e); }
        self.decisions += 1; // A completed disposition, without inference.
        self.index.record(&final_.observation, &final_.source, final_.start, final_.end, &final_.words, &final_.notes, &record)?;
        crate::diagnostics::event_stream(&serde_json::json!({"event":"final","dry_run":self.settings.dry_run,"id":final_.observation.id,"source":final_.observation.source,"speaker":final_.observation.speaker,"text":final_.observation.text,"decision":record,"would_wake":false}));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;
    const LINE: &str = "Okay, I have both enabled right now. We have the um AirPods and we have the Shure microphone.";

    /// The 21 Oct 00:10:05 pair: Shure first, AirPods 173 ms later, same words, spans overlapping by 94%.
    #[test]
    fn the_same_line_on_two_microphones_is_one_utterance() {
        let mut h = HeardTwice::default();
        assert_eq!(h.check("mic:shure:32", "u393", 1_256_021_057_558_375, 1_256_031_937_558_375, LINE), None);
        assert_eq!(h.check("mic:airpods:26", "u394", 1_256_021_634_762_833, 1_256_033_874_762_833, LINE).as_deref(), Some("u393"));
        // A third microphone is the same utterance too.
        assert_eq!(h.check("mic:built-in", "u395", 1_256_021_100_000_000, 1_256_032_000_000_000, LINE).as_deref(), Some("u393"));
        assert!(h.copies.contains("u394") && h.copies.contains("u395") && !h.copies.contains("u393"));
    }

    /// The recognisers differ a little between microphones; an early send carries only the words up to its trigger.
    #[test]
    fn small_recognition_differences_and_an_early_partial_still_match() {
        let mut h = HeardTwice::default();
        assert_eq!(h.check("mic:a", "1", 10 * S, 20 * S, LINE), None);
        let variant = "Okay I have both enabled right now. We have the AirPods and we have the sure microphone.";
        assert_eq!(h.check("mic:b", "2", 10 * S + S / 2, 21 * S, variant).as_deref(), Some("1"));
        let mut h = HeardTwice::default();
        assert_eq!(h.check("mic:a", "1", 10 * S, 13 * S, "Heimdall, can you check"), None);
        assert_eq!(h.check("mic:b", "2", 10 * S, 16 * S, "Heimdall, can you check the log for me?").as_deref(), Some("1"));
    }

    /// Worst cases that must stay two utterances: the same microphone (the user repeating himself), different words at
    /// the same time, the same words at different times, and empty text.
    #[test]
    fn repeats_on_one_microphone_and_different_speech_stay_separate() {
        let mut h = HeardTwice::default();
        assert_eq!(h.check("mic:a", "1", 10 * S, 11 * S, "Heimdall?"), None);
        assert_eq!(h.check("mic:a", "2", 10 * S, 11 * S, "Heimdall?"), None, "the same microphone is never its own copy");
        assert_eq!(h.check("mic:b", "3", 10 * S, 14 * S, "what's the weather in Warsaw tomorrow"), None, "different words");
        assert_eq!(h.check("mic:b", "4", 40 * S, 41 * S, "Heimdall?"), None, "said again later");
        assert_eq!(h.check("mic:c", "5", 10 * S, 11 * S, ""), None, "no words");
        // Spans that barely touch are not the same speech.
        assert_eq!(h.check("mic:c", "6", 13 * S + S / 2, 20 * S, "what's the weather in Warsaw tomorrow"), None);
        assert!(h.copies.is_empty());
    }

    /// The comparison window is bounded: old finals leave it, and it never holds more than `KEEP`.
    #[test]
    fn old_finals_leave_the_window() {
        let mut h = HeardTwice::default();
        for i in 0..40u64 {
            assert_eq!(h.check("mic:a", &i.to_string(), i * 60 * S, i * 60 * S + S, &format!("line {i}")), None);
            assert!(h.recent.len() <= KEEP);
        }
        assert_eq!(h.recent.len(), 1, "everything older than 30 s after its end is gone");
    }
}
