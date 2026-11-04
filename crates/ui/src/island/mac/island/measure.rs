//! Text widths for the island's layout, from AppKit (the same fonts the rasteriser draws with), cached per string
//! and role: microseconds per cached string. Lines are wrapped on per-word widths (`Measure::words`): each distinct
//! word is shaped once and a line's width is the sum of its words and spaces (identical to shaping the line:
//! `words_sum_to_the_whole_line` below).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use objc2::rc::Retained;
use objc2_app_kit::{NSAttributedStringNSStringDrawing as _, NSFont};

use super::super::util::attributed;
use crate::island::object::{Font, Measure};

/// The AppKit font for a role.
pub(crate) fn ns_font(f: Font) -> Retained<NSFont> {
    if f.monospaced() {
        NSFont::monospacedSystemFontOfSize_weight(f.size(), f.weight())
    } else if f.monospaced_digits() {
        NSFont::monospacedDigitSystemFontOfSize_weight(f.size(), f.weight())
    } else {
        NSFont::systemFontOfSize_weight(f.size(), f.weight())
    }
}

/// CoreText widths, cached.
#[derive(Default)]
pub(crate) struct AppKitMeasure {
    cache: RefCell<Widths>,
    fonts: RefCell<HashMap<Font, Retained<NSFont>>>,
}

/// Cached strings per generation: a day's distinct words (about 1-2k) and lines fit in one; when it fills, it becomes
/// the old generation (still read, a hit moving back) and the oldest is dropped, so the cache never empties in the
/// middle of a layout (a full clear made a 400-row layout wipe it 12-24 times, audit 27 Oct) and never grows without
/// bound (at most twice this).
const CACHE_MAX: usize = 8192;

#[derive(Default)]
struct Widths {
    new: HashMap<Font, HashMap<String, f64>>,
    old: HashMap<Font, HashMap<String, f64>>,
    n: usize,
}

impl Widths {
    fn get(&mut self, text: &str, font: Font) -> Option<f64> {
        if let Some(w) = self.new.get(&font).and_then(|m| m.get(text)) {
            return Some(*w);
        }
        let w = self.old.get_mut(&font).and_then(|m| m.remove(text))?;
        self.put(text, font, w);
        Some(w)
    }
    fn put(&mut self, text: &str, font: Font, w: f64) {
        if self.n >= CACHE_MAX {
            self.old = std::mem::take(&mut self.new);
            self.n = 0;
            ROTATIONS.fetch_add(1, Relaxed);
        }
        self.new.entry(font).or_default().insert(text.to_owned(), w);
        self.n += 1;
    }
}

/// Counters for the bench (`heimdall-ui-demo --bench-chat`) and the tests: width calls, CoreText misses, their time,
/// cache rotations and the characters shaped.
pub(crate) static CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static MISSES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static MISS_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static ROTATIONS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static MISS_CHARS: AtomicUsize = AtomicUsize::new(0);

/// The bench's stall (`BENCH_STALL_MS`): the next CoreText call sleeps this long, once, to check that a slow apply
/// delays the opening motion and never skips it. Zero in the app.
pub(crate) static STALL_NEXT_MS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reset_counters() {
    for c in [&CALLS, &MISSES, &ROTATIONS, &MISS_CHARS] {
        c.store(0, Relaxed);
    }
    MISS_NS.store(0, Relaxed);
}

pub(crate) fn counters() -> String {
    format!(
        "calls={} misses={} miss_ms={:.1} clears={} miss_chars={}",
        CALLS.load(Relaxed),
        MISSES.load(Relaxed),
        MISS_NS.load(Relaxed) as f64 / 1e6,
        ROTATIONS.load(Relaxed),
        MISS_CHARS.load(Relaxed)
    )
}

/// Where SF switches to its display cut.
const DISPLAY_PT: f64 = 20.0;

