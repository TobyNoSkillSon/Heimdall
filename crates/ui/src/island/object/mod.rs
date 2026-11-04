//! The island as one object (DESIGN.md, `lab/island-design/`): one state machine, one layout engine, one motion
//! clock. Everything Heimdall shows at the notch — lines, the agent's turn, the voice, questions, notes, the fast
//! reader, drawing mode's keys and light, pointing marks, health — is a state of this object, drawn as one black
//! shape that grows out of the notch and returns into it.
//!
//! The interface (lab/testing/TOOLING.md §1.2): `apply(event, t)` is the only way state changes; `frame(t)` is a pure
//! function giving the shape, every content slot, the drawing light and the pointing marks at `t`;
//! `needs_frames(t)` says whether anything moves; `deadline()` is the next one-shot change; `hit(t, p)` says what a
//! click at `p` lands on; `track(t)` is the shape's motion for anything that follows it. Time is a number the caller
//! passes in: the app's seconds since launch, or the simulator's virtual seconds. Nothing here reads a clock or
//! touches AppKit; `mac/island` paints a `FrameModel` and forwards input.

pub(crate) mod chat_layout;
mod engine;
pub mod gallery;
pub mod geom;
mod layout;
pub mod measure;
pub mod prose;
pub mod reader;
mod scene;
pub mod sim;
pub mod spring;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod ask_tests;
#[cfg(test)]
mod open_chat_tests;
#[cfg(test)]
mod composer_drop_tests;
#[cfg(test)]
mod heard_exit_tests;
#[cfg(test)]
mod notch_tests;
#[cfg(test)]
mod switch_tests;
#[cfg(test)]
mod vella_tests;
mod emergency_tests;
#[cfg(test)]
mod busy_tests;
#[cfg(test)]
mod countdown_tests;
#[cfg(test)]
mod stuck_reply_tests;

use std::sync::Arc;

pub use engine::Island;
pub use geom::{Display, Point, Rect, Shape};
pub use layout::{question_fits_one_line, composer_placeholder, note_max_w, reply_max_w, Class, Ear, Glyph, Lip, NoteView, QLook, ReaderFace, Scene, CHUNK_SIDE, NOTE_PIN_GAP, REPLY_CUT, VEIL_INTO};
pub use measure::{Approx, Font, Fuzz, FuzzWidth, Measure};
pub use scene::{rubber, BUSY_READ, EMERGENCY_SYMBOL, MAX_TURN_NOTES, NOTE_UNATTENDED, NUDGE_COALESCE, NUDGE_QUEUE, NUDGE_SECONDS, NUDGE_WOBBLE, PULL_OUT, RESTART_READ};

use crate::agent_notice::{AgentNotice, AgentNoticeAction};
use crate::island::{Activity, CommandCue, HealthControl, HealthCue, IslandReport};
use crate::reader::{FastReadUpdate, ReadResult};

/// A colour, sRGB 0–255 with alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const WHITE: Rgba = Rgba([255, 255, 255, 255]);
    pub const BLACK: Rgba = Rgba([0, 0, 0, 255]);
    pub const GREEN: Rgba = Rgba([0x30, 0xD1, 0x58, 255]);
    pub const RED: Rgba = Rgba([0xFF, 0x45, 0x3A, 255]);
    pub const ORANGE: Rgba = Rgba([0xFF, 0x9F, 0x0A, 255]);
    pub const BLUE: Rgba = Rgba([0x0A, 0x84, 0xFF, 255]);
    /// The user's drawing ink.
    pub const INK: Rgba = Rgba([0xFF, 0x3B, 0x30, 255]);
    /// Key hints inside buttons.
    pub const GREY: Rgba = Rgba([0x8E, 0x8E, 0x93, 255]);
    /// A pressed button's fill.
    pub const PRESSED: Rgba = Rgba([0xD9, 0xD9, 0xD9, 255]);

    pub fn white(alpha: f64) -> Rgba {
        Rgba([255, 255, 255, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8])
    }
    pub fn alpha(self) -> f64 {
        self.0[3] as f64 / 255.0
    }
    pub fn with_alpha(self, a: f64) -> Rgba {
        let [r, g, b, _] = self.0;
        Rgba([r, g, b, (a.clamp(0.0, 1.0) * 255.0).round() as u8])
    }
}

