//! A reply's text as the user reads it on the island (bug note 2026-10-21T19-21-01: "it's just one long paragraph and I'm
//! unable to read through this"): each line as written starts its own line, a blank line is a paragraph gap, a list
//! line ("- ", "* ", "• ", "1. ", "2) ") keeps a bullet or its number with its wrapped lines hung under its text, and
//! the first sentence (the answer) is bold when more follows it. Markdown's `**`, backticks and heading marks are
//! dropped (a heading reads bold). Used by the collapsed lines and the pulled-down reply alike.

use super::measure::{ellipsize, wrap, Font, Measure};

/// The space a blank line leaves between paragraphs (half a line: they read apart without a hole).
pub const PARA_GAP: f64 = 10.0;
/// The space between list items, and between a list and the line next to it.
pub const ITEM_GAP: f64 = 4.0;
/// The answer's font: the spoken font's size, bold.
pub const STRONG: Font = Font::Strong;
/// A first sentence longer than this is not a one-line answer; it stays in the body's weight.
const ANSWER_MAX: usize = 160;

/// One laid-out line: its text, its left edge (a list item's wrapped lines hang under its text) and its top, in
/// points from the text's top-left; each is `line_h` tall.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRow {
    pub text: String,
    pub x: f64,
    pub y: f64,
    /// Bold (`STRONG`): the answer or a heading.
    pub strong: bool,
}

impl TextRow {
    pub fn font(&self) -> Font {
        if self.strong {
            STRONG
        } else {
            Font::Spoken
        }
    }
}

/// Rows, the widest row's right edge, and the whole height.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prose {
    pub rows: Vec<TextRow>,
    pub widest: f64,
    pub height: f64,
    /// More than one paragraph or any list item: it reads as a page (left-aligned), not a caption (centred).
    pub page: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Block {
    Para { text: String, strong: bool },
    Item { marker: String, text: String },
}

/// A list line's marker (a bullet for "-", "*", "•", "–"; the number as written for "1." or "1)") and its text.
fn item(line: &str) -> Option<(String, &str)> {
    let t = line.trim_start();
    for m in ["- ", "* ", "• ", "– ", "— "] {
        if let Some(r) = t.strip_prefix(m) {
            let r = r.trim_start();
            return (!r.is_empty()).then(|| ("•".to_string(), r));
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits) {
        let rest = &t[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            let r = r.trim_start();
            return (!r.is_empty()).then(|| (format!("{}.", &t[..digits]), r));
        }
    }
    None
}

/// Markdown marks a reply may carry that the island draws as plain text.
fn plain(s: &str) -> String {
    s.replace("**", "").replace('`', "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A heading line ("# Title" to "###### Title"): its words.
fn heading(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        let r = t[hashes..].strip_prefix(' ')?.trim();
        return (!r.is_empty()).then_some(r);
    }
    None
}

/// The byte index just past the first sentence's end (".", "!" or "?" before a space), when there is one and it is a
/// real sentence: two words or more, not cut at an abbreviation or a lone initial.
pub fn first_sentence_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let next = i + c.len_utf8();
        if next >= bytes.len() || !text[next..].starts_with(char::is_whitespace) {
            continue;
        }
        let head = &text[..i];
        let last = head.rsplit(char::is_whitespace).next().unwrap_or("");
        if c == '.' {
            let lower = last.to_lowercase();
            let abbreviation = matches!(lower.as_str(), "e.g" | "i.e" | "vs" | "etc" | "mr" | "mrs" | "dr" | "st" | "no" | "approx");
            if abbreviation || last.chars().count() < 2 {
                continue;
            }
        }
        if head.split_whitespace().count() < 2 {
            return None;
        }
        return Some(next);
    }
    None
}

