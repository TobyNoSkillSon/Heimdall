//! Property tests over random texts at every dial speed, plus targeted timing laws.

use rsvp::{dial, plan, tokenize, Config, Pause, Plan};

/// Small deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const FUNCTION: &[&str] = &["the", "a", "of", "to", "in", "is", "and", "it", "for", "on", "with", "that", "was", "be", "as", "at", "by", "we", "i", "się", "w", "na"];
const CONTENT: &[&str] = &[
    "build", "tests", "model", "failed", "loading", "limit", "minutes", "target", "problem", "change", "embedding", "harness",
    "implementation", "reading", "speed", "default", "comprehension", "privacy", "rota", "invoice", "dependency", "notes", "dog",
    "readers", "studies", "straightforward", "Heimdall", "Marta", "GPU", "CI", "state-of-the-art", "don't", "it's", "User's",
    "240", "3.14", "1,000", "40%", "10:30", "2026-10-18", "https://example.com/a?b=c", "fast_read.rs", "spotkanie", "dzisiaj",
];
const AFTER: &[&str] = &["", "", "", "", "", "", ",", ".", "!", "?", ";", ":", " —", "...", ")"];

fn random_text(rng: &mut Rng, words: usize) -> String {
    let mut s = String::new();
    for i in 0..words {
        if i > 0 {
            s.push_str(if rng.below(40) == 0 { "\n\n" } else { " " });
        }
        let w = if rng.below(2) == 0 { rng.pick(FUNCTION) } else { rng.pick(CONTENT) };
        if rng.below(30) == 0 {
            s.push('(');
        }
        s.push_str(w);
        s.push_str(rng.pick(AFTER));
    }
    s
}

fn budget(p: &Plan, wpm: u32) -> f64 {
    p.words as f64 * 60_000.0 / wpm as f64
}

/// Maps each unit to its tokens by word count.
fn unit_tokens(text: &str, p: &Plan) -> Vec<Vec<rsvp::Token>> {
    let tokens = tokenize(text);
    let mut out = Vec::new();
    let mut at = 0;
    for u in &p.units {
        out.push(tokens[at..at + u.word_count].to_vec());
        at += u.word_count;
    }
    assert_eq!(at, tokens.len(), "units cover every token exactly once");
    out
}

#[test]
fn random_texts_hold_every_invariant_at_every_dial_speed() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let c0 = Config::default();
    let speeds = dial();
    let mut plans = 0;
    for case in 0..400 {
        let n = 1 + rng.below(if case % 10 == 0 { 600 } else { 80 });
        let text = random_text(&mut rng, n);
        let base = plan(&text, &c0);
        let groups = unit_tokens(&text, &base);
        for &wpm in &speeds {
            let c = Config { wpm, ..c0.clone() };
            let p = plan(&text, &c);
            plans += 1;
            // Duration law: the whole message takes words / wpm, pauses included.
            let target = budget(&p, wpm);
            assert!((p.total_ms as f64 - target).abs() <= 1.0, "case {case} at {wpm}: {} ms vs {target:.1} ms", p.total_ms);
            let sum: u64 = p.units.iter().map(|u| (u.duration_ms + u.pause_after_ms) as u64).sum();
            assert_eq!(sum, p.total_ms as u64);
            // Retiming the same units equals planning afresh.
            let mut r = base.clone();
            r.retime(wpm);
            assert_eq!(r, p, "case {case}: retime({wpm}) differs from a fresh plan");
            for (u, unit) in p.units.iter().enumerate() {
                assert!(!unit.text.trim().is_empty(), "empty unit");
                assert!(unit.word_count >= 1 && unit.word_count <= c.max_chunk_words);
                assert!(unit.word_count == 1 || unit.text.chars().count() <= c.max_chunk_chars, "too wide: {:?}", unit.text);
                assert!(unit.duration_ms + 1 >= c.min_unit_ms && unit.duration_ms <= c.max_unit_ms + 1, "{:?} {} ms", unit.text, unit.duration_ms);
                assert!(unit.focus_index < unit.text.chars().count());
                assert!(unit.text.chars().nth(unit.focus_index).is_some_and(|ch| ch.is_alphanumeric() || unit.text.chars().all(|x| !x.is_alphanumeric())));
                let g = &groups[u];
                // Never across a sentence end or a punctuation pause.
                assert!(g.iter().all(|t| t.sentence == unit.sentence_index), "{:?} crosses a sentence", unit.text);
                assert!(g[..g.len() - 1].iter().all(|t| t.pause == Pause::None), "{:?} crosses a pause", unit.text);
                // Punctuation adds a pause (the last unit's ending belongs to the reader).
                let last = u + 1 == p.units.len();
                if !last {
                    assert_eq!(unit.pause_after_ms > 0, g.last().unwrap().pause != Pause::None, "{:?}", unit.text);
                } else {
                    assert_eq!(unit.pause_after_ms, 0);
                }
            }
            // Sentence starts and seeking agree with the units.
            for (u, unit) in p.units.iter().enumerate() {
                let s = p.sentence_start(u);
                assert_eq!(p.units[s].sentence_index, unit.sentence_index);
                assert!(s == 0 || p.units[s - 1].sentence_index < unit.sentence_index);
                let pos = p.at(p.start_ms(u) + unit.duration_ms.saturating_sub(1)).unwrap();
                assert_eq!(pos.unit, u);
            }
        }
        // Deterministic.
        assert_eq!(plan(&text, &c0), base);
    }
    assert!(plans > 5000);
}

