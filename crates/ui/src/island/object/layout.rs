//! The layout engine: a `Scene` (what the island says) becomes a `Layout` (the shape's target size, every item's
//! rect and content, the click targets), on DESIGN.md §2's template:
//!
//! ```text
//!  [ leading ear 50 ][        NOTCH 185        ][ trailing ear 50 ]   ← y 0–32, inside the menu-bar band
//!  [               content column, centred under the notch         ]   ← y 32 down
//! ```
//!
//! Numbers are the mockup renderer's (`lab/island-design/render/render.swift`), so the app draws what the user saw.
//! Everything is centred on the notch line or mirrored about it; the only deliberate exception is a reader unit,
//! whose recognition letter sits on the centre line.

use std::sync::Arc;

use super::geom::{Display, Rect, FLARE};
use super::measure::{wrap, wrap_even, Font, Measure};
use super::prose::{self, TextRow};
use super::{Capsule, Content, Fx, Mark, Placed, Rgba, Target, Token, NOTE_GAP, NOTE_HEAD};
use crate::agent_notice::{AskQuestion, GrantItem, QuestionAnswer};
use crate::island::SendState;

/// Side padding of column content: a shape is its content plus this on each side, never narrower than the notch.
pub const PAD: f64 = 22.0;
/// The most side padding any state keeps (a note's body sits 28 in): the simulator's sized-to-content bound (I21).
pub const PAD_MAX: f64 = 28.0;
/// The most room any state keeps below its lowest element (a question's answers): I21's bound.
pub const PAD_BOTTOM_MAX: f64 = 16.0;
pub const LINE: f64 = 20.0;
/// The tallest bitmap the island draws (Core Animation's texture limit, `raster::Canvas`), in pixels.
pub const MAX_RASTER_PX: f64 = 16384.0;
/// The pulled-down reply's last line when the reply is longer than one bitmap holds.
pub const REPLY_CUT: &str = "… the rest is in the Agent window";
/// Lines of the pulled-down reply one bitmap holds at `scale` (409 at 2×): its body is drawn once as one image.
#[cfg(test)]
pub fn reply_max_lines(scale: f64) -> usize {
    (MAX_RASTER_PX / (LINE * scale.max(1.0))).floor() as usize
}
pub const EAR: f64 = 50.0;
/// Vella's mark in an ear and over its words: its app icon's tile is mostly margin, so it draws larger than a symbol
/// for its bars to read at a symbol's weight.
pub const VELLA_EAR: f64 = 22.0;
pub const VELLA_STACK: f64 = 26.0;
pub const MAX_W: f64 = 480.0;
/// The pulled-down turn holding a note (a table) may grow this wide, so a table shows its columns whole (bug note
/// 2026-10-20T23-12-49: "the table has a fixed size"), and never closer than `SCREEN_MARGIN` to a screen's sides.
pub const WIDE_W: f64 = 700.0;
pub const SCREEN_MARGIN: f64 = 160.0;
/// The fade over a scrolling body's bottom while more is below.
pub const FADE_H: f64 = 22.0;
/// How far the veil (the body blurred under the top bar) reaches into the window, where the sharp lines fade in.
pub const VEIL_INTO: f64 = FADE_H;
/// A note's close X takes clicks only while this much of it is in view (a trackpad tap's height).
pub const CLOSE_MIN_H: f64 = 24.0;
/// The widest the pulled-down turn may be on `d`.
pub fn reply_max_w(d: &Display) -> f64 {
    let w = d.width - 2.0 * SCREEN_MARGIN;
    if w.is_finite() { w.clamp(MAX_W, WIDE_W) } else { MAX_W }
}
/// The widest a note's own drawing may be on `d` (the app draws it before it is laid out).
pub fn note_max_w(d: &Display) -> f64 {
    reply_max_w(d) - 2.0 * PAD_MAX
}
/// Lip, stack and working geometry.
pub const LIP_H: f64 = 56.0;
pub const LIP_Y: f64 = 44.0;
/// The pulled-down reply's pin, this far in from the shape's right edge (clear of its rounded corner).
pub const PIN_INSET: f64 = 32.0;
/// A note's pin, this far left of its close X (centre to centre).
pub const NOTE_PIN_GAP: f64 = 28.0;
/// How far the peek lip shows below the notch's bottom edge.
pub const PEEK_BELOW: f64 = 14.0;
pub const STACK_ICON_Y: f64 = 48.0;
pub const STACK_WORD_Y: f64 = 74.0;
pub const STACK_H: f64 = 94.0;
pub const LIP_BAND: f64 = 22.0;
/// The composer at the foot of the pulled-down conversation: its height and the gap above it.
pub const COMPOSER_H: f64 = 36.0;
pub const COMPOSER_GAP: f64 = 8.0;
/// A drop in the composer: its thumbnail's side, the gap between two, and their inset from the composer's edge.
pub const ATTACH: f64 = 52.0;
const ATTACH_GAP: f64 = 6.0;
const ATTACH_INSET: f64 = 8.0;

/// How much taller the composer is for what was dropped into it: one row of thumbnails above its line.
fn attach_row(attached: &[crate::island::drop::DropItem]) -> f64 {
    if attached.is_empty() { 0.0 } else { ATTACH_INSET + ATTACH }
}

/// The composer's attachments (a drop on the pull-down while he types, `Event::ComposerAttach`): a thumbnail each in
/// one row at the composer's top-left, an image as itself, anything else as its symbol on a dark tile, each with a
/// remove badge (`Target::ComposerDetach`); more than the row holds end in a "+n" tile. Ids follow the item, so the
/// rest glide into place when one goes.
fn attachments(b: &mut B, attached: &[crate::island::drop::DropItem], composer: Rect) {
    use std::hash::{Hash, Hasher};
    if attached.is_empty() {
        return;
    }
    let fit = (((composer.w - 2.0 * ATTACH_INSET + ATTACH_GAP) / (ATTACH + ATTACH_GAP)).floor() as usize).max(1);
    let shown = if attached.len() > fit { fit - 1 } else { attached.len() };
    let y = composer.y + ATTACH_INSET;
    let x_of = |k: usize| composer.x + ATTACH_INSET + k as f64 * (ATTACH + ATTACH_GAP);
    for (k, item) in attached.iter().take(shown).enumerate() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        item.hash(&mut h);
        let id = format!("reply.composer.attach.{:x}", h.finish());
        let x = x_of(k);
        let image = item.kind == crate::island::drop::DropKind::Image;
        b.push(&id, Content::Thumb { path: item.path.clone(), image, symbol: item.kind.symbol().into(), radius: 10.0 }, Rect::new(x, y, ATTACH, ATTACH));
        let (bx, by) = (x + ATTACH - 11.0, y + 11.0);
        b.glyph(&format!("{id}.x"), &Glyph::new("xmark.circle.fill").colors(&[Rgba::WHITE, Rgba([0, 0, 0, 170])]), 15.0, bx, by);
        b.targets.push((Target::ComposerDetach(k), Rect::centered(bx, by, 24.0, 24.0)));
    }
    if shown < attached.len() {
        let x = x_of(shown);
        b.push("reply.composer.attach.more", Content::Panel { fill: Rgba::white(0.10), ring: None, radius: 10.0 }, Rect::new(x, y, ATTACH, ATTACH));
        let more = format!("+{}", attached.len() - shown);
        let tw = b.width(&more, Font::Field);
        b.push("reply.composer.attach.more.n", Content::Text { lines: vec![more], font: Font::Field, color: Rgba::white(0.7), line_h: 22.0 }, Rect::new(x + (ATTACH - tw) / 2.0, y + (ATTACH - 22.0) / 2.0, tw.max(1.0), 22.0));
    }
}
/// The pulled-down conversation's working row (thinking dots under the last message) while the agent's turn runs.
pub const WORKING_ROW: f64 = 26.0;
/// The bottom band's click target: the band and a little above it (a trackpad tap needs 24 pt).
pub const BAND_TARGET: f64 = 30.0;
/// The reader keeps one width for every unit (units cut in instantly; the shape never resizes per word): its widest
/// unit, `CHUNK_SIDE` either side of the centre line, plus the side padding.
pub const READER_W: f64 = 2.0 * CHUNK_SIDE + 2.0 * PAD;
pub const READER_WORD_Y: f64 = 74.0;
pub const READER_CTRL_Y: f64 = 128.0;
/// Either side of a reader unit may reach this far from the centre line before its type shrinks (30 → 24 pt).
pub const CHUNK_SIDE: f64 = 190.0;
pub const DRAW_H: f64 = 88.0;
pub const DRAW_BTN_Y: f64 = 59.0;
pub const DRAW_BTN: (f64, f64) = (112.0, 34.0);
/// Between the voice's last line and a question docked under it (centre gap beyond the lines).
pub const Q_DOCK_GAP: f64 = 18.0;
/// The question card (DESIGN.md's island at 580 pt): its width, its inner width (22 pt padding either side), and its
/// rows. Numbers from `lab/ui-renders/ask-card` (the design's mockup, 24 Oct).
pub const ASK_W: f64 = 580.0;
const ASK_INNER: f64 = ASK_W - 2.0 * PAD;
/// Whether `text` stays on one line as a card's question (no countdown), by the simulator's width estimate: for callers
/// that keep a question to one line (the voice cards, `crates/app/src/voice_suggest.rs`). The estimate runs about 8 %
/// wide of AppKit for this font; the limit is set from real renders (`question_line_tests`), on the safe side.
pub fn question_fits_one_line(text: &str) -> bool {
    use super::measure::{Approx, Font, Measure};
    Approx.width(text, Font::Question) <= ASK_INNER * 1.07
}
const ASK_PAD_TOP: f64 = 16.0;
const ASK_HEAD: f64 = 22.0;
const ASK_CHIP_H: f64 = 26.0;
const ASK_CHIP_GAP: f64 = 6.0;
const ASK_Q_LINE: f64 = 25.0;
const ASK_OPT_H: f64 = 50.0;
const ASK_OPT_GAP: f64 = 5.0;
/// A granting option's well (approval grants, 4 Nov): its padding, a line of SF Mono 12, the gap between items, the
/// text's indent after the symbol, and the room below the well inside the row.
const GRANT_PAD_X: f64 = 10.0;
const GRANT_PAD_Y: f64 = 7.0;
const GRANT_LINE: f64 = 17.0;
const GRANT_ITEM_GAP: f64 = 3.0;
const GRANT_INDENT: f64 = 20.0;
const GRANT_BELOW: f64 = 12.0;
/// The well runs from the label's column less its padding to 12 pt inside the row: the text gets the rest.
const GRANT_TEXT_W: f64 = ASK_INNER - 52.0 + GRANT_PAD_X - 12.0 - 2.0 * GRANT_PAD_X - GRANT_INDENT;
const ASK_BLUE: Rgba = Rgba([0x39, 0x87, 0xE5, 255]);
/// The Other row's placeholder: the one line the user reads before he types his own answer.
const OTHER_PLACEHOLDER: &str = "Type your own answer";
/// The send arrow inside the Other row's right edge (lit once he types); its hit area is a whole 44 pt target.
const ASK_ARROW_D: f64 = 26.0;
const ASK_TARGET: f64 = 44.0;
/// A step chip's leading mark (tick or open dot) and its gap to the label; the gap between the chips and Send.
const ASK_MARK: f64 = 15.0;
const ASK_SEND_GAP: f64 = 10.0;
/// A countdown card's time left: m:ss beside a ring that empties, at the right of the question's first line, the ring
/// over the options' dots (with step chips, at the left of their row). The question wraps short of it, the same width
/// every second.
const ASK_RING_D: f64 = 20.0;
const ASK_RING_GAP: f64 = 7.0;
/// The ring (from 33 pt in), the gap and m:ss under ten minutes (about 29 pt), and 11 pt clear of the question.
const ASK_COUNT_ROOM: f64 = 80.0;
/// A naming card's guessed names (`agent_notice::OtherField`): one-click chips in one row, this tall, at least this
/// wide, with this padding either side of the name and this gap between them.
const NAME_CHIP_H: f64 = 40.0;
const NAME_CHIP_MIN_W: f64 = 96.0;
const NAME_CHIP_PAD: f64 = 20.0;
const NAME_CHIP_GAP: f64 = 8.0;
/// The quiet answers after the name field ("Not important"): a low pill this tall, with this padding.
const NAME_AFTER_H: f64 = 34.0;
const NAME_AFTER_PAD: f64 = 16.0;
/// Between the conversation's window and a question docked under it.
pub const Q_DOCK_TOP: f64 = 16.0;
/// A status row above a body (question, note, reader) pushes it down this far.
pub const STATUS_ROW: f64 = 30.0;
/// Spoken sentences wrap at this width.
pub const SPEAK_W: f64 = 420.0;
/// The speaking row's (and a line to read's) most lines.
pub const SPEAK_LINES: usize = super::scene::SPEAK_LINES;

