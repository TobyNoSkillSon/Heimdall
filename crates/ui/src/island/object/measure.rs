//! Text measuring. The layout asks a `Measure` for widths; the app answers with CoreText (cached per string and
//! font, `mac/island`), the simulator with a fast deterministic estimate, and the fuzz tests with adversarial widths.

/// The island's type roles (DESIGN.md §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    /// 1–3 word labels: 15 pt semibold.
    Label,
    /// Spoken sentences and lines to read: 15 pt medium, 20 pt lines.
    Spoken,
    /// A reply's answer (its first sentence) or heading among lines to read: 15 pt bold.
    Strong,
    /// Ear numerals: 13 pt semibold, monospaced digits.
    EarNum,
    /// Button labels: 14 pt semibold.
    Button,
    /// Key hints inside a button: 12 pt medium.
    Key,
    /// A reader unit at this many points, medium (30, shrinking to 24).
    Reader(u8),
    /// The reader's title while it waits: 22 pt semibold.
    ReaderTitle,
    /// The paused reader's sentence: 17 pt medium.
    Sentence,
    /// Blue label capsules of pointing marks: 13 pt semibold.
    Small,
    /// The conversation's action rows: 13 pt medium.
    Caption,
    /// A question card's question: 21 pt semibold.
    Question,
    /// A question card's option label: 16 pt medium.
    OptionLabel,
    /// A question card's option description: 13 pt regular.
    Describe,
    /// A question card's step chip: 12.5 pt semibold.
    Chip,
    /// A question card's Other field and the composer: 15 pt regular.
    Field,
    /// A path or login item an option grants (`agent_notice::GrantItem`): 12 pt SF Mono regular, every character
    /// its own column, so a space or a look-alike is never lost.
    Path,
}

impl Font {
    pub fn size(self) -> f64 {
        match self {
            Font::Label | Font::Spoken | Font::Strong => 15.0,
            Font::EarNum | Font::Small | Font::Caption => 13.0,
            Font::Button => 14.0,
            Font::Key => 12.0,
            Font::Reader(s) => s as f64,
            Font::ReaderTitle => 22.0,
            Font::Sentence => 17.0,
            Font::Question => 21.0,
            Font::OptionLabel => 16.0,
            Font::Describe => 13.0,
            Font::Chip => 12.5,
            Font::Field => 15.0,
            Font::Path => 12.0,
        }
    }
    /// The weight, as AppKit's `NSFontWeight` value.
    pub fn weight(self) -> f64 {
        match self {
            Font::Label | Font::EarNum | Font::Button | Font::ReaderTitle | Font::Small => 0.3,
            Font::Spoken | Font::Key | Font::Reader(_) | Font::Sentence | Font::Caption => 0.23,
            Font::Strong => 0.4,
            Font::Question | Font::Chip => 0.3,
            Font::OptionLabel => 0.3,
            Font::Describe | Font::Field | Font::Path => 0.0,
        }
    }
    pub fn monospaced_digits(self) -> bool {
        matches!(self, Font::EarNum)
    }
    /// Every character monospaced (SF Mono), not only the digits.
    pub fn monospaced(self) -> bool {
        matches!(self, Font::Path)
    }
}

/// Width of a string in a font, in points.
pub trait Measure {
    fn width(&self, text: &str, font: Font) -> f64;
    /// Each word's width and the width a space adds between two words, when a run of words joined by single spaces
    /// measures as the sum (CoreText: advances add, and nothing kerns across a space; `mac/island/measure.rs` proves
    /// it on a broad text set). The wrap then measures each word once and sums, instead of shaping every candidate
    /// line (52k CoreText calls for a 400-row day, audit 27 Oct). `None`: every run is measured whole.
    fn words(&self, words: &[&str], font: Font) -> Option<(Vec<f64>, f64)> {
        let _ = (words, font);
        None
    }
}

/// Widths of runs of words joined by single spaces (`words[i..j]`): prefix sums when the measure offers per-word
/// widths, else each run measured whole.
pub(crate) struct Spans<'a> {
    m: &'a dyn Measure,
    words: &'a [&'a str],
    font: Font,
    /// Prefix sums of the words' widths, and the space's.
    sums: Option<(Vec<f64>, f64)>,
}