/// A symbol effect, played by the system (`NSSymbolEffect` on the slot's image view, in the render server): the
/// island only says which and when the symbol has visibly arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fx {
    None,
    /// Once, when the symbol has arrived: the system's bounce.
    Bounce,
    /// Once, when the symbol has arrived: the system's wiggle (errors).
    Wiggle,
    /// Repeating while held: the system's rotate (restarting).
    Rotate,
    /// Repeating while held: the system's breathe (compacting at rest).
    Breathe,
    /// Repeating while held: the system's variable colour, one layer lit after another and back (catching up). Only
    /// for a symbol with variable-colour layers (`VARIABLE_SYMBOLS`); any other symbol breathes instead.
    Variable,
}

/// Symbols checked to have variable-colour layers (rendered at variable value 0, 0.5 and 1 on macOS 27, 31 Oct).
pub const VARIABLE_SYMBOLS: &[&str] = &["square.stack.3d.up.fill", "square.stack.3d.up", "square.3.layers.3d", "progress.indicator", "timelapse", "ellipsis", "chart.bar.fill", "wave.3.right"];

impl Fx {
    /// A loop held while its state lasts (played by the render server; nothing ticks for it).
    pub fn repeats(self) -> bool {
        matches!(self, Fx::Rotate | Fx::Breathe | Fx::Variable)
    }
}

/// What a slot draws. Plain data: the app rasterises text and symbols once per distinct content (`key`), never per
/// frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// An SF Symbol at `size` points (semibold unless `bold`), in its palette (one colour, or glyph + fill).
    Symbol { name: String, size: f64, palette: Vec<Rgba>, bold: bool },
    /// Heimdall's helmet, `size` points tall.
    Helmet { size: f64, color: Rgba },
    /// Centred lines of text, `line_h` apart.
    Text { lines: Vec<String>, font: Font, color: Rgba, line_h: f64 },
    /// A reader unit: the character at `focus` (a char index) red and centred on the slot's anchor.
    Unit { text: String, focus: usize, size: f64 },
    /// The paused reader's sentence: the read part red, the rest white.
    Split { read: String, rest: String },
    /// Three 8 pt dots; their opacities come from the slot's loop phase (an iterative wave, 0.9 s).
    Dots,
    /// Five bars following the voice's level (flat when the voice stopped).
    Waveform { level: u16, flat: bool },
    /// A loading spinner; its phase comes from the slot.
    Spinner,
    /// A white capsule button.
    Capsule { label: String, key: Option<String>, symbol: Option<String>, style: Capsule },
    /// A rounded rectangle (a question card's option row, tile, chip, key cap or Send), filled and/or ringed (`ring`:
    /// colour and width).
    Panel { fill: Rgba, ring: Option<(Rgba, f64)>, radius: f64 },
    /// A native text field (typed text: the Other row, the composer). Drawn by the window as a real field, placed on
    /// the slot's rect; what the user types goes to `owner` (`Target::AskOther`, `Target::Composer`). `text` is what the
    /// model holds for it (the Other row's answer, empty when none): the window shows it whenever the model changes
    /// it, so a field never keeps text the model dropped. `focus_until_ms`: the field takes the keyboard when it is first
    /// placed before then (`agent_notice::OtherField`). `None` for the composer, which the user alone writes. A field has
    /// no placeholder of its own: the layout draws it as a text element while the field is empty, so a label is never
    /// drawn twice (the native placeholder sat 2 pt off the layout's, a ghost).
    Field { font: Font, owner: Target, text: Option<String>, focus_until_ms: u64 },
    /// A filled circle `d` points across.
    Dot { color: Rgba, d: f64 },
    /// A countdown's ring, `d` points across: a faint track and, over it, the share still left (`left_milli`
    /// thousandths) ending at twelve o'clock, so the spent part grows clockwise from the top.
    Ring { left_milli: u16, d: f64 },
    /// A 2 × 9 pt red tick (the reader's focus lead-in).
    Tick,
    /// The 36 × 5 pt grabber on a lip band.
    Grabber { hot: bool },
    /// The bottom of a scrolling body fading to black while more of it is below (`on`); nothing at its end.
    Fade { on: bool },
    /// The top of the scrolling conversation fading in from black while more of it is above (`on`).
    FadeTop { on: bool },
    /// The pulled-down last turn: the reply's lines (left-aligned, as written), then the turn's notes, each a header
    /// (icon, title, close X) over its body drawn by the app (`Placed`). Drawn once whole, `h` points of it show from
    /// `scroll` points down.
    /// Each row keeps its own left edge and top (paragraph gaps, a list item's hanging lines) and weight (`prose`).
    /// `marks` draw the conversation around the turn (the user's bubbles, earlier replies, action and notice rows).
    /// `veil`: the part scrolled up under the top bar, drawn blurred and fading out toward the screen's edge, as an
    /// iOS material (bug note 2026-10-26T19-14-21: "I just want the blur, so above that is just blurred text and
    /// blurred colors"), not cut by a black band. 0 for a plain body; for a veil, the band's bottom (island points from
    /// the top): the veil fades in from the shape's top and out toward that edge.
    Body { lines: Vec<prose::TextRow>, font: Font, color: Rgba, line_h: f64, h: f64, scroll: f64, notes: Vec<Placed>, marks: Vec<Mark>, veil: f64 },
    /// Vella's own mark (its app icon, `assets/island/vella.png`), `size` points square: the user dictates to Vella.
    Vella { size: f64 },
    /// Something dropped into the composer, filling its slot with corner radius `radius`: the image at `path` cropped to
    /// fill, or SF Symbol `symbol` on a dark tile (a file, link or text, or an image that cannot be read).
    Thumb { path: Option<std::path::PathBuf>, image: bool, symbol: String, radius: f64 },
}