/// A symbol as a scene names it.
#[derive(Clone, Debug, PartialEq)]
pub struct Glyph {
    pub name: String,
    pub palette: Vec<Rgba>,
    pub fx: Fx,
    pub bold: bool,
    pub alpha: f64,
}

/// Heimdall's own helmet (not an SF Symbol).
pub const HELMET: &str = "heimdall.helmet";
/// Vella's mark (its app icon, not an SF Symbol): the user dictates to Vella.
pub const VELLA: &str = "vella.mark";

impl Glyph {
    pub fn new(name: &str) -> Glyph {
        Glyph { name: name.into(), palette: vec![Rgba::WHITE], fx: Fx::None, bold: false, alpha: 1.0 }
    }
    pub fn colors(mut self, palette: &[Rgba]) -> Glyph {
        self.palette = palette.to_vec();
        self
    }
    pub fn fx(mut self, fx: Fx) -> Glyph {
        self.fx = fx;
        self
    }
    pub fn bold(mut self) -> Glyph {
        self.bold = true;
        self
    }
    pub fn alpha(mut self, a: f64) -> Glyph {
        self.alpha = a;
        self
    }
}

/// One ear's content.
#[derive(Clone, Debug, PartialEq)]
pub enum Ear {
    Glyph(Glyph),
    /// The voice's bars.
    Wave { level: u16, flat: bool },
    Spinner,
    /// A numeral (the reader's speed and time left).
    Num(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lip {
    /// The user's words went to the agent.
    Sent,
    /// The agent thinks (or works with nothing new to say).
    Dots,
    /// The agent compacts its conversation.
    Compress,
    /// The pointer reached the notch while there is a last turn to open: a short lip with a grabber shows just below
    /// the notch's bottom edge (the notch itself hides anything above it), and the notch and the lip open the last
    /// turn. Gone as soon as the pointer leaves: nothing draws at rest and no strip takes clicks below the notch
    /// (bug note 2026-10-20T22-43-17, option a).
    Peek,
    /// A floating island newly needs the user (night 1022): an orange bell (waiting) or a blue question mark under the
    /// notch for about a second while the pill jiggles (`Island::shape_at`), then it retracts.
    Nudge { question: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QLook {
    Open,
    Chosen(usize),
    Expired,
    Unavailable,
    Connecting,
}

/// A note in the pulled-down turn: its icon and title (the header with its close X) and its body's drawn size.
#[derive(Clone, Debug, PartialEq)]
pub struct NoteView {
    pub id: String,
    /// The update count of this id (its drawing was redrawn).
    pub rev: u64,
    pub icon: Glyph,
    pub title: String,
    pub body: (f64, f64),
}

/// The voice's words over a question card: the newest sentences, its level, and whether its audio stopped.
#[derive(Clone, Debug, PartialEq)]
pub struct Speech {
    pub sentences: Vec<String>,
    pub level: u16,
    pub stopped: bool,
}

/// A question docked in another state: its id and card.
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub id: String,
    pub card: Card,
}

/// A question card as the layout draws it: its questions, the tab on screen, the user's picks so far, and its look.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub questions: Vec<AskQuestion>,
    pub tab: usize,
    pub picks: Vec<QuestionAnswer>,
    pub look: QLook,
    /// A countdown card's time left (`NoticeAction::Ask::countdown`); None for every other card.
    pub countdown: Option<crate::agent_notice::Countdown>,
}

/// What the reader's centre shows.
pub type ReaderFace = super::reader::Face;

/// What the island says now (DESIGN.md §3's state map, by size class).
#[derive(Clone, Debug, PartialEq)]
pub enum Scene {
    Rest,
    /// The ear pair alone, inside the menu bar (command heard, tokens at rest).
    Ears { left: Ear, right: Ear, target: Option<Target> },
    Lip(Lip),
    /// Icon over 1–3 words (lines, results, health); an optional white button below.
    Stack { icon: Glyph, words: String, button: Option<(String, Option<Target>)> },
    /// Dots over words: the agent's acknowledgement ("On it").
    Working { words: String },
    /// Lines to read, no icon (a spoken line with the voice off).
    Lines { text: String },
    /// Helmet · waveform in the ears, the reply's sentences below as they are spoken (each added, none replaced), a
    /// moment as a status row above them.
    Speaking { sentences: Vec<String>, status: Option<(Glyph, String)>, stopped: bool, level: u16 },
    /// The last turn pulled down: the voice's ears (flat bars when it is quiet), the whole reply as written
    /// (paragraphs), then the notes the agent showed in that turn (each with its close X), all scrolling as one when
    /// long, the lip band.
    /// Pulled down by the user, the conversation around the turn (`chat`) sits above and below it; `follow`: until he
    /// scrolls, it opens on the latest exchange (his last words, then the reply) rather than at `scroll`.
    /// A question open while the user has the conversation pulled down docks at its bottom (`ask`), above the band.
    /// `working`: the agent's turn runs (Pi's or the relay's, docs/claude-relay.md): the thinking dots sit under the
    /// conversation, above the composer (bug note 30 Oct 03:00).
    Reply { paragraphs: Vec<String>, notes: Vec<NoteView>, chat: crate::island::chat::Around, follow: bool, tail: bool, ask: Option<Ask>, status: Option<(Glyph, String)>, level: u16, flat: bool, scroll: f64, hot: bool, composer_empty: bool, attached: Vec<crate::island::drop::DropItem>, working: bool, copied: bool },
    Listening,
    /// A question card; while Heimdall speaks (`speech`), his words stay on top and the card docks below them (the user, 23
    /// Oct 22:07: "I should see you talking, and the decision goes down").
    /// `thinking`: the agent is working on the user's words while the card stays up; its dots sit above the card.
    /// An open card ends in the pull-down's lip band (`hot`: the pointer is on it), which tucks it away
    /// (`Target::AskTuck`, bug note 2026-10-28T19-45-55); it has no close X.
    Question { id: String, status: Option<(Glyph, String)>, card: Card, speech: Option<Speech>, thinking: bool, hot: bool },
    Reader { speed: u32, left: String, face: ReaderFace, playing: bool, started: bool, hot: bool, status: Option<(Glyph, String)> },
    Drawing { ink: bool, ears: Option<(Glyph, Glyph)>, send: SendState, close: SendState },
}

/// Size classes (DESIGN.md §2), for coverage and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    Rest,
    Ears,
    Lip,
    Working,
    Stack,
    StackButton,
    Row,
    Lines,
    Question,
    /// The pulled-down last turn (its reply and notes).
    Reply,
    Reader,
    Drawing,
}

/// One laid-out element.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: Arc<str>,
    pub content: Arc<Content>,
    pub key: u64,
    pub rect: Rect,
    pub ear: bool,
    /// Its content changes are cuts, never cross-fades (reader units, numerals, the waveform, the clock).
    pub live: bool,
    pub fx: Fx,
    pub alpha: f64,
    /// Clipped by the shape (bodies).
    pub clip: bool,
    /// Exempt from the centring check (a reader unit sits on its focus letter; ear groups are centred as groups).
    pub free: bool,
    /// A question card's element: fades in with the motion, clipped by the outline, rather than by the edge.
    pub gate_top: bool,
}

/// A scene laid out.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub w: f64,
    pub h: f64,
    pub flare: f64,
    pub items: Vec<Item>,
    pub targets: Vec<(Target, Rect)>,
    /// Frames a second the declared loops need (0: none).
    pub loop_fps: f64,
    pub class: Class,
    /// How far the ears reach from the centre line (their outer edge): they show once the shape holds them whole.
    pub ear_reach: f64,
    /// How far the pulled-down turn can scroll (0 for every other state).
    pub scroll_max: f64,
    /// Where the pulled-down turn shows now (what a `follow` scene chose).
    pub scroll_at: f64,
    /// Words on screen a Copy can take, each with where it shows: a message of the pulled-down conversation, the turn's
    /// reply (bug note 2026-10-30T22-16-36: "he couldn't select or copy it").
    pub copies: Vec<(Rect, String)>,
}

impl Layout {
    pub fn rest(d: &Display) -> Layout {
        let s = d.rest();
        Layout { w: s.w, h: s.h, flare: s.flare, items: vec![], targets: vec![], loop_fps: 0.0, class: Class::Rest, ear_reach: 0.0, scroll_max: 0.0, scroll_at: 0.0, copies: vec![] }
    }
    pub fn is_rest(&self) -> bool {
        self.class == Class::Rest
    }
}

struct B<'a> {
    m: &'a dyn Measure,
    items: Vec<Item>,
    targets: Vec<(Target, Rect)>,
    loop_fps: f64,
    nw: f64,
    nh: f64,
    scroll_max: f64,
    scroll_at: f64,
    copies: Vec<(Rect, String)>,
    /// The island window's height (`Display::island_max_h`): a granting option drawn past it takes no click.
    max_h: f64,
}