/// A summed width this close to a decision's edge (does it fit?) is measured whole, so every break is the one the
/// whole-line measure makes.
const EDGE: f64 = 0.5;

/// A wrap capped at `max_lines` with more than this many words a line measures whole: the cap cuts it after a few
/// lines, and per-word widths of a huge paste would fill the cache for nothing.
const SUM_WORDS_PER_LINE: usize = 40;

impl<'a> Spans<'a> {
    /// Spans for a wrap into at most `max_lines` lines.
    fn for_wrap(m: &'a dyn Measure, words: &'a [&'a str], font: Font, max_lines: usize) -> Spans<'a> {
        if max_lines != usize::MAX && words.len() > max_lines.saturating_mul(SUM_WORDS_PER_LINE) {
            return Spans { m, words, font, sums: None };
        }
        Spans::new(m, words, font)
    }
    pub(crate) fn new(m: &'a dyn Measure, words: &'a [&'a str], font: Font) -> Spans<'a> {
        let sums = m
            .words(words, font)
            .filter(|(ws, space)| ws.len() == words.len() && space.is_finite() && ws.iter().all(|w| w.is_finite()))
            .map(|(ws, space)| {
                let mut p = Vec::with_capacity(ws.len() + 1);
                p.push(0.0);
                for w in ws {
                    p.push(p[p.len() - 1] + w);
                }
                (p, space)
            });
        Spans { m, words, font, sums }
    }
    pub(crate) fn text(&self, i: usize, j: usize) -> String {
        self.words[i..j].join(" ")
    }
    fn whole(&self, i: usize, j: usize) -> f64 {
        if j <= i {
            return 0.0;
        }
        self.m.width(&self.text(i, j), self.font)
    }
    /// The run's width.
    pub(crate) fn width(&self, i: usize, j: usize) -> f64 {
        match &self.sums {
            _ if j <= i => 0.0,
            Some((p, space)) => p[j] - p[i] + space * (j - i - 1) as f64,
            None => self.whole(i, j),
        }
    }
    /// The run's width for a decision against `max_w`: measured whole when the sum is within `EDGE` of it.
    fn near(&self, i: usize, j: usize, max_w: f64) -> f64 {
        let w = self.width(i, j);
        if self.sums.is_some() && (w - max_w).abs() <= EDGE {
            self.whole(i, j)
        } else {
            w
        }
    }
    fn fits(&self, i: usize, j: usize, max_w: f64) -> bool {
        let w = self.near(i, j, max_w);
        w.is_finite() && w <= max_w
    }
}

/// A deterministic estimate close to SF Pro's advance widths (within a few per cent on English text), for the
/// simulator: microseconds per string and no AppKit.
#[derive(Clone, Copy, Debug, Default)]
pub struct Approx;

impl Measure for Approx {
    fn width(&self, text: &str, font: Font) -> f64 {
        let em: f64 = text
            .chars()
            .map(|c| match c {
                'i' | 'l' | 'j' | '|' | '!' | '\'' | '.' | ',' | ':' | ';' => 0.26,
                'f' | 't' | 'r' | 'I' | ' ' | '(' | ')' => 0.33,
                'm' | 'w' | 'M' | 'W' => 0.86,
                '0'..='9' => 0.6,
                'A'..='Z' => 0.66,
                c if c.is_ascii() => 0.55,
                // Wide scripts and emoji.
                c if (c as u32) >= 0x2E80 => 1.0,
                _ => 0.6,
            })
            .sum();
        let weight = 1.0 + (font.weight() - 0.23) * 0.4;
        em * font.size() * weight
    }
}

/// Adversarial widths that do not depend on fonts: every string measures the same chosen width.
#[derive(Clone, Copy, Debug)]
pub struct Fuzz(pub FuzzWidth);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuzzWidth {
    Zero,
    One,
    /// Exactly the widest column a line may take (420 pt).
    Max,
    MaxPlusOne,
    TenTimesMax,
}

/// The widest a line of text may be.
pub const MAX_LINE: f64 = 420.0;

impl Measure for Fuzz {
    fn width(&self, text: &str, _font: Font) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        match self.0 {
            FuzzWidth::Zero => 0.0,
            FuzzWidth::One => 1.0,
            FuzzWidth::Max => MAX_LINE,
            FuzzWidth::MaxPlusOne => MAX_LINE + 1.0,
            FuzzWidth::TenTimesMax => MAX_LINE * 10.0,
        }
    }
}

/// Greedy word wrap into at most `max_lines` lines of at most `max_w` points; the last line is ellipsized when the
/// text does not fit. Returns the lines and the widest line's width.
pub fn wrap(m: &dyn Measure, text: &str, font: Font, max_w: f64, max_lines: usize) -> (Vec<String>, f64) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let s = Spans::for_wrap(m, &words, font, max_lines);
    let (lines, widest) = wrap_in(&s, max_w, max_lines);
    (lines.into_iter().map(|l| l.text(&s)).collect(), widest)
}

/// A path or login item (`Font::Path`) as lines of at most `max_w`, every character kept: joined, the lines are the
/// text exactly. A line breaks after the last `/` that leaves it at least half full, else at the widest fit, never next
/// to a space (so no space hides at a line's edge) and never before a combining mark (an accent stays on its letter).
/// Never cut, never an ellipsis. The widest fit is found by halving, so a long path costs a few measures a line.
pub fn wrap_path(m: &dyn Measure, text: &str, font: Font, max_w: f64) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let joins = |c: char| matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F);
    // A break before `chars[k]` keeps both sides visible and whole.
    let clean = |k: usize| chars[k - 1] != ' ' && chars[k] != ' ' && !joins(chars[k]);
    let fits = |a: usize, b: usize| m.width(&chars[a..b].iter().collect::<String>(), font) <= max_w;
    let mut lines = vec![];
    let mut at = 0;
    while at < chars.len() {
        let mut end = chars.len();
        if !fits(at, end) {
            // The longest prefix that fits (at least one character), by halving: widths grow with the prefix.
            let (mut lo, mut hi) = (at + 1, chars.len());
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if fits(at, mid) { lo = mid } else { hi = mid }
            }
            end = lo;
            let room = end - at;
            if let Some(slash) = (at + room.div_ceil(2)..=end).rev().find(|&k| k > at && chars[k - 1] == '/' && clean(k)) {
                end = slash;
            } else if let Some(k) = (at + 1..=end).rev().find(|&k| clean(k)) {
                end = k;
            }
        }
        lines.push(chars[at..end].iter().collect());
        at = end;
    }
    lines
}

