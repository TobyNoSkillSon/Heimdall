//! The conversation's rows laid out as marks in a body (`crate::island::chat`), iMessage's way: the user's words in blue
//! bubbles on the right, Heimdall's replies as plain text on the left (as the live reply below them reads), actions
//! as one quiet centred line, a notice as a small card with its pin. No names, times or headings: the side says who.

use super::geom::Rect;
use super::layout::LINE;
use super::measure::{ellipsize, wrap, Font, Measure};
use super::{prose, Mark, Rgba};
use crate::island::chat::{ActionKind, ChatKind, Heard, NoteShape};

/// The user's bubble: iMessage's blue in dark mode.
pub const BUBBLE: Rgba = Rgba([0x0A, 0x84, 0xFF, 255]);
/// A bubble's text inset, sideways and down.
const BUBBLE_X: f64 = 12.0;
const BUBBLE_Y: f64 = 7.0;
/// A bubble's text is at most this share of the column (iMessage leaves the other side's margin).
const BUBBLE_SHARE: f64 = 0.74;
/// Rows by the same side sit this close; a change of side leaves more.
const GAP_SAME: f64 = 8.0;
const GAP_TURN: f64 = 14.0;
/// An action row's height and a notice card's.
const ACTION_H: f64 = 24.0;
pub const CARD_H: f64 = 38.0;
/// Quiet ink: action words and the marks on a reply the user did not hear whole.
const QUIET: f64 = 0.5;
/// A reply that never reached the user is drawn this faint.
const UNHEARD: f64 = 0.45;
/// The most lines one of the user's bubbles shows.
const BUBBLE_LINES: usize = 12;

/// Laid-out rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    pub marks: Vec<Mark>,
    pub h: f64,
    /// Each notice card (its id) and where it is, from the block's top-left.
    pub cards: Vec<(String, Rect)>,
    /// The top of each of the user's bubbles.
    pub owners: Vec<f64>,
    /// Each message's words as written and its band (the column's width, the row's height, from the block's top-left):
    /// what a Copy on it puts on the clipboard (bug note 2026-10-30T22-16-36).
    pub texts: Vec<(Rect, String)>,
    /// The marks cut at row boundaries into pieces about `CHUNK_H` tall, each from its own top (`y0` to `y1` of the
    /// block): each is one bitmap, drawn only while it is in view, so a whole day scrolls without one huge image.
    pub chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chunk {
    pub y0: f64,
    pub y1: f64,
    pub marks: Vec<Mark>,
}

/// A chunk's height before the next row starts a new one.
pub const CHUNK_H: f64 = 1000.0;

/// Cuts `marks` (rows starting at `starts`: their top and first mark) into chunks; each chunk holds a clear spacer so
/// its drawing is exactly `y1 - y0` tall.
fn chunks(marks: &[Mark], starts: &[(f64, usize)], h: f64) -> Vec<Chunk> {
    let mut cuts: Vec<(f64, usize)> = vec![];
    for &(y, k) in starts {
        if cuts.last().is_none_or(|(y0, _)| y - y0 >= CHUNK_H) {
            cuts.push((y, k));
        }
    }
    let mut out = vec![];
    for (i, &(y0, k0)) in cuts.iter().enumerate() {
        let (y1, k1) = cuts.get(i + 1).copied().unwrap_or((h, marks.len()));
        let shift = |m: &Mark| match m.clone() {
            Mark::Fill { x, y, w, h, r, color } => Mark::Fill { x, y: y - y0, w, h, r, color },
            Mark::Text { text, x, y, line_h, font, color } => Mark::Text { text, x, y: y - y0, line_h, font, color },
            Mark::Symbol { name, size, palette, cx, cy } => Mark::Symbol { name, size, palette, cx, cy: cy - y0 },
        };
        let mut ms = vec![Mark::Fill { x: 0.0, y: 0.0, w: 0.0, h: (y1 - y0).max(0.0), r: 0.0, color: Rgba([0, 0, 0, 0]) }];
        ms.extend(marks[k0..k1.max(k0)].iter().map(shift));
        out.push(Chunk { y0, y1, marks: ms });
    }
    out
}