impl AppKitMeasure {
    fn shaped(&self, text: &str, font: Font) -> f64 {
        let t0 = std::time::Instant::now();
        let stall = STALL_NEXT_MS.swap(0, Relaxed);
        if stall > 0 {
            std::thread::sleep(std::time::Duration::from_millis(stall));
        }
        let f = self.fonts.borrow_mut().entry(font).or_insert_with(|| ns_font(font)).clone();
        let w = attributed(text, &f, &objc2_app_kit::NSColor::whiteColor()).size().width;
        MISS_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        MISSES.fetch_add(1, Relaxed);
        MISS_CHARS.fetch_add(text.chars().count(), Relaxed);
        w
    }
}

impl Measure for AppKitMeasure {
    fn width(&self, text: &str, font: Font) -> f64 {
        CALLS.fetch_add(1, Relaxed);
        if text.is_empty() {
            return 0.0;
        }
        if let Some(w) = self.cache.borrow_mut().get(text, font) {
            return w;
        }
        let w = self.shaped(text, font);
        self.cache.borrow_mut().put(text, font, w);
        w
    }

    fn words(&self, words: &[&str], font: Font) -> Option<(Vec<f64>, f64)> {
        // SF's text cut (under 20 pt) sums exactly, in every script tried; its display cut (20 pt and up: the
        // question, the reader) kerns across spaces by up to 8 pt a line, so those runs are shaped whole.
        if font.size() >= DISPLAY_PT {
            return None;
        }
        // What a space adds between two words: "a a" less two "a" (no kerning pair touches the space).
        let space = self.width("a a", font) - 2.0 * self.width("a", font);
        Some((words.iter().map(|w| self.width(w, font)).collect(), space))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::object::measure::{wrap, wrap_even};

    /// Lines of many scripts and shapes: English with kerning-prone capitals and punctuation, Polish, German,
    /// French, numbers and units, code and paths, URLs, CJK (no spaces, one run), Arabic and Hebrew (right to
    /// left, mixed with Latin), Devanagari and Thai (shaping), emoji (ZWJ families, flags, skin tones, keycaps), and
    /// combining marks.
    pub(crate) fn broad_text() -> Vec<String> {
        let mut v: Vec<String> = [
            "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.",
            "AV WA Ta To Yo P. T. \"Quoted\" 'single' (paren) [bracket] {brace} — dash – en-dash … ellipsis!",
            "Your Vella, Vireo and Arsenal builds are green; Verdict's tests: 412 passed, 2 failed (timeouts).",
            "Zażółć gęślą jaźń. Pociąg do Poznania odjeżdża o 17:45 z peronu 3, a bilet kosztuje 49,90 zł.",
            "Größenänderung der Fenster ist schön; Straße, Übermaß und Öl. Ça va très bien, merci à vous.",
            "Free memory 61.4 GB of 128 GB, swap 0 B, GPU 37 %, battery 84 % (charging), 62 °C, 1,024 × 768 px.",
            "Run `cargo test -p ui -j2` in the Heimdall checkout; see crates/ui/src/island/object/measure.rs:139.",
            "Open https://example.com/a/very/long/path?query=value&other=thing#fragment and then http://x.io too.",
            "今日は晴れです。明日の会議は午後三時に移動しました。 我们明天见。 오늘 회의는 세 시입니다.",
            "مرحبا بك في Heimdall، الاجتماع غدا في الساعة الثالثة. שלום, הפגישה מחר בשלוש אחר הצהריים.",
            "नमस्ते, कल की बैठक तीन बजे है। สวัสดีครับ พรุ่งนี้ประชุมบ่ายสามโมง",
            "Family 👨‍👩‍👧‍👦 flags 🇵🇱 🇬🇧 🇺🇸 skin 👍🏽 👋🏿 keycap 1️⃣ #️⃣ hearts ❤️‍🔥 and 🧑🏻‍💻 at work ✅ ⚠️ 🎉",
            "Combining: e\u{301}le\u{300}ve, n\u{303}o, a\u{308}, Z\u{335}a\u{336}l\u{337}g\u{338}o\u{339} text.",
            "I I I I i i i l l l W W W M M M m m m 1 1 1 0 0 0 . , ; : ! ? - _ / \\ | @ # $ % ^ & * + =",
            "A very-long-hyphenated-compound-word-that-is-wider-than-the-column-for-sure-and-then-some-more-words here.",
            "Short.",
            "Two words",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Generated sentences from the bench's vocabulary, many lengths.
        let words = [
            "the", "build", "is", "green", "and", "I", "kept", "old", "one", "as", "rollback", "Dave", "User", "Verdict", "Wi-Fi", "Thursday",
            "Arsenal", "Vella", "AV", "Poznań", "gigabytes", "sixty-two", "I'll", "you're", "today's", "GPU", "memory", "48", "3.5", "99%",
        ];
        let mut x = 7u64;
        for len in 3..90 {
            let mut s = vec![];
            for _ in 0..len {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                s.push(words[(x >> 33) as usize % words.len()]);
            }
            v.push(s.join(" ") + ".");
        }
        v
    }

    /// A measure that shapes every run whole (no per-word widths): what the layout measured before.
    struct Whole<'a>(&'a AppKitMeasure);
    impl Measure for Whole<'_> {
        fn width(&self, text: &str, font: Font) -> f64 {
            self.0.width(text, font)
        }
    }