/// A wrapped line: a run of the words, or a run cut with an ellipsis.
enum Line {
    Run(usize, usize),
    Cut(String),
}

impl Line {
    fn text(self, s: &Spans) -> String {
        match self {
            Line::Run(i, j) => s.text(i, j),
            Line::Cut(t) => t,
        }
    }
    fn width(&self, s: &Spans) -> f64 {
        match self {
            Line::Run(i, j) => s.width(*i, *j),
            Line::Cut(t) => s.m.width(t, s.font),
        }
    }
}

fn wrap_in(s: &Spans, max_w: f64, max_lines: usize) -> (Vec<Line>, f64) {
    let n = s.words.len();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    // The line being filled: words[start..i].
    let mut start: Option<usize> = None;
    let mut truncated = false;
    for i in 0..n {
        match start {
            None => start = Some(i),
            Some(a) if s.fits(a, i + 1, max_w) => {}
            Some(a) => {
                runs.push((a, i));
                start = Some(i);
                if runs.len() == max_lines {
                    truncated = true;
                    start = None;
                    break;
                }
            }
        }
    }
    if let Some(a) = start {
        if runs.len() == max_lines {
            truncated = true;
        } else {
            runs.push((a, n));
        }
    }
    // A word wider than the column, or text that did not fit in the lines: ellipsize (the last line always says
    // the text goes on).
    let k = runs.len();
    let mut lines: Vec<Line> = runs
        .iter()
        .enumerate()
        .map(|(r, &(i, j))| {
            let force = truncated && r + 1 == k;
            if force || !s.fits(i, j, max_w) {
                Line::Cut(ellipsize(s.m, &s.text(i, j), s.font, max_w, force))
            } else {
                Line::Run(i, j)
            }
        })
        .collect();
    // Text that fits in its lines is re-broken evenly, so a paragraph never ends on a lone word ("step.").
    if !truncated && k >= 2 {
        if let Some(even) = balance(s, max_w, k) {
            lines = even.into_iter().map(|(i, j)| Line::Run(i, j)).collect();
        }
    }
    let widest = lines.iter().map(|l| l.width(s)).fold(0.0, f64::max).min(max_w);
    (lines, widest)
}