/// One drawn piece of the conversation in a body, in points from the body's top-left.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    /// A rounded rectangle (a message bubble, a notice row's card), corner radius `r`.
    Fill { x: f64, y: f64, w: f64, h: f64, r: f64, color: Rgba },
    /// One line of text from `x`, `y` its row's top, `line_h` its row's height.
    Text { text: String, x: f64, y: f64, line_h: f64, font: Font, color: Rgba },
    /// An SF Symbol centred at (`cx`, `cy`).
    Symbol { name: String, size: f64, palette: Vec<Rgba>, cx: f64, cy: f64 },
}

impl Mark {
    /// How far down it reaches.
    pub fn bottom(&self) -> f64 {
        match self {
            Mark::Fill { y, h, .. } => y + h,
            Mark::Text { y, line_h, .. } => y + line_h,
            Mark::Symbol { size, cy, .. } => cy + size * 0.6,
        }
    }
}

/// A note in the pulled-down turn, in points from the body's top-left: its header row at `y` (`NOTE_HEAD` tall, the
/// body's width), then the app's drawing of its detail (`w` × `h`, the notice's raster by `id`).
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub id: String,
    /// Bumped on every update of the same id (the raster under `id` was redrawn).
    pub rev: u64,
    pub icon: String,
    pub palette: Vec<Rgba>,
    pub title: String,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A note's header row in the pulled-down turn: icon, title and the close X.
pub const NOTE_HEAD: f64 = 30.0;
/// Space above a note's header when something is above it.
pub const NOTE_GAP: f64 = 14.0;

impl Content {
    /// A body's whole height (its lines, then its notes).
    pub fn body_full(&self) -> f64 {
        match self {
            Content::Body { lines, line_h, notes, marks, .. } => {
                let text = lines.last().map_or(0.0, |r| r.y + line_h);
                notes.iter().map(|n| n.y + NOTE_HEAD + n.h).chain(marks.iter().map(Mark::bottom)).fold(text, f64::max)
            }
            _ => 0.0,
        }
    }
}