fn blocks(lines: &[String]) -> Vec<(Block, f64)> {
    let mut out: Vec<(Block, f64)> = vec![];
    let mut blank = false;
    for line in lines {
        if line.trim().is_empty() {
            blank = !out.is_empty();
            continue;
        }
        let block = if let Some(h) = heading(line) {
            Block::Para { text: plain(h), strong: true }
        } else if let Some((marker, text)) = item(line) {
            Block::Item { marker, text: plain(text) }
        } else {
            Block::Para { text: plain(line), strong: false }
        };
        let gap = match (&block, out.last()) {
            (_, None) => 0.0,
            _ if blank => PARA_GAP,
            (Block::Item { .. }, _) | (_, Some((Block::Item { .. }, _))) => ITEM_GAP,
            _ => 0.0,
        };
        blank = false;
        out.push((block, gap));
    }
    // The answer first: the reply's first sentence, semibold, on its own line(s), when more follows it.
    if let Some((Block::Para { text, strong: false }, _)) = out.first() {
        let more_blocks = out.len() > 1;
        if let Some(end) = first_sentence_end(text) {
            let (answer, rest) = (text[..end].trim().to_string(), text[end..].trim().to_string());
            if answer.chars().count() <= ANSWER_MAX && (!rest.is_empty() || more_blocks) {
                out[0] = (Block::Para { text: answer, strong: true }, 0.0);
                if !rest.is_empty() {
                    out.insert(1, (Block::Para { text: rest, strong: false }, 0.0));
                }
            }
        } else if more_blocks && text.chars().count() <= ANSWER_MAX && text.split_whitespace().count() >= 2 && ends_sentence(text) {
            // A first line that is one whole sentence, with more lines after it.
            out[0].0 = Block::Para { text: text.clone(), strong: true };
        }
    }
    out
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end().ends_with(['.', '!', '?'])
}

/// Lays out `lines` (the reply split at its line breaks) in a column `max_w` wide.
pub fn lay(m: &dyn Measure, lines: &[String], max_w: f64, line_h: f64) -> Prose {
    let mut rows: Vec<TextRow> = vec![];
    let mut y = 0.0;
    let mut widest: f64 = 0.0;
    let blocks = blocks(lines);
    let page = blocks.iter().any(|(b, gap)| matches!(b, Block::Item { .. }) || *gap >= PARA_GAP);
    for (k, (block, gap)) in blocks.into_iter().enumerate() {
        if k > 0 {
            y += gap;
        }
        match block {
            Block::Para { text, strong } => {
                let font = if strong { STRONG } else { Font::Spoken };
                let (ls, w) = wrap(m, &text, font, max_w, usize::MAX);
                for l in ls {
                    rows.push(TextRow { text: l, x: 0.0, y, strong });
                    y += line_h;
                }
                widest = widest.max(w);
            }
            Block::Item { marker, text } => {
                let lead = format!("{marker}\u{2002}");
                let indent = width(m, &lead);
                let (ls, w) = wrap(m, &text, Font::Spoken, (max_w - indent).max(1.0), usize::MAX);
                for (j, l) in ls.into_iter().enumerate() {
                    let (text, x) = if j == 0 { (format!("{lead}{l}"), 0.0) } else { (l, indent) };
                    rows.push(TextRow { text, x, y, strong: false });
                    y += line_h;
                }
                widest = widest.max(indent + w);
            }
        }
    }
    Prose { rows, widest: widest.min(max_w), height: y, page }
}

fn width(m: &dyn Measure, s: &str) -> f64 {
    let w = m.width(s, Font::Spoken);
    if w.is_finite() {
        w.max(0.0)
    } else {
        0.0
    }
}

/// `prose` kept to its first `max` rows; when rows were cut, the last kept row ends with an ellipsis.
pub fn keep(m: &dyn Measure, mut prose: Prose, max: usize, max_w: f64, line_h: f64) -> Prose {
    if prose.rows.len() <= max || max == 0 {
        return prose;
    }
    prose.rows.truncate(max);
    if let Some(last) = prose.rows.last_mut() {
        last.text = ellipsize(m, &last.text, last.font(), (max_w - last.x).max(1.0), true);
        prose.height = last.y + line_h;
    }
    prose
}