thread_local! {
    /// The last few blocks laid out (before and after the turn, the gallery's), by their rows, width and measure:
    /// scrolling and the voice's level re-lay the pull-down every event, never its conversation.
    static CACHE: std::cell::RefCell<Vec<(u64, std::rc::Rc<Block>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `lay`, cached.
pub fn lay_cached(m: &dyn Measure, rows: &[ChatKind], w: f64) -> std::rc::Rc<Block> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rows.hash(&mut h);
    w.to_bits().hash(&mut h);
    // The measure: what it says of a probe (Approx, AppKit and the fuzzers differ).
    m.width("Heimdall Mm 0", Font::Spoken).to_bits().hash(&mut h);
    let key = h.finish();
    if let Some(b) = CACHE.with(|c| c.borrow().iter().find(|(k, _)| *k == key).map(|(_, b)| b.clone())) {
        return b;
    }
    let b = std::rc::Rc::new(lay(m, rows, w));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.retain(|(k, _)| *k != key);
        c.insert(0, (key, b.clone()));
        c.truncate(4);
    });
    b
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Owner,
    Heimdall,
    Middle,
}

fn side(k: &ChatKind) -> Side {
    match k {
        ChatKind::Owner(_) => Side::Owner,
        ChatKind::Heimdall { .. } | ChatKind::Note { .. } => Side::Heimdall,
        ChatKind::Action { .. } => Side::Middle,
    }
}

/// The gap between a row on side `a` and the next on side `b`.
pub fn gap(a: Option<&ChatKind>, b: Option<&ChatKind>) -> f64 {
    match (a, b) {
        (Some(a), Some(b)) if side(a) == side(b) => GAP_SAME,
        (Some(_), Some(_)) => GAP_TURN,
        _ => 0.0,
    }
}

fn fin(x: f64) -> f64 {
    if x.is_finite() { x.max(0.0) } else { 0.0 }
}

/// One row laid out from y 0: its marks, its height, its notice card and whether it is the user's bubble.
#[derive(Clone, Debug, Default)]
struct Row {
    marks: Vec<Mark>,
    h: f64,
    card: Option<(String, Rect)>,
    owner: bool,
    /// The message's words, when it is one of the user's or Heimdall's (`Block::texts`).
    copy: Option<String>,
}

thread_local! {
    /// Each row laid out, by its content, the width and the measure (two generations, so a day's rows stay while
    /// the oldest unused go): a new message lays out that message alone, never the day again (audit 27 Oct: one new
    /// row re-laid all 400, 0.6-1.1 s on the main thread).
    static ROWS: std::cell::RefCell<RowCache> = std::cell::RefCell::new(RowCache::default());
}

/// Rows cached per generation.
const ROWS_MAX: usize = 2048;

#[derive(Default)]
struct RowCache {
    new: std::collections::HashMap<u64, std::rc::Rc<Row>>,
    old: std::collections::HashMap<u64, std::rc::Rc<Row>>,
}

impl RowCache {
    fn get(&mut self, key: u64) -> Option<std::rc::Rc<Row>> {
        if let Some(r) = self.new.get(&key) {
            return Some(r.clone());
        }
        let r = self.old.remove(&key)?;
        self.put(key, r.clone());
        Some(r)
    }
    fn put(&mut self, key: u64, r: std::rc::Rc<Row>) {
        if self.new.len() >= ROWS_MAX {
            self.old = std::mem::take(&mut self.new);
        }
        self.new.insert(key, r);
    }
}

/// Forgets every laid-out row and block (benches and tests that time a cold layout).
pub fn clear_cache() {
    ROWS.with(|c| *c.borrow_mut() = RowCache::default());
    CACHE.with(|c| c.borrow_mut().clear());
}

fn shifted(m: &Mark, dy: f64) -> Mark {
    match m.clone() {
        Mark::Fill { x, y, w, h, r, color } => Mark::Fill { x, y: y + dy, w, h, r, color },
        Mark::Text { text, x, y, line_h, font, color } => Mark::Text { text, x, y: y + dy, line_h, font, color },
        Mark::Symbol { name, size, palette, cx, cy } => Mark::Symbol { name, size, palette, cx, cy: cy + dy },
    }
}

