//! Echo guard (night 23 Oct; bug notes 22 Oct 23:26 and 23:40): through speakers the microphone hears Heimdall's own
//! replies, not only its acks. On build 996f5e3 a reply came back as "Mic · the user", cut Heimdall off as a barge-in and
//! woke the agent with its own words; each echo then played a new ack, which echoed too (the 23:40 loop).
//!
//! Apple's echo cancellation removes most of it while Heimdall speaks on speakers (capture's `set_echo_cancellation`,
//! driven by `bridge::echo_cancel`). This is the backup layer the user asked for (22 Oct 23:28): the words on his
//! microphone are compared with the text Heimdall was playing at that moment, fuzzily, because the echo comes back
//! misheard ("a quick ack" as "a quick act", "set off a wake" as "set off away").
//!
//! - Only what played through speakers counts (`SpeechPolicy::played`, sentence by sentence as the speaker starts it,
//!   with the route read then). Through headphones and AirPods nothing is ever dropped.
//! - Only words heard while that sentence sounded, from `LEAD_NS` before its start to `TAIL_NS` after its end, can be
//!   echo; the user repeating Heimdall later is his.
//! - The heard words are aligned with the played words (a local alignment that allows misheard, missing and extra
//!   words). Matched and misheard words are echo; two or more extra words in a row are the user talking over it. A word
//!   misheard as two (the name heard as two short words) or two run together match too (23 Oct 17:34, 17:35).
//! - Each played word is echoed at most once: the user repeating what Heimdall just said is his (23 Oct 17:35:35).
//! - One or two words must match exactly, and his control words ("stop", "wait", the name) are echo only inside a
//!   longer run, or as a sentence's first word heard right as it began (23 Oct 17:36:19).
//! - A heard word must come within 1.5 s of when its played word sounded: the user answering "Push it." is his.
//! - An utterance that keeps fewer than two words of his is Heimdall's own voice: archived only, never a turn, a
//!   decision or a wake. Otherwise the echo words are left out of what is sent (and of command matching).
use listen::Word;

/// A sentence Heimdall played through speakers, host clock. `end_ns` is `u64::MAX` while it still plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Played {
    pub start_ns: u64,
    pub end_ns: u64,
    pub text: String,
}
/// How long before a sentence's start a heard word may still be its echo: the hook fires as the speaker takes the
/// first sample, and Parakeet dates word starts up to ~430 ms early (screen-phrase bench, 22 Oct).
pub(crate) const LEAD_NS: u64 = 500_000_000;
/// How long after its last sample a sentence may still be on the microphone: output and input latency, the room's
/// tail, and Parakeet's late word ends.
pub(crate) const TAIL_NS: u64 = 1_200_000_000;
/// The fewest of the user's own words an utterance over Heimdall's echo needs to stay his (the barge-in's rule too).
pub(crate) const OWNER_WORDS: usize = 2;
/// The slowest pace a sentence still playing may have, at any speed (Jarvis at natural speed: about 2.9 words a second).
const WORD_NS: u64 = 350_000_000;
/// The fastest, at any speed: Jarvis at the user's 160% spoke 190-210 ms a word on 30 Oct 03:08 (archive, Heimdall's own
/// playback tap). A sentence still playing has no end yet, so its k-th word sounded somewhere between two paces; one
/// fixed pace of 350 ms put the second half of each sentence past `TIME_SLACK_NS` and kept it as the user's (bug note
/// 2026-10-30T03-08-53: "me whenever you like." and "the speakers won't wake me …" woke the agent).
const FAST_WORD_NS: u64 = 150_000_000;
/// A word at natural speed (`judge_paced`): 2.9-3.1 words a second; at 160% that is the measured 190-210 ms.
const NATURAL_WORD_NS: f32 = 330_000_000.;
/// How far a sentence's own pace may stray from the speed setting's (pauses, short and long words).
const PACE_SPREAD: f32 = 0.35;
/// How far a heard word may be from the moment its played word sounded: output and mic latency, Parakeet's early
/// starts, and the even spread's error.
const TIME_SLACK_NS: u64 = 1_500_000_000;
/// A sentence's first word sounds at its known start, not an estimate: heard within this of it (Parakeet dates a
/// word up to ~430 ms early; the echo comes 0.1–0.3 s late), it is the echo, even one of his control words (23 Oct
/// 17:36:19: the opening "Yes" of a Heimdall sentence came back as the user's "Yes." and woke the agent).
const FIRST_WORD_NS: u64 = 600_000_000;
/// The user's control words: never echo on a match of fewer than three words.
const CONTROL: &[&str] = &["stop", "wait", "no", "yes", "cancel", "heimdall", "hey", "hold", "pause", "quiet"];
/// A control word, or the configured name (Personalise).
fn control(w: &str) -> bool {
    CONTROL.contains(&w) || decide::names::current().exact(w)
}
/// Heimdall's name as `barge_in::words` spells it.
const NAME: &str = "heimdall";
struct Token {
    text: String,
    /// Letters of the whole word, on its first token only ("I'll" is "i" + "will": "ill" and ""), so a misheard word
    /// split in two (the name heard as two short words) is compared by what was written.
    letters: String,
    /// Heard: the word's start. Played: the earliest it could sound.
    at_ns: u64,
    /// Played: the latest it could sound (`at_ns` once the sentence has ended). Heard: `at_ns`.
    late_ns: u64,
    /// Heard: the index of its word.
    word: usize,
    /// Heard: inside a played sentence's window. Played: not yet taken by an earlier stretch.
    eligible: bool,
    /// Played: the first word of its sentence, sounding exactly at `at_ns`.
    first: bool,
}
fn tokens(text: &str, at: impl Fn(usize) -> (u64, u64), word: usize, eligible: bool) -> Vec<Token> {
    let mut out = vec![];
    for raw in text.split_whitespace() {
        let letters: String = raw.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
        for (k, text) in words(raw).into_iter().enumerate() {
            let n = out.len();
            let (at_ns, late_ns) = at(n);
            out.push(Token { text, letters: if k == 0 { letters.clone() } else { String::new() }, at_ns, late_ns, word, eligible, first: n == 0 });
        }
    }
    out
}