#[test]
fn faster_dial_means_shorter_message() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/build-failure.txt")).unwrap();
    let totals: Vec<u32> = dial().into_iter().map(|wpm| plan(&text, &Config { wpm, ..Config::default() }).total_ms).collect();
    assert!(totals.windows(2).all(|w| w[0] > w[1]), "{totals:?}");
}

/// Pseudo-words of one syllable per two letters, all outside the frequency lists, so only length differs.
fn pseudo(letters: usize) -> String {
    let syl = ["ba", "ko", "ti", "mu", "re", "lo", "pa", "ni"];
    let mut s: String = (0..letters.div_ceil(2)).map(|i| syl[i % syl.len()]).collect();
    s.truncate(letters);
    format!("z{s}")
}

#[test]
fn longer_words_show_longer() {
    let mut rng = Rng(42);
    for _ in 0..50 {
        let lengths: Vec<usize> = (0..40).map(|_| 2 + rng.below(16)).collect();
        let text = lengths.iter().map(|&n| pseudo(n)).collect::<Vec<_>>().join(" ");
        for wpm in dial() {
            let p = plan(&text, &Config { wpm, ..Config::default() });
            assert_eq!(p.units.len(), lengths.len(), "pseudo-words never chunk");
            for i in 1..lengths.len() {
                for j in 1..lengths.len() {
                    let (a, b) = (&p.units[i], &p.units[j]);
                    if lengths[i] > lengths[j] {
                        assert!(a.duration_ms >= b.duration_ms, "{} {} ms vs {} {} ms", a.text, a.duration_ms, b.text, b.duration_ms);
                    }
                    if lengths[i] >= lengths[j] + 2 && a.duration_ms < Config::default().max_unit_ms && b.duration_ms > Config::default().min_unit_ms {
                        assert!(a.duration_ms > b.duration_ms, "{} vs {}", a.text, b.text);
                    }
                }
            }
        }
    }
}

fn duration_of(p: &Plan, text: &str) -> u32 {
    p.units.iter().find(|u| u.text == text).unwrap_or_else(|| panic!("no unit {text:?} in {:?}", p.units.iter().map(|u| &u.text).collect::<Vec<_>>())).duration_ms
}

#[test]
fn frequent_words_are_faster_than_rare_ones_of_the_same_length() {
    // "think" is common, "blark" is not; both five letters, one syllable.
    let p = plan("zoba think zoba blark zoba", &Config::default());
    assert!(duration_of(&p, "think") < duration_of(&p, "blark"));
}

#[test]
fn numbers_names_and_acronyms_are_slower_than_words_of_their_length() {
    let c = Config::default();
    let p = plan("zoba house, zoba 48213, zoba", &c);
    assert!(duration_of(&p, "48213,") > duration_of(&p, "house,"));
    let p = plan("zoba water, zoba Marta, zoba", &c);
    assert!(duration_of(&p, "Marta,") > duration_of(&p, "water,"));
    let p = plan("zoba bat, zoba GPU, zoba", &c);
    assert!(duration_of(&p, "GPU,") > duration_of(&p, "bat,"));
}

#[test]
fn punctuation_pauses_grow_with_the_break() {
    let c = Config::default();
    let pause_after = |text: &str, unit: &str| plan(text, &c).units.iter().find(|u| u.text.starts_with(unit)).unwrap().pause_after_ms;
    let words = "zoba zobi zobu zobe zoby zobo zobul zobil";
    let none = pause_after(words, "zobu");
    let comma = pause_after(&words.replace("zobu", "zobu,"), "zobu");
    let stop = pause_after(&words.replace("zobu zobe", "zobu. Zobe"), "zobu");
    let para = pause_after(&words.replace("zobu zobe", "zobu.\n\nZobe"), "zobu");
    assert_eq!(none, 0);
    assert!(0 < comma && comma < stop && stop < para, "{comma} {stop} {para}");
}

#[test]
fn first_unit_after_a_pause_shows_longer() {
    let p = plan("Zoba zoba zoba zoba. Zoba zoba zoba zoba, zoba zoba zoba.", &Config::default());
    let d: Vec<u32> = p.units.iter().map(|u| u.duration_ms).collect();
    assert!(d[0] > d[1], "first unit of the message {d:?}");
    assert!(d[4] > d[5] && d[4] > d[2], "first after a sentence {d:?}");
    assert!(d[8] > d[9] && d[8] < d[4], "after a clause, half the extra {d:?}");
}

#[test]
fn chunking_never_spans_long_words_and_respects_width() {
    for max in [8, 10, 14, 20] {
        let c = Config { max_chunk_chars: max, ..Config::default() };
        let p = plan("of the implementation in a minute to be honest it is a test of the system for the five pilot users", &c);
        for u in &p.units {
            assert!(u.word_count == 1 || u.text.chars().count() <= max, "{max}: {:?}", u.text);
            assert!(!u.text.contains("implementation") || u.text == "implementation" || u.word_count == 2 && u.text.split(' ').next().unwrap().len() <= 3);
        }
    }
}

#[test]
fn custom_limits_hold() {
    let c = Config { min_unit_ms: 180, max_unit_ms: 600, wpm: 450, ..Config::default() };
    let text = "a of the comprehension https://example.com/a/very/long/path/that/goes/on is it";
    let p = plan(text, &c);
    assert!(p.units.iter().all(|u| u.duration_ms + 1 >= 180 && u.duration_ms <= 601), "{:?}", p.units);
    assert!((p.total_ms as f64 - budget(&p, 450)).abs() <= 1.0);
}