/// Longest text (in words) that is re-broken; longer text keeps the greedy lines.
const BALANCE_WORDS: usize = 48;

/// The same `k` lines as the greedy wrap (the fewest possible), broken to minimise the slack of every line but the
/// last, with a lone word as the last line costed as the worst case. `None` when the text cannot be broken so.
/// As `wrap`, with the lines as even as they can be (the widest as narrow as possible): centred captions (a spoken
/// sentence) read as one block and keep the island narrow, never a full line over a short tail.
pub fn wrap_even(m: &dyn Measure, text: &str, font: Font, max_w: f64, max_lines: usize) -> (Vec<String>, f64) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let s = Spans::for_wrap(m, &words, font, max_lines);
    let (lines, w) = wrap_in(&s, max_w, max_lines);
    let k = lines.len();
    let n = words.len();
    let whole_runs = lines.iter().all(|l| matches!(l, Line::Run(..)));
    if k < 2 || !whole_runs || n > BALANCE_WORDS {
        return (lines.into_iter().map(|l| l.text(&s)).collect(), w);
    }
    // The least widest line over every way to cut the words into `k` lines (each line fitting).
    let mut best = vec![vec![f64::INFINITY; n + 1]; k + 1];
    let mut cut = vec![vec![0usize; n + 1]; k + 1];
    best[0][0] = 0.0;
    for r in 1..=k {
        for j in r..=n {
            if r == k && j != n {
                continue;
            }
            for i in (r - 1)..j {
                if !best[r - 1][i].is_finite() {
                    continue;
                }
                let lw = s.near(i, j, max_w);
                if !lw.is_finite() || lw > max_w {
                    continue;
                }
                let c = best[r - 1][i].max(lw);
                if c < best[r][j] - 1e-9 {
                    best[r][j] = c;
                    cut[r][j] = i;
                }
            }
        }
    }
    if !best[k][n].is_finite() {
        return (lines.into_iter().map(|l| l.text(&s)).collect(), w);
    }
    let mut out = Vec::with_capacity(k);
    let (mut r, mut j) = (k, n);
    while r > 0 {
        let i = cut[r][j];
        out.push((i, j));
        j = i;
        r -= 1;
    }
    out.reverse();
    let widest = out.iter().map(|&(i, j)| s.width(i, j)).fold(0.0, f64::max).min(max_w);
    (out.into_iter().map(|(i, j)| s.text(i, j)).collect(), widest)
}

fn balance(s: &Spans, max_w: f64, k: usize) -> Option<Vec<(usize, usize)>> {
    let n = s.words.len();
    if n < k || n > BALANCE_WORDS {
        return None;
    }
    // cost[r][j]: least cost of the first `j` words in `r` lines; cut[r][j]: where the r-th line starts.
    let mut cost = vec![vec![f64::INFINITY; n + 1]; k + 1];
    let mut cut = vec![vec![0usize; n + 1]; k + 1];
    cost[0][0] = 0.0;
    for r in 1..=k {
        let ends: Vec<usize> = if r == k { vec![n] } else { (r..n).collect() };
        for j in ends {
            for i in (r - 1)..j {
                if !cost[r - 1][i].is_finite() {
                    continue;
                }
                let w = s.near(i, j, max_w);
                if !(w.is_finite() && w <= max_w) {
                    continue;
                }
                let bad = if r == k {
                    if j - i == 1 && n > 1 { 4.0 * max_w * max_w } else { 0.0 }
                } else {
                    (max_w - w).powi(2)
                };
                if cost[r - 1][i] + bad < cost[r][j] {
                    cost[r][j] = cost[r - 1][i] + bad;
                    cut[r][j] = i;
                }
            }
        }
    }
    if !cost[k][n].is_finite() {
        return None;
    }
    let mut out = Vec::with_capacity(k);
    let (mut r, mut j) = (k, n);
    while r > 0 {
        let i = cut[r][j];
        out.push((i, j));
        j = i;
        r -= 1;
    }
    out.reverse();
    Some(out)
}

fn fits(m: &dyn Measure, s: &str, font: Font, max_w: f64) -> bool {
    let w = m.width(s, font);
    w.is_finite() && w <= max_w
}