/// What the guard found in one utterance.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    /// Per heard word: Heimdall's echo.
    pub echo: Vec<bool>,
    /// The played text the words were compared with.
    pub reference: String,
}
impl Verdict {
    pub(crate) fn any(&self) -> bool {
        self.echo.iter().any(|e| *e)
    }
    /// Heimdall's own voice: some echo, and fewer than `OWNER_WORDS` words left for the user.
    pub(crate) fn own(&self) -> bool {
        self.any() && self.echo.iter().filter(|e| !**e).count() < OWNER_WORDS
    }
    /// The words that are not echo.
    pub(crate) fn kept(&self, words: &[Word]) -> Vec<Word> {
        words.iter().zip(&self.echo).filter(|(_, e)| !**e).map(|(w, _)| w.clone()).collect()
    }
}

/// Compares heard words with what played through speakers around them. `None` when no word lies inside a played
/// sentence's window (nothing to compare).
pub(crate) fn judge(heard: &[Word], played: &[Played]) -> Option<Verdict> {
    judge_paced(heard, played, None)
}
/// `judge` with Heimdall's voice speed known (percent of natural / 100): a sentence still playing is paced from it,
/// within `PACE_SPREAD`, so the user repeating its words a few seconds later stays his (review 30 Oct). Unknown speed:
/// any pace from `FAST_WORD_NS` to `WORD_NS`.
pub(crate) fn judge_paced(heard: &[Word], played: &[Played], speed: Option<f32>) -> Option<Verdict> {
    let (fast, slow) = match speed.filter(|s| s.is_finite() && *s >= 0.5 && *s <= 3.) {
        Some(s) => ((NATURAL_WORD_NS / s * (1. - PACE_SPREAD)) as u64, (NATURAL_WORD_NS / s * (1. + PACE_SPREAD)) as u64),
        None => (FAST_WORD_NS, WORD_NS),
    };
    let window = |p: &Played, w: &Word| w.end_ns.saturating_add(LEAD_NS) >= p.start_ns && w.start_ns <= p.end_ns.saturating_add(TAIL_NS);
    let eligible: Vec<bool> = heard.iter().map(|w| played.iter().any(|p| window(p, w))).collect();
    if !eligible.iter().any(|e| *e) {
        return None;
    }
    let mut sentences: Vec<&Played> = played.iter().filter(|p| heard.iter().zip(&eligible).any(|(w, e)| *e && window(p, w))).collect();
    sentences.sort_by_key(|p| p.start_ns);
    let reference = sentences.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join(" ");
    // Played words with the moment each was about to sound: spread evenly over its sentence (an open sentence between
    // the fastest and the slowest speaking pace), so a word the user says a few seconds after Heimdall said it ("Should I
    // push it?" "Push it.") is not its echo (review 23 Oct).
    let mut r: Vec<Token> = vec![];
    for p in &sentences {
        let n = words(&p.text).len().max(1) as u64;
        if p.end_ns == u64::MAX {
            r.extend(tokens(&p.text, |k| (p.start_ns + fast * k as u64, p.start_ns + slow * k as u64), 0, true));
        } else {
            let length = p.end_ns.saturating_sub(p.start_ns);
            r.extend(tokens(&p.text, |k| { let t = p.start_ns + length * k as u64 / n; (t, t) }, 0, true));
        }
    }
    // Heard tokens ("it's" is two), each with its word and whether it may be echo at all.
    let mut h: Vec<Token> = vec![];
    for (i, w) in heard.iter().enumerate() {
        h.extend(tokens(&w.text, |_| (w.start_ns, w.start_ns), i, eligible[i]).into_iter().map(|t| Token { first: false, ..t }));
    }
    let mut echo_token = vec![false; h.len()];
    let mut taken = vec![false; h.len()];
    let candidates = h.iter().filter(|t| t.eligible).count();
    let c = Compare { exact: candidates <= 2, last: h.len().saturating_sub(1) };
    // The weakest stretch that counts: a short utterance wholly matching ("Okay."), else about three words in a row,
    // so a word or two the user shares with the reply ("the", "it is") are never cut from his sentence.
    let need = candidates.clamp(1, 3) as i32 * NEAR;
    for _ in 0..4 {
        let Some(found) = align(&h, &taken, &r, &c) else { break };
        let matched = found.ops.iter().filter(|op| matches!(op, Op::Match(..))).count();
        if found.score < need || matched * 2 < found.h_end - found.h_start {
            break;
        }
        // Inside the aligned stretch: matched and misheard words are echo, and so is a single extra word; two or
        // more extra words in a row are the user's.
        let mut run: Vec<usize> = vec![];
        let flush = |run: &mut Vec<usize>, echo: &mut Vec<bool>| {
            if run.len() < OWNER_WORDS {
                for &i in run.iter() {
                    echo[i] = true;
                }
            }
            run.clear();
        };
        for op in &found.ops {
            match *op {
                Op::Match(i, _) | Op::Misheard(i, _) => {
                    flush(&mut run, &mut echo_token);
                    echo_token[i] = true;
                }
                Op::Extra(i) => run.push(i),
                Op::Missing => {}
            }
        }
        flush(&mut run, &mut echo_token);
        // His control words (the name among them) stay his unless they lie inside a run of Heimdall's words long enough
        // to be its echo, or match a sentence's first word exactly as it began.
        if matched < 3 {
            for op in &found.ops {
                let (Op::Match(i, j) | Op::Misheard(i, j)) = *op else { continue };
                let at_start = matches!(op, Op::Match(..)) && r[j].first && h[i].at_ns.abs_diff(r[j].at_ns) <= FIRST_WORD_NS;
                if control(&h[i].text) && !at_start {
                    echo_token[i] = false;
                }
            }
        }
        for t in taken.iter_mut().take(found.h_end).skip(found.h_start) {
            *t = true;
        }
        // Heimdall said each word once: a stretch it echoed is used up, so the user repeating it right after is his (23 Oct
        // 17:35:35: the user answering a question by repeating its last words lost both copies to the one played phrase).
        for t in r.iter_mut().take(found.r_end).skip(found.r_start) {
            t.eligible = false;
        }
    }
    let mut echo = vec![true; heard.len()];
    let mut has_token = vec![false; heard.len()];
    for (k, t) in h.iter().enumerate() {
        has_token[t.word] = true;
        echo[t.word] &= echo_token[k];
    }
    // A word without letters (a stray mark) goes with its neighbours: it is echo only inside echo.
    for i in 0..heard.len() {
        if !has_token[i] {
            echo[i] = (i > 0 && echo[i - 1]) && (i + 1 >= heard.len() || echo[i + 1]);
        }
    }
    Some(Verdict { echo, reference })
}