/// A capsule's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capsule {
    /// White, black text.
    White,
    /// White with a 1.5 pt ring 2 pt outside (under the pointer).
    Hover,
    /// 0.95 scale, grey fill (held down).
    Pressed,
    /// The chosen answer: white with a check.
    Chosen,
    /// An unchosen answer after a choice: 12 % white, dim text (a state, not a button).
    Dim,
}

impl Content {
    /// A stable key for caching the content's raster.
    pub fn key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            // A field's text is not its identity: typing changes the text and never the field (no ghost, no motion).
            Content::Field { font, owner, .. } => format!("Field {font:?} {owner:?}").hash(&mut h),
            _ => format!("{self:?}").hash(&mut h),
        }
        h.finish()
    }
    /// Whether its pixels change with the loop phase (drawn as live layers, not a cached raster).
    pub fn animated(&self) -> bool {
        matches!(self, Content::Dots | Content::Spinner)
    }
}

/// One thing drawn in the island at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// Identity across states: the same id in two states is the same element (it stays and glides).
    pub id: Arc<str>,
    pub content: Arc<Content>,
    /// `content.key()`.
    pub key: u64,
    /// Where it draws now (scale applied), island coordinates.
    pub rect: Rect,
    /// The content's natural size (what its raster is made at).
    pub size: (f64, f64),
    pub opacity: f64,
    /// Gaussian blur radius, points.
    pub blur: f64,
    /// The symbol effect asked for, and whether the slot has visibly arrived (opacity ≥ 0.9 and the shape within a
    /// point of its target): arrival effects start then, not while the symbol is still fading in.
    pub fx: Fx,
    pub arrived: bool,
    /// Loop phase in [0, 1) for dots and spinners.
    pub phase: f64,
    /// An ear (beside the notch, inside the menu-bar band) rather than column content.
    pub ear: bool,
    /// Clipped by the shape's outline (bodies that may be swallowed by the notch while retracting).
    pub clip: bool,
    /// When this content arrived (a motion start or the event that changed it).
    pub since: f64,
}

/// The drawing light at one moment (DESIGN.md §4, "Drawing mode on the same clock").
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Light {
    /// How far the light has run from the island's flares round the screen edge, 0–1 of each half.
    pub run: f64,
    /// Its brightness: the breathing (0.3–1), scaled by the run while it opens or closes.
    pub opacity: f64,
    /// The capture's white flash over the screen (0 to 0.16).
    pub flash: f64,
}

/// Everything at the notch at one time. Plain data, so invariants are arithmetic on it.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameModel {
    pub t: f64,
    /// False at rest: nothing is drawn and the window can be ordered out.
    pub visible: bool,
    pub shape: Shape,
    /// The island's outline lit (drawing mode), 0–1.
    pub glow: f64,
    pub slots: Vec<Slot>,
    pub light: Light,
    /// The agent's pointing marks' opacity (0: none showing).
    pub pointing: f64,
    /// The motion this frame belongs to: its id and start.
    pub motion: (u64, f64),
    /// Something is still moving (the shape, content, an arrival effect, the light, the flash).
    pub moving: bool,
    /// The display the frame is laid out for.
    pub display: Display,
    /// The target layout's size (what the shape is moving to).
    pub target: (f64, f64),
    /// Night 1022: the nudge's landing jiggle is playing (a deliberate swell, kept by the user's call on 27 Oct): the
    /// motion judge does not count its width reversals as unsmooth.
    pub jiggle: bool,
}

/// Whether the caller must keep drawing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FrameNeed {
    /// Nothing moves: no frames, no timers except `deadline`.
    None,
    /// Only declared loops move (thinking dots, spinner, rotate, breathe, the light's breathing): `fps` frames a
    /// second, phase-locked to `epoch` with `period`.
    Loop { epoch: f64, period: f64, fps: f64 },
    /// A motion runs: every display frame until it settles.
    Moving,
}