/// `s` cut to fit `max_w` with an ellipsis (always ends with one when `force`).
pub fn ellipsize(m: &dyn Measure, s: &str, font: Font, max_w: f64, force: bool) -> String {
    if !force && fits(m, s, font, max_w) {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    // Binary search the longest prefix that fits with the ellipsis.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
        if fits(m, &cand, font, max_w) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A granted path is drawn whole: every character kept in order, no line wider than the room, no line starting or
    /// ending with a space, a break after a `/` where one leaves the line at least half full.
    #[test]
    fn a_granted_path_wraps_whole_and_never_hides_a_space() {
        let paths = [
            "~/Backups/T3 Code (Nightly).app",
            "~/Library/Application Support/t3code/very-long-folder-name-without-any-slash-in-it-at-all-1234567890",
            "/Users/t/Work/Projects/Heimdall/lab/drama3/runs/2026-10-29 night  two spaces/checkpoints/step 4000",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "~/写真/古い バックアップ/2026",
            "swiftpm-testing-helper",
            "~/Backups/cafe\u{301}-cafe\u{301}-cafe\u{301}-cafe\u{301}-cafe\u{301}-cafe\u{301}-cafe\u{301}-cafe\u{301}-cafe\u{301}",
        ];
        for max_w in [120.0, 260.0, 456.0] {
            for p in paths {
                let lines = wrap_path(&Approx, p, Font::Path, max_w);
                assert_eq!(lines.concat(), p, "every character kept at {max_w}");
                for (k, l) in lines.iter().enumerate() {
                    assert!(!l.is_empty());
                    assert!(Approx.width(l, Font::Path) <= max_w || l.chars().count() == 1, "{l:?} fits {max_w}");
                    if k + 1 < lines.len() {
                        assert!(!l.ends_with(' ') && !lines[k + 1].starts_with(' '), "{p:?} at {max_w}: {lines:?}");
                        assert!(!lines[k + 1].starts_with('\u{301}'), "an accent stays on its letter: {lines:?}");
                    }
                }
            }
        }
        assert_eq!(wrap_path(&Approx, "~/Backups/T3 Code (Nightly).app", Font::Path, 456.0), vec!["~/Backups/T3 Code (Nightly).app"]);
        let narrow = wrap_path(&Approx, "/Users/t/Work/Projects/Heimdall/lab/drama3/runs", Font::Path, 200.0);
        assert!(narrow[0].ends_with('/'), "breaks after a slash: {narrow:?}");
    }

    #[test]
    fn wraps_long_text_into_three_lines_and_ellipsizes() {
        let text = "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback. ".repeat(6);
        let (lines, w) = wrap(&Approx, &text, Font::Spoken, 420.0, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('…'));
        assert!(w <= 420.0);
        for l in &lines {
            assert!(Approx.width(l, Font::Spoken) <= 420.0, "{l}");
        }
    }

    #[test]
    fn a_paragraph_never_ends_on_a_lone_word_when_the_same_lines_can_hold_it() {
        let text = "Heimdall walks you through the whole build pipeline one stage at a time, step by step.";
        let greedy_width = Approx.width("Heimdall walks you through the whole build pipeline one stage at a time, step", Font::Spoken);
        // A column where greedy breaks off "step by step." (the last line would be "step." alone).
        let max_w = greedy_width * 0.98;
        let (lines, _) = wrap(&Approx, text, Font::Spoken, max_w, 3);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.last().unwrap().contains(' '), "no lone last word: {lines:?}");
        assert_eq!(lines.join(" "), text, "every word kept, in order");
        for l in &lines {
            assert!(Approx.width(l, Font::Spoken) <= max_w, "{l}");
        }
        // Text that is cut keeps its ellipsis (not re-broken).
        let (cut, _) = wrap(&Approx, &text.repeat(3), Font::Spoken, max_w, 2);
        assert!(cut.last().unwrap().ends_with('…'), "{cut:?}");
    }

    #[test]
    fn one_word_wider_than_the_column_is_cut() {
        let (lines, _) = wrap(&Approx, &"x".repeat(200), Font::Spoken, 100.0, 3);
        assert_eq!(lines.len(), 1);
        assert!(Approx.width(&lines[0], Font::Spoken) <= 100.0);
        let (lines, w) = wrap(&Fuzz(FuzzWidth::TenTimesMax), "a b c d", Font::Spoken, 420.0, 3);
        assert_eq!(lines.len(), 3);
        assert!(w <= 420.0);
        let (lines, w) = wrap(&Approx, "", Font::Spoken, 420.0, 3);
        assert!(lines.is_empty() && w == 0.0);
    }

    /// A spoken sentence breaks into even lines (the widest as narrow as possible), never a full line over a short
    /// tail; the words and their order are kept and every line fits.
    #[test]
    fn spoken_sentences_break_evenly() {
        let text = "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.";
        let (plain, pw) = wrap(&Approx, text, Font::Spoken, 420.0, 6);
        let (even, ew) = wrap_even(&Approx, text, Font::Spoken, 420.0, 6);
        assert_eq!(even.len(), plain.len());
        assert_eq!(even.join(" "), text);
        assert!(ew <= pw + 1e-9, "{ew} {pw}");
        let widths: Vec<f64> = even.iter().map(|l| Approx.width(l, Font::Spoken)).collect();
        let (lo, hi) = widths.iter().fold((f64::INFINITY, 0.0f64), |(a, b), w| (a.min(*w), b.max(*w)));
        assert!(hi - lo < 0.35 * hi, "{even:?} {widths:?}");
        for f in [Fuzz(FuzzWidth::Zero), Fuzz(FuzzWidth::TenTimesMax)] {
            let (lines, w) = wrap_even(&f, text, Font::Spoken, 420.0, 6);
            assert!(w.is_finite() && !lines.is_empty());
        }
        assert_eq!(wrap_even(&Approx, "", Font::Spoken, 420.0, 6).0.len(), 0);
    }


    /// Approx's widths with per-word sums offered (Approx adds per character, so the sum is exact), counting the
    /// runs of more than one word it is asked to measure whole: each is one CoreText call in the app.
    pub(crate) struct Summing {
        pub(crate) lines: std::cell::Cell<usize>,
        pub(crate) calls: std::cell::Cell<usize>,
    }
    impl Summing {
        pub(crate) fn new() -> Summing {
            Summing { lines: 0.into(), calls: 0.into() }
        }
    }
    impl Measure for Summing {
        fn width(&self, text: &str, font: Font) -> f64 {
            self.calls.set(self.calls.get() + 1);
            if text.trim().contains(' ') {
                self.lines.set(self.lines.get() + 1);
            }
            Approx.width(text, font)
        }
        fn words(&self, words: &[&str], font: Font) -> Option<(Vec<f64>, f64)> {
            Some((words.iter().map(|w| self.width(w, font)).collect(), Approx.width(" ", font)))
        }
    }

    /// Audit 27 Oct: wrapping shaped every candidate line (52k CoreText calls for a 400-row day). With per-word
    /// widths a paragraph is measured word by word; a whole line is shaped only at a fit's edge.
    /// Review 27 Oct: a huge paste into a bubble capped at a few lines is not measured word by word.
    #[test]
    fn a_huge_capped_text_measures_no_words_past_its_cap() {
        let text = (0..5000).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let m = Summing::new();
        let (lines, _) = wrap(&m, &text, Font::Spoken, 300.0, 12);
        assert_eq!(lines.len(), 12);
        assert!(m.calls.get() < 1000, "{} widths measured for 12 lines", m.calls.get());
    }

    #[test]
    fn a_long_paragraph_is_wrapped_on_word_widths_not_shaped_line_by_line() {
        let text = "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback; the tests pass except two timeouts that fail identically on the base commit.".repeat(2);
        let m = Summing::new();
        let (lines, _) = wrap(&m, &text, Font::Spoken, 420.0, usize::MAX);
        assert!(lines.len() >= 3, "{lines:?}");
        assert!(m.lines.get() <= 4, "{} whole lines shaped for {} lines", m.lines.get(), lines.len());
        let m = Summing::new();
        let (even, _) = wrap_even(&m, "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.", Font::Spoken, 420.0, 6);
        assert!(even.len() >= 2);
        assert!(m.lines.get() <= 4, "{} whole lines shaped", m.lines.get());
        // The same lines as shaping each line whole.
        assert_eq!(wrap(&Summing::new(), &text, Font::Spoken, 300.0, 6).0, wrap(&Approx, &text, Font::Spoken, 300.0, 6).0);
    }
}