/// Lays `rows` out in a column `w` wide, from y 0.
pub fn lay(m: &dyn Measure, rows: &[ChatKind], w: f64) -> Block {
    use std::hash::{Hash, Hasher};
    let w = fin(w).max(120.0);
    // The measure: what it says of a probe (Approx, AppKit and the fuzzers differ).
    let probe = m.width("Heimdall Mm 0", Font::Spoken).to_bits();
    let mut b = Block::default();
    let mut y = 0.0;
    let mut starts: Vec<(f64, usize)> = vec![];
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            y += gap(rows.get(i - 1), Some(row));
        }
        starts.push((y, b.marks.len()));
        let mut h = std::collections::hash_map::DefaultHasher::new();
        row.hash(&mut h);
        w.to_bits().hash(&mut h);
        probe.hash(&mut h);
        let key = h.finish();
        let r = match ROWS.with(|c| c.borrow_mut().get(key)) {
            Some(r) => r,
            None => {
                let r = std::rc::Rc::new(lay_row(m, row, w));
                ROWS.with(|c| c.borrow_mut().put(key, r.clone()));
                r
            }
        };
        if r.owner {
            b.owners.push(y);
        }
        b.marks.extend(r.marks.iter().map(|mk| shifted(mk, y)));
        if let Some((id, rect)) = &r.card {
            b.cards.push((id.clone(), Rect::new(rect.x, rect.y + y, rect.w, rect.h)));
        }
        if let Some(text) = &r.copy {
            b.texts.push((Rect::new(0.0, y, w, r.h), text.clone()));
        }
        y += r.h;
    }
    b.h = y;
    b.chunks = chunks(&b.marks, &starts, b.h);
    b
}