/// What a click lands on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// Option `option` of question `question` on card `id` (a click, or Option plus its digit).
    AskPick { id: String, question: usize, option: usize },
    /// Question `question`'s step chip: its tab.
    AskTab { id: String, question: usize },
    /// Question `question`'s Other row: its field takes the keyboard.
    AskOther { id: String, question: usize },
    /// Question `id`'s bottom lip (the pull-down's grabber band): a click or a drag up tucks the card away. It stays
    /// pending, unanswered, and the notch brings it back as it pulls the last turn down (bug note 2026-10-28T19-45-55).
    AskTuck(String),
    /// Send on card `id`: answers every question.
    AskSend(String),
    /// The expanded conversation's composer: its field takes the keyboard, Return sends.
    Composer,
    /// The remove badge of the composer's attachment at this index (a drop on the pull-down while he types).
    ComposerDetach(usize),
    Dismiss(String),
    Connect(String),
    /// The problem's fix button ("Open Settings").
    HealthButton,
    /// A token at rest (the whole ear pair): a click opens what it stands for.
    Token(Token),
    /// A note's close X in the pulled-down turn: removes it for good.
    NoteClose(String),
    /// A note's pin in the pulled-down turn: that note alone leaves as a floating island.
    NotePin(String),
    /// An earlier notice's row in the conversation: it leaves as a floating island.
    ChatNote(String),
    ReaderPlay,
    ReaderBack,
    ReaderRepeat,
    ReaderSlower,
    ReaderFaster,
    /// The words: pause and show the sentence, or resume.
    ReaderFace,
    /// The reader's bottom band: pulls it up.
    ReaderBand,
    /// The pulled-down reply's bottom band: pulls it up.
    ReplyBand,
    /// The voice's words under the notch: a click puts them away until the next reply (bug note 2026-10-31T23-37-26:
    /// "a click dismisses it"). The audio plays on; the pull-down keeps the words.
    Hush,
    /// The pulled-down reply's copy glyph: the reply as written goes to the clipboard.
    ReplyCopy,
    /// The pulled-down reply's pin (night 1022): the reply leaves the notch as a floating document island.
    ReplyPin,
    DrawUndo,
    DrawClear,
    DrawClose,
    DrawSend,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Problem,
    Reader,
    /// The notch at rest: pulled down (a click, a drag or a swipe down), it shows the last reply.
    Reply,
}

/// What the island tells the app (through `UiAction`) or the window host (drawing mode's strokes and capture).
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Notice(AgentNoticeAction),
    /// The user sent text typed in the composer: the same action as the Agent window's Send (`steer` false), so the agent
    /// takes it as his typed message.
    Typed(String),
    /// The user sent the composer with what he dropped into it (`text` trimmed, possibly empty): his typed message with
    /// the drop's items, as a drop under his words (`UiAction::TypedWithDrop`).
    TypedDrop { text: String, items: Vec<crate::island::drop::DropItem> },
    Health { key: String, control: HealthControl },
    Read(ReadResult),
    /// Put this text on the clipboard (`Target::ReplyCopy`).
    Copy(String),
    /// Capture the display with the user's ink; the island's keys retract with the light.
    DrawSend,
    /// Close the overlay and discard the ink.
    DrawClose,
    DrawUndo,
    DrawClear,
    Report(IslandReport),
    /// The pulled-down turn was pulled out (dragged down past `PULL_OUT`, or its pin): the app keeps its reply
    /// (`text`, empty when none) and its notes (ids) as a floating document island (`UiAction::PullOut`). A note's own
    /// pin, or an earlier notice's row, sends that note alone.
    PullOut { text: String, notes: Vec<String> },
}