impl B<'_> {
    fn ear_x(&self) -> f64 {
        self.nw / 2.0 + EAR / 2.0
    }
    fn push(&mut self, id: &str, content: Content, rect: Rect) -> &mut Item {
        let key = content.key();
        if content.animated() {
            self.loop_fps = self.loop_fps.max(30.0);
        }
        self.items.push(Item { id: id.into(), content: Arc::new(content), key, rect, ear: false, live: false, fx: Fx::None, alpha: 1.0, clip: false, free: false, gate_top: false });
        self.items.last_mut().expect("pushed")
    }
    fn glyph(&mut self, id: &str, g: &Glyph, size: f64, cx: f64, cy: f64) -> &mut Item {
        let content = if g.name == HELMET {
            Content::Helmet { size, color: g.palette.first().copied().unwrap_or(Rgba::WHITE) }
        } else if g.name == VELLA {
            Content::Vella { size }
        } else {
            Content::Symbol { name: g.name.clone(), size, palette: g.palette.clone(), bold: g.bold }
        };
        let (fx, alpha) = (g.fx, g.alpha);
        let box_ = size * 1.15;
        let item = self.push(id, content, Rect::centered(cx, cy, box_, box_));
        item.fx = fx;
        item.alpha = alpha;
        item
    }
    fn text(&mut self, id: &str, text: &str, font: Font, color: Rgba, max_w: f64, max_lines: usize, cx: f64, cy0: f64) -> (f64, usize) {
        let (lines, w) = wrap(self.m, text, font, max_w, max_lines);
        let n = lines.len();
        if n == 0 {
            return (0.0, 0);
        }
        let w = w.max(1.0).min(max_w);
        self.push(id, Content::Text { lines, font, color, line_h: LINE }, Rect::new(cx - w / 2.0, cy0 - LINE / 2.0, w, LINE * n as f64));
        (w, n)
    }
    fn width(&self, text: &str, font: Font) -> f64 {
        let w = self.m.width(text, font);
        if w.is_finite() {
            w.max(0.0)
        } else {
            0.0
        }
    }
    fn ear(&mut self, side: f64, e: &Ear, id: &str) {
        let (cx, cy) = (side * self.ear_x(), self.nh / 2.0);
        match e {
            Ear::Glyph(g) => {
                let size = if g.name == HELMET { 20.0 } else if g.name == VELLA { VELLA_EAR } else { 16.0 };
                self.glyph(id, g, size, cx, cy).ear = true;
            }
            Ear::Wave { level, flat } => {
                let item = self.push(&format!("{id}.wave"), Content::Waveform { level: *level, flat: *flat }, Rect::centered(cx, cy, 27.0, 18.0));
                item.ear = true;
                item.live = true;
            }
            Ear::Spinner => {
                self.push(id, Content::Spinner, Rect::centered(cx, cy, 18.0, 18.0)).ear = true;
            }
            Ear::Num(text) => {
                let tw = self.width(text, Font::EarNum).clamp(1.0, EAR - 4.0);
                let t = self.push(&format!("{id}.num"), Content::Text { lines: vec![text.clone()], font: Font::EarNum, color: Rgba::white(0.8), line_h: 16.0 }, Rect::centered(cx, cy, tw, 16.0));
                t.ear = true;
                t.live = true;
            }
        }
    }
    /// An icon and words on one line centred at `cy`; returns its width.
    fn inline(&mut self, id: &str, icon: &Glyph, words: &str, icon_size: f64, cy: f64, max_w: f64) -> f64 {
        let words_w = self.width(words, Font::Label).min(max_w - icon_size - 7.0).max(1.0);
        let total = icon_size + 7.0 + words_w;
        let x0 = -total / 2.0;
        self.glyph(&format!("{id}.icon"), icon, icon_size, x0 + icon_size / 2.0, cy).free = true;
        let text = super::measure::ellipsize(self.m, words, Font::Label, words_w + 0.5, false);
        let t = self.push(&format!("{id}.words"), Content::Text { lines: vec![text], font: Font::Label, color: Rgba::WHITE, line_h: LINE }, Rect::new(x0 + icon_size + 7.0, cy - LINE / 2.0, words_w, LINE));
        t.free = true;
        total
    }
    fn capsule(&mut self, id: &str, label: &str, key: Option<&str>, symbol: Option<&str>, style: Capsule, cx: f64, cy: f64, w: f64, h: f64) -> Rect {
        let s = if style == Capsule::Pressed { 0.95 } else { 1.0 };
        let rect = Rect::centered(cx, cy, w * s, h * s);
        self.push(id, Content::Capsule { label: label.into(), key: key.map(Into::into), symbol: symbol.map(Into::into), style }, rect);
        Rect::centered(cx, cy, w, h)
    }
    /// Content plus the side padding, never narrower than the notch ("the animation for you thinking is
    /// literally three dots. It doesn't need to make the dynamic island thicker").
    fn col_w(&self, content_w: f64) -> f64 {
        (content_w + 2.0 * PAD).ceil().clamp(self.nw.min(MAX_W), MAX_W)
    }
    fn status(&mut self, status: &Option<(Glyph, String)>) -> f64 {
        match status {
            Some((g, words)) => {
                self.inline("status", g, words, 15.0, 50.0, 400.0);
                STATUS_ROW
            }
            None => 0.0,
        }
    }
    /// The newest sentences that fit the speaking row's lines from `y` (each line its own element, `voice.s<k>.<j>`):
    /// their widest line and the last line's centre.
    fn sentences(&mut self, sentences: &[String], y0: f64) -> (f64, Option<f64>) {
        self.sentences_max(sentences, y0, SPEAK_LINES)
    }
    /// `sentences` with at most `max_lines` lines (a card docked below them takes the rest of the bound).
    fn sentences_max(&mut self, sentences: &[String], y0: f64, max_lines: usize) -> (f64, Option<f64>) {
        let kept = newest_sentences(self.m, sentences, max_lines);
        let (mut y, mut widest, mut last) = (y0, 0.0f64, None);
        for (k, lines, w) in &kept {
            for (j, line) in lines.iter().enumerate() {
                let lw = self.width(line, Font::Spoken).clamp(1.0, *w);
                self.push(&format!("voice.s{k}.{j}"), Content::Text { lines: vec![line.clone()], font: Font::Spoken, color: Rgba::WHITE, line_h: LINE }, Rect::new(-lw / 2.0, y - LINE / 2.0, lw, LINE));
                last = Some(y);
                y += LINE;
            }
            widest = widest.max(*w);
        }
        (widest, last)
    }
    /// The question card: the head (icon, step chips when there are several questions), the question's lines, its
    /// options as rows (tile, label, description, a dot or box), the Other row, and the foot (key hints and Send).
    /// `top` is the card's top edge; returns the card's width and its bottom. `name` prefixes its elements' ids, so a
    /// card docked somewhere else is another element and cross-fades rather than gliding over what leaves.
    fn ask(&mut self, name: &str, id: &str, card: &Card, top: f64) -> (f64, f64) {
        let from = self.items.len();
        let n = card.questions.len();
        let Some(tab) = (n > 0).then(|| card.tab.min(n - 1)) else { return (ASK_W, top) };
        let q = &card.questions[tab];
        let (left, right) = (-ASK_INNER / 2.0, ASK_INNER / 2.0);
        let open = card.look == QLook::Open;
        let dim = matches!(card.look, QLook::Expired | QLook::Unavailable);
        // The head row holds the step chips only, and only with several questions: a single question starts at the top.
        let head_cy = top + ASK_PAD_TOP + ASK_HEAD / 2.0;
        // Something answered: Send commits the card, any question left without a pick going as unanswered (bug note
        // 2026-11-02T16-27-21: one question of two ticked, and no way to send).
        let sendable = open && crate::agent_notice::can_send(id, &card.questions, &card.picks);
        // Step chips, one per question, at the right, and Send after them; a chip is its question's tab. Each chip
        // leads with its state: a tick once answered, an accent dot while it still needs a pick.
        if n > 1 {
            let labels: Vec<String> = card.questions.iter().enumerate().map(|(k, x)| x.header.clone().unwrap_or_else(|| format!("Q{}", k + 1))).collect();
            // Each chip is at least 44 pt (a trackpad's target), and its position is laid out with that width.
            let widths: Vec<f64> = labels.iter().map(|l| (self.width(l, Font::Chip).min(120.0) + 24.0 + ASK_MARK).max(44.0)).collect();
            let send_cx = right - ASK_ARROW_D / 2.0;
            let mut x = send_cx - ASK_ARROW_D / 2.0 - ASK_SEND_GAP - widths.iter().sum::<f64>() - ASK_CHIP_GAP * (n - 1) as f64;
            for (k, label) in labels.iter().enumerate() {
                // A tap target is never narrower than a trackpad's (44 pt), whatever its label measures.
                let w = widths[k];
                let done = card.picks.get(k).is_some_and(|p| !p.labels.is_empty() || p.other.is_some());
                let (fill, ink) = if k == tab { (Rgba::WHITE, Rgba::BLACK) } else if done { (Rgba::white(0.08), Rgba::white(0.8)) } else { (Rgba([0, 0, 0, 0]), Rgba::white(0.62)) };
                let ring = (k != tab && !done).then_some((Rgba::white(0.25), 1.0));
                let rect = Rect::new(x, head_cy - ASK_CHIP_H / 2.0, w, ASK_CHIP_H);
                self.push(&format!("{name}.chip.{k}"), Content::Panel { fill, ring, radius: ASK_CHIP_H / 2.0 }, rect);
                let tw = self.width(label, Font::Chip).min(w - 24.0 - ASK_MARK).max(1.0);
                let x0 = x + (w - tw - ASK_MARK) / 2.0;
                // The mark: each state (and the tab's, white or not) its own element, so a pick or a tab change
                // cross-fades it rather than recolouring in place (a symbol recoloured in place is not redrawn in time).
                let on = if k == tab { "tab" } else { "off" };
                if done {
                    self.glyph(&format!("{name}.chip.{k}.tick.{on}"), &Glyph::new("checkmark").colors(&[ink]).bold(), 9.0, x0 + 5.0, head_cy).free = true;
                } else {
                    self.push(&format!("{name}.chip.{k}.open.{on}"), Content::Dot { color: ASK_BLUE, d: 6.0 }, Rect::centered(x0 + 5.0, head_cy, 6.0, 6.0));
                }
                self.push(&format!("{name}.chip.{k}.word"), Content::Text { lines: vec![label.clone()], font: Font::Chip, color: ink, line_h: ASK_CHIP_H }, Rect::new(x0 + ASK_MARK, head_cy - ASK_CHIP_H / 2.0, tw, ASK_CHIP_H));
                if open {
                    self.targets.push((Target::AskTab { id: id.into(), question: k }, rect));
                }
                x += w + ASK_CHIP_GAP;
            }
            if open {
                self.send_disc(name, id, sendable, send_cx, head_cy);
            }
        }
        // The question, left-aligned: one text element for all its lines, so its lines travel together (one line per
        // element let them slide over one another on arrival and when the card moved).
        let beside = card.countdown.is_some() && n == 1;
        let q_w = if beside { ASK_INNER - ASK_COUNT_ROOM } else { ASK_INNER };
        let (lines, _) = wrap(self.m, &q.text, Font::Question, q_w, 3);
        let head = if n > 1 { ASK_HEAD + 16.0 } else { 0.0 };
        let first = top + ASK_PAD_TOP + head + ASK_Q_LINE / 2.0;
        if let Some(c) = card.countdown {
            // Live: each second's numerals and ring cut in place, never a motion.
            let text = c.text();
            let tw = self.width(&text, Font::EarNum).max(1.0);
            let (ring_cx, text_x, cy) = if beside {
                // Centred on the option rows' dot or box column (`control` below).
                let ring_cx = right - 12.0 - 11.0;
                (ring_cx, ring_cx - ASK_RING_D / 2.0 - ASK_RING_GAP - tw, first)
            } else {
                let ring_cx = left + ASK_RING_D / 2.0;
                (ring_cx, ring_cx + ASK_RING_D / 2.0 + ASK_RING_GAP, head_cy)
            };
            self.push(&format!("{name}.countdown.ring"), Content::Ring { left_milli: c.left_milli, d: ASK_RING_D }, Rect::centered(ring_cx, cy, ASK_RING_D, ASK_RING_D)).live = true;
            self.push(&format!("{name}.countdown"), Content::Text { lines: vec![text], font: Font::EarNum, color: Rgba::white(0.62), line_h: 18.0 }, Rect::new(text_x, cy - 9.0, tw, 18.0)).live = true;
        }
        let lw = lines.iter().map(|l| self.width(l, Font::Question)).fold(1.0, f64::max).clamp(1.0, q_w);
        let color = Rgba::white(if dim { 0.55 } else { 1.0 });
        let count = lines.len().max(1);
        if !lines.is_empty() {
            self.push(&format!("{name}.q"), Content::Text { lines, font: Font::Question, color, line_h: ASK_Q_LINE }, Rect::new(left, first - ASK_Q_LINE / 2.0, lw, ASK_Q_LINE * count as f64));
        }
        let qbottom = first + ASK_Q_LINE * (count - 1) as f64 + ASK_Q_LINE / 2.0;
        // An expired or unavailable card keeps its question and offers Dismiss alone.
        if dim {
            let cy = qbottom + 16.0 + 16.0;
            let rect = self.capsule(&format!("{name}.dismiss"), "Dismiss", None, None, Capsule::White, 0.0, cy, 100.0, 32.0);
            self.targets.push((Target::Dismiss(id.into()), rect));
            return (ASK_W, self.ask_bottom(from, top));
        }
        let picked: Vec<String> = card.picks.get(tab).map(|p| p.labels.clone()).unwrap_or_default();
        let chosen = match card.look {
            QLook::Chosen(c) => Some(c),
            _ => None,
        };
        let mut y = qbottom + 14.0;
        // A naming card (a voice card): the guessed names as chips, the name field, then "Not
        // important": quote, chips, field, last the quiet answer. One question, one click.
        if let Some(f) = q.field.as_ref().filter(|_| n == 1 && !q.multi_select && q.allow_other) {
            let split = q.options.len().saturating_sub(f.after);
            if split > 0 {
                y += self.name_chips(name, id, q, split, open, chosen, left, y) + 10.0;
            }
            let other = card.picks.get(tab).and_then(|p| p.other.clone());
            let bottom = if open { self.other_row(name, id, q, tab, other, true, sendable, left, right, y) } else { y - ASK_OPT_GAP };
            let mut ay = bottom + 10.0;
            for i in split..q.options.len() {
                // A quiet pill under the field, left-aligned with the names: an answer, a step below them.
                let room = ASK_INNER - 2.0 * NAME_AFTER_PAD;
                let label = super::measure::ellipsize(self.m, &q.options[i].label, Font::Button, room, false);
                let on = chosen == Some(i);
                let a = if chosen.is_some() && !on { 0.35 } else { 1.0 };
                let lw = self.width(&label, Font::Button).clamp(1.0, room);
                let rect = Rect::new(left, ay, lw + 2.0 * NAME_AFTER_PAD, NAME_AFTER_H);
                self.push(&format!("{name}.o{i}.row"), Content::Panel { fill: if on { Rgba::WHITE } else { Rgba::white(0.07 * a) }, ring: None, radius: NAME_AFTER_H / 2.0 }, rect);
                let ink = if on { Rgba::BLACK } else { Rgba::white(0.7 * a) };
                self.push(&format!("{name}.o{i}.label"), Content::Text { lines: vec![label], font: Font::Button, color: ink, line_h: NAME_AFTER_H }, Rect::new(left + NAME_AFTER_PAD, ay, lw, NAME_AFTER_H));
                if open {
                    self.targets.push((Target::AskPick { id: id.into(), question: tab, option: i }, rect));
                }
                ay += NAME_AFTER_H + NAME_CHIP_GAP;
            }
            return (ASK_W, self.ask_bottom(from, top));
        }
        for (i, o) in q.options.iter().enumerate() {
            let on = picked.contains(&o.label) || chosen == Some(i);
            let a = if chosen.is_some() && !on { 0.35 } else { 1.0 };
            // An option that grants (approval grants, 4 Nov): the row grows by a well that names each path and login
            // item in full, so the user sees exactly what his click approves.
            let well = self.grant_lines(&o.grant);
            let well_h = grant_well_h(&well);
            let row = Rect::new(left, y, ASK_INNER, ASK_OPT_H + if well.is_empty() { 0.0 } else { well_h + GRANT_BELOW });
            let cy = y + ASK_OPT_H / 2.0;
            self.push(&format!("{name}.o{i}.row"), Content::Panel { fill: Rgba::white(if on { 0.11 } else { 0.05 }), ring: None, radius: 16.0 }, row);
            let tile = Rect::new(left + 10.0, cy - 15.0, 30.0, 30.0);
            self.push(&format!("{name}.o{i}.tile"), Content::Panel { fill: Rgba::white(if on { 1.0 } else { 0.10 }), ring: None, radius: 9.0 }, tile);
            self.push(&format!("{name}.o{i}.digit"), Content::Text { lines: vec![(i + 1).to_string()], font: Font::Button, color: if on { Rgba::BLACK } else { Rgba::white(a) }, line_h: 30.0 }, tile);
            let text_x = left + 52.0;
            let max_label = ASK_INNER - 52.0 - 12.0 - 22.0 - 12.0;
            let label_cy = if o.description.is_some() { cy - 10.0 } else { cy };
            let label = super::measure::ellipsize(self.m, &o.label, Font::OptionLabel, max_label, false);
            let lw = self.width(&label, Font::OptionLabel).clamp(1.0, max_label);
            self.push(&format!("{name}.o{i}.label"), Content::Text { lines: vec![label], font: Font::OptionLabel, color: Rgba::white(a), line_h: 20.0 }, Rect::new(text_x, label_cy - 10.0, lw, 20.0));
            if let Some(d) = &o.description {
                let dt = super::measure::ellipsize(self.m, d, Font::Describe, max_label, false);
                let dw = self.width(&dt, Font::Describe).clamp(1.0, max_label);
                self.push(&format!("{name}.o{i}.desc"), Content::Text { lines: vec![dt], font: Font::Describe, color: Rgba::white(0.72 * a), line_h: 16.0 }, Rect::new(text_x, cy + 11.0 - 8.0, dw, 16.0));
            }
            // The control: a dot (one answer) or a box (several), filled blue with a tick when picked.
            let cx = right - 12.0 - 11.0;
            let control = Rect::new(cx - 11.0, cy - 11.0, 22.0, 22.0);
            let radius = if q.multi_select { 7.0 } else { 11.0 };
            let (fill, ring) = if on { (ASK_BLUE, None) } else { (Rgba([0, 0, 0, 0]), Some((Rgba::white(0.35 * a), 1.5))) };
            self.push(&format!("{name}.o{i}.control"), Content::Panel { fill, ring, radius }, control);
            if on {
                self.glyph(&format!("{name}.o{i}.tick"), &Glyph::new("checkmark").colors(&[Rgba::WHITE]).bold(), 11.0, cx, cy).free = true;
            }
            if !well.is_empty() {
                self.grant_well(&format!("{name}.o{i}.grant"), &well, a, text_x - GRANT_PAD_X, y + ASK_OPT_H, right - 12.0);
            }
            // A grant cut off by the window's bottom is not one the user could read: it takes no click (review 4 Nov
            // evening; `agent::actions` keeps a card's grants short enough to fit).
            let whole = well.is_empty() || row.max_y() + PAD_BOTTOM_MAX <= self.max_h;
            if open && whole {
                self.targets.push((Target::AskPick { id: id.into(), question: tab, option: i }, row));
            }
            y += row.h + ASK_OPT_GAP;
        }
        y -= ASK_OPT_GAP;
        // One question: its Other row holds Send. A multi-select question always has the row, as its arrow is the
        // only way to send several picks. Several questions: Send sits after the chips, and the row is for words only.
        // Option plus a digit answers an option silently (no key row).
        let in_row = n == 1;
        if open && (q.allow_other || (in_row && q.multi_select)) {
            let other = card.picks.get(tab).and_then(|p| p.other.clone());
            self.other_row(name, id, q, tab, other, in_row, sendable, left, right, y + ASK_OPT_GAP);
        }
        // No close X (bug note 2026-10-28T19-45-55: "I should have the lip, not the X"): an open card on its own ends
        // in the pull-down's lip band (`Scene::Question`), which tucks it away unanswered; Esc in its field still
        // dismisses it.
        (ASK_W, self.ask_bottom(from, top))
    }
    /// A naming card's guessed names: the first `split` options as one-click chips in one row from `left`, at `y`.
    /// Returns the row's height.
    #[allow(clippy::too_many_arguments)]
    fn name_chips(&mut self, name: &str, id: &str, q: &AskQuestion, split: usize, open: bool, chosen: Option<usize>, left: f64, y: f64) -> f64 {
        let max_w = (ASK_INNER - NAME_CHIP_GAP * (split - 1) as f64) / split as f64;
        let mut x = left;
        for (i, o) in q.options.iter().enumerate().take(split) {
            let on = chosen == Some(i);
            let a = if chosen.is_some() && !on { 0.35 } else { 1.0 };
            let room = (max_w - 2.0 * NAME_CHIP_PAD).max(1.0);
            let label = super::measure::ellipsize(self.m, &o.label, Font::OptionLabel, room, false);
            let lw = self.width(&label, Font::OptionLabel).clamp(1.0, room);
            let w = (lw + 2.0 * NAME_CHIP_PAD).clamp(NAME_CHIP_MIN_W.min(max_w), max_w);
            let rect = Rect::new(x, y, w, NAME_CHIP_H);
            self.push(&format!("{name}.o{i}.row"), Content::Panel { fill: if on { Rgba::WHITE } else { Rgba::white(0.13 * a) }, ring: None, radius: NAME_CHIP_H / 2.0 }, rect);
            let ink = if on { Rgba::BLACK } else { Rgba::white(a) };
            self.push(&format!("{name}.o{i}.label"), Content::Text { lines: vec![label], font: Font::OptionLabel, color: ink, line_h: NAME_CHIP_H }, Rect::new(x + (w - lw) / 2.0, y, lw, NAME_CHIP_H));
            if open {
                self.targets.push((Target::AskPick { id: id.into(), question: 0, option: i }, rect));
            }
            x += w + NAME_CHIP_GAP;
        }
        NAME_CHIP_H
    }
    /// The Other row at `oy`: the same height, radius, fill and tile as an option, with a plain field for his own
    /// answer and (`in_row`: one question) a send arrow inside its right edge (dim while empty, accent once typed).
    /// Returns its bottom.
    #[allow(clippy::too_many_arguments)]
    fn other_row(&mut self, name: &str, id: &str, q: &AskQuestion, tab: usize, other: Option<String>, in_row: bool, sendable: bool, left: f64, right: f64, oy: f64) -> f64 {
        let ocy = oy + ASK_OPT_H / 2.0;
        let typed = other.is_some();
        self.push(&format!("{name}.other.row"), Content::Panel { fill: Rgba::white(if typed { 0.11 } else { 0.05 }), ring: None, radius: 16.0 }, Rect::new(left, oy, ASK_INNER, ASK_OPT_H));
        let tile = Rect::new(left + 10.0, ocy - 15.0, 30.0, 30.0);
        self.push(&format!("{name}.other.tile"), Content::Panel { fill: Rgba::white(if typed { 0.22 } else { 0.10 }), ring: None, radius: 9.0 }, tile);
        // The glyphs keep one palette and dim by alpha: a symbol that changes colour in place is not redrawn in time.
        self.glyph(&format!("{name}.other.pencil"), &Glyph::new("pencil").colors(&[Rgba::white(0.7)]), 13.0, tile.x + 15.0, ocy).free = true;
        let arrow_cx = right - 12.0 - ASK_TARGET / 2.0;
        let field_x = left + 52.0;
        let field_r = if in_row { arrow_cx - ASK_TARGET / 2.0 - 4.0 } else { right - 16.0 };
        let field = Rect::new(field_x, ocy - 11.0, field_r - field_x, 22.0);
        self.push(&format!("{name}.other.field"), Content::Field { font: Font::OptionLabel, owner: Target::AskOther { id: id.into(), question: tab }, text: Some(other.clone().unwrap_or_default()), focus_until_ms: q.field.as_ref().map_or(0, |f| f.focus_until_ms) }, field);
        // The placeholder is drawn here while the field is empty; the field itself draws none (one label, one layer).
        if !typed {
            let placeholder = q.field.as_ref().map_or(OTHER_PLACEHOLDER, |f| f.placeholder.as_str());
            let pw = self.width(placeholder, Font::OptionLabel).clamp(1.0, field.w);
            self.push(&format!("{name}.other.placeholder"), Content::Text { lines: vec![placeholder.into()], font: Font::OptionLabel, color: Rgba::white(0.56), line_h: 22.0 }, Rect::new(field_x, ocy - 11.0, pw, 22.0)).free = true;
        }
        if in_row {
            self.send_disc(name, id, sendable, arrow_cx, ocy);
        }
        self.targets.push((Target::AskOther { id: id.into(), question: tab }, Rect::new(left, oy, ASK_INNER, ASK_OPT_H)));
        oy + ASK_OPT_H
    }
    /// The card's bottom: its lowest element (from `from`) and the bottom padding, never less than the top edge.
    /// Every element of the card is `free` and clipped by the outline: the card is a left-aligned block (rows, Other,
    /// foot) that fades in element by element, so its centring holds only once it has all arrived (its width is fixed
    /// by `ASK_W`, and tests check it). `gate_top` keeps the fade on the motion (`engine`).
    fn ask_bottom(&mut self, from: usize, top: f64) -> f64 {
        for it in &mut self.items[from..] {
            it.free = true;
            it.clip = true;
            it.gate_top = true;
        }
        self.items[from..].iter().map(|i| i.rect.max_y()).fold(top, f64::max) + PAD_BOTTOM_MAX
    }
    /// Send: a disc with an arrow, the accent colour once something is answered (and a target then), dim before. Each
    /// state is its own glyph, so the colour is never changed in place.
    fn send_disc(&mut self, name: &str, id: &str, lit: bool, cx: f64, cy: f64) {
        self.push(&format!("{name}.send"), Content::Dot { color: if lit { ASK_BLUE } else { Rgba::white(0.10) }, d: ASK_ARROW_D }, Rect::centered(cx, cy, ASK_ARROW_D, ASK_ARROW_D));
        let (state, ink) = if lit { ("lit", Rgba::WHITE) } else { ("dim", Rgba::white(0.3)) };
        self.glyph(&format!("{name}.send.arrow.{state}"), &Glyph::new("arrow.up").colors(&[ink]).bold(), 12.0, cx, cy).free = true;
        if lit {
            self.targets.push((Target::AskSend(id.into()), Rect::centered(cx, cy, ASK_TARGET, ASK_TARGET)));
        }
    }
    /// An option's grant as the well draws it: per item its symbol and its lines (`measure::wrap_path`), never cut.
    fn grant_lines(&self, grant: &[GrantItem]) -> Vec<(&'static str, Vec<String>)> {
        let room = GRANT_TEXT_W;
        grant.iter().map(|g| match g {
            GrantItem::Delete(p) => ("trash", super::measure::wrap_path(self.m, p, Font::Path, room)),
            GrantItem::LoginItem(n) => ("power", super::measure::wrap_path(self.m, n, Font::Path, room)),
        }).collect()
    }
    /// The well under a granting option, from `x` to `right` at `top`: a recessed panel, and per item a dim symbol
    /// beside its monospaced lines, one element per line (left-aligned).
    fn grant_well(&mut self, name: &str, well: &[(&'static str, Vec<String>)], a: f64, x: f64, top: f64, right: f64) {
        let h = grant_well_h(well);
        self.push(&format!("{name}.well"), Content::Panel { fill: Rgba([0, 0, 0, (0.32 * 255.0 * a) as u8]), ring: Some((Rgba::white(0.06 * a), 1.0)), radius: 10.0 }, Rect::new(x, top, right - x, h));
        let mut ly = top + GRANT_PAD_Y;
        for (k, (symbol, lines)) in well.iter().enumerate() {
            self.glyph(&format!("{name}.{k}.symbol"), &Glyph::new(symbol).colors(&[Rgba::white(0.5 * a)]), 10.5, x + GRANT_PAD_X + 6.0, ly + GRANT_LINE / 2.0).free = true;
            for (j, line) in lines.iter().enumerate() {
                let w = self.width(line, Font::Path).clamp(1.0, GRANT_TEXT_W);
                self.push(&format!("{name}.{k}.{j}"), Content::Text { lines: vec![line.clone()], font: Font::Path, color: Rgba::white(0.88 * a), line_h: GRANT_LINE }, Rect::new(x + GRANT_PAD_X + GRANT_INDENT, ly, w, GRANT_LINE));
                ly += GRANT_LINE;
            }
            ly += GRANT_ITEM_GAP;
        }
    }
    fn button_w(&self, label: &str) -> f64 {
        (self.width(label, Font::Button) + 40.0).clamp(120.0, 240.0)
    }
}

/// A grant well's height: its padding, every line, and the gaps between items.
fn grant_well_h(well: &[(&'static str, Vec<String>)]) -> f64 {
    if well.is_empty() {
        return 0.0;
    }
    let lines: usize = well.iter().map(|(_, l)| l.len()).sum();
    2.0 * GRANT_PAD_Y + GRANT_LINE * lines as f64 + GRANT_ITEM_GAP * (well.len() - 1) as f64
}

/// Lays `scene` out for `d`, measuring with `m`.
pub fn layout(scene: &Scene, d: &Display, m: &dyn Measure) -> Layout {
    let (nw, nh) = d.notch_size();
    let mut b = B { m, items: vec![], targets: vec![], loop_fps: 0.0, nw, nh, scroll_max: 0.0, scroll_at: 0.0, copies: vec![], max_h: d.island_max_h() };
    let pair = nw + 2.0 * EAR;
    let (w, h, class) = match scene {
        Scene::Rest => return Layout::rest(d),
        Scene::Ears { left, right, target } => {
            b.ear(-1.0, left, "ear.l");
            b.ear(1.0, right, "ear.r");
            if let Some(t) = target {
                // The whole ear pair, inside the shape's rounded corners.
                b.targets.push((t.clone(), Rect::new(-pair / 2.0 + 8.0, 0.0, pair - 16.0, nh - 4.0)));
            }
            (pair, nh, Class::Ears)
        }
        Scene::Lip(Lip::Peek) => {
            let h = nh + PEEK_BELOW;
            b.push("peek.grabber", Content::Grabber { hot: true }, Rect::centered(0.0, nh + PEEK_BELOW / 2.0, 36.0, 5.0));
            // The notch as at rest (never less, or the pointer at its edge would show and hide the lip in turn), and
            // the lip below it (overlapping the notch upward, a tap-sized band) inside the shape's rounded bottom corners.
            b.targets.push((Target::Token(Token::Reply), Rect::new(-nw / 2.0 + 8.0, 0.0, nw - 16.0, nh - 4.0)));
            b.targets.push((Target::Token(Token::Reply), Rect::new(-nw / 2.0 + 16.0, h - 3.0 - 26.0, nw - 32.0, 26.0)));
            (b.col_w(0.0), h, Class::Lip)
        }
        Scene::Lip(lip) => {
            match lip {
                Lip::Sent => {
                    b.glyph("lip.sent", &Glyph::new("arrow.up").bold().fx(Fx::Bounce), 16.0, 0.0, LIP_Y);
                }
                Lip::Dots => {
                    b.push("turn.dots", Content::Dots, Rect::centered(0.0, LIP_Y, 36.0, 8.0));
                }
                // The compacting stack's icon settles into the lip in the same slot (it glides, no double).
                Lip::Compress => {
                    b.glyph("stack.icon", &Glyph::new("rectangle.compress.vertical").fx(Fx::Breathe), 16.0, 0.0, LIP_Y);
                }
                // The bell rings (the system's wiggle), the question mark bounces, as the shape lands.
                Lip::Nudge { question } => {
                    let g = if *question {
                        Glyph::new("questionmark.circle.fill").colors(&[Rgba::WHITE, Rgba::BLUE]).fx(Fx::Bounce)
                    } else {
                        Glyph::new("bell.fill").colors(&[Rgba::ORANGE]).fx(Fx::Wiggle)
                    };
                    b.glyph("lip.nudge", &g, 17.0, 0.0, LIP_Y);
                }
                Lip::Peek => unreachable!("laid out above"),
            }
            // One glyph or the dots under the notch: the notch's own width, only deeper.
            (b.col_w(0.0), LIP_H, Class::Lip)
        }
        Scene::Stack { icon, words, button } => {
            let size = if icon.name == HELMET { 26.0 } else if icon.name == VELLA { VELLA_STACK } else { 24.0 };
            b.glyph("stack.icon", icon, size, 0.0, STACK_ICON_Y);
            let (tw, n) = b.text("stack.words", words, Font::Label, Rgba::WHITE, 360.0, 3, 0.0, STACK_WORD_Y);
            let n = n.max(1);
            let words_bottom = STACK_WORD_Y + LINE * (n - 1) as f64;
            match button {
                Some((label, target)) => {
                    let bw = b.button_w(label);
                    let by = words_bottom + 41.0;
                    let style = if target.is_some() { Capsule::White } else { Capsule::Dim };
                    let r = b.capsule("stack.button", label, None, None, style, 0.0, by, bw, 32.0);
                    if let Some(t) = target {
                        b.targets.push((t.clone(), r));
                    }
                    (b.col_w(tw.max(bw).max(size * 1.15)), by + 30.0, Class::StackButton)
                }
                // No words: the icon alone, as deep as it needs.
                None if tw <= 0.0 => (b.col_w(size * 1.15), STACK_ICON_Y + size * 1.15 / 2.0 + 10.0, Class::Stack),
                None => (b.col_w(tw.max(size * 1.15)), words_bottom + 20.0, Class::Stack),
            }
        }
        Scene::Working { words } => {
            b.push("turn.dots", Content::Dots, Rect::centered(0.0, STACK_ICON_Y, 36.0, 8.0));
            let (tw, _) = b.text("turn.words", words, Font::Label, Rgba::WHITE, 360.0, 1, 0.0, STACK_WORD_Y);
            (b.col_w(tw.max(36.0)), STACK_H, Class::Working)
        }
        Scene::Lines { text } => {
            // The line up to the speaking row's lines (the pull-down holds the rest), as written: its line breaks,
            // paragraph gaps and list items kept, the answer bold (`prose`, bug note 2026-10-21T19-21-01). One
            // paragraph reads as a caption, centred; more, or a list, as a page: left-aligned in a centred column.
            let paras: Vec<String> = text.split('\n').map(str::to_string).collect();
            let p = prose::keep(b.m, prose::lay(b.m, &paras, SPEAK_W, LINE), SPEAK_LINES, SPEAK_W, LINE);
            let w = p.widest.max(1.0);
            for (k, row) in p.rows.iter().enumerate() {
                let lw = b.width(&row.text, row.font()).clamp(1.0, (SPEAK_W - row.x).max(1.0));
                let x = if p.page { -w / 2.0 + row.x } else { -lw / 2.0 };
                let item = b.push(&format!("lines.{k}"), Content::Text { lines: vec![row.text.clone()], font: row.font(), color: Rgba::WHITE, line_h: LINE }, Rect::new(x, 50.0 + row.y - LINE / 2.0, lw, LINE));
                item.free = p.page;
            }
            (b.col_w(if p.rows.is_empty() { 0.0 } else { w }), 53.0 + p.height.max(LINE), Class::Lines)
        }
        Scene::Speaking { sentences, status, stopped, level } => {
            b.ear(-1.0, &Ear::Glyph(Glyph::new(HELMET)), "voice.helmet");
            b.ear(1.0, &Ear::Wave { level: *level, flat: *stopped }, "voice");
            let mut widest: f64 = 0.0;
            let top = match status {
                Some((g, words)) => {
                    widest = b.inline("voice.line", g, words, 17.0, 50.0, 436.0);
                    STATUS_ROW
                }
                None => 0.0,
            };
            // The newest sentences that fit in the row's lines, each starting its own line, oldest on top. A sentence
            // keeps its place and its id while others are added below it, so it never moves sideways or reflows;
            // when the row is full the oldest leaves at the top and the rest glide up.
            // Each line is its own element, so a sentence being added shows line by line as the growing edge passes
            // it (never a half-revealed line, never an empty band waiting for the whole sentence).
            let kept = newest_sentences(b.m, sentences, SPEAK_LINES);
            let mut y = 50.0 + top;
            for (k, lines, w) in &kept {
                for (j, line) in lines.iter().enumerate() {
                    let lw = b.width(line, Font::Spoken).clamp(1.0, *w);
                    b.push(&format!("voice.s{k}.{j}"), Content::Text { lines: vec![line.clone()], font: Font::Spoken, color: Rgba::WHITE, line_h: LINE }, Rect::new(-lw / 2.0, y - LINE / 2.0, lw, LINE));
                    y += LINE;
                }
                widest = widest.max(*w);
            }
            if kept.is_empty() && status.is_none() {
                // No words (they did not come within the wait): the ear pair alone.
                (pair, nh, Class::Ears)
            } else {
                // The last line's centre plus 23 (73 for one line, as "Speaking with words").
                let last = if kept.is_empty() { 50.0 } else { y - LINE };
                (b.col_w(widest).max(pair), last + 23.0, Class::Row)
            }
        }
        Scene::Reply { paragraphs, notes, chat, follow, tail, ask, status, level, flat, scroll, hot, composer_empty, attached, working, copied } => {
            b.ear(-1.0, &Ear::Glyph(Glyph::new(HELMET)), "voice.helmet");
            // The live bars while the voice plays; quiet, the system's waveform glyph (flat bars read as an
            // ellipsis, "still loading").
            if *flat {
                b.ear(1.0, &Ear::Glyph(Glyph::new("waveform")), "reply.voice");
            } else {
                b.ear(1.0, &Ear::Wave { level: *level, flat: false }, "voice");
            }
            let top = b.status(status);
            // The whole reply as written, left-aligned in a column centred under the notch: its line breaks,
            // paragraph gaps and list items kept, the answer bold (`prose`, bug note 2026-10-21T19-21-01).
            let p = prose::lay(b.m, paragraphs, SPEAK_W, LINE);
            let mut widest = p.widest;
            let mut lines: Vec<TextRow> = p.rows;
            let talk = !chat.before.is_empty() || !chat.after.is_empty();
            // The turn's notes below the reply: a header (icon, title, pin, close X) over the app's drawing of each.
            // The column grows for a wide table, up to the screen-bound maximum.
            let max_doc = (reply_max_w(d) - 2.0 * if notes.is_empty() { PAD } else { PAD_MAX }).max(1.0);
            let body_of = |n: &NoteView| {
                let w = if n.body.0.is_finite() { n.body.0.clamp(0.0, max_doc) } else { 0.0 };
                let h = if n.body.1.is_finite() { n.body.1.max(0.0) } else { 0.0 };
                (w, h)
            };
            // The reply and the notes are two bodies, each one bitmap below Core Animation's limit (a bitmap past it
            // draws nothing: review 19 Oct, and three 200-row tables in review of 063424a). The reply ends with a
            // pointer; the oldest notes give way to the newest.
            let limit = MAX_RASTER_PX / d.scale.max(1.0);
            let mut kept: Vec<&NoteView> = vec![];
            let mut block = 0.0;
            for n in notes.iter().rev() {
                let add = NOTE_HEAD + body_of(n).1 + if kept.is_empty() { 0.0 } else { NOTE_GAP };
                if block + add > limit {
                    break;
                }
                block += add;
                kept.push(n);
            }
            kept.reverse();
            // One bitmap holds the reply: rows past its height give way to a last line that says so.
            let room = MAX_RASTER_PX / d.scale.max(1.0);
            if lines.last().is_some_and(|r| r.y + LINE > room) {
                lines.retain(|r| r.y + 2.0 * LINE <= room);
                let y = lines.last().map_or(0.0, |r| r.y + LINE);
                lines.push(TextRow { text: REPLY_CUT.into(), x: 0.0, y, strong: false });
                widest = widest.max(b.width(REPLY_CUT, Font::Spoken).min(SPEAK_W));
            }
            let mut doc_w = widest;
            for n in &kept {
                let head = 16.0 + 7.0 + b.width(&n.title, Font::Label) + 12.0 + 18.0 + NOTE_PIN_GAP;
                doc_w = doc_w.max(body_of(n).0).max(head);
            }
            // The conversation keeps one column: as wide as a reply may be (wider only for a table). Opened with
            // nothing yet (⌃⌘H), the composer alone has that width to type in.
            if talk || (lines.is_empty() && kept.is_empty()) {
                doc_w = doc_w.max(SPEAK_W);
            }
            let doc_w = doc_w.max(1.0).min(max_doc);
            // A table that widens the column widens the reply's lines with it (bug note 2026-10-25T23-17-01: the
            // reply wrapped at 420 pt in a 700 pt sheet, a band left empty on its right).
            if doc_w > SPEAK_W + 0.5 && !lines.is_empty() {
                let p = prose::lay(b.m, paragraphs, doc_w, LINE);
                widest = p.widest;
                lines = p.rows;
                if lines.last().is_some_and(|r| r.y + LINE > room) {
                    lines.retain(|r| r.y + 2.0 * LINE <= room);
                    let y = lines.last().map_or(0.0, |r| r.y + LINE);
                    lines.push(TextRow { text: REPLY_CUT.into(), x: 0.0, y, strong: false });
                    widest = widest.max(b.width(REPLY_CUT, Font::Spoken).min(doc_w));
                }
            }
            let lines_h = lines.last().map_or(0.0, |r| r.y + LINE);
            // The notes' block, from its own top; it starts below the reply.
            let mut y = 0.0;
            let mut placed = vec![];
            for n in &kept {
                if !placed.is_empty() {
                    y += NOTE_GAP;
                }
                let (w, h) = body_of(n);
                placed.push(Placed { id: n.id.clone(), rev: n.rev, icon: n.icon.name.clone(), palette: n.icon.palette.clone(), title: n.title.clone(), y, w, h });
                y += NOTE_HEAD + h;
            }
            let notes_y0 = if lines.is_empty() || placed.is_empty() { lines_h } else { lines_h + NOTE_GAP };
            let turn_h = if placed.is_empty() { lines_h } else { notes_y0 + y };
            // The conversation before the turn and after it, laid out once (`lay_cached`) and drawn in chunks.
            let (before, after): (&[crate::island::chat::ChatKind], &[crate::island::chat::ChatKind]) = (&chat.before, &chat.after);
            let pre = super::chat_layout::lay_cached(b.m, before, doc_w);
            let post = super::chat_layout::lay_cached(b.m, after, doc_w);
            // The turn is Heimdall's: it sits under his own rows close, under the user's a turn apart.
            let turn = turn_h > 0.0;
            let reply_row = crate::island::chat::ChatKind::Heimdall { text: String::new(), heard: crate::island::chat::Heard::Whole };
            let shift = if pre.h > 0.0 && turn { pre.h + super::chat_layout::gap(before.last(), Some(&reply_row)) } else { pre.h };
            let post_y0 = if post.h > 0.0 && (turn || pre.h > 0.0) {
                shift + turn_h + super::chat_layout::gap(Some(if turn { &reply_row } else { before.last().unwrap_or(&reply_row) }), after.first())
            } else {
                shift + turn_h
            };
            let full = (post_y0 + post.h).max(LINE);
            // Taller than half the screen, it scrolls inside the shape (DESIGN.md §3, notes).
            let y0 = 40.0 + top;
            // A question docked under the conversation: its height, from a dry layout (it is laid out for real below).
            // The dock's height is that of the open card: answering it retracts the dock as one piece, not by stepping the
            // conversation's height down under the motion.
            let dock = ask.as_ref().map(|a| {
                let mut open = a.card.clone();
                open.look = QLook::Open;
                let mut dry = B { m: b.m, items: vec![], targets: vec![], loop_fps: 0.0, nw, nh, scroll_max: 0.0, scroll_at: 0.0, copies: vec![], max_h: b.max_h };
                dry.ask("q.dock", &a.id, &open, 0.0).1 + Q_DOCK_TOP
            });
            let work_h = if *working { WORKING_ROW } else { 0.0 };
            let max_h = (d.height / 2.0 - y0 - 8.0 - LIP_BAND - dock.unwrap_or(0.0) - work_h - COMPOSER_GAP - COMPOSER_H - attach_row(attached)).max(LINE * 3.0);
            let visible = if placed.is_empty() && !talk { full.min((max_h / LINE).floor() * LINE) } else { full.min(max_h.floor()) };
            b.scroll_max = (full - visible).max(0.0);
            // Following, it opens on the latest exchange: the user's last words before the turn at the top, else the turn,
            // and the end of the conversation when that is all there is (the newest at the bottom, as in Messages).
            // After a send or a new message it follows the conversation's end, all the way down (bug note
            // 2026-10-26T19-14-21-3).
            let anchor = if !turn || *tail {
                f64::INFINITY
            } else {
                pre.owners.last().copied().filter(|t| shift - t < visible).unwrap_or(shift)
            };
            let scroll = if *follow { anchor } else { *scroll };
            let scroll = if scroll.is_finite() || scroll == f64::INFINITY { scroll.clamp(0.0, b.scroll_max) } else { 0.0 };
            b.scroll_at = scroll;
            // A target that scrolls with the body takes clicks only while enough of it is in view.
            let in_view = |r: Rect| -> Option<Rect> {
                let (top_y, bottom_y) = (r.y.max(y0), (r.y + r.h).min(y0 + visible));
                (bottom_y - top_y >= CLOSE_MIN_H.min(r.h)).then(|| Rect::new(r.x, top_y, r.w, bottom_y - top_y))
            };
            let left = -doc_w / 2.0;
            // Each note's pin and close X, where its header is now; scrolled (mostly) out of the window they take no
            // clicks (scrolling stays a change in place: `Island::in_place`).
            for p in &placed {
                let (cx, cy) = (doc_w / 2.0 - 9.0, y0 + shift + notes_y0 + p.y + NOTE_HEAD / 2.0 - scroll);
                if let Some(r) = in_view(Rect::new(cx - 14.0, cy - 14.0, 28.0, 28.0)) {
                    b.targets.push((Target::NoteClose(p.id.clone()), r));
                }
                if let Some(r) = in_view(Rect::new(cx - NOTE_PIN_GAP - 14.0, cy - 14.0, 28.0, 28.0)) {
                    b.targets.push((Target::NotePin(p.id.clone()), r));
                }
            }
            for (blk, at) in [(&pre, 0.0), (&post, post_y0)] {
                for (id, r) in &blk.cards {
                    if let Some(r) = in_view(Rect::new(left + r.x, y0 + at + r.y - scroll, r.w, r.h)) {
                        b.targets.push((Target::ChatNote(id.clone()), r));
                    }
                }
                // Each message's words where they show now, for Copy (bug note 2026-10-30T22-16-36).
                for (r, text) in &blk.texts {
                    if let Some(r) = in_view(Rect::new(left + r.x, y0 + at + r.y - scroll, r.w, r.h)) {
                        b.copies.push((r, text.clone()));
                    }
                }
            }
            let reply_text = paragraphs.join("\n").trim().to_string();
            if !reply_text.is_empty() && lines_h > 0.0 {
                if let Some(r) = in_view(Rect::new(left, y0 + shift - scroll, doc_w, lines_h)) {
                    b.copies.push((r, reply_text));
                }
            }
            // Each body shows the part of it inside the window [scroll, scroll + visible]; scrolled out, it keeps its
            // place with no height (scrolling is a change in place, never a motion). The reply's words keep their own
            // width at the column's left edge, so a note leaving moves them without redrawing them.
            let window = |from: f64, to: f64| -> (f64, f64, f64) {
                let (a, z) = (scroll.max(from), (scroll + visible).min(to));
                if z > a { (y0 + a - scroll, z - a, a - from) } else if from >= scroll + visible { (y0 + visible, 0.0, 0.0) } else { (y0, 0.0, (to - from).max(0.0)) }
            };
            // Under the top bar (the ears' band, from the shape's top to the window's), the body scrolled up past the
            // window goes on, blurred, and reaches a little into the window where its sharp lines fade in (bug note
            // 2026-10-26T19-14-21: a hard black cut). Nothing above the start: no veil at the top. Every body has its
            // veil, with no height while none shows (scrolling stays a change in place).
            let veil_window = |from: f64, to: f64| -> Option<(f64, f64, f64)> {
                if scroll <= 0.5 {
                    return None;
                }
                let (a, z) = ((scroll - y0).max(from), (scroll + VEIL_INTO).min(to));
                (z - a > 0.5).then(|| (y0 + a - scroll, z - a, a - from))
            };
            let veil_band = y0 + VEIL_INTO;
            let push_veil = |b: &mut B, id: String, from: f64, to: f64, x: f64, w: f64, lines: &[TextRow], notes: &[Placed], marks: &[Mark]| {
                let (body, rect) = match veil_window(from, to) {
                    Some((vy, vh, vs)) => (Content::Body { lines: lines.to_vec(), font: Font::Spoken, color: Rgba::WHITE, line_h: LINE, h: vh, scroll: vs, notes: notes.to_vec(), marks: marks.to_vec(), veil: veil_band }, Rect::new(x, vy, w, vh)),
                    None => (Content::Body { lines: vec![], font: Font::Spoken, color: Rgba::WHITE, line_h: LINE, h: 0.0, scroll: 0.0, notes: vec![], marks: vec![], veil: veil_band }, Rect::new(x, 0.0, w, 0.0)),
                };
                let item = b.push(&id, body, rect);
                item.live = true;
                item.clip = true;
                item.free = true;
            };
            // Each chunk of the conversation is one body; only those in view carry their marks (and get a bitmap).
            let chunk_bodies = |b: &mut B, name: &str, blk: &super::chat_layout::Block, at: f64| {
                for (k, c) in blk.chunks.iter().enumerate() {
                    let (ry, rh, rs) = window(at + c.y0, at + c.y1);
                    let marks = if rh > 0.0 { c.marks.clone() } else { vec![] };
                    let body = Content::Body { lines: vec![], font: Font::Spoken, color: Rgba::WHITE, line_h: LINE, h: rh, scroll: rs, notes: vec![], marks, veil: 0.0 };
                    let item = b.push(&format!("{name}.{k}"), body, Rect::new(left, ry, doc_w, rh));
                    item.live = true;
                    item.clip = true;
                    push_veil(b, format!("{name}.{k}.veil"), at + c.y0, at + c.y1, left, doc_w, &[], &[], &c.marks);
                }
            };
            chunk_bodies(&mut b, "reply.before", &pre, 0.0);
            if !lines.is_empty() || (placed.is_empty() && !talk) {
                let (ry, rh, rs) = window(shift, shift + lines_h);
                let w = widest.max(1.0).min(doc_w);
                let item = b.push("reply.body", Content::Body { lines: lines.clone(), font: Font::Spoken, color: Rgba::WHITE, line_h: LINE, h: rh, scroll: rs, notes: vec![], marks: vec![], veil: 0.0 }, Rect::new(left, ry, w, rh));
                item.live = true;
                item.clip = true;
                // Left-aligned in the centred column (centred itself when no note widens the column).
                item.free = true;
                push_veil(&mut b, "reply.body.veil".into(), shift, shift + lines_h, left, w, &lines, &[], &[]);
            }
            if !placed.is_empty() {
                let (ry, rh, rs) = window(shift + notes_y0, shift + turn_h);
                let item = b.push("reply.notes", Content::Body { lines: vec![], font: Font::Spoken, color: Rgba::WHITE, line_h: LINE, h: rh, scroll: rs, notes: placed.clone(), marks: vec![], veil: 0.0 }, Rect::new(left, ry, doc_w, rh));
                item.live = true;
                item.clip = true;
                push_veil(&mut b, "reply.notes.veil".into(), shift + notes_y0, shift + turn_h, left, doc_w, &[], &placed, &[]);
            }
            chunk_bodies(&mut b, "reply.after", &post, post_y0);
            // Scrolling: the window's bottom fades out while more is below (review of 063424a: a long table gave no
            // sign it scrolls). Always there while it can scroll, so reaching the end changes it in place.
            if b.scroll_max > 0.0 {
                let fade = b.push("reply.fade", Content::Fade { on: scroll < b.scroll_max - 0.5 }, Rect::new(left, y0 + visible - FADE_H, doc_w, FADE_H));
                fade.live = true;
                // The body goes on above too: its top fades into the veil under the bar rather than cutting a line.
                let top = b.push("reply.fade.top", Content::FadeTop { on: scroll > 0.5 }, Rect::new(left, y0, doc_w, FADE_H));
                top.live = true;
            }
            // The turn runs: the thinking dots under the conversation, outside its scroll (always in view), centred as
            // the lip's and a question card's are. Pi's turn and the relay's show the same.
            if *working {
                b.push("reply.working", Content::Dots, Rect::centered(0.0, y0 + visible + WORKING_ROW / 2.0, 36.0, 8.0)).live = true;
            }
            // The question, below the window and outside its scroll: always in reach.
            if let Some(a) = ask {
                let from = b.items.len();
                b.ask("q.dock", &a.id, &a.card, y0 + visible + work_h + Q_DOCK_TOP);
                // It rides with the window's bottom edge as one piece (a cut, as the band's grabber and pin do): its
                // words never glide over its own buttons while the conversation resizes.
                for it in &mut b.items[from..] {
                    it.live = true;
                }
            }
            // The composer sits under the conversation and the docked question, above the band (`Target::Composer`).
            let comp_top = y0 + visible + work_h + dock.unwrap_or(0.0) + COMPOSER_GAP;
            // What was dropped into it sits in the composer above its line (`attach_row`).
            let row = attach_row(attached);
            let h = comp_top + row + COMPOSER_H + 8.0 + LIP_BAND;
            // As wide as the speaking row was: the same object, pulled down (wider only for a note that needs it; a
            // note keeps the note's 28 either side, clear of the corners while it retracts).
            let side = if notes.is_empty() { PAD } else { PAD_MAX };
            let width = (doc_w + 2.0 * side).ceil().clamp(nw.min(MAX_W), reply_max_w(d)).max(pair);
            let cw = (width - 2.0 * PAD).max(160.0);
            let composer = Rect::new(-cw / 2.0, comp_top, cw, COMPOSER_H + row);
            let comp_from = b.items.len();
            b.push("reply.composer.fill", Content::Panel { fill: Rgba::white(0.08), ring: None, radius: COMPOSER_H / 2.0 }, composer);
            attachments(&mut b, attached, composer);
            let field = Rect::new(-cw / 2.0 + 16.0, comp_top + row + 7.0, cw - 32.0, 22.0);
            let message = composer_placeholder();
            b.push("reply.composer", Content::Field { font: Font::Field, owner: Target::Composer, text: None, focus_until_ms: 0 }, field);
            // The placeholder is drawn here while the composer is empty (as the Other row's is).
            if *composer_empty {
                let tw = b.width(&message, Font::Field);
                b.push("reply.composer.placeholder", Content::Text { lines: vec![message.clone()], font: Font::Field, color: Rgba::white(0.56), line_h: 22.0 }, Rect::new(field.x, field.y, tw.max(1.0), 22.0)).free = true;
            }
            // The composer fades in with the motion, clipped by the outline, like a card (`ask_bottom`): on the edge gate
            // its fill and placeholder would appear within a frame as the conversation grows past them.
            for it in &mut b.items[comp_from..] {
                it.clip = true;
                it.gate_top = true;
            }
            b.targets.push((Target::Composer, composer));
            b.push("reply.grabber", Content::Grabber { hot: *hot }, Rect::centered(0.0, h - 11.0, 36.0, 5.0)).live = true;
            // The pin (night 1022): the turn leaves as a floating document island, its notes with it (23 Oct: a table
            // could not be pulled out). Only with something to keep, in the band's right corner (its target ahead of
            // the band's).
            if paragraphs.iter().any(|p| !p.trim().is_empty()) || !notes.is_empty() {
                let (px, py) = (width / 2.0 - PIN_INSET, h - 11.0);
                // Rides with the grabber (live, as it is: the band's elements move together).
                let pin = b.glyph("reply.pin", &Glyph::new("pin.fill").alpha(0.55), 11.0, px, py);
                pin.free = true;
                pin.live = true;
                b.targets.push((Target::ReplyPin, Rect::centered(px, py - 4.0, 28.0, 24.0)));
            }
            // Copy (bug note 2026-10-30T22-16-36: "he couldn't select or copy it"): the reply as written goes to the
            // clipboard; a check stands in for the glyph until the pull-down closes.
            if paragraphs.iter().any(|p| !p.trim().is_empty()) {
                let (cx, cy) = (-width / 2.0 + PIN_INSET, h - 11.0);
                let glyph = if *copied { Glyph::new("checkmark").alpha(0.55) } else { Glyph::new("doc.on.doc").alpha(0.55) };
                let copy = b.glyph("reply.copy", &glyph, 11.0, cx, cy);
                copy.free = true;
                copy.live = true;
                b.targets.push((Target::ReplyCopy, Rect::centered(cx, cy - 4.0, 28.0, 24.0)));
            }
            b.targets.push((Target::ReplyBand, band(width, h)));
            (width, h, Class::Reply)
        }
        Scene::Listening => {
            b.ear(-1.0, &Ear::Glyph(Glyph::new(HELMET)), "listen.helmet");
            b.glyph("listen.wave", &Glyph::new("waveform"), 19.0, b.ear_x(), nh / 2.0).ear = true;
            let (tw, _) = b.text("listen.words", "Listening", Font::Label, Rgba::WHITE, 360.0, 1, 0.0, 50.0);
            (b.col_w(tw).max(pair), 73.0, Class::Row)
        }
        Scene::Question { id, status, card, speech, thinking, hot } => {
            // Thinking dots sit in the card's top padding, under the notch: the card does not move when they come or go
            // (a moving card glided its lines across one another when the agent's turn timed out at 600 s).
            let top = if *thinking && status.is_none() {
                b.push("turn.dots", Content::Dots, Rect::centered(0.0, b.nh + ASK_PAD_TOP / 2.0, 36.0, 8.0));
                0.0
            } else {
                b.status(status)
            };
            // Speaking: his words on top, as the speaking row lays them (the same elements, so they stay where they
            // were), the card below them.
            let card_top = match speech {
                Some(sp) => {
                    b.ear(-1.0, &Ear::Glyph(Glyph::new(HELMET)), "voice.helmet");
                    b.ear(1.0, &Ear::Wave { level: sp.level, flat: sp.stopped }, "voice");
                    // The most his words may take above the card: the newest sentences whose lines still leave the card
                    // inside the bound a shape may reach on this display (`height / 2 + 120`, the sim's I10). A normal
                    // display keeps all six lines; a short one keeps fewer, so the card stays whole.
                    let mut probe = B { m: b.m, items: vec![], targets: vec![], loop_fps: 0.0, nw: b.nw, nh: b.nh, scroll_max: 0.0, scroll_at: 0.0, copies: vec![], max_h: b.max_h };
                    let card_h = probe.ask("q.under", id, card, 0.0).1;
                    // 3 % under the bound: the height spring overshoots its target by a few points (random-962 peaked at
                    // 524.8 for a 519.5 target against 520).
                    let limit = (d.height / 2.0 + 120.0) * 0.97;
                    let speech_top = 50.0 + top;
                    let lines = (0..=SPEAK_LINES)
                        .rev()
                        .find(|&k| {
                            let n: usize = newest_sentences(b.m, &sp.sentences, k).iter().map(|s| s.1.len()).sum();
                            let card_top = if n == 0 { nh + top } else { speech_top + (n - 1) as f64 * LINE + LINE / 2.0 + Q_DOCK_GAP };
                            card_top + card_h <= limit
                        })
                        .unwrap_or(0);
                    let (_, last) = b.sentences_max(&sp.sentences, speech_top, lines);
                    match last {
                        Some(last) => last + LINE / 2.0 + Q_DOCK_GAP,
                        None => nh + top,
                    }
                }
                None => nh + top,
            };
            let (widest, bottom) = b.ask(if speech.is_some() { "q.under" } else { "q" }, id, card, card_top);
            let w = b.col_w(widest).max(if speech.is_some() { pair } else { 0.0 });
            // Without a notch nothing could bring a tucked card back: no lip there.
            if card.look != QLook::Open || !d.has_notch() {
                (w, bottom, Class::Question)
            } else {
                // The pull-down's lip band, as the conversation and the reader end: 8 pt under the card's last row, the
                // grabber in the band, the band a target (a click, or a drag up through the host). It tucks the card
                // away; the question stays pending and the notch brings it back.
                let h = bottom - PAD_BOTTOM_MAX + 8.0 + LIP_BAND;
                b.push("q.grabber", Content::Grabber { hot: *hot }, Rect::centered(0.0, h - 11.0, 36.0, 5.0)).live = true;
                b.targets.push((Target::AskTuck(id.clone()), band(w, h)));
                (w, h, Class::Question)
            }
        }
        Scene::Reader { speed, left, face, playing, started, hot, status } => {
            b.ear(-1.0, &Ear::Num(speed.to_string()), "reader.speed");
            b.ear(1.0, &Ear::Num(left.clone()), "reader.left");
            let top = b.status(status);
            let fy = READER_WORD_Y + top;
            let cy = READER_CTRL_Y + top;
            let face_item: (Content, Rect, bool, Fx) = match face {
                ReaderFace::Title(t) => {
                    let tt = super::measure::ellipsize(b.m, t, Font::ReaderTitle, 400.0, false);
                    let tw = b.width(&tt, Font::ReaderTitle).clamp(1.0, 400.0);
                    (Content::Text { lines: vec![tt], font: Font::ReaderTitle, color: Rgba::WHITE, line_h: 28.0 }, Rect::centered(0.0, fy, tw, 28.0), false, Fx::None)
                }
                ReaderFace::Focus => (Content::Dot { color: Rgba::RED, d: 8.0 }, Rect::centered(0.0, fy, 8.0, 8.0), false, Fx::None),
                ReaderFace::Blank => (Content::Text { lines: vec![], font: Font::Reader(30), color: Rgba::WHITE, line_h: 36.0 }, Rect::centered(0.0, fy, 1.0, 36.0), false, Fx::None),
                ReaderFace::Unit { text, focus } => {
                    let chars: Vec<char> = text.chars().collect();
                    let i = (*focus).min(chars.len().saturating_sub(1));
                    let pre: String = chars[..i.min(chars.len())].iter().collect();
                    let mid: String = chars.get(i).map(|c| c.to_string()).unwrap_or_default();
                    let post: String = chars.get(i + 1..).map(|s| s.iter().collect()).unwrap_or_default();
                    let mut size = 30u8;
                    let sides = |s: u8| {
                        let f = Font::Reader(s);
                        let half = b.width(&mid, f) / 2.0;
                        (b.width(&pre, f) + half, b.width(&post, f) + half)
                    };
                    while size > 24 {
                        let (l, r) = sides(size);
                        if l <= CHUNK_SIDE && r <= CHUNK_SIDE {
                            break;
                        }
                        size -= 1;
                    }
                    let (l, r) = sides(size);
                    // Never wider than the reader: a unit that still does not fit is clipped by the shape.
                    let (l, r) = (l.min(CHUNK_SIDE + 30.0), r.min(CHUNK_SIDE + 30.0));
                    (Content::Unit { text: text.clone(), focus: i, size: size as f64 }, Rect::new(-l, fy - 18.0, (l + r).max(1.0), 36.0), true, Fx::None)
                }
                ReaderFace::Sentence { read, rest } => {
                    let tw = b.width(&format!("{read}{rest}"), Font::Sentence).clamp(1.0, 420.0);
                    (Content::Split { read: read.clone(), rest: rest.clone() }, Rect::centered(0.0, fy, tw, 24.0), false, Fx::None)
                }
                ReaderFace::End => (Content::Symbol { name: "faceid".into(), size: 40.0, palette: vec![Rgba::GREEN], bold: false }, Rect::centered(0.0, fy, 46.0, 46.0), false, Fx::Bounce),
            };
            let (content, rect, free, fx) = face_item;
            let item = b.push("reader.face", content, rect);
            item.live = true;
            item.free = free;
            item.fx = fx;
            item.clip = true;
            let ticks = matches!(face, ReaderFace::Focus);
            for (id, y) in [("reader.tick.top", fy - 30.0 + 4.5), ("reader.tick.bottom", fy + 21.0 + 4.5)] {
                let it = b.push(id, if ticks { Content::Tick } else { Content::Dot { color: Rgba::RED, d: 0.0 } }, Rect::centered(0.0, y, 2.0, 9.0));
                it.live = true;
            }
            let dim = |on: bool| if on { 1.0 } else { 0.3 };
            b.glyph("reader.slower", &Glyph::new("minus").bold(), 17.0, -168.0, cy);
            b.glyph("reader.back", &Glyph::new("backward.end.fill").alpha(dim(*started)), 16.0, -84.0, cy);
            // A white circle and the system's symbol on it: play ↔ pause swaps with the replace transition.
            b.push("reader.play", Content::Dot { color: Rgba::WHITE, d: 44.0 }, Rect::centered(0.0, cy, 44.0, 44.0));
            let (name, dx) = if *playing { ("pause.fill", 0.0) } else { ("play.fill", 1.5) };
            // Live: play ↔ pause is a change in place (the image view's replace transition), not a motion.
            b.glyph("reader.play.icon", &Glyph::new(name).colors(&[Rgba::BLACK]).bold(), 18.0, dx, cy).live = true;
            b.glyph("reader.repeat", &Glyph::new("arrow.counterclockwise").bold().alpha(dim(*started)), 16.0, 84.0, cy);
            b.glyph("reader.faster", &Glyph::new("plus").bold(), 17.0, 168.0, cy);
            let h = cy + 22.0 + 10.0 + LIP_BAND;
            b.push("reader.grabber", Content::Grabber { hot: *hot }, Rect::centered(0.0, h - 11.0, 36.0, 5.0)).live = true;
            let mut t = vec![(Target::ReaderSlower, -168.0), (Target::ReaderPlay, 0.0), (Target::ReaderFaster, 168.0)];
            if *started {
                t.push((Target::ReaderBack, -84.0));
                t.push((Target::ReaderRepeat, 84.0));
            }
            for (target, x) in t {
                b.targets.push((target, Rect::centered(x, cy, 44.0, 44.0)));
            }
            b.targets.push((Target::ReaderFace, Rect::centered(0.0, fy, 400.0, 44.0)));
            b.targets.push((Target::ReaderBand, band(READER_W, h)));
            (READER_W, h, Class::Reader)
        }
        Scene::Drawing { ink, ears, send, close } => {
            match ears {
                // The same slots as undo and clear: the system's replace transition swaps them in place.
                Some((l, r)) => {
                    b.ear(-1.0, &Ear::Glyph(l.clone()), "draw.undo");
                    b.ear(1.0, &Ear::Glyph(r.clone()), "draw.clear");
                }
                None => {
                    let a = if *ink { 1.0 } else { 0.35 };
                    b.ear(-1.0, &Ear::Glyph(Glyph::new("arrow.uturn.backward").bold().alpha(a)), "draw.undo");
                    b.ear(1.0, &Ear::Glyph(Glyph::new("trash").alpha(a)), "draw.clear");
                    if *ink {
                        let ex = b.ear_x();
                        b.targets.push((Target::DrawUndo, Rect::centered(-ex, nh / 2.0, EAR, nh)));
                        b.targets.push((Target::DrawClear, Rect::centered(ex, nh / 2.0, EAR, nh)));
                    }
                }
            }
            let style = |s: SendState| match s {
                SendState::Rest => Capsule::White,
                SendState::Hover => Capsule::Hover,
                SendState::Pressed => Capsule::Pressed,
            };
            let (bw, bh) = DRAW_BTN;
            let cx = 6.0 + bw / 2.0;
            let close_rect = b.capsule("draw.close", "Close", Some("esc"), None, style(*close), -cx, DRAW_BTN_Y, bw, bh);
            let send_rect = b.capsule("draw.send", "Send", Some("↩"), None, style(*send), cx, DRAW_BTN_Y, bw, bh);
            // Hover and press answer the pointer at once (cuts, not motions).
            for it in b.items.iter_mut().filter(|i| &*i.id == "draw.close" || &*i.id == "draw.send") {
                it.live = true;
                it.free = true;
            }
            b.targets.push((Target::DrawClose, close_rect));
            b.targets.push((Target::DrawSend, send_rect));
            (pair, DRAW_H, Class::Drawing)
        }
    };
    // The shape holds every element: wider for content that needs it (up to 480), and anything still too big is
    // clipped by the shape rather than drawn past it.
    let reach = b.items.iter().filter(|i| !i.ear).map(|i| i.rect.x.abs().max(i.rect.max_x().abs())).fold(0.0, f64::max);
    // The reader keeps its one width: a unit past `CHUNK_SIDE` is clipped, never a wider reader (I21).
    let w = if class == Class::Reader { w } else { w.max(2.0 * reach + 2.0 * 12.0) };
    let most = most_w(class, d);
    let w = w.clamp(nw.min(most), most);
    for it in &mut b.items {
        let wide = it.rect.x < -w / 2.0 + 4.0 || it.rect.max_x() > w / 2.0 - 4.0;
        let deep = it.rect.max_y() > h - 2.0;
        if !it.ear && (wide || deep) {
            it.clip = true;
        }
    }
    // The turn's lip (sent, thinking, compacting), its working words and the voice offer nothing else to click: the
    // whole shape opens the conversation (the voice's: puts its words away), as the notch does at rest (bug note 2026-10-25T21-28-46: it could not be
    // clicked open while Heimdall was thinking). The click opens it only when there is something to show.
    let passive = matches!(scene, Scene::Lip(Lip::Sent | Lip::Dots | Lip::Compress) | Scene::Working { .. } | Scene::Speaking { .. });
    if passive && b.targets.is_empty() {
        // Between the rounded bottom corners, the shape's full depth (inside the outline everywhere).
        let c = (super::Shape { w, h, flare: FLARE }.corner() + 1.0).min(w / 2.0 - 16.0);
        // The voice's words are put away by a click instead (`Target::Hush`); the notch at rest still opens them.
        let target = if matches!(scene, Scene::Speaking { .. }) { Target::Hush } else { Target::Token(Token::Reply) };
        b.targets.push((target, Rect::new(-w / 2.0 + c, 0.0, w - 2.0 * c, h - 1.0)));
    }
    let ear_reach = b.items.iter().filter(|i| i.ear).map(|i| i.rect.x.abs().max(i.rect.max_x().abs())).fold(0.0, f64::max);
    Layout { w, h, flare: FLARE, items: b.items, targets: b.targets, loop_fps: b.loop_fps, class, ear_reach, scroll_max: b.scroll_max, scroll_at: b.scroll_at, copies: b.copies }
}

/// The composer's placeholder, the one place its words come from: the island draws it while the composer is empty
/// (the native field draws none; bug note 2026-10-25T23-12-07).
pub fn composer_placeholder() -> String {
    format!("Message {}", crate::identity::name())
}

/// I21, sized to its content: how far a laid-out shape is larger than its content needs, or `None` when it fits. The
/// content is every element, ears included (an ear pair needs the ears' width); the bound is the content plus
/// `PAD_MAX` either side and `PAD_BOTTOM_MAX` below, never less than the notch. The reader is exempt by design: it
/// keeps one width (`READER_W`, its widest unit) so the shape never resizes per word.
pub fn oversize(l: &Layout, d: &Display) -> Option<String> {
    if matches!(l.class, Class::Rest) {
        return None;
    }
    if l.class == Class::Reader {
        return (l.w > READER_W + 0.5).then(|| format!("reader {:.0} wide, more than its widest unit needs ({READER_W:.0})", l.w));
    }
    let (nw, nh) = d.notch_size();
    let (mut reach, mut bottom) = (0.0f64, 0.0f64);
    for it in l.items.iter().filter(|i| i.rect.is_finite() && i.rect.w > 0.0 && i.rect.h > 0.0) {
        reach = reach.max(it.rect.x.abs()).max(it.rect.max_x().abs());
        bottom = bottom.max(it.rect.max_y());
    }
    let max_w = (2.0 * reach + 2.0 * PAD_MAX).ceil().max(nw).min(most_w(l.class, d));
    let max_h = (bottom + PAD_BOTTOM_MAX).ceil().max(nh);
    let mut out = vec![];
    if l.w > max_w + 0.5 {
        out.push(format!("{:.0} wide for {:.0} of content (at most {max_w:.0})", l.w, 2.0 * reach));
    }
    if l.h > max_h + 0.5 {
        out.push(format!("{:.0} deep for content down to {bottom:.0} (at most {max_h:.0})", l.h));
    }
    (!out.is_empty()).then(|| format!("{:?}: {}", l.class, out.join(", ")))
}

/// The widest a shape of this class may be: a question card its own width, the pulled-down reply up to its limit.
fn most_w(class: Class, d: &Display) -> f64 {
    match class {
        Class::Reply => reply_max_w(d),
        Class::Question => ASK_W,
        _ => MAX_W,
    }
}

/// A body's bottom band as a click target: the band and a little above it, inside the shape's rounded corners.
fn band(w: f64, h: f64) -> Rect {
    let a = super::geom::Shape { w, h, flare: FLARE }.corner();
    let lift = 3.0;
    let inset = if a > lift { a - (a * a - (a - lift) * (a - lift)).max(0.0).sqrt() + 1.0 } else { 1.0 };
    Rect::new(-w / 2.0 + inset, h - BAND_TARGET - lift, w - 2.0 * inset, BAND_TARGET)
}

/// The token target for a scene's ears.
pub fn token(t: Token) -> Option<Target> {
    Some(Target::Token(t))
}

/// The newest sentences whose lines fit in `max_lines`, oldest first, each with its index, lines and width. The newest
/// always shows (cut to `max_lines` if it alone is longer).
pub fn newest_sentences(m: &dyn Measure, sentences: &[String], max_lines: usize) -> Vec<(usize, Vec<String>, f64)> {
    let mut kept = vec![];
    let mut n = 0;
    for (k, s) in sentences.iter().enumerate().rev() {
        let (lines, w) = wrap_even(m, s, Font::Spoken, SPEAK_W, max_lines);
        if lines.is_empty() {
            continue;
        }
        if !kept.is_empty() && n + lines.len() > max_lines {
            break;
        }
        n += lines.len();
        kept.push((k, lines, w.max(1.0).min(SPEAK_W)));
    }
    kept.reverse();
    kept
}

#[cfg(test)]
mod question_line_tests {
    /// Rendered 3 Nov (lab/speakers/voice-naming/render/suggest/): the first two stayed on one line, the last two
    /// wrapped. The second really fit but is just over the safe limit: it is cut a word earlier.
    #[test]
    fn the_one_line_estimate_matches_the_renders() {
        for (text, one) in [
            ("Are you talking to Kanan? \u{201c}It was way faster than last…\u{201d}", true),
            ("New voice: \u{201c}It was way faster than last week, honestly.\u{201d}", false),
            ("Are you talking to Mari? Or Ellie? \u{201c}It was way faster than…\u{201d}", false),
            ("New voice: \u{201c}Whoever wins Monday's WWDC Workshop owes…\u{201d}", false),
        ] {
            assert_eq!(super::question_fits_one_line(text), one, "{text}");
        }
    }
}