    /// The per-word sum is the whole line's width, on every script, and every wrap breaks the same lines.
    #[test]
    fn words_sum_to_the_whole_line_and_every_wrap_is_identical() {
        let report = identical_on_broad_text();
        assert!(report.starts_with("identical"), "{report}");
    }

    pub(crate) fn identical_on_broad_text() -> String {
        let m = AppKitMeasure::default();
        let whole = Whole(&m);
        let mut worst: f64 = 0.0;
        let mut lines = 0;
        for text in broad_text() {
            let fonts = [Font::Spoken, Font::Strong, Font::Caption, Font::Label, Font::Sentence, Font::OptionLabel, Font::Describe, Font::Chip, Font::Small, Font::Field, Font::Question, Font::ReaderTitle];
            for font in fonts {
                // The sum against the whole run, for every prefix of the words (the display cut has no sums).
                let words: Vec<&str> = text.split_whitespace().collect();
                if let Some((ws, space)) = m.words(&words, font) {
                    let mut sum = 0.0;
                    for j in 0..words.len() {
                        sum += ws[j] + if j > 0 { space } else { 0.0 };
                        let real = m.width(&words[..=j].join(" "), font);
                        worst = worst.max((sum - real).abs());
                    }
                } else {
                    assert!(font.size() >= DISPLAY_PT, "{font:?} sums");
                }
                for max_w in [60.0, 97.5, 140.0, 200.0, 310.6, 420.0, 700.0] {
                    for lines_max in [1, 3, 6, usize::MAX] {
                        let a = wrap(&whole, &text, font, max_w, lines_max);
                        let b = wrap(&m, &text, font, max_w, lines_max);
                        if a.0 != b.0 || (a.1 - b.1).abs() > 1e-6 {
                            return format!("wrap differs: {font:?} {max_w} {lines_max} {text:?}\n{a:?}\n{b:?}");
                        }
                        let a = wrap_even(&whole, &text, font, max_w, lines_max);
                        let b = wrap_even(&m, &text, font, max_w, lines_max);
                        if a.0 != b.0 || (a.1 - b.1).abs() > 1e-6 {
                            return format!("wrap_even differs: {font:?} {max_w} {lines_max} {text:?}\n{a:?}\n{b:?}");
                        }
                        lines += b.0.len();
                    }
                }
            }
        }
        if worst > 1e-3 {
            return format!("sum error {worst:.4} pt");
        }
        format!("identical: {lines} lines wrapped both ways; worst sum error {worst:.2e} pt")
    }
}