/// One row, from y 0.
fn lay_row(m: &dyn Measure, row: &ChatKind, w: f64) -> Row {
    let width = |s: &str, f: Font| fin(m.width(s, f));
    let y = 0.0;
    let mut r = Row::default();
    match row {
        ChatKind::Owner(text) => {
            let max = (w * BUBBLE_SHARE - 2.0 * BUBBLE_X).max(40.0);
            let (lines, tw) = wrap(m, text, Font::Spoken, max, BUBBLE_LINES);
            if lines.is_empty() {
                return r;
            }
            let tw = fin(tw).clamp(1.0, max);
            let (bw, bh) = (tw + 2.0 * BUBBLE_X, LINE * lines.len() as f64 + 2.0 * BUBBLE_Y);
            let x = w - bw;
            r.owner = true;
            r.copy = Some(text.trim().to_string());
            r.marks.push(Mark::Fill { x, y, w: bw, h: bh, r: (bh / 2.0).min(17.0), color: BUBBLE });
            for (k, line) in lines.into_iter().enumerate() {
                r.marks.push(Mark::Text { text: line, x: x + BUBBLE_X, y: y + BUBBLE_Y + LINE * k as f64, line_h: LINE, font: Font::Spoken, color: Rgba::WHITE });
            }
            r.h = bh;
        }
        ChatKind::Heimdall { text, heard } => {
            let paragraphs: Vec<String> = text.split('\n').map(str::to_string).collect();
            // The column's width: a reply's (420 pt), wider only beside a wide table (bug note 2026-10-25T23-17-01).
            let p = prose::lay(m, &paragraphs, w, LINE);
            if p.rows.is_empty() {
                return r;
            }
            let ink = if *heard == Heard::Never { Rgba::white(UNHEARD) } else { Rgba::WHITE };
            let last = p.rows.last().cloned().expect("not empty");
            for pr in p.rows {
                let font = pr.font();
                r.marks.push(Mark::Text { text: pr.text, x: pr.x, y: y + pr.y, line_h: LINE, font, color: ink });
            }
            // Not heard whole: a small mark after its last words (cut short, or never reached him).
            let mark = match heard {
                Heard::Whole => None,
                Heard::Cut => Some("waveform.badge.minus"),
                Heard::Never => Some("speaker.slash.fill"),
            };
            if let Some(name) = mark {
                let end = last.x + width(&last.text, last.font()) + 13.0;
                r.marks.push(Mark::Symbol { name: name.into(), size: 11.0, palette: vec![Rgba::white(QUIET)], cx: end.min(w - 7.0), cy: y + last.y + LINE / 2.0 });
            }
            r.h = p.height.max(LINE);
            r.copy = Some(text.trim().to_string());
        }
        ChatKind::Action { kind, text, answer } => {
            let (icon, palette) = match kind {
                ActionKind::Started => ("play.circle", vec![Rgba::white(QUIET)]),
                ActionKind::Finished => ("checkmark.circle.fill", vec![Rgba::WHITE, Rgba::GREEN]),
                ActionKind::Failed => ("xmark.circle.fill", vec![Rgba::WHITE, Rgba::RED]),
                ActionKind::Question => ("questionmark.bubble", vec![Rgba::white(QUIET)]),
            };
            let answer = answer.as_deref().map(str::trim).filter(|a| !a.is_empty());
            let aw = answer.map_or(0.0, |a| width(a, Font::Caption).min(160.0) + 18.0);
            let room = (w - 13.0 - 6.0 - if answer.is_some() { aw + 8.0 } else { 0.0 }).max(20.0);
            let words = ellipsize(m, text.split_whitespace().collect::<Vec<_>>().join(" ").as_str(), Font::Caption, room, false);
            let tw = width(&words, Font::Caption).min(room);
            let total = 13.0 + 6.0 + tw + if answer.is_some() { 8.0 + aw } else { 0.0 };
            let x0 = (w - total) / 2.0;
            let cy = y + ACTION_H / 2.0;
            r.marks.push(Mark::Symbol { name: icon.into(), size: 12.0, palette, cx: x0 + 6.5, cy });
            r.marks.push(Mark::Text { text: words, x: x0 + 19.0, y: cy - LINE / 2.0, line_h: LINE, font: Font::Caption, color: Rgba::white(QUIET) });
            if let Some(a) = answer {
                let ax = x0 + 19.0 + tw + 8.0;
                r.marks.push(Mark::Fill { x: ax, y: cy - 10.0, w: aw, h: 20.0, r: 10.0, color: Rgba::white(0.14) });
                let a = ellipsize(m, a, Font::Caption, aw - 18.0 + 0.5, false);
                r.marks.push(Mark::Text { text: a, x: ax + 9.0, y: cy - LINE / 2.0, line_h: LINE, font: Font::Caption, color: Rgba::WHITE });
            }
            r.h = ACTION_H;
        }
        ChatKind::Note { id, title, shape } => {
            let icon = match shape {
                NoteShape::Table => "tablecells",
                NoteShape::Text => "doc.text",
                NoteShape::Svg => "chart.xyaxis.line",
                NoteShape::Image => "photo",
            };
            let title = if title.trim().is_empty() { "Note" } else { title.trim() };
            let room = (w - 36.0 - 40.0).max(20.0);
            let title = ellipsize(m, title, Font::Label, room, false);
            let cw = (36.0 + width(&title, Font::Label).min(room) + 40.0).min(w);
            let cy = y + CARD_H / 2.0;
            r.marks.push(Mark::Fill { x: 0.0, y, w: cw, h: CARD_H, r: 12.0, color: Rgba::white(0.09) });
            r.marks.push(Mark::Symbol { name: icon.into(), size: 14.0, palette: vec![Rgba::WHITE], cx: 19.0, cy });
            r.marks.push(Mark::Text { text: title, x: 36.0, y: cy - LINE / 2.0, line_h: LINE, font: Font::Label, color: Rgba::WHITE });
            r.marks.push(Mark::Symbol { name: "pin.fill".into(), size: 11.0, palette: vec![Rgba::white(0.55)], cx: cw - 18.0, cy });
            r.card = Some((id.clone(), Rect::new(0.0, y, cw, CARD_H)));
            r.h = CARD_H;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::measure::{Approx, Fuzz, FuzzWidth};
    use super::super::Content;
    use super::*;

    fn rows() -> Vec<ChatKind> {
        vec![
            ChatKind::Owner("How much RAM is free?".into()),
            ChatKind::Heimdall { text: "About 60 GB.\n\nNo swap in use.".into(), heard: Heard::Whole },
            ChatKind::Action { kind: ActionKind::Question, text: "Clear the model cache?".into(), answer: Some("Yes".into()) },
            ChatKind::Note { id: "ram".into(), title: "Memory by app".into(), shape: NoteShape::Table },
            ChatKind::Heimdall { text: "I'll stop there.".into(), heard: Heard::Cut },
            ChatKind::Owner("Thanks".into()),
        ]
    }

    #[test]
    fn owner_is_on_the_right_heimdall_on_the_left_and_rows_never_overlap_or_leave_the_column() {
        let w = 420.0;
        let b = lay(&Approx, &rows(), w);
        assert_eq!(b.owners.len(), 2);
        let fills: Vec<_> = b.marks.iter().filter_map(|m| match m { Mark::Fill { x, y, w, h, color, .. } => Some((*x, *y, *w, *h, *color)), _ => None }).collect();
        let bubbles: Vec<_> = fills.iter().filter(|f| f.4 == BUBBLE).collect();
        assert_eq!(bubbles.len(), 2);
        for f in &bubbles {
            assert!((f.0 + f.2 - w).abs() < 1e-9, "right-aligned: {f:?}");
            assert!(f.2 <= w * BUBBLE_SHARE + 1e-9);
        }
        for m in &b.marks {
            if let Mark::Text { x, .. } = m {
                assert!(*x >= 0.0 && *x < w);
            }
        }
        // Rows go down: each row's top is below the last row's bottom.
        let mut tops: Vec<f64> = b.owners.clone();
        tops.sort_by(f64::total_cmp);
        assert!(tops[0] == 0.0 && tops[1] > 100.0);
        assert!(b.marks.iter().all(|m| m.bottom() <= b.h + 1.0));
        assert_eq!(b.cards.len(), 1);
        assert_eq!(b.cards[0].0, "ram");
        assert!(b.marks.iter().any(|m| matches!(m, Mark::Symbol { name, .. } if name == "waveform.badge.minus")), "the reply he talked over is marked");
    }

    #[test]
    fn a_long_day_is_cut_into_chunks_that_hold_every_mark_once_at_row_boundaries() {
        let day: Vec<ChatKind> = (0..120).flat_map(|_| rows()).collect();
        let b = lay(&Approx, &day, 420.0);
        assert!(b.h > 10.0 * CHUNK_H && b.chunks.len() > 10);
        assert_eq!(b.chunks.iter().map(|c| c.marks.len() - 1).sum::<usize>(), b.marks.len(), "each mark in one chunk (and a spacer each)");
        assert_eq!(b.chunks.first().map(|c| c.y0), Some(0.0));
        assert!((b.chunks.last().unwrap().y1 - b.h).abs() < 1e-9);
        for w in b.chunks.windows(2) {
            assert_eq!(w[0].y1, w[1].y0, "contiguous");
        }
        for c in &b.chunks {
            let tall = c.y1 - c.y0;
            assert!(tall < CHUNK_H + 400.0, "about a chunk tall: {tall}");
            let content = Content::Body { lines: vec![], font: Font::Spoken, color: Rgba::WHITE, line_h: LINE, h: 0.0, scroll: 0.0, notes: vec![], marks: c.marks.clone(), veil: 0.0 };
            assert!((content.body_full() - tall).abs() < 1e-9, "drawn exactly as tall as its window: {} vs {tall}", content.body_full());
        }
        assert!(std::rc::Rc::ptr_eq(&lay_cached(&Approx, &day, 420.0), &lay_cached(&Approx, &day, 420.0)), "laid out once");
    }

    #[test]
    fn an_unheard_reply_is_faint_and_marked() {
        let b = lay(&Approx, &[ChatKind::Heimdall { text: "You missed this.".into(), heard: Heard::Never }], 420.0);
        assert!(b.marks.iter().any(|m| matches!(m, Mark::Text { color, .. } if color.alpha() < 0.5)));
        assert!(b.marks.iter().any(|m| matches!(m, Mark::Symbol { name, .. } if name == "speaker.slash.fill")));
    }

    #[test]
    fn adversarial_widths_stay_finite_and_inside() {
        for f in [FuzzWidth::Zero, FuzzWidth::One, FuzzWidth::Max, FuzzWidth::MaxPlusOne, FuzzWidth::TenTimesMax] {
            for w in [0.0, 120.0, 420.0, f64::NAN, 1e9] {
                let b = lay(&Fuzz(f), &rows(), w);
                assert!(b.h.is_finite() && b.h > 0.0);
                for m in &b.marks {
                    assert!(m.bottom().is_finite(), "{f:?} {w}: {m:?}");
                }
            }
        }
    }

    /// Audit 27 Oct: one new row re-laid the whole day (0.6-1.1 s). Each row is cached: appending one measures only
    /// that row.
    #[test]
    fn a_new_row_lays_out_only_that_row() {
        use super::super::measure::tests::Summing;
        let day: Vec<ChatKind> = (0..60).flat_map(|_| rows()).collect();
        clear_cache();
        let m = Summing::new();
        let first = lay(&m, &day, 420.0);
        let all = m.calls.get();
        let mut more = day.clone();
        more.push(ChatKind::Heimdall { text: "One more reply, with a second sentence that wraps onto another line of the column.".into(), heard: Heard::Whole });
        let m = Summing::new();
        let next = lay(&m, &more, 420.0);
        let one = Summing::new();
        clear_cache();
        let _ = lay(&one, &more[more.len() - 1..], 420.0);
        assert!(m.calls.get() <= one.calls.get() + 2, "{} calls for one new row (that row alone: {}, the day: {all})", m.calls.get(), one.calls.get());
        assert_eq!(next.marks[..first.marks.len()], first.marks[..], "the rows before it are as they were");
        assert!(next.h > first.h);
    }
}
