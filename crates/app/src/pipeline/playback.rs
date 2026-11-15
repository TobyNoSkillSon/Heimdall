//! Playback from the Mac's own output is not the user (bug note 2026-11-12T12-58-51): a YouTube narrator playing from the
//! Mac's speakers ("freedom we have, and the more people who are engaged in democracy…", 12 Nov 12:58:41) came back
//! through the Shure as "Mic · the user" and woke the agent. The browser's own audio stream had the same words a moment
//! earlier ("Hey internets…", archived as `audio`). Its voice check had no verdict (the line ran over Heimdall's
//! reply, and the clip of his kept words came back empty), and no verdict leaves his microphone his.
//!
//! Two rules, both from the apps' own audio (every recorded app but Heimdall's own output, calls included):
//! - **Same words.** A line on any Mac microphone whose words are mostly (`COVERED`) inside runs of `RUN` or more
//!   words in a row that an app said around the same time (`PAD_NS`) is that app's playback: "Unknown", context only,
//!   never a wake. A line that addresses Heimdall by name where the app did not stays his, and so does a line his
//!   print verified as his (his own voice can come back in a call's audio).
//! - **Not his voice while an app talks** (heimdall_voice.rs `app_audible_verdict`): a line clearly unlike his print
//!   is not given the benefit of the doubt while an app was talking around it, even when quiet.
//!
//! Measured on the archive (read-only copy of 12 Nov 18:52; the same rule over archived lines, app words of the lines
//! within a second of each microphone line): of 17,647 lines on his microphones since 20 Oct, 12,067 while an app
//! spoke, 22 labelled the user fall under the same-words rule. 21 read as playback (Discord friends and browser videos
//! through the speakers); one ("Heimdall, can you give me the model?" over a friend's "Can you give me the model for
//! that?") is his and is kept by the name rule. 15 more were a narrator labelled Heimdall's own voice (his voices
//! score 0.53-0.62 against it); they become "Unknown" too. Playback lines with a voice score (scores are kept since 11
//! Nov) scored 0.15-0.29 against his print (his own lines: median 0.80, 5th percentile 0.60) but were quiet, below
//! `SPEECH_RMS`, so they had no verdict.
use super::*;

/// How long an app's words stay comparable after they ended.
const KEEP_NS: u64 = 60_000_000_000;
/// App finals kept.
const KEEP: usize = 64;
/// Around the line: app words this close to it count (the two streams' clocks and recognisers differ a little).
pub(super) const PAD_NS: u64 = 1_000_000_000;
/// Words in a row that make a run.
pub(super) const RUN: usize = 3;
/// A line needs this many words to be called playback.
pub(super) const MIN_WORDS: usize = 6;
/// Share of the line's words inside runs.
pub(super) const COVERED: f32 = 0.8;

#[derive(Default)]
pub(super) struct Playback {
    /// Recent app finals: source and words (host clock).
    recent: VecDeque<(String, Vec<Word>)>,
}
impl Playback {
    pub(super) fn heard(&mut self, source: &str, words: &[Word]) {
        let Some(end) = words.iter().map(|w| w.end_ns).max() else { return };
        while self.recent.front().is_some_and(|(_, w)| w.last().is_some_and(|l| l.end_ns.saturating_add(KEEP_NS) < end)) || self.recent.len() >= KEEP {
            self.recent.pop_front();
        }
        self.recent.push_back((source.to_owned(), words.to_vec()));
    }
    fn words(&self) -> impl Iterator<Item = &Word> {
        self.recent.iter().flat_map(|(_, w)| w.iter())
    }
}

/// Lowercase word tokens of `words` (a recogniser's word can carry punctuation).
pub(super) fn tokens<'a>(words: impl IntoIterator<Item = &'a Word>) -> Vec<String> {
    words
        .into_iter()
        .flat_map(|w| w.text.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|t| !t.is_empty()).map(str::to_lowercase).collect::<Vec<_>>())
        .collect()
}

/// Which of `line`'s tokens lie in runs of at least `RUN` tokens in a row that `played` also has in a row.
pub(super) fn covered(line: &[String], played: &[String]) -> Vec<bool> {
    let mut mask = vec![false; line.len()];
    for i in 0..line.len() {
        for j in 0..played.len() {
            let mut k = 0;
            while i + k < line.len() && j + k < played.len() && line[i + k] == played[j + k] {
                k += 1;
            }
            if k >= RUN {
                mask[i..i + k].iter_mut().for_each(|m| *m = true);
            }
        }
    }
    mask
}