impl super::Pipeline {
    /// The echo guard's verdict on words from the user's microphone (`None`: not his microphone, no words, or nothing
    /// played through speakers around them).
    pub(super) fn echo_verdict(&self, source: &str, words: &[Word]) -> Option<Verdict> {
        let (first, last) = (words.first()?, words.last()?);
        if !self.owner(source) {
            return None;
        }
        let played = self.tools.speech_policy.played_around(first.start_ns, last.end_ns);
        if played.is_empty() {
            return None;
        }
        judge_paced(words, &played, Some(self.tools.voice_speed.load(std::sync::atomic::Ordering::Acquire) as f32 / 100.))
    }
    /// Journals what the guard did to a final utterance (`echo_guard` evidence: the live check reads it).
    pub(super) fn record_echo(&mut self, source: &str, utterance: &str, words: &[Word], v: &Verdict) {
        let heard = listen::Transcript::from_words(words.to_vec()).text;
        let kept = listen::Transcript::from_words(v.kept(words)).text;
        let at = words.first().map_or(0, |w| w.start_ns);
        let record = serde_json::json!({"schema":1,"outcome":if v.own() {"heimdalls_own_voice"} else {"echo_words_left_out"},
            "heard":heard,"kept":kept,"reference":v.reference,"echo":v.echo});
        if let Err(e) = self.index.evidence.event("echo_guard", source, &[utterance.to_owned()], at, record) {
            eprintln!("Echo guard evidence failed: {e:#}");
        }
        eprintln!("echo guard: {} \"{}\"{}", if v.own() { "Heimdall's own voice on the microphone, archived only:" } else { "left Heimdall's echo out of" },
            heard.chars().take(120).collect::<String>(), if v.own() { String::new() } else { format!(" (kept \"{}\")", kept.chars().take(120).collect::<String>()) });
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    /// The heard token at this index matches the played word at the second (exactly or nearly; a misheard word split
    /// in two matches with both tokens, two played words run together with the one).
    Match(usize, usize),
    /// The heard token stands where a played word was, but differs: misheard.
    Misheard(usize, usize),
    /// A heard token with no played word.
    Extra(usize),
    /// A played word not heard.
    Missing,
}
struct Alignment {
    score: i32,
    ops: Vec<Op>,
    h_start: usize,
    h_end: usize,
    r_start: usize,
    r_end: usize,
}
const EXACT: i32 = 4;
const NEAR: i32 = 3;
const MISHEARD: i32 = -2;
const GAP: i32 = -2;
/// How heard and played tokens are compared in one utterance.
struct Compare {
    /// A word or two must match exactly: one letter off would make his "wait" Heimdall's "want" (review 23 Oct).
    exact: bool,
    /// The utterance's last heard token: Parakeet cuts a word off at the end ("ans" for "answer").
    last: usize,
}
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Stop,
    Pair,
    Extra,
    Missing,
    /// Two or three heard tokens for one played word.
    Split(usize),
    /// One heard token for two played words.
    Merge,
}

/// The best local alignment of the untaken, eligible heard tokens with the played words not yet echoed
/// (Smith–Waterman, with split and run-together words). A taken or ineligible heard token ends any stretch.
fn align(h: &[Token], taken: &[bool], all: &[Token], c: &Compare) -> Option<Alignment> {
    let live: Vec<usize> = (0..all.len()).filter(|&j| all[j].eligible).collect();
    let r: Vec<&Token> = live.iter().map(|&j| &all[j]).collect();
    let (n, m) = (h.len(), r.len());
    if n == 0 || m == 0 {
        return None;
    }
    let usable = |i: usize| h[i].eligible && !taken[i];
    let mut score = vec![vec![0i32; m + 1]; n + 1];
    let mut step = vec![vec![Step::Stop; m + 1]; n + 1];
    let mut best = (0, 0, 0);
    for i in 1..=n {
        if !usable(i - 1) {
            continue;
        }
        for j in 1..=m {
            let mut s = (0, Step::Stop);
            let mut offer = |v: i32, st: Step| {
                if v > s.0 {
                    s = (v, st);
                }
            };
            offer(score[i - 1][j - 1] + pair(&h[i - 1], i - 1, r[j - 1], c), Step::Pair);
            offer(score[i - 1][j] + GAP, Step::Extra);
            offer(score[i][j - 1] + GAP, Step::Missing);
            for k in 2..=3 {
                if i >= k && (i - k..i).all(usable) {
                    if let Some(v) = joined(&h[i - k..i], &[r[j - 1]], c) {
                        offer(score[i - k][j - 1] + v, Step::Split(k));
                    }
                }
            }
            if j >= 2 {
                if let Some(v) = joined(&h[i - 1..i], &[r[j - 2], r[j - 1]], c) {
                    offer(score[i - 1][j - 2] + v, Step::Merge);
                }
            }
            (score[i][j], step[i][j]) = s;
            if s.0 > best.0 {
                best = (s.0, i, j);
            }
        }
    }
    if best.0 == 0 {
        return None;
    }
    let (top, mut i, mut j) = best;
    let (h_end, r_end) = (i, j);
    let mut ops = vec![];
    while i > 0 && j > 0 && score[i][j] > 0 {
        match step[i][j] {
            Step::Stop => break,
            Step::Pair => {
                let v = pair(&h[i - 1], i - 1, r[j - 1], c);
                ops.push(if v > 0 { Op::Match(i - 1, live[j - 1]) } else { Op::Misheard(i - 1, live[j - 1]) });
                i -= 1;
                j -= 1;
            }
            Step::Extra => {
                ops.push(Op::Extra(i - 1));
                i -= 1;
            }
            Step::Missing => {
                ops.push(Op::Missing);
                j -= 1;
            }
            Step::Split(k) => {
                for t in (i - k..i).rev() {
                    ops.push(Op::Match(t, live[j - 1]));
                }
                i -= k;
                j -= 1;
            }
            Step::Merge => {
                ops.push(Op::Match(i - 1, live[j - 2]));
                i -= 1;
                j -= 2;
            }
        }
    }
    ops.reverse();
    // A stretch never begins on anything but a match (local alignment), so trailing and leading gaps are absent.
    Some(Alignment { score: top, ops, h_start: i, h_end, r_start: live[j], r_end: live[r_end - 1] + 1 })
}