/// Everything that can happen to the island (lab/testing/TOOLING.md §1.3).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Heimdall's voice: `true` when audio starts playing, `false` when the audio actually ended (the last sample
    /// played, or playback was cut). This is the "audio ended" input the speaking look ends on.
    Speaking(bool),
    /// The sentence being spoken now (`None`: the stream ended; the words go with the voice's grace).
    SpokenText(Option<String>),
    /// The last reply as written (line breaks kept): what pulling the notch down shows.
    LastReply(String),
    /// That reply never reached the user: it leaves the pull-down (no-op once a later reply replaced it).
    WithdrawReply(String),
    /// The conversation around the last turn, oldest first (`crate::island::chat`): shown when the user pulls it down.
    Chat(Vec<crate::island::chat::ChatEntry>),
    /// The playing voice's loudness, 0–1 (NaN and out-of-range values are clamped).
    SpeechLevel(f32),
    /// The user's speech was delivered to the agent.
    Sent,
    /// ("That is an animation, but also speech"): the turn's lip (send arrow, thinking dots) waits
    /// this many more seconds, the spoken ack's own moment (`Settings::ack_delay_ms` of quiet after his last voice);
    /// `f64::INFINITY` while he talks, 0 shows it now.
    Hold(f64),
    /// The agent thinks or compacts (`Some`), or its turn ended (`None`).
    Activity(Option<Activity>),
    /// The user stopped the agent (barge-in): the voice and the turn end at once.
    Stop,
    /// Heimdall's audio was cut (an interrupt, barge-in, Speaking off): the speaking look ends at once, with no grace.
    /// Unlike `Speaking(false)` (the audio ended naturally, which holds the look for `GRACE`), and the turn is untouched.
    SpeechCut,
    /// An agent notice: a line, a question, Connect T3, an outcome; `body` is a note's measured size.
    Notice { notice: AgentNotice, body: Option<(f64, f64)> },
    /// The agent cleared one of its notices (or questions) by id.
    NoticeClear(String),
    ClearNotices,
    /// A question answered by voice.
    CloseQuestion { id: String, answer: String },
    /// Voice answered one question of an incomplete card: the card shows that pick and moves on (nothing is sent).
    AskSet { id: String, answer: crate::agent_notice::QuestionAnswer },
    /// The user picked option `option` of question `question` (a click on a card). The first event is the one that answers
    /// a single-question single-select card.
    AskPick { id: String, question: usize, option: usize },
    /// Option plus a digit (1 to 4): option N-1 of the card's question on its tab.
    AskDigit(usize),
    /// Esc in question `id`'s field: closes it as dismissed, the way an answer closes it.
    AskDismiss(String),
    /// The user typed `text` in question `question`'s Other field (empty clears it).
    AskOther { id: String, question: usize, text: String },
    /// Show question `question` of card `id` on its tab.
    AskTab { id: String, question: usize },
    /// Send on card `id`.
    AskSend(String),
    /// Return in card `id`'s Other field: the next open question, or Send once none is left.
    AskReturn(String),
    /// The user sent what he typed in the composer (`text`, already trimmed).
    ComposerSent(String),
    /// The composer's text changed to empty (`true`) or not (`false`): the placeholder shows while it is empty.
    ComposerEmpty(bool),
    /// A drop on the open pull-down while he writes (`drop::route`): its items ride with the composer's message.
    ComposerAttach(Vec<crate::island::drop::DropItem>),
    /// ⌃⌘H: the conversation pulled down to type in, as the user's own pull-down, with its composer even
    /// when there is no turn or conversation to show yet.
    OpenChat,
    Command(CommandCue),
    Health(HealthCue),
    /// Listening turned off: commands go, health stays.
    ListeningOff,
    Reader(FastReadUpdate),
    DrawOpen,
    DrawClose,
    /// How many strokes the overlay holds now.
    Ink(usize),
    /// Send's capture failed: the island says so.
    CaptureFailed(String),
    /// The agent pointed: this many marks (0 clears them).
    Point(usize),
    /// A click (mouse up on the same target as its mouse down, or a trackpad tap).
    Click(Target),
    /// A click on a card option or Send whose press and release both came from the HID system (`click_source`): the
    /// only click that can grant (`agent_notice::GrantItem`). Only the island's click target makes one.
    HardwareClick(Target),
    /// The pointer entered (`true`) or left (`false`) the island's shape: a pull-down the agent opened waits while it
    /// is over.
    Pointer(bool),
    /// The pointer is over this target (or none).
    Hover(Option<Target>),
    /// A button is held down (or released).
    Press(Option<Target>),
    /// Scroll inside the pulled-down turn, points (positive: content moves up).
    Scroll(f64),
    Display(Display),
    /// A floating island newly needs the user (night 1022): one small nudge at the notch, a bell (`question: false`) or a
    /// question mark. Bursts coalesce; while the island shows anything else, one waits (`scene::NUDGE_QUEUE`).
    Nudge { question: bool },
    /// Emergency mode (docs/claude-relay.md; bug note 27 Oct 19:40: "The emergency triangle next to the notch never
    /// showed"): while on, the agent's glyph and an orange warning triangle rest beside the notch; still, drawn once.
    Emergency(bool),
    /// A switch turned on or off ("a little animation … when we stop listening and stop seeing"). With
    /// `moment` (a shortcut or a menu tile) its glyph shows in its ear and turns, slashed or whole, then the island
    /// settles; Listening and Seeing off also hold a dim token in the ears until they are on again. Without `moment`
    /// only the held state changes (launch, the app's own report).
    Switch { switch: Switch, on: bool, moment: bool },
    /// A drag down on the pulled-down reply, points from where it began (`Some`), or its release (`None`): the shape
    /// stretches with a rubber band; released past `PULL_OUT` the reply is pulled out, otherwise it springs back.
    Pull(Option<f64>),
    /// A deadline or frame: nothing happened, time passed.
    Tick,
    /// The user started (`on`) or stopped dictating to Vella (docs/vella.md): its mark shows with a few words, then rests
    /// in the left ear beside Vella's shortcut (`keys`) until he stops; then the token goes with one motion.
    Vella { on: bool, keys: Option<String> },
}