/// Whether `line` (a microphone's words) is the apps' `played` words heard again (module docs).
pub(super) fn is_playback(line: &[Word], played: &[Word]) -> bool {
    let (l, p) = (tokens(line), tokens(played));
    if l.len() < MIN_WORDS || p.len() < RUN {
        return false;
    }
    let mask = covered(&l, &p);
    let share = mask.iter().filter(|m| **m).count() as f32 / l.len() as f32;
    if share < COVERED {
        return false;
    }
    // His own address to Heimdall over the playback stays his (judged on the recognised text, punctuation included:
    // the name check reads sentence ends and commas).
    let line_text = listen::Transcript::from_words(line.to_vec()).text;
    let played_text = listen::Transcript::from_words(played.to_vec()).text;
    !(decide::send::addressed_by_name(&line_text) && !decide::send::addressed_by_name(&played_text))
}

impl Pipeline {
    /// A recorded app's final (not Heimdall's own output, not a microphone): kept to recognise it on his microphone.
    pub(super) fn note_app_speech(&mut self, source: &str, words: &[Word]) {
        if self.user_source(source) || self.own_audio(source) || !self.sources.contains_key(source) {
            return;
        }
        self.mic_voices.playback.heard(source, words);
    }
    /// The words the apps said within `PAD_NS` of [from, to]: kept finals, and every app utterance still open.
    pub(super) fn app_words_around(&self, from: u64, to: u64) -> Vec<Word> {
        let near = |w: &&Word| w.end_ns.saturating_add(PAD_NS) >= from && w.start_ns <= to.saturating_add(PAD_NS);
        let open = self
            .active
            .iter()
            .filter(|(s, _)| !self.user_source(s) && !self.own_audio(s) && self.sources.contains_key(*s))
            .flat_map(|(_, u)| u.prefix.iter());
        let mut out: Vec<Word> = self.mic_voices.playback.words().chain(open).filter(near).cloned().collect();
        out.sort_by_key(|w| w.start_ns);
        out.dedup_by(|a, b| a.start_ns == b.start_ns && a.text == b.text);
        out
    }
    /// An app was talking around [from, to].
    pub(super) fn app_audible(&self, from: u64, to: u64) -> bool {
        !self.app_words_around(from, to).is_empty()
    }
    /// The line `words` on Mac microphone `source` repeats what an app played around it.
    pub(super) fn playback_on_mic(&self, source: &str, words: &[Word]) -> bool {
        if self.phone_source(source) || !self.user_source(source) {
            return false;
        }
        let (Some(first), Some(last)) = (words.first(), words.last()) else { return false };
        is_playback(words, &self.app_words_around(first.start_ns, last.end_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn said(text: &str, start_s: f64) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, t)| {
                let s = ((start_s + i as f64 * 0.3) * 1e9) as u64;
                Word { text: t.into(), start_ns: s, end_ns: s + 250_000_000, confidence: 0.9 }
            })
            .collect()
    }
    /// The 12 Nov 12:58 line: the narrator's words from the browser, heard again on the Shure.
    #[test]
    fn the_narrator_heard_through_the_speakers_is_playback() {
        let app = said("Hey internets, one thing that's commonly done these days is equating democracy with freedom. That the more democracy we have, the more freedom we have, and the more people who are engaged in democracy, the more people voting", 0.);
        let mic = said("freedom we have, and the more people who are engaged in democracy, the more people voting", 7.);
        assert!(is_playback(&mic, &app));
    }
    /// His own reply to what played, a short "yeah", or the same words long after are not playback.
    #[test]
    fn his_answer_to_the_video_is_his() {
        let app = said("I don't think it ever will be, bro. Bro, a billion is not that much", 0.);
        assert!(!is_playback(&said("A billion. A billion is not that much. Like you can hide anywhere.", 1.), &app), "he quotes three words of it and goes on");
        assert!(!is_playback(&said("Yeah not that much", 1.), &app), "too short to tell");
        assert!(!is_playback(&said("I don't think it will be as good as some not, but no.", 1.), &app));
    }
    /// Over the playback, addressing Heimdall by name stays his (measured: "Heimdall, can you give me the model?" over
    /// a friend's "Can you give me the model for that?").
    #[test]
    fn his_address_to_heimdall_over_the_playback_stays_his() {
        let app = said("can you give me the model for that", 0.);
        assert!(!is_playback(&said("Heimdall, can you give me the model?", 0.5), &app));
        assert!(is_playback(&said("so can you give me the model for that", 0.5), &app));
        // The name after a sentence ends is an address too (review 12 Nov: the check reads punctuation).
        assert!(!is_playback(&said("Wait. Heimdall, can you give me the model for that", 0.5), &app));
    }
    #[test]
    fn runs_need_three_words_in_a_row() {
        let t = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(covered(&t("a b c d"), &t("x a b y c d")), vec![false; 4], "pairs are not runs");
        assert_eq!(covered(&t("a b c d"), &t("x a b c")), vec![true, true, true, false]);
    }
}