/// A heard and a played token's score: alike, and heard near when it sounded.
/// A heard word came when its played word could have sounded, within `TIME_SLACK_NS`.
fn timely(heard: &Token, played: &Token) -> bool {
    heard.at_ns.saturating_add(TIME_SLACK_NS) >= played.at_ns && heard.at_ns <= played.late_ns.saturating_add(TIME_SLACK_NS)
}
fn pair(heard: &Token, index: usize, played: &Token, c: &Compare) -> i32 {
    if !timely(heard, played) {
        return MISHEARD;
    }
    if played.text == NAME {
        return name(&heard.letters, c.exact);
    }
    match similarity(&heard.text, &played.text) {
        NEAR if c.exact => MISHEARD,
        // The utterance's last word cut off by Parakeet: a beginning of a longer played word ("ans" for "answer"). Never
        // a short word or one of his control words: his trailing "no" is not Heimdall's "now" (review 24 Oct).
        MISHEARD if !c.exact && index == c.last && heard.text.len() >= 2 && played.text.len() >= 5 && played.text.starts_with(&heard.text)
            && !control(&heard.text) => NEAR,
        s => s,
    }
}
/// Heard tokens run together against played words run together ("echocheck" for "echo check", "Haym doll" for
/// "Heimdall"): a score only when they are alike, timed by the first of each.
fn joined(heard: &[Token], played: &[&Token], c: &Compare) -> Option<i32> {
    // A run starts on a word, never on a contraction's second half.
    if heard[0].letters.is_empty() || !timely(&heard[0], played[0]) {
        return None;
    }
    let h: String = heard.iter().map(|t| t.letters.as_str()).collect();
    let p: String = played.iter().map(|t| t.text.as_str()).collect();
    let v = if played.len() == 1 && played[0].text == NAME { name(&h, c.exact) } else { similarity(&h, &p) };
    match v {
        NEAR if c.exact => None,
        v if v > 0 => Some(v),
        _ => None,
    }
}
/// Heard letters against Heimdall's own name, said at that moment. Parakeet mishears it widely (split in two short
/// words, "Hemdall", "Heimdal"), and the moment it was said makes a looser match safe: up to three letters off, starting
/// with its "h" and about its length ("hey" + "heimdall" run together is not it); one letter off when a word or two
/// must match exactly.
fn name(letters: &str, exact: bool) -> i32 {
    let d = edit_distance(&letters.chars().collect::<Vec<_>>(), &NAME.chars().collect::<Vec<_>>());
    if d == 0 || (exact && d <= 1) {
        EXACT
    } else if !exact && letters.starts_with('h') && letters.len().abs_diff(NAME.len()) <= 2 && d <= 3 {
        NEAR
    } else {
        MISHEARD
    }
}
/// How alike a heard word and a played word are: the same, nearly the same (one or two letters off, or one is the
/// other with an ending: "install"/"installs", "act"/"ack"), or different.
fn similarity(heard: &str, played: &str) -> i32 {
    if heard == played {
        return EXACT;
    }
    if heard.len().abs_diff(played.len()) > 3 {
        return MISHEARD;
    }
    let (a, b): (Vec<char>, Vec<char>) = (heard.chars().collect(), played.chars().collect());
    let shorter = a.len().min(b.len());
    if shorter >= 3 {
        let allowed = if shorter <= 5 { 1 } else { 2 };
        if edit_distance(&a, &b) <= allowed {
            return NEAR;
        }
        if shorter >= 4 && (heard.starts_with(played) || played.starts_with(heard)) && a.len().abs_diff(b.len()) <= 3 {
            return NEAR;
        }
    }
    MISHEARD
}
fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            row[j + 1] = (previous[j] + usize::from(ca != cb)).min(previous[j + 1] + 1).min(row[j] + 1);
        }
        previous = row;
    }
    previous[b.len()]
}