/// What `Event::Switch` turns. Listening sits in the left ear and Seeing in the right, in the moment and the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Switch {
    Listening,
    Seeing,
    /// Every floating island shown or hidden (⌃⌘,): a moment only, no token.
    Islands,
}

impl Switch {
    /// The SF Symbol while on; off is the same symbol slashed, so the system's magic replace draws the slash.
    pub fn symbol(self) -> &'static str {
        match self {
            Switch::Listening => "mic",
            Switch::Seeing => "eye",
            Switch::Islands => "rectangle.on.rectangle",
        }
    }
    pub fn glyph_name(self, on: bool) -> String {
        if on { self.symbol().into() } else { format!("{}.slash", self.symbol()) }
    }
}

/// The shape's motion, sampled for anything that follows it (the drawing overlay's island glow, an observer).
/// Never empty, by construction: the 17:54 crash indexed `paths.len() - 1` on an empty track.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// The motion's start (island seconds).
    pub begin: f64,
    /// Seconds between samples.
    pub step: f64,
    pub first: Shape,
    pub rest: Vec<Shape>,
}

impl Track {
    pub fn len(&self) -> usize {
        1 + self.rest.len()
    }
    pub fn is_empty(&self) -> bool {
        false
    }
    pub fn get(&self, i: usize) -> Shape {
        if i == 0 {
            self.first
        } else {
            self.rest.get(i - 1).copied().unwrap_or_else(|| self.last())
        }
    }
    pub fn last(&self) -> Shape {
        self.rest.last().copied().unwrap_or(self.first)
    }
    /// The sample at time `t` (clamped to the track).
    pub fn at(&self, t: f64) -> Shape {
        if self.step <= 0.0 || !t.is_finite() {
            return self.last();
        }
        let i = ((t - self.begin) / self.step).round().max(0.0);
        self.get(if i.is_finite() { i as usize } else { 0 })
    }
}

/// How long a line reads (DESIGN.md §2, the old `notice_seconds`): 1.8–3.4 s by kind, a little longer for a long
/// line (50 ms a character past 12), at most 8 s. A long hold puts a box over the user's tabs, the "never clears"
/// complaint; the design review held them to the spec.
pub fn read_seconds(kind: crate::agent_notice::NoticeKind, text: &str) -> f64 {
    use crate::agent_notice::NoticeKind as K;
    let base = match kind {
        K::Working => 1.8,
        K::Looking | K::Success => 2.0,
        K::Info => 2.6,
        K::Warning => 3.0,
        K::Error => 3.4,
    };
    (base + (text.chars().count() as f64 - 12.0).max(0.0) * 0.05).min(8.0)
}