/// Words, lowercased, letters and digits only, with common contractions spelled out ("it's" is "it is"), so the
/// recognizer's spelling of Heimdall's words on the microphone matches what it said.
pub(super) fn words(text: &str) -> Vec<String> {
    let mut out = vec![];
    for w in text.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’') {
        let w = w.replace('’', "'").trim_matches('\'').to_lowercase();
        if w.is_empty() {
            continue;
        }
        let expanded: &[&str] = match w.as_str() {
            "it's" => &["it", "is"],
            "that's" => &["that", "is"],
            "there's" => &["there", "is"],
            "here's" => &["here", "is"],
            "what's" => &["what", "is"],
            "let's" => &["let", "us"],
            "i'm" => &["i", "am"],
            "i'll" => &["i", "will"],
            "you'll" => &["you", "will"],
            "it'll" => &["it", "will"],
            "i've" => &["i", "have"],
            "you're" => &["you", "are"],
            "we're" => &["we", "are"],
            "they're" => &["they", "are"],
            "isn't" => &["is", "not"],
            "don't" => &["do", "not"],
            "doesn't" => &["does", "not"],
            "can't" => &["can", "not"],
            "won't" => &["will", "not"],
            "didn't" => &["did", "not"],
            _ => {
                out.push(w);
                continue;
            }
        };
        out.extend(expanded.iter().map(|s| s.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;
    /// Words spread evenly from `start` at `per` ns each.
    fn said(text: &str, start: u64, per: u64) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, t)| Word { text: t.into(), start_ns: start + i as u64 * per, end_ns: start + (i as u64 + 1) * per - 10_000_000, confidence: 0.9 })
            .collect()
    }
    fn played(text: &str, start: u64, end: u64) -> Played {
        Played { start_ns: start, end_ns: end, text: text.into() }
    }
    fn text(words: &[Word]) -> String {
        words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// The 22 Oct 23:26 echo (build 996f5e3): the reply played through the MacBook speakers, and the microphone heard
    /// its middle, misheard in places. All of it is Heimdall's.
    #[test]
    fn the_23_26_reply_echo_is_heimdalls_own_voice() {
        let reply = [
            "You're on speakers now, so the echo guard can be tested.",
            "Ask me something short and stop talking.",
            "You should hear a quick ack, then my answer.",
            "Neither should come back to me as you talking or set off a wake.",
            "One thing won't work through speakers: interrupting me, since there's no echo cancellation yet.",
        ];
        let spans: Vec<Played> = reply.iter().enumerate().map(|(i, t)| played(t, 100 * S + i as u64 * 3 * S, 100 * S + (i as u64 + 1) * 3 * S)).collect();
        let heard = said("echo guard can be tested. Ask me something short and stop talking. You should hear a quick act, then my answer. Neither should come back to me as you talking or set off away. One thing will", 101 * S, 330_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(v.own(), "{:?}", v.kept(&heard));
    }
    /// The 23:40 loop: an ack's echo ("Let me check.") and a reply's echo.
    #[test]
    fn the_23_40_ack_and_reply_echoes_are_heimdalls() {
        let ack = [played("Let me check.", 50 * S, 51 * S)];
        assert!(judge(&said("Let me check.", 50 * S + 200_000_000, 200_000_000), &ack).unwrap().own());
        let reply = [played("Also, that \"Let me check\" right after was my own ack echoing back from the speakers as you.", 60 * S, 65 * S)];
        let heard = said("Also that let me check right after was my own hack echoing back from the speaker.", 60 * S + 300_000_000, 300_000_000);
        assert!(judge(&heard, &reply).unwrap().own());
        let okay = [played("Okay.", 70 * S, 70 * S + 600_000_000)];
        assert!(judge(&said("Okay.", 70 * S + 100_000_000, 400_000_000), &okay).unwrap().own());
    }
    /// The recorded 23:40 words and playback times (lab/echo-guard/incidents, host ns): the ack "Let me check." played
    /// 27.464–28.413 s and Parakeet dated its echo's last word to 29.083 s, 70 ms past the old 600 ms ack tail; "Wait,
    /// no, you can hear me?" over the "Okay." ack (47.721–48.237 s) was really the user. ("La dice.", the next "Let me
    /// check." garbled, is the ack's own-sound span's: `replay_of_the_recorded_incidents`.)
    #[test]
    fn the_recorded_23_40_words_and_times() {
        let b = 1_427_000_000_000_000u64;
        let w = |t: &str, s: u64, e: u64| Word { text: t.into(), start_ns: b + s, end_ns: b + e, confidence: 0.9 };
        let check = [played("Let me check.", b + 27_464_355_792, b + 28_413_427_291)];
        let heard = [w("Let", 27_643_393_250, 27_803_393_250), w("me", 27_899_393_250, 28_123_393_250), w("check.", 28_123_393_250, 29_083_393_250)];
        assert!(judge(&heard, &check).unwrap().own());
        let okay = [played("Okay.", b + 47_720_828_375, b + 48_237_047_583)];
        let heard = [w("Wait,", 47_867_393_250, 48_251_393_250), w("no,", 48_251_393_250, 48_315_393_250), w("you", 48_315_393_250, 48_475_393_250),
            w("can", 48_475_393_250, 48_555_393_250), w("hear", 48_715_393_250, 49_115_393_250), w("me?", 49_115_393_250, 49_755_393_250)];
        assert!(!judge(&heard, &okay).unwrap().any(), "User over an ack is his");
    }
    /// The 23:26 barge-in's last window (the one the decider stopped): two misheard words ("away", "was") broke the
    /// old own-voice rule; aligned with what played they are echo.
    #[test]
    fn the_23_26_barge_window_is_heimdalls() {
        let spans = [
            played("You should hear a quick ack, then my answer.", 10 * S, 13 * S),
            played("Neither should come back to me as you talking or set off a wake.", 13 * S, 17 * S),
            played("One thing won't work through speakers: interrupting me, since there's no echo cancellation yet.", 17 * S, u64::MAX),
        ];
        let heard = said("Then my answer neither should come back to me as you talking, or set off away. One thing was.", 12 * S, 300_000_000);
        assert!(judge(&heard, &spans).unwrap().own());
    }
    /// The user talking over the echo keeps his words; the echo words are left out of what is sent.
    #[test]
    fn owners_words_over_the_echo_stay_his() {
        let spans = [played("The build failed because the linker could not find the Metal library.", 10 * S, 14 * S)];
        let heard = said("the linker could not hey wait use the other branch find the metal library", 11 * S, 200_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(!v.own());
        assert_eq!(text(&v.kept(&heard)), "hey wait use the other branch");
        // Before the echo, and after it.
        let heard = said("stop stop listen to me the linker could not find", 11 * S, 250_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(!v.own());
        assert_eq!(text(&v.kept(&heard)), "stop stop listen to me");
    }
    /// A few common words the user shares with what Heimdall says are not cut from his sentence.
    #[test]
    fn shared_common_words_are_not_echo() {
        let spans = [played("It is ready, and the island shows it.", 10 * S, 13 * S)];
        let heard = said("no it is the wrong one, open the other", 10 * S + 500_000_000, 250_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(!v.any(), "{:?}", v);
        // "wait" alone, not in the reply: his.
        assert!(!judge(&said("wait", 11 * S, 300_000_000), &spans).unwrap().any());
        assert!(!judge(&said("wait no", 11 * S, 300_000_000), &spans).unwrap().any());
    }
    /// Only words heard while the sentence sounded (and its tail) can be its echo: the user repeating Heimdall a little
    /// later is his.
    #[test]
    fn words_outside_the_played_window_are_always_owners() {
        let spans = [played("Let me check.", 50 * S, 51 * S)];
        assert_eq!(judge(&said("Let me check.", 53 * S, 300_000_000), &spans), None);
        assert_eq!(judge(&said("Let me check.", 48 * S, 300_000_000), &spans), None, "before it began");
        // Across the end of the tail: the part inside is echo, the rest is his.
        let heard = said("let me check that file for me please", 51 * S + 300_000_000, 300_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(!v.own(), "{:?}", v);
    }
    /// A sentence still playing (no end yet) counts to now.
    #[test]
    fn an_open_sentence_counts() {
        let spans = [played("Here is the summary of the three threads.", 10 * S, u64::MAX)];
        assert!(judge(&said("here is the summery of the", 10 * S + 200_000_000, 300_000_000), &spans).unwrap().own());
    }
    /// Replay of both recorded incidents (22 Oct 23:25–23:26 and 23:39–23:41, build 996f5e3, MacBook speakers, Shure
    /// MV7i): Parakeet's words on the microphone that night, against what Heimdall played, timed by its own process
    /// tap and worded as it was really said (`echo_guard_incidents.json`, from lab/echo-guard/incidents). Every echo is
    /// Heimdall's; every line of the user's stays whole.
    #[test]
    fn replay_of_the_recorded_incidents() {
        #[derive(serde::Deserialize)]
        struct Fixture { mic: Vec<Line>, played: Vec<Span>, acks: Vec<Ack> }
        #[derive(serde::Deserialize)]
        struct Ack { start_ns: u64, end_ns: u64 }
        #[derive(serde::Deserialize)]
        struct Line { local: String, echo: bool, words: Vec<W> }
        #[derive(serde::Deserialize)]
        struct W { text: String, start_ns: u64, end_ns: u64 }
        #[derive(serde::Deserialize)]
        struct Span { start_ns: u64, end_ns: u64, text: String }
        let f: Fixture = serde_json::from_str(include_str!("echo_guard_incidents.json")).unwrap();
        let played: Vec<Played> = f.played.iter().map(|s| played(&s.text, s.start_ns, s.end_ns)).collect();
        // The acks' own-sound spans as voice_bank.rs opens them (`ACK_LEAD` before, `ACK_TAIL` after).
        let policy = crate::bridge::speech_policy::SpeechPolicy::default();
        for a in &f.acks {
            policy.own_sound_until(a.start_ns - crate::voice_bank::ACK_LEAD.as_nanos() as u64, a.end_ns, a.end_ns + crate::voice_bank::ACK_TAIL.as_nanos() as u64);
        }
        assert_eq!((f.mic.len(), f.mic.iter().filter(|l| l.echo).count()), (9, 6));
        for line in &f.mic {
            let words: Vec<Word> = line.words.iter().map(|w| Word { text: w.text.clone(), start_ns: w.start_ns, end_ns: w.end_ns, confidence: 0.9 }).collect();
            let v = judge(&words, &played);
            let ack = policy.cue_covers(words[0].start_ns, words.last().unwrap().end_ns);
            if line.echo {
                assert!(ack || v.as_ref().is_some_and(|v| v.own()), "{} {:?}", line.local, v.map(|v| text(&v.kept(&words))));
            } else {
                assert!(!ack && v.as_ref().is_none_or(|v| !v.any()), "{} lost words: {:?}", line.local, v);
            }
        }
    }
    /// Review 23 Oct, finding 2: Heimdall says "On it." (0.9 s) through speakers; 0.2 s after it ends the user says
    /// "No, wait" (0.6 s), inside the ack's tail. Words that begin after the ack's last sample are his, whatever the
    /// tail; its echo, begun while it sounded, is Heimdall's up to the end of the tail; a late echo of its own words is
    /// the text guard's.
    #[test]
    fn a_quick_reply_after_an_ack_is_owners_and_its_echo_is_not() {
        use crate::voice_bank::{ACK_LEAD, ACK_TAIL};
        let policy = crate::bridge::speech_policy::SpeechPolicy::default();
        let (start, end) = (10 * S, 10 * S + 900_000_000);
        policy.own_sound_until(start - ACK_LEAD.as_nanos() as u64, end, end + ACK_TAIL.as_nanos() as u64);
        let reply = said("No, wait", end + 200_000_000, 300_000_000);
        assert!(reply.last().unwrap().end_ns < end + ACK_TAIL.as_nanos() as u64, "his words lie inside the tail");
        assert!(!policy.cue_covers(reply[0].start_ns, reply.last().unwrap().end_ns), "his reply was taken for the ack");
        let ledger = [played("On it.", start, end)];
        assert!(judge(&reply, &ledger).is_none_or(|v| !v.any()), "nor is it the ack's text");
        // The ack's echo, stretched by Parakeet past its end: Heimdall's.
        assert!(policy.cue_covers(start + 80_000_000, end + 1_100_000_000));
        // A late echo of its last word, begun after it: the text guard's.
        let late = said("it", end + 100_000_000, 250_000_000);
        assert!(!policy.cue_covers(late[0].start_ns, late[0].end_ns));
        assert!(judge(&late, &ledger).is_some_and(|v| v.own()), "the late echo became User's");
    }
    /// Review 23 Oct: a lone word one letter off a played word is his ("wait" is not "want"), his control words
    /// stay his on a short match, and an answer said after the question has sounded is his ("Push it.").
    #[test]
    fn short_words_control_words_and_answers_stay_owners() {
        let spans = [played("I want to stop the old build and start the new one.", 10 * S, 13 * S)];
        assert!(!judge(&said("wait", 11 * S, 300_000_000), &spans).unwrap().any());
        assert!(!judge(&said("stop", 11 * S, 300_000_000), &spans).unwrap().any(), "his stop over a sentence saying stop");
        assert!(!judge(&said("stop it", 11 * S, 300_000_000), &spans).unwrap().any());
        assert!(judge(&said("to stop the old build", 10 * S + 500_000_000, 250_000_000), &spans).unwrap().own(), "inside a run: echo");
        let question = [played("Should I push it?", 20 * S, 21 * S + 200_000_000)];
        assert!(!judge(&said("Push it.", 22 * S + 200_000_000, 300_000_000), &question).unwrap().any(), "his answer a second later");
        assert!(judge(&said("should I push it", 20 * S + 200_000_000, 300_000_000), &question).unwrap().own(), "its echo");
    }
    /// The 23 Oct 17:34–17:37 live test (build 748cb57, MacBook speakers), rebuilt with invented sentences and that
    /// day's timing structure (`echo_guard_incidents_synthetic.json`, from lab/echo-guard/synthetic-1023): every
    /// kind of microphone final and barge-in window seen that day.
    /// - A whole reply came back with Heimdall's name heard as two short words; the old guard kept those two words
    ///   and the agent woke.
    /// - Echo windows with two words run together cut Heimdall off as barge-ins.
    /// - the user answered a question by repeating its last words; the first copy (heard while Heimdall still said them)
    ///   is the echo, his repeat was lost.
    /// - A sentence's opening "Yes" came back alone as the user's "Yes." and woke the agent.
    #[test]
    fn replay_of_the_23_oct_live_test() {
        assert_eq!(replay(include_str!("echo_guard_incidents_synthetic.json")), (14, 23));
    }
    /// The same replay on the real recording of that test, kept outside the repo (it holds the user's own words). Opt in:
    /// HEIMDALL_ECHO_INCIDENTS_1023=<path to echo_guard_incidents_1023.json>; skipped when unset or absent.
    #[test]
    fn replay_of_the_real_23_oct_recording_when_present() {
        let Some(json) = std::env::var_os("HEIMDALL_ECHO_INCIDENTS_1023").and_then(|p| std::fs::read_to_string(p).ok()) else {
            eprintln!("skipped: HEIMDALL_ECHO_INCIDENTS_1023 not set or not readable");
            return;
        };
        assert_eq!(replay(&json), (15, 21));
    }
    /// Every microphone line judged as expected, and every barge-in window Heimdall's own voice. Returns the counts.
    fn replay(json: &str) -> (usize, usize) {
        #[derive(serde::Deserialize)]
        struct Fixture { played: Vec<Span>, mic: Vec<Line>, barge: Vec<Barge> }
        #[derive(serde::Deserialize)]
        struct Span { start_ns: u64, end_ns: u64, text: String }
        #[derive(serde::Deserialize)]
        struct Line { text: String, expect: String, kept: Option<String>, words: Vec<W> }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Barge { onset_ns: u64, saying: String, words: Vec<W> }
        #[derive(serde::Deserialize)]
        struct W { text: String, start_ns: u64, end_ns: u64 }
        let words = |ws: &[W]| -> Vec<Word> { ws.iter().map(|w| Word { text: w.text.clone(), start_ns: w.start_ns, end_ns: w.end_ns, confidence: 0.9 }).collect() };
        let f: Fixture = serde_json::from_str(json).unwrap();
        let played: Vec<Played> = f.played.iter().map(|s| played(&s.text, s.start_ns, s.end_ns)).collect();
        for line in &f.mic {
            let heard = words(&line.words);
            let v = judge(&heard, &played);
            match line.expect.as_str() {
                "own" => assert!(v.as_ref().is_some_and(|v| v.own()), "{}: {:?}", line.text, v.map(|v| text(&v.kept(&heard)))),
                "owner" => assert!(v.as_ref().is_none_or(|v| !v.any()), "{} lost words: {:?}", line.text, v.map(|v| text(&v.kept(&heard)))),
                _ => {
                    let v = v.expect("compared");
                    assert!(!v.own(), "{}", line.text);
                    assert_eq!(listen::Transcript::from_words(v.kept(&heard)).text, line.kept.clone().unwrap(), "{}", line.text);
                }
            }
        }
        // The barge-in windows are no longer judged by words (decision 31: by voice only); they stay in the fixture.
        (f.mic.len(), f.barge.len())
    }
    /// Heimdall's own name inside its reply, misheard or not, is its echo, and so the name never wakes it (23 Oct
    /// 17:34:44). The user saying the name over a sentence without it stays his.
    #[test]
    fn heimdalls_name_inside_its_reply_is_its_echo() {
        let spans = [played("Interrupt me any time in the middle by saying \"Heimdall, stop\".", 10 * S, 13 * S)];
        let heard = said("interrupt me any time in the middle by saying, Haym doll stop.", 10 * S + 100_000_000, 230_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(v.own(), "{:?}", text(&v.kept(&heard)));
        // Only the name and the word after it: his, even said as Heimdall says them (review 24 Oct: losing his
        // "Heimdall, stop" is worse than a rare short echo of it).
        assert!(!judge(&said("Heimdall, stop.", 12 * S, 300_000_000), &spans).unwrap().any());
        // "Hey Heimdall" run together is not a misheard name.
        assert_eq!(name("heyheimdall", false), MISHEARD);
        // The user calling it over a sentence that does not say it.
        let other = [played("The build finished and the island shows it.", 10 * S, 13 * S)];
        assert!(!judge(&said("Heimdall, stop.", 11 * S, 300_000_000), &other).unwrap().any());
        assert_eq!(name("haymdoll", false), NEAR);
        assert_eq!(name("haymdoll", true), MISHEARD, "a lone short match must be exact");
        assert_eq!(name("mindful", false), MISHEARD);
    }
    /// A word run together or split apart by Parakeet still matches (23 Oct 17:35:06: an echo window with two words run
    /// together cut Heimdall off), and the last word cut short is its beginning. (Sentences invented.)
    #[test]
    fn run_together_and_cut_off_words_match() {
        let spans = [played("The echo check failed. My last answer came back as your words.", 10 * S, 14 * S)];
        assert!(judge(&said("The echocheck failed.", 10 * S + 100_000_000, 400_000_000), &spans).unwrap().own());
        let heard = said("The echocheck failed. My last ans", 10 * S + 100_000_000, 330_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(v.echo.iter().all(|e| *e), "{:?}", text(&v.kept(&heard)));
    }
    /// Each played word is echoed once: the user repeating it right after is his (23 Oct 17:35:35; sentences invented).
    #[test]
    fn a_played_phrase_is_echoed_once_and_his_repeat_stays_his() {
        let spans = [played("The filter ships switched off today.", 20 * S, 21 * S + 340_000_000), played("Shall I restart it now?", 21 * S + 340_000_000, 22 * S + 800_000_000)];
        let heard = said("Restart it now? Sure, restart it now and check if it holds.", 21 * S + 860_000_000, 160_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert_eq!(text(&v.kept(&heard)), "Sure, restart it now and check if it holds.");
        // Said twice by Heimdall, heard twice: both echo.
        let twice = [played("Restart it now. Then restart it now again.", 30 * S, 32 * S)];
        assert!(judge(&said("restart it now then restart it now again", 30 * S + 100_000_000, 240_000_000), &twice).unwrap().own());
    }
    /// A sentence's first word heard as it began is its echo, even a control word; later in a sentence his
    /// "yes" or "stop" stay his (23 Oct 17:36:19).
    #[test]
    fn a_sentences_first_word_heard_as_it_began_is_its_echo() {
        let spans = [played("Yes, the echo came back again.", 10 * S, 11 * S + 500_000_000)];
        assert!(judge(&said("Yes.", 10 * S - 20_000_000, 190_000_000), &spans).unwrap().own());
        assert!(!judge(&said("Yes.", 10 * S + 900_000_000, 190_000_000), &spans).unwrap().any(), "a second later: his");
        let stop = [played("I want to stop the old build and start the new one.", 10 * S, 13 * S)];
        assert!(!judge(&said("stop", 10 * S, 300_000_000), &stop).unwrap().any(), "not its first word");
        // Only an exact first word: his "yes" heard as "Okay" began is his.
        let okay = [played("Okay, both changes go into the next pass.", 10 * S, 13 * S)];
        assert!(!judge(&said("Yes.", 10 * S, 190_000_000), &okay).unwrap().any());
    }
    /// Review 24 Oct: the cut-off last word must not pull his trailing words into the echo ("I'll push it" echoed,
    /// then his "wait, no": "no" is not the start of "now").
    #[test]
    fn his_trailing_no_is_not_a_cut_off_now() {
        let spans = [played("I'll push it now.", 10 * S, 11 * S + 200_000_000)];
        let heard = said("I'll push it wait no", 10 * S + 100_000_000, 250_000_000);
        let v = judge(&heard, &spans).unwrap();
        assert!(!v.own(), "{:?}", v);
        assert_eq!(text(&v.kept(&heard)), "wait no");
    }
    /// Bug note 2026-10-30T03-08-53, on the archive's own timings (mic words as Parakeet dated them; sentence starts
    /// from Heimdall's playback tap, ms after 510346 s host time): Jarvis at 160% through the MacBook speakers, each
    /// final judged while its sentence still played. The guard kept "me whenever you like." and "the speakers won't wake
    /// me or count as you, …" as the user's and the second woke the agent. Every word is Heimdall's echo.
    #[test]
    fn the_03_08_echo_at_160_percent_is_heimdalls_to_the_last_word() {
        const B: u64 = 510_346 * S;
        let heard = |line: &str| -> Vec<Word> {
            line.split(' ').map(|w| {
                let (text, t) = w.split_once('@').unwrap();
                let (a, b) = t.split_once('-').unwrap();
                Word { text: text.into(), start_ns: B + a.parse::<u64>().unwrap() * 1_000_000, end_ns: B + b.parse::<u64>().unwrap() * 1_000_000, confidence: 0.9 }
            }).collect()
        };
        let ms = |t: u64| B + t * 1_000_000;
        let first = heard("I'll@3118-3198 keep@3188-3358 talking@3358-3518 for@3518-3598 a@3598-3678 bit,@3673-3758 so@3843-3918 interrupt@3918-4238 me@4243-4318 whenever@4318-4558 you@4558-4878 like.@4878-5518");
        let spans = [
            played("Okay, let's test it.", ms(1400), ms(2500)),
            played("I'll keep talking for a bit, so interrupt me whenever you like.", ms(2500), u64::MAX),
        ];
        let v = judge(&first, &spans).unwrap();
        assert!(v.own(), "kept {:?}", text(&v.kept(&first)));
        let second = heard("If@5846-6166 echo@6518-6566 cancellation@6582-7126 works,@7160-7366 my@7356-7446 own@7501-7606 voice@7606-7846 coming@7846-8086 out@8061-8166 of@8166-8246 the@8246-8326 speakers@8471-8726 won't@8846-8966 wake@8886-9206 me@9241-9366 or@9457-9526 count@9586-9766 as@9766-9846 you,@9871-10086 and@10086-10246 won't@10269-10486 show@10486-10646 up@10646-10726 as@10726-10886 your@10986-11046 words@11116-11286 in@11286-11366 the@11396-11446 transcript.@11446-12886");
        let spans = [
            played("I'll keep talking for a bit, so interrupt me whenever you like.", ms(2500), ms(5850)),
            played("If echo cancellation works, my own voice coming out of the speakers won't wake me or count as you, and won't show up as your words in the transcript.", ms(5850), u64::MAX),
        ];
        let v = judge(&second, &spans).unwrap();
        assert!(v.own(), "kept {:?}", text(&v.kept(&second)));
        // At natural speed the same open sentence still matches (the slow end of the range).
        let slow = said("I'll keep talking for a bit, so interrupt me whenever you like.", 10 * S + 100_000_000, 340_000_000);
        assert!(judge(&slow, &[played("I'll keep talking for a bit, so interrupt me whenever you like.", 10 * S, u64::MAX)]).unwrap().own());
        // The user repeating its words seconds after it said them is still his.
        let late = said("interrupt me whenever you like", 18 * S, 250_000_000);
        assert!(!judge(&late, &[played("I'll keep talking for a bit, so interrupt me whenever you like.", 10 * S, u64::MAX)]).unwrap().any());
        // With the speed known (160%), the same echo is caught, and a repeat four seconds after it was said is his.
        assert!(judge_paced(&second, &spans, Some(1.6)).unwrap().own());
        let open = [played("I'll keep talking for a bit, so interrupt me whenever you like.", 10 * S, u64::MAX)];
        let repeat = said("interrupt me whenever you like", 15 * S + 500_000_000, 250_000_000);
        assert!(!judge_paced(&repeat, &open, Some(1.6)).unwrap().any(), "his repeat at 160%");
        assert!(judge_paced(&said("I'll keep talking for a bit, so interrupt me whenever you like.", 10 * S + 50_000_000, 300_000_000), &open, Some(1.0)).unwrap().own());
    }
    #[test]
    fn near_words_match() {
        assert_eq!(similarity("act", "ack"), NEAR);
        assert_eq!(similarity("installs", "install"), NEAR);
        assert_eq!(similarity("speaker", "speakers"), NEAR);
        assert_eq!(similarity("hack", "ack"), NEAR);
        assert!(similarity("stop", "start") < 0);
        assert!(similarity("no", "on") < 0, "two-letter words must be exact");
        assert_eq!(edit_distance(&['a', 'b'], &['b']), 1);
    }
}
