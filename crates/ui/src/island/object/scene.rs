//! The island's one state machine. Every source of content (the voice, the agent's turn, lines, questions, notes,
//! the reader, drawing mode, pointing, commands and health) keeps its state here, and `scene(t)` decides the one
//! thing the island says at `t`. `deadline()` is the next time that can change without an event.
//!
//! Precedence (highest first): a body the user is using (drawing mode, the reader, a question, the last turn pulled down)
//! with any moment as a status row above it; the voice, with any moment inline; a moment (a command result or health
//! message, then an agent line, then the turn's new status); the turn's lip; a restart or problem held; a pending
//! command; tokens at rest (problem, reader, a part down). A note is never a token: it belongs to the turn that
//! showed it, in the pull-down under the notch, and retires when a later turn replaces it or the user closes it.
//! Long-held states settle below the menu bar only while there is something to read (DESIGN.md §2): words show for
//! their read time, then the island settles to the lip, the ears or rest.

use std::collections::HashMap;

use super::layout::{token, Ear, Glyph, Lip, NoteView, QLook, Scene, HELMET};
use super::reader::Reader;
use super::spring::LIGHT;
use super::{read_seconds, Event, Fx, Light, Output, Rgba, Target, Token};
use crate::agent_notice::{AgentNotice, AgentNoticeAction, Detail, NoticeKind, NoticeOutcome, Notices, Phase as QPhase};
use crate::island::{result_text, Activity, CommandKind, Cues, HealthTone, Look, SendState, Tone};
use crate::reader::{FastReadUpdate, ReadOutcome};

/// The send arrow's lip: long enough for its bounce, then the thinking dots (or rest).
pub const SENT_SECONDS: f64 = 0.8;
/// After the audio ends, the speaking look holds this long (flattened bars, the words as they were), so a gap
/// between two sentences never retracts it. Then it simply retracts: no success mark after speech (
/// "after speaking, the success animation is not needed").
pub const GRACE: f64 = 0.45;
/// A voice starting again this soon after the last one ended continues the same reply: its sentences stay and the
/// new ones are added (a synthesis gap mid-reply never starts the text over).
pub const REPLY_JOIN: f64 = 3.0;
/// A sentence arriving this soon before its audio's start belongs to that reply.
pub const EARLY_WORDS: f64 = 0.5;
/// A written reply (`LastReply`) arrives this long at most before its own words are spoken (synthesis of the first
/// sentence); older, it belongs to an earlier reply.
pub const WRITTEN_LEAD: f64 = 15.0;
/// A voice starting this long after the last written reply, without that reply's own voice in between, belongs to
/// a later turn (`reply_starts`).
pub const TURN_LEAD: f64 = 60.0;
/// The speaking row shows at most this many lines: the newest sentences (each starting its own line) that fit; the
/// whole reply stays in the pull-down.
pub const SPEAK_LINES: usize = 6;
/// A reply keeps at most this many sentences.
pub const MAX_SENTENCES: usize = 400;
/// How long the voice's look waits for its words before it shows the ears alone.
pub const WORDS_WAIT: f64 = 0.25;
/// A voice that says it plays but sends no level, words or end for this long has stopped (a lost end signal must
/// never leave the waveform running: the 17:47 and 17:54 complaints).
pub const VOICE_STALL: f64 = 3.0;
/// A voice reporting only silence for this long has stopped (`voice_stall_at`).
pub const VOICE_SILENT: f64 = 10.0;
/// A level above this is sound (a sign of the voice's life); the silence a stalled stream reports is not.
pub const AUDIBLE_LEVEL: f32 = 0.001;
pub const COMPACT_READ: f64 = 2.5;
/// A turn without news for this long is over (a lost turn end never holds the lip for good).
pub const ACTIVITY_CAP: f64 = 600.0;
/// The pull-down an agent's note opened pulls itself up after this long on screen without the pointer over it (bug
/// note 2026-10-20T18-41-17: an agent's note left open read as a stuck island). Nothing stays beside the notch: the
/// notch itself opens the last turn again. One the user pulled down himself stays until he pulls it up.
pub const NOTE_UNATTENDED: f64 = 12.0;
/// The most notes one turn keeps (the oldest goes first); the app keeps a drawing for each.
pub const MAX_TURN_NOTES: usize = 6;
/// Notes the user closed are remembered by id (an update of a closed note never brings it back), the newest this many.
const CLOSED_KEPT: usize = 64;
/// An answered question retracts at once: the click (or Heimdall's spoken answer) is its confirmation, no chosen-mark
/// stage before it leaves ("animations should be simple").
pub const ANSWERED: f64 = 0.0;
/// An expired question waits for Dismiss at most this long.
pub const EXPIRED_HOLD: f64 = 30.0;
/// A problem's stack and button read this long, then it rests as a token (a click opens it again).
pub const PROBLEM_READ: f64 = 6.0;
pub const RESTART_READ: f64 = 3.0;
/// Catching up reads this long from its arrival (long enough to see its count fall once), then rests as a token in
/// the ears until it ends; new words for it never open it again.
pub const BUSY_READ: f64 = 4.0;
/// A line stays at least this long before a different one replaces it (a flood shows the newest, ≤ 1 change a
/// second). A line to read (a reply shown as text) stays its whole read time (`line_hold`).
pub const MIN_LINE: f64 = 1.0;
/// Drawing mode: a moment shows in the ears this long, the keys staying put.
pub const LOOK_SECONDS: f64 = 1.0;
/// A failed capture's warning after Send reads this long.
pub const ATTACHED_SECONDS: f64 = 2.0;
/// Pointing marks without a voice stay this long, and fade over `POINT_FADE` (with the voice, they fade with it).
pub const POINT_ALONE: f64 = 4.0;
pub const POINT_FADE: f64 = 0.36;
pub const POINT_IN: f64 = 0.2;
/// The drawing light breathes between 0.3 and 1, 3.2 s each way.
pub const BREATH_PERIOD: f64 = 6.4;
pub const FLASH_SECONDS: f64 = 0.25;

// Night 1022, floating islands and the notch.
/// A nudge shows this long (the shape grows, jiggles, the glyph rings or bounces, then it retracts).
pub const NUDGE_SECONDS: f64 = 1.1;
/// The pill's jiggle runs this long from the moment the shape lands (three soft swells, each smaller).
pub const NUDGE_WOBBLE: f64 = 0.42;
/// Nudges asked for within this long after one ended are the same burst: dropped.
pub const NUDGE_COALESCE: f64 = 3.0;
/// A nudge that could not play (the pull-down open, the voice, a question, a line…) waits at most this long, then
/// is dropped: the floating island keeps its own attention pulse.
pub const NUDGE_QUEUE: f64 = 30.0;
/// A drag down on the pulled-down reply this long (points, before the rubber band) pulls it out on release.
pub const PULL_OUT: f64 = 72.0;
/// The rubber band's reach: the stretch approaches this many points however far the drag goes.
const RUBBER_REACH: f64 = 160.0;
/// The stretch follows the pointer with this time constant (a flick's 40-point step is eased over a few frames).
const PULL_FOLLOW: f64 = 0.05;
/// A released stretch springs back on this (critically damped, quicker than the retraction, so it never outlasts it).
const PULL_BACK: super::spring::Spring = super::spring::Spring { response: 0.26, damping: 1.0 };

/// How far the shape stretches for a drag of `x` points (UIScrollView's rubber band: 1:1 at first, then ever less).
pub fn rubber(x: f64) -> f64 {
    if !x.is_finite() || x <= 0.0 {
        return 0.0;
    }
    RUBBER_REACH * (1.0 - 1.0 / (x * 0.55 / RUBBER_REACH + 1.0))
}

/// A switch's moment (24 Oct): the glyph as it was until `SWITCH_TURN`, then as it is now, for `SWITCH_SECONDS`.
pub const SWITCH_SECONDS: f64 = 1.5;
/// When the glyph turns: after the shape has landed and the glyph has arrived (the grow takes 0.22 s, arrival
/// effects start 0.23 s after the motion), so the slash is drawn where the user is already looking.
pub const SWITCH_TURN: f64 = 0.45;
/// The token while Listening or Seeing is off: both ears dim alike (a pair fades as a pair, I2); the slash says
/// which one is off.
pub const SWITCH_TOKEN_ALPHA: f64 = 0.5;

/// Dictating to Vella (docs/vella.md): Vella's mark over these words for `VELLA_SECONDS` as it starts, then the mark
/// at rest in the left ear beside Vella's shortcut, drawn once (no effect, no timer), until it stops.
pub const VELLA_SECONDS: f64 = 1.6;
pub const VELLA_WORDS: &str = "Dictating to Vella";
/// A shortcut longer than this many characters does not fit an ear: the keyboard symbol stands for it.
const VELLA_KEYS_MAX: usize = 4;

/// The user dictating to Vella: since when, Vella's shortcut, and whether its moment still shows.
#[derive(Clone, Debug)]
struct VellaLook {
    since: f64,
    keys: Option<String>,
    moment: bool,
}

/// Listening and Seeing as the island holds them, and the newest switch moment (switch, on, from when).
#[derive(Clone, Copy, Debug, Default)]
struct Switches {
    listening_off: bool,
    seeing_off: bool,
    moment: Option<(super::Switch, bool, f64)>,
}

/// A nudge asked for (night 1022): waiting until the island has nothing else to show, then shown from `shown_at`.
#[derive(Clone, Copy, Debug)]
struct Nudge {
    question: bool,
    asked: f64,
    shown_at: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct Voice {
    on: bool,
    /// When this reply's audio started (the look waits a moment for its words).
    on_at: f64,
    /// A reply has started before (`on_at` is a real time).
    started: bool,
    /// The audio ended at this time; the look holds for `GRACE`.
    ended_at: Option<f64>,
    /// When the last audio ended (a voice starting again within `REPLY_JOIN` continues its reply).
    last_end: Option<f64>,
    /// The reply's sentences as they were spoken, oldest first: each new sentence is added, none is replaced
    /// ("we don't disappear them until the speech is done … we just add them").
    sentences: Vec<String>,
    /// When a sentence came while no voice played (its audio may start a moment later).
    early: Option<f64>,
    level: f32,
    /// The last sign of sound: its start, its words, an audible level.
    last_news: f64,
    /// The last report of any kind, silence included (a stream still alive).
    last_report: f64,
    /// The user clicked the words away (`Target::Hush`): this reply's look stays down until the next reply starts.
    hushed: bool,
}

/// The last turn pulled down under the notch ("we should be also able to pull the dynamic island down
/// and that should show the last response"; 20 Oct: "the lip which I can click and I can see your last response").
#[derive(Clone, Copy, Debug, Default)]
struct ReplyView {
    /// Points scrolled down.
    scroll: f64,
    /// The pointer is over its bottom band.
    hot: bool,
    /// The user pulled it down (it stays until he pulls it up); otherwise an agent's note opened it.
    owner: bool,
    /// Open on screen without the pointer over it since this time (one an agent's note opened pulls itself up
    /// `NOTE_UNATTENDED` later).
    unattended: Option<f64>,
    /// Opened from the keyboard (⌃⌘H): it shows with only its composer when there is nothing else yet.
    keys: bool,
    /// The user scrolled it: it stays where he left it. Until then it follows the latest exchange (`Layout::scroll_at`).
    scrolled: bool,
    /// Since it opened, the user sent or a new message arrived while it followed: it follows the conversation's end, all
    /// the way down (bug note 2026-10-26T19-14-21-3: "whenever I send a message and there is a new message, the
    /// island should automatically scroll all the way down").
    tail: bool,
    /// Its scroll is at the end of the body (as last clamped): a new message brings it along even after a scroll.
    at_end: bool,
    /// Its reply was copied to the clipboard (the copy glyph shows a check).
    copied: bool,
}

#[derive(Clone, Debug)]
enum PopKind {
    Working(String),
    Compacting,
}

#[derive(Clone, Debug)]
struct Pop {
    kind: PopKind,
    shown_at: Option<f64>,
    read: f64,
}

#[derive(Clone, Debug, Default)]
struct Turn {
    sent_at: Option<f64>,
    /// The lip waits until then (`Event::Hold`): the ack's moment; the send arrow's time counts from it.
    held_until: f64,
    activity: Option<Activity>,
    last_news: f64,
    pop: Option<Pop>,
}

#[derive(Clone, Debug)]
struct Line {
    id: String,
    kind: NoticeKind,
    text: String,
    shown_at: Option<f64>,
    /// Shown inside the voice's row (when the voice ends, a nearly read line ends with it).
    inline: bool,
}

/// A note the agent showed (`island_notice` with a detail): part of the turn that showed it.
#[derive(Clone, Debug)]
struct Note {
    id: String,
    /// Bumped on each update of the same id.
    rev: u64,
    icon: Glyph,
    title: String,
    body: (f64, f64),
}

#[derive(Clone, Debug)]
struct Draw {
    ink: usize,
    send: SendState,
    close: SendState,
    /// The ears show a moment (a screenshot's eye · check) until this time.
    swap: Option<(Glyph, Glyph, f64)>,
    /// The moment already shown in the ears (so it shows once).
    swapped: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LightState {
    open_at: Option<f64>,
    /// Closing: when, and the run and brightness it closed from.
    close: Option<(f64, f64, f64)>,
    flash_at: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
struct Pointing {
    since: f64,
    fade_from: Option<f64>,
}

/// The island's state.
#[derive(Clone, Debug)]
pub(super) struct Model {
    /// The composer at the foot of the conversation holds no text (its placeholder shows).
    composer_empty: bool,
    /// What was dropped on the pull-down while he wrote: it goes with the composer's next send.
    composer_attached: Vec<crate::island::drop::DropItem>,
    pub cues: Cues,
    /// Wall-clock milliseconds at island time 0 (questions expire on the wall clock).
    pub wall0: f64,
    voice: Voice,
    turn: Turn,
    line: Option<Line>,
    next_line: Option<Line>,
    questions: Notices,
    answered_at: HashMap<String, f64>,
    expired_at: HashMap<String, f64>,
    /// The last turn's notes, oldest first (bug note 2026-10-20T23-34-14: a table stayed pinned for 20 minutes and a
    /// click opened it instead of the last reply).
    notes: Vec<Note>,
    note_rev: u64,
    /// Notes the user closed with their X, newest last.
    closed: Vec<String>,
    /// The agent's turns as the island counts them: a turn ends with its reply as written (`LastReply`); the next
    /// reply, spoken reply or note starts the next one, and the last turn's notes retire then.
    turn_no: u64,
    sealed: bool,
    /// A new agent run started (idle to thinking) since the turn began: whatever it shows is a later turn's, even
    /// when the last run ended without a written reply (NO_REPLY, review of 063424a).
    run_over: bool,
    /// The turn the reply's words (as written or as spoken) belong to: the pull-down shows them only with that turn.
    reply_turn: u64,
    /// The pointer is over the island (`Event::Pointer`).
    pointer: bool,
    /// The last reply as written (`LastReply`), else the last spoken reply's sentences; and its pull-down when open.
    written: Option<String>,
    /// When `written` arrived.
    written_at: f64,
    reply: Option<Vec<String>>,
    reply_open: Option<ReplyView>,
    /// The pointer is over the notch's target (or the peek lip): `Lip::Peek` shows at rest.
    peek: bool,
    /// The open questions the user tucked away with their lip (bug note 2026-10-28T19-45-55): still pending, off the
    /// island until the notch brings them back (or one docks in his pulled-down conversation). A question that arrives
    /// meanwhile shows; an id leaves once its question is answered, closed or expired (`scene`).
    tucked: Vec<String>,
    /// The pointer is on the question card's lip band.
    ask_hot: bool,
    reader: Option<Reader>,
    draw: Option<Draw>,
    light: LightState,
    attached: Option<(Glyph, String, Option<f64>)>,
    pointing: Option<Pointing>,
    /// Held states already shown expanded (restart, problem), by label, and a token's reopening.
    held_seen: HashMap<String, f64>,
    expand: Option<(String, f64)>,
    /// The last time the scene was resolved (the bug-note clock's next second counts from it).
    last_t: f64,
    /// The expiry (wall-clock ms) of the countdown card on screen in the last scene, while time is left: the scene
    /// changes once a second until then. None when no countdown shows, so nothing wakes for it.
    countdown_until: Option<u64>,
    /// The command or health moment drawing mode already showed in its ears: once drawing closes it does not show a
    /// second time as a stack (one moment, one appearance).
    ears_said: Option<String>,
    pub outputs: Vec<Output>,
    /// Night 1022: a floating island's nudge, waiting or showing, and when the last one ended.
    nudge: Option<Nudge>,
    /// Emergency mode is on: its token rests beside the notch (`Event::Emergency`).
    emergency: bool,
    nudge_end: f64,
    /// Night 1022: a drag down on the pulled-down reply while held (raw points, the stretch when they arrived, and
    /// when), and the stretch springing back after it (stretch, release time, settled time).
    pull: Option<(f64, f64, f64)>,
    pull_back: Option<(f64, f64, f64)>,
    switches: Switches,
    /// The conversation around the last turn (`Event::Chat`), oldest first: shown when the user pulls the notch down.
    history: Vec<crate::island::chat::ChatEntry>,
    /// The user dictating to Vella (`Event::Vella`).
    vella: Option<VellaLook>,
}

/// A reply as the pull-down keeps it: line breaks and list markers stay; trailing spaces and runs of blank lines go.
fn as_written(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    lines.join("\n").trim().to_string()
}

impl Model {
    pub fn new() -> Model {
        Model {
            composer_empty: true,
            composer_attached: Vec::new(),
            cues: Cues::default(),
            wall0: 0.0,
            voice: Voice::default(),
            turn: Turn::default(),
            line: None,
            next_line: None,
            questions: Notices::default(),
            answered_at: HashMap::new(),
            expired_at: HashMap::new(),
            notes: vec![],
            note_rev: 0,
            closed: vec![],
            turn_no: 0,
            sealed: false,
            run_over: false,
            reply_turn: 0,
            pointer: false,
            written: None,
            written_at: 0.0,
            reply: None,
            reply_open: None,
            peek: false,
            tucked: vec![],
            ask_hot: false,
            reader: None,
            draw: None,
            light: LightState::default(),
            attached: None,
            pointing: None,
            held_seen: HashMap::new(),
            expand: None,
            last_t: 0.0,
            countdown_until: None,
            ears_said: None,
            outputs: vec![],
            nudge: None,
            emergency: false,
            nudge_end: f64::NEG_INFINITY,
            pull: None,
            pull_back: None,
            switches: Switches::default(),
            history: vec![],
            vella: None,
        }
    }

    fn wall_ms(&self, t: f64) -> u64 {
        let ms = self.wall0 + t * 1000.0;
        if ms.is_finite() && ms > 0.0 {
            ms as u64
        } else {
            0
        }
    }

    fn t_of_wall(&self, ms: u64) -> f64 {
        (ms as f64 - self.wall0) / 1000.0
    }

    /// The next time a countdown to `until` shows another second: where its whole seconds left (rounded up) drop by
    /// one, the last at `until` itself (0:00); none after. Half a millisecond late, so the wall clock read there has
    /// crossed the second (it truncates) and the same deadline never comes back.
    fn countdown_tick(&self, until: u64) -> Option<f64> {
        let left = until.checked_sub(self.wall_ms(self.last_t)).filter(|l| *l > 0)?;
        let secs = left.div_ceil(1000);
        Some(self.t_of_wall(until - (secs - 1) * 1000) + 0.0005)
    }

    fn reports(&mut self) {
        for r in self.cues.take_reports() {
            self.outputs.push(Output::Report(r));
        }
    }

    // ------------------------------------------------------------------ events

    pub fn apply(&mut self, ev: Event, t: f64) {
        self.expire(t);
        match ev {
            Event::Speaking(true) => {
                let joined = self.voice.on || self.voice.ended_at.is_some() || self.voice.last_end.is_some_and(|e| t - e < REPLY_JOIN);
                if !joined {
                    // A new reply: the last one's words leave the row (the pull-down keeps them), unless its first
                    // sentence came a moment before its audio.
                    if self.voice.early.is_none_or(|e| t - e > EARLY_WORDS) {
                        self.voice.sentences.clear();
                    }
                    self.reply_starts(t);
                    self.voice.on_at = t;
                    self.voice.started = true;
                    self.voice.hushed = false;
                }
                self.voice.early = None;
                self.voice.on = true;
                self.voice.ended_at = None;
                self.voice.last_news = t;
                self.voice.last_report = t;
            }
            Event::Speaking(false) => self.voice_ended(t),
            Event::SpokenText(text) => {
                if let Some(text) = text.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty()) {
                    let speaking = self.voice.on || self.voice.ended_at.is_some();
                    if !speaking && self.voice.last_end.is_none_or(|e| t - e >= REPLY_JOIN) {
                        // Words before their audio: a new reply starts with them.
                        if self.voice.early.is_none_or(|e| t - e > EARLY_WORDS) {
                            self.voice.sentences.clear();
                            self.voice.hushed = false;
                            self.reply_starts(t);
                        }
                        self.voice.early = Some(t);
                    }
                    // Added, never replacing the one before (the same sentence twice in a row is one).
                    if self.voice.sentences.last() != Some(&text) {
                        if self.voice.sentences.len() == MAX_SENTENCES {
                            self.voice.sentences.remove(0);
                        }
                        self.voice.sentences.push(text);
                    }
                    self.reply = Some(self.voice.sentences.clone());
                    if self.voice.on {
                        self.voice.last_news = t;
                        self.voice.last_report = t;
                    }
                }
            }
            Event::WithdrawReply(text) => {
                // Only the reply it names: a later one stays. Its turn's words go with it (the voice never said any of
                // it, so what `reply` holds is an earlier reply's), and the peek lip has nothing to open.
                if self.written.as_deref() == Some(as_written(&text).as_str()) {
                    self.written = None;
                    self.reply = None;
                    self.close_if_empty();
                }
            }
            Event::Chat(history) => {
                if history != self.history {
                    self.follow_new_message();
                }
                self.history = history;
                self.close_if_empty();
            }
            Event::LastReply(text) => {
                let text = as_written(&text);
                if !text.is_empty() {
                    // The reply as written ends its turn: a turn already ended (or a new run) means this one is new.
                    if self.closed() {
                        self.next_turn();
                    }
                    if self.written.as_deref() != Some(text.as_str()) {
                        self.follow_new_message();
                    }
                    self.written = Some(text);
                    self.written_at = t;
                    self.reply_turn = self.turn_no;
                    self.sealed = true;
                }
            }
            Event::SpeechLevel(x) => {
                let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
                if self.voice.on {
                    self.voice.level = x;
                    self.voice.last_report = t;
                    // Only sound is a sign of life: a stream that reports silence forever (a device that stopped
                    // calling back) must not hold the words on the notch (bug note 2026-10-31T23-37-26).
                    if x > AUDIBLE_LEVEL {
                        self.voice.last_news = t;
                    }
                }
            }
            Event::Sent => {
                // The user's words reached the agent. The conversation he pulled down stays open through sending,
                // thinking and the voice's turns, following its newest message, until he closes it (bug note
                // 2026-10-26T19-14-48, reversing 2026-10-21T18-50-16); a pull-down an agent's note opened pulls itself
                // up, and the turn's send arrow, then its thinking dots, show. The notch still opens the last turn.
                match &mut self.reply_open {
                    Some(v) if v.owner => {
                        v.scrolled = false;
                        v.tail = true;
                    }
                    _ => self.reply_open = None,
                }
                // A drag under way lets go (springs back, nothing pulled out).
                self.release_pull(t);
                self.turn.sent_at = Some(t);
                self.turn.last_news = t;
            }
            Event::Hold(secs) => {
                self.turn.held_until = if secs.is_nan() { t } else { t + secs.max(0.0) };
                self.turn.last_news = t;
            }
            Event::Activity(a) => {
                match a {
                    Some(a) => {
                        // Idle to thinking: a new run. Its content starts a new turn (Ready itself closes nothing: it
                        // can land before or after the run's reply, which comes on another channel).
                        if self.turn.activity.is_none() && a == Activity::Thinking {
                            self.run_over = true;
                        }
                        if self.turn.activity != Some(a) && a == Activity::Compacting {
                            self.turn.pop = Some(Pop { kind: PopKind::Compacting, shown_at: None, read: COMPACT_READ });
                        }
                        self.turn.activity = Some(a);
                    }
                    None => {
                        // The turn ends by retracting: no success mark after it ("we don't need success
                        // after everything that happened").
                        self.turn.activity = None;
                        if matches!(self.turn.pop, Some(Pop { kind: PopKind::Compacting, .. })) {
                            self.turn.pop = None;
                        }
                    }
                }
                self.turn.last_news = t;
            }
            Event::SpeechCut => self.voice_cut(t),
            Event::Stop => {
                self.voice_cut(t);
                self.turn = Turn { last_news: t, ..Turn::default() };
                if let Some(p) = &mut self.pointing {
                    p.fade_from.get_or_insert(t);
                }
            }
            Event::Notice { notice, body } => self.notice(notice, body, t),
            Event::NoticeClear(id) => {
                if self.line.as_ref().is_some_and(|l| l.id == id) {
                    self.line = self.next_line.take();
                }
                if self.next_line.as_ref().is_some_and(|l| l.id == id) {
                    self.next_line = None;
                }
                self.questions.dismiss(&id);
                self.remove_note(&id);
            }
            Event::ClearNotices => {
                self.line = None;
                self.next_line = None;
                self.questions.clear();
                self.notes.clear();
                self.close_if_empty();
            }
            Event::CloseQuestion { id, answer } => {
                if self.questions.close_spoken(&id, &answer) {
                    if let Some(item) = self.questions.items.iter().find(|i| i.notice.id == id || i.request_id() == Some(id.as_str())) {
                        self.answered_at.insert(item.notice.id.clone(), t);
                    }
                }
            }
            Event::AskSet { id, answer } => {
                let now_ms = self.wall_ms(t);
                self.questions.set(&id, &answer, now_ms);
            }
            Event::AskPick { id, question, option } => self.ask_pick(&id, question, option, t),
            Event::AskDigit(digit) => {
                // Option plus a digit: the card on screen, the option on its current tab.
                if let Some(item) = self.card_item().filter(|_| self.card_keys() > 0) {
                    let (id, tab) = (item.notice.id.clone(), item.tab);
                    if digit >= 1 {
                        self.ask_pick(&id, tab, digit - 1, t);
                    }
                }
            }
            Event::AskOther { id, question, text } => {
                let now_ms = self.wall_ms(t);
                if let Some(action) = self.questions.other(&id, question, &text, now_ms) {
                    self.outputs.push(Output::Notice(action));
                }
            }
            Event::AskTab { id, question } => {
                self.questions.tab(&id, question);
            }
            Event::AskSend(id) => self.ask_send(&id, t),
            Event::AskReturn(id) => {
                let now_ms = self.wall_ms(t);
                if let (_, Some(action)) = self.questions.returned(&id, now_ms) {
                    self.answered_at.insert(id.clone(), t);
                    self.outputs.push(Output::Notice(action));
                }
            }
            Event::AskDismiss(id) => self.ask_dismiss(&id, t),
            Event::ComposerSent(text) => {
                let text = text.trim();
                // With something dropped into it, Return sends even with no words: the drop goes as his message.
                if !text.is_empty() || !self.composer_attached.is_empty() {
                    let items = std::mem::take(&mut self.composer_attached);
                    self.outputs.push(if items.is_empty() { Output::Typed(text.to_string()) } else { Output::TypedDrop { text: text.to_string(), items } });
                    if let Some(v) = self.reply_open.as_mut().filter(|v| v.owner) {
                        v.scrolled = false;
                        v.tail = true;
                    }
                }
                self.composer_empty = true;
            }
            Event::ComposerEmpty(empty) => self.composer_empty = empty,
            Event::ComposerAttach(items) => {
                // The same file dropped twice rides once.
                for item in items {
                    if !self.composer_attached.contains(&item) {
                        self.composer_attached.push(item);
                    }
                }
            }
            Event::OpenChat => {
                self.peek = false;
                if !self.reply_open.is_some_and(|v| v.owner) {
                    self.reply_open = Some(ReplyView { owner: true, keys: true, ..ReplyView::default() });
                }
            }
            Event::Command(cue) => {
                self.cues.cue(cue, t);
            }
            Event::Health(cue) => {
                if let crate::island::HealthCue::Notice { label, .. } = &cue {
                    if self.questions.health(label) {
                        // Connect T3 and unavailable answers update the question in place.
                    }
                }
                self.cues.health(cue, t);
            }
            Event::ListeningOff => self.cues.clear(t),
            Event::Reader(update) => self.reader_update(update, t),
            Event::DrawOpen => {
                if self.draw.is_none() {
                    self.draw = Some(Draw { ink: 0, send: SendState::Rest, close: SendState::Rest, swap: None, swapped: None });
                    // The light starts from where it is (a close in flight turns round).
                    let run = self.light_now(t).run;
                    self.light = LightState { open_at: Some(t - inverse_light(run)), close: None, flash_at: None };
                    self.attached = None;
                }
            }
            Event::DrawClose => {
                if self.draw.take().is_some() {
                    self.close_light(t);
                }
            }
            Event::Ink(n) => {
                if let Some(d) = &mut self.draw {
                    d.ink = n;
                }
            }
            Event::CaptureFailed(reason) => {
                let words = if reason.trim().is_empty() { "Not attached".to_string() } else { reason.trim().to_string() };
                self.attached = Some((Glyph::new("exclamationmark.triangle.fill").colors(&[Rgba::BLACK, Rgba::ORANGE]).fx(Fx::Bounce), words, None));
            }
            Event::Point(n) => {
                if n == 0 {
                    if let Some(p) = &mut self.pointing {
                        p.fade_from.get_or_insert(t);
                    }
                } else {
                    self.pointing = Some(Pointing { since: t, fade_from: None });
                }
            }
            Event::Click(target) => self.click(target, t),
            // The user's own pointer on a card option or Send (`click_source`): the same click, marked so a grant counts.
            Event::HardwareClick(Target::AskPick { id, question, option }) => self.ask_pick_by(&id, question, option, t, true),
            Event::HardwareClick(Target::AskSend(id)) => self.ask_send_by(&id, t, true),
            Event::HardwareClick(target) => self.click(target, t),
            Event::Pointer(over) => {
                self.pointer = over;
                // Leaving drops the hover with it: the host says again what is under the pointer (`Hover`), and an exit
                // that never came cannot leave the peek lip drawn at rest (night 1022 review 5).
                if !over {
                    self.peek = false;
                    self.ask_hot = false;
                }
                if let Some(v) = &mut self.reply_open {
                    // Leaving starts the unattended time over (`scene` marks it from now).
                    v.unattended = None;
                }
            }
            Event::Hover(target) => self.hover(target, false),
            Event::Press(target) => self.hover(target, true),
            Event::Scroll(dy) => {
                let shows = self.reply_shows();
                if let Some(v) = self.reply_open.as_mut().filter(|_| shows && dy.is_finite()) {
                    // Clamped to the body's length by the engine once laid out (`clamp_reply_scroll`); from where it
                    // shows now, which follows the latest exchange until the user scrolls.
                    v.scroll = (v.scroll + dy).max(0.0);
                    v.scrolled = true;
                    // Scrolling it is reading it.
                    v.unattended = None;
                }
            }
            // Night 1022: a burst is one nudge; a waiting one becomes a question if any of the burst is one.
            Event::Nudge { question } => match &mut self.nudge {
                Some(n) if n.shown_at.is_none() => n.question |= question,
                Some(_) => {}
                None if t - self.nudge_end < NUDGE_COALESCE => {}
                None => self.nudge = Some(Nudge { question, asked: t, shown_at: None }),
            },
            Event::Emergency(on) => self.emergency = on,
            Event::Switch { switch, on, moment } => {
                match switch {
                    super::Switch::Listening => self.switches.listening_off = !on,
                    super::Switch::Seeing => self.switches.seeing_off = !on,
                    super::Switch::Islands => {}
                }
                if moment {
                    self.switches.moment = Some((switch, on, t));
                }
            }
            Event::Pull(Some(d)) => {
                if self.reply_shows() && self.can_pull_out() && d.is_finite() {
                    self.pull = Some((d.max(0.0), self.stretch(t), t));
                    self.pull_back = None;
                    if let Some(v) = &mut self.reply_open {
                        v.unattended = None;
                    }
                }
            }
            Event::Pull(None) => {
                if let Some((p, _, _)) = self.pull {
                    self.release_pull(t);
                    if p >= PULL_OUT {
                        self.pull_out();
                    }
                }
            }
            Event::Display(_) | Event::Tick => {}
            // Dictating to Vella: a start shows its moment once; a keybind change while on only changes the token.
            Event::Vella { on: true, keys } => match &mut self.vella {
                Some(v) => v.keys = keys,
                None => self.vella = Some(VellaLook { since: t, keys, moment: true }),
            },
            Event::Vella { on: false, .. } => self.vella = None,
        }
        self.expire(t);
        self.reports();
    }

    /// The audio is cut: the bars flatten and the speaking look ends at once (no grace, `GRACE` is the natural end's).
    /// What was said stays in the pull-down; the next voice is a new reply.
    fn voice_cut(&mut self, t: f64) {
        if self.voice.on || self.voice.ended_at.is_some() {
            // The look's grace is `t - GRACE + GRACE = t`: it is over now.
            self.voice.on = false;
            self.voice.ended_at = Some(t - GRACE);
            self.voice.level = 0.0;
            self.voice.sentences.clear();
            self.voice.last_end = None;
            self.voice.early = None;
        }
    }

    /// A new spoken reply starts at `t`. The pull-down keeps the reply as written only while it is this one's: no new
    /// run has started since it was written and it is at most `WRITTEN_LEAD` old. Otherwise it is the last turn's
    /// text, and the pull-down shows this reply's spoken sentences instead (review 19 Oct).
    ///
    /// A voice that is not the written reply's own words belongs to a later turn (its notes retire) when a new run
    /// started, or the written reply is more than `TURN_LEAD` old. A pause inside the turn's own voice (a voice retry
    /// takes about 6 s) and a slow first sentence stay in its turn.
    fn reply_starts(&mut self, t: f64) {
        if self.written.is_some() && (self.run_over || t - self.written_at > WRITTEN_LEAD) {
            self.written = None;
        }
        if self.written.is_none() {
            let later_turn = self.run_over || (self.sealed && t - self.written_at > TURN_LEAD);
            if self.closed() && later_turn {
                self.next_turn();
            }
            self.reply_turn = self.turn_no;
        }
    }

    /// The turn has ended: its reply as written came, or a new run started.
    fn closed(&self) -> bool {
        self.sealed || self.run_over
    }

    /// A later turn starts: the last one's notes retire ("we are past this conversation"), and a
    /// pull-down an agent's note opened goes with them.
    fn next_turn(&mut self) {
        self.turn_no += 1;
        self.sealed = false;
        self.run_over = false;
        self.notes.clear();
        if self.reply_open.is_some_and(|v| !v.owner) {
            self.reply_open = None;
        }
    }

    fn remove_note(&mut self, id: &str) -> bool {
        let before = self.notes.len();
        self.notes.retain(|n| n.id != id);
        let removed = self.notes.len() != before;
        self.close_if_empty();
        removed
    }

    /// The pull-down with nothing left to show closes (never an empty box waiting for content).
    fn close_if_empty(&mut self) {
        if !self.reply_shows() {
            self.reply_open = None;
        }
    }

    /// When an unended voice counts as stopped: `VOICE_STALL` after its last report of any kind (a lost end signal),
    /// or `VOICE_SILENT` after its last sound while it reports only silence (a stalled stream, bug note
    /// 2026-10-31T23-37-26; long enough for a slow sentence mid-reply, review 1 Nov).
    fn voice_stall_at(&self) -> f64 {
        (self.voice.last_report + VOICE_STALL).min(self.voice.last_news + VOICE_SILENT)
    }

    fn voice_ended(&mut self, t: f64) {
        if self.voice.on {
            self.voice.on = false;
            self.voice.ended_at = Some(t);
            self.voice.last_end = Some(t);
            self.voice.level = 0.0;
            if let Some(p) = &mut self.pointing {
                p.fade_from.get_or_insert(t);
            }
        }
    }

    fn notice(&mut self, notice: AgentNotice, body: Option<(f64, f64)>, t: f64) {
        let id = notice.id.clone();
        // Questions and Connect T3: the question body.
        if notice.action.is_some() {
            let now_ms = self.wall_ms(t);
            self.questions.upsert(notice, now_ms);
            return;
        }
        let kind = match &notice.outcome {
            Some(NoticeOutcome::T3Connected | NoticeOutcome::Answered { .. }) => NoticeKind::Success,
            Some(_) => NoticeKind::Warning,
            None => notice.kind,
        };
        // A title or working words read as one line; a line to read keeps its line breaks (below).
        let text = notice.text.split_whitespace().collect::<Vec<_>>().join(" ");
        // A line with a pull-down joins its turn's notes and opens the pull-down under the notch.
        if let Some(detail) = notice.detail {
            // Closed by the user: for good, updates included.
            if self.closed.iter().any(|c| *c == id) {
                return;
            }
            let icon = match (&detail, kind) {
                (Detail::Table { .. }, _) => Glyph::new("tablecells"),
                (Detail::Text(_), NoticeKind::Error) => Glyph::new("xmark.circle.fill").colors(&[Rgba::WHITE, Rgba::RED]),
                (Detail::Text(_), _) => Glyph::new("doc.text"),
                (Detail::Svg(_), _) => Glyph::new("chart.xyaxis.line"),
                (Detail::Image { .. }, _) => Glyph::new("photo"),
            };
            let body = body.filter(|(w, h)| w.is_finite() && h.is_finite()).unwrap_or((344.0, 80.0));
            let title = if text.is_empty() { "Note".to_string() } else { text };
            // A note after the turn's reply, or in a later run, is a later turn's.
            if self.closed() {
                self.next_turn();
            }
            self.note_rev += 1;
            let rev = self.note_rev;
            match self.notes.iter_mut().find(|n| n.id == id) {
                Some(n) => *n = Note { id, rev, icon, title, body },
                None => {
                    if self.notes.len() == MAX_TURN_NOTES {
                        self.notes.remove(0);
                    }
                    self.notes.push(Note { id, rev, icon, title, body });
                }
            }
            // Opened by the note it is the agent's (it pulls itself up); one the user has open stays his.
            match &mut self.reply_open {
                Some(v) => v.unattended = None,
                None => self.reply_open = Some(ReplyView::default()),
            }
            return;
        }
        if kind == NoticeKind::Working {
            if !text.is_empty() {
                let read = read_seconds(NoticeKind::Working, &text);
                self.turn.pop = Some(Pop { kind: PopKind::Working(text), shown_at: None, read });
                self.turn.last_news = t;
            }
            return;
        }
        if text.is_empty() {
            return;
        }
        // Paragraphs and list items stay (bug note 2026-10-21T19-21-01: "it's just one long paragraph").
        let text = keep_breaks(&notice.text);
        // The same line again updates in place (its read time starts over); a different one replaces it once the
        // one on screen has been readable for a moment.
        if let Some(cur) = self.line.as_mut().filter(|l| l.id == id) {
            cur.kind = kind;
            cur.text = text;
            if cur.shown_at.is_some() {
                cur.shown_at = Some(t);
            }
            return;
        }
        let line = Line { id, kind, text, shown_at: None, inline: false };
        // A status line never pushes out a reply waiting its turn (it would never show); a warning or error does.
        if matches!(line.kind, NoticeKind::Info) && !reads_as_lines(line.kind, &line.text)
            && self.next_line.as_ref().is_some_and(|n| reads_as_lines(n.kind, &n.text)) {
            return;
        }
        match &self.line {
            // A line to read waits its turn too before it shows (bug note 22 Oct 15:13: a second reply wiped the first).
            Some(cur) if cur.shown_at.map_or(reads_as_lines(cur.kind, &cur.text) && reads_as_lines(line.kind, &line.text), |s| t - s < line_hold(cur, &line)) => self.next_line = Some(line),
            _ => {
                self.line = Some(line);
                self.next_line = None;
            }
        }
    }

    fn reader_update(&mut self, update: FastReadUpdate, t: f64) {
        match update {
            FastReadUpdate::Show(message) => {
                if let Some(old) = self.reader.take() {
                    self.outputs.push(Output::Read(old.result_as(ReadOutcome::Dismissed)));
                }
                self.reader = Some(Reader::new(message));
            }
            FastReadUpdate::StartWpm { id, wpm } => {
                if let Some(r) = self.reader.as_mut().filter(|r| r.message.id == id) {
                    r.start_wpm(wpm);
                }
            }
            FastReadUpdate::Withdraw(id) => {
                if self.reader.as_ref().is_some_and(|r| r.message.id == id) {
                    self.reader = None;
                }
            }
        }
        let _ = t;
    }

    fn hover(&mut self, target: Option<Target>, press: bool) {
        if let Some(d) = &mut self.draw {
            let state = |hit: bool| match (hit, press) {
                (false, _) => SendState::Rest,
                (true, false) => SendState::Hover,
                (true, true) => SendState::Pressed,
            };
            d.send = state(target == Some(Target::DrawSend));
            d.close = state(target == Some(Target::DrawClose));
        }
        if let Some(v) = &mut self.reply_open {
            v.hot = target == Some(Target::ReplyBand);
        }
        self.ask_hot = matches!(target, Some(Target::AskTuck(_)));
        // The pointer over the notch (or the peek lip under it): the lip shows while it stays.
        self.peek = target == Some(Target::Token(Token::Reply));
    }

    /// The user picked option `option` of question `question` on card `id`. A single-question card answers at once; the
    /// answer leaves the island after its flash, as before. Otherwise the pick is kept and the app hears it.
    fn ask_pick(&mut self, id: &str, question: usize, option: usize, t: f64) {
        self.ask_pick_by(id, question, option, t, false)
    }
    /// `ask_pick`, saying whether the user's own pointer made it (`Event::HardwareClick`).
    fn ask_pick_by(&mut self, id: &str, question: usize, option: usize, t: f64, by_hand: bool) {
        let now_ms = self.wall_ms(t);
        match self.questions.pick_by(id, question, option, now_ms, by_hand) {
            Some(action @ AgentNoticeAction::Answer { .. }) => {
                self.answered_at.insert(id.to_string(), t);
                self.outputs.push(Output::Notice(action));
            }
            Some(action) => self.outputs.push(Output::Notice(action)),
            None => {}
        }
    }

    /// Send on card `id`: answers every question, once.
    fn ask_send(&mut self, id: &str, t: f64) {
        self.ask_send_by(id, t, false)
    }
    fn ask_send_by(&mut self, id: &str, t: f64, by_hand: bool) {
        let now_ms = self.wall_ms(t);
        if let Some(action) = self.questions.send_by(id, now_ms, by_hand) {
            self.answered_at.insert(id.to_string(), t);
            self.outputs.push(Output::Notice(action));
        }
    }

    /// The user dismissed open question `id` (Esc in its field): it leaves the island and the agent is told "dismissed".
    fn ask_dismiss(&mut self, id: &str, t: f64) {
        if let Some(action) = self.questions.dismiss_open(id) {
            self.answered_at.insert(id.to_string(), t);
            self.outputs.push(Output::Notice(action));
        }
    }

    fn click(&mut self, target: Target, t: f64) {
        match target {
            Target::AskPick { id, question, option } => self.ask_pick(&id, question, option, t),
            Target::AskTab { id, question } => {
                self.questions.tab(&id, question);
            }
            // The window's field takes the keyboard (Other, the composer); nothing to decide here.
            Target::AskOther { .. } | Target::Composer => {}
            Target::ComposerDetach(k) => {
                if k < self.composer_attached.len() {
                    self.composer_attached.remove(k);
                }
            }
            Target::AskSend(id) => self.ask_send(&id, t),
            // The card's lip: tucked away, unanswered; nothing goes to the agent.
            Target::AskTuck(id) => {
                if !self.tucked.contains(&id) {
                    self.tucked.push(id);
                }
                self.ask_hot = false;
            }
            Target::Dismiss(id) => {
                if !self.questions.close(&id) {
                    self.questions.dismiss(&id);
                }
                self.expired_at.remove(&id);
            }
            Target::Connect(id) => {
                // The app's contract (as the old card): one ConnectT3 per click; the notice waits as Connecting.
                if let Some(action) = self.questions.connect(&id) {
                    self.outputs.push(Output::Notice(action));
                }
            }
            Target::HealthButton => {
                let on_screen_unacked = matches!(self.cues.held(), Look::Status { tone: HealthTone::Problem, .. })
                    || matches!(self.cues.on_screen(), Some(Look::Status { tone: HealthTone::Problem, .. }));
                if on_screen_unacked {
                    if let Some((key, control)) = self.cues.acknowledge(t) {
                        self.outputs.push(Output::Health { key, control });
                    }
                } else if let Some((key, control, ..)) = self.cues.problem() {
                    self.outputs.push(Output::Health { key, control });
                }
                self.expand = None;
            }
            Target::Token(Token::Problem) => {
                if let Some((_, _, _, label, _)) = self.cues.problem() {
                    self.expand = Some((label, t + PROBLEM_READ));
                }
            }
            // The last turn pulled down from the notch: the user's, it stays; its bottom band pulls it up again.
            // A tucked question comes back first: the decision the user put aside is what the notch holds.
            Target::Token(Token::Reply) => {
                self.peek = false;
                if self.tucked_pending() {
                    self.tucked.clear();
                } else if self.reply_available() {
                    self.reply_open = Some(ReplyView { owner: true, ..ReplyView::default() });
                }
            }
            Target::ReplyBand => self.reply_open = None,
            Target::Hush => self.voice.hushed = true,
            Target::ReplyCopy => {
                let text = self.turn_paragraphs().join("\n").trim().to_string();
                if let (false, Some(v)) = (text.is_empty(), self.reply_open.as_mut()) {
                    v.copied = true;
                    self.outputs.push(Output::Copy(text));
                }
            }
            Target::ReplyPin => self.pull_out(),
            // One note alone (its pin, or an earlier notice's row in the conversation) leaves as a floating island.
            Target::NotePin(id) | Target::ChatNote(id) => {
                if self.reply_shows() {
                    self.outputs.push(Output::PullOut { text: String::new(), notes: vec![id] });
                    self.reply_open = None;
                }
            }
            // A note's X: gone for good.
            Target::NoteClose(id) => {
                if self.remove_note(&id) {
                    self.closed.retain(|c| *c != id);
                    if self.closed.len() == CLOSED_KEPT {
                        self.closed.remove(0);
                    }
                    self.closed.push(id);
                }
                if let Some(v) = &mut self.reply_open {
                    v.unattended = None;
                }
            }
            Target::Token(Token::Reader) => {
                if let Some(r) = &mut self.reader {
                    r.reopen();
                }
            }
            Target::ReaderBand => {
                if let Some(r) = &mut self.reader {
                    r.pull_up(t);
                }
            }
            Target::ReaderPlay | Target::ReaderFace => {
                if let Some(r) = &mut self.reader {
                    r.toggle(t);
                }
            }
            Target::ReaderBack => {
                if let Some(r) = &mut self.reader {
                    r.back(t);
                }
            }
            Target::ReaderRepeat => {
                if let Some(r) = &mut self.reader {
                    r.repeat(t);
                }
            }
            Target::ReaderSlower | Target::ReaderFaster => {
                if let Some(r) = &mut self.reader {
                    r.nudge(target == Target::ReaderFaster, t);
                }
            }
            Target::DrawUndo => {
                if self.draw.as_ref().is_some_and(|d| d.ink > 0) {
                    self.outputs.push(Output::DrawUndo);
                }
            }
            Target::DrawClear => {
                if self.draw.as_ref().is_some_and(|d| d.ink > 0) {
                    self.outputs.push(Output::DrawClear);
                }
            }
            Target::DrawClose => {
                if self.draw.take().is_some() {
                    self.close_light(t);
                    self.outputs.push(Output::DrawClose);
                }
            }
            Target::DrawSend => {
                if self.draw.take().is_some() {
                    // The screen's flash is the capture's confirmation; the keys simply retract with the light (no
                    // "Attached" stage after it).
                    self.close_light(t);
                    self.light.flash_at = Some(t);
                    self.outputs.push(Output::DrawSend);
                }
            }
        }
    }

    fn close_light(&mut self, t: f64) {
        let now = self.light_now(t);
        self.light = LightState { open_at: None, close: Some((t, now.run, now.opacity)), flash_at: self.light.flash_at };
    }

    // ------------------------------------------------------------------ time

    /// Ends everything whose time is up at `t` (deadlines pass through here, early or late).
    fn expire(&mut self, t: f64) {
        self.cues.tick(t);
        // Night 1022: a nudge shown its time ends; a drag on a pull-down that went (a send, a question) lets go.
        if self.nudge.and_then(|n| n.shown_at).is_some_and(|s| t >= s + NUDGE_SECONDS) {
            self.nudge = None;
            self.nudge_end = t;
        }
        if self.switches.moment.is_some_and(|(_, _, at)| t >= at + SWITCH_SECONDS) {
            self.switches.moment = None;
        }
        if let Some(v) = self.vella.as_mut().filter(|v| v.moment && t >= v.since + VELLA_SECONDS) {
            v.moment = false;
        }
        if self.pull.is_some() && !self.reply_shows() {
            self.release_pull(t);
        }
        if self.pull_back.is_some_and(|(_, _, end)| t >= end) {
            self.pull_back = None;
        }
        // Voice: a lost end signal, then the grace.
        if self.voice.on && t >= self.voice_stall_at() {
            let at = self.voice_stall_at();
            self.voice_ended(at);
        }
        if self.voice.ended_at.is_some_and(|e| t >= e + GRACE) {
            self.voice.ended_at = None;
        }
        // Turn.
        if self.turn.sent_at.is_some_and(|s| t >= s.max(self.turn.held_until) + SENT_SECONDS) {
            self.turn.sent_at = None;
        }
        if self.turn.activity.is_some() && t >= self.turn.last_news + ACTIVITY_CAP {
            self.turn.activity = None;
        }
        if self.turn.pop.as_ref().is_some_and(|p| p.shown_at.is_some_and(|s| t >= s + p.read)) {
            self.turn.pop = None;
        }
        // Lines.
        if let Some(cur) = &self.line {
            // Every line, errors included, reads and retracts (the approved frames; only a health problem rests
            // as a token).
            if cur.shown_at.is_some_and(|s| t >= s + read_seconds(cur.kind, &cur.text)) {
                self.line = self.next_line.take();
            }
        }
        if let (Some(cur), Some(next)) = (&self.line, &self.next_line) {
            if cur.shown_at.is_some_and(|s| t >= s + line_hold(cur, next)) {
                self.line = self.next_line.take();
            }
        }
        // Questions.
        let now_ms = self.wall_ms(t);
        self.questions.expire(now_ms);
        for item in &self.questions.items {
            if matches!(item.phase, QPhase::Expired | QPhase::Unavailable) {
                self.expired_at.entry(item.notice.id.clone()).or_insert(t);
            }
        }
        let done: Vec<String> = self.answered_at.iter().filter(|(_, at)| t >= **at + ANSWERED).map(|(id, _)| id.clone()).collect();
        for id in done {
            self.answered_at.remove(&id);
            self.questions.dismiss(&id);
        }
        let stale: Vec<String> = self.expired_at.iter().filter(|(_, at)| t >= **at + EXPIRED_HOLD).map(|(id, _)| id.clone()).collect();
        for id in stale {
            self.expired_at.remove(&id);
            if !self.questions.close(&id) {
                self.questions.dismiss(&id);
            }
        }
        self.expired_at.retain(|id, _| self.questions.items.iter().any(|i| &i.notice.id == id));
        self.answered_at.retain(|id, _| self.questions.items.iter().any(|i| &i.notice.id == id));
        // A pull-down an agent's note opened, left unattended, pulls itself up (nothing stays beside the notch).
        if self.reply_open.is_some_and(|v| !v.owner && v.unattended.is_some_and(|s| t >= s + NOTE_UNATTENDED)) {
            self.reply_open = None;
        }
        // Reader.
        if let Some(r) = &mut self.reader {
            if r.tick(t) {
                let r = self.reader.take().expect("checked");
                let outcome = if r.pulled.is_some() { ReadOutcome::Dismissed } else { ReadOutcome::Read };
                let result = if outcome == ReadOutcome::Read { r.result() } else { r.result_as(ReadOutcome::Dismissed) };
                self.outputs.push(Output::Read(result));
            }
        }
        // Drawing.
        if let Some(d) = &mut self.draw {
            if d.swap.as_ref().is_some_and(|(_, _, u)| t >= *u) {
                d.swap = None;
            }
        }
        if let Some((_, _, Some(until))) = &self.attached {
            if t >= *until {
                self.attached = None;
            }
        }
        if let Some((tc, r0, _)) = self.light.close {
            if t - tc > 0.0 && LIGHT.eval(r0, 0.0, 0.0, t - tc).0 < 0.002 && self.light.flash_at.is_none_or(|f| t >= f + FLASH_SECONDS) {
                self.light = LightState::default();
            }
        }
        if self.light.flash_at.is_some_and(|f| t >= f + FLASH_SECONDS) {
            self.light.flash_at = None;
        }
        // Pointing.
        if let Some(p) = &mut self.pointing {
            if p.fade_from.is_none() && !self.voice.on && self.voice.ended_at.is_none() && t >= p.since + POINT_ALONE {
                p.fade_from = Some(p.since + POINT_ALONE);
            }
            if p.fade_from.is_some_and(|f| t >= f + POINT_FADE) {
                self.pointing = None;
            }
        }
        // Held expansions.
        if self.expand.as_ref().is_some_and(|(_, u)| t >= *u) {
            self.expand = None;
        }
    }

    /// The next time the scene changes without an event.
    pub fn deadline(&self) -> Option<f64> {
        let mut d: Vec<f64> = vec![];
        d.extend(self.cues.deadline());
        if self.voice.on {
            d.push(self.voice_stall_at());
            if self.voice.sentences.is_empty() && self.voice.on_at + WORDS_WAIT > self.last_t {
                d.push(self.voice.on_at + WORDS_WAIT);
            }
        }
        d.extend(self.voice.ended_at.map(|e| e + GRACE));
        d.extend(self.turn.sent_at.map(|s| s.max(self.turn.held_until) + SENT_SECONDS).filter(|d| d.is_finite()));
        if self.turn.held_until.is_finite() && self.turn.held_until > self.last_t {
            d.push(self.turn.held_until);
        }
        if self.turn.activity.is_some() {
            d.push(self.turn.last_news + ACTIVITY_CAP);
        }
        if let Some(p) = &self.turn.pop {
            d.extend(p.shown_at.map(|s| s + p.read));
        }
        if let Some(cur) = &self.line {
            if let Some(s) = cur.shown_at {
                d.push(s + read_seconds(cur.kind, &cur.text));
                if let Some(next) = &self.next_line {
                    d.push(s + line_hold(cur, next));
                }
            }
        }
        if let Some(ms) = self.questions.next_expiry() {
            d.push(self.t_of_wall(ms));
        }
        if let Some(until) = self.countdown_until {
            d.extend(self.countdown_tick(until));
        }
        d.extend(self.answered_at.values().map(|a| a + ANSWERED));
        d.extend(self.expired_at.values().map(|a| a + EXPIRED_HOLD));
        if let Some(v) = self.reply_open.filter(|v| !v.owner) {
            d.extend(v.unattended.map(|s| s + NOTE_UNATTENDED));
        }
        if let Some(r) = &self.reader {
            d.extend(r.deadline());
        }
        if let Some(Draw { swap: Some((_, _, u)), .. }) = &self.draw {
            d.push(*u);
        }
        if let Some((_, _, Some(u))) = &self.attached {
            d.push(*u);
        }
        if let Some(s) = self.nudge.and_then(|n| n.shown_at) {
            d.push(s + NUDGE_SECONDS);
        }
        if let Some((_, _, at)) = self.switches.moment {
            if at + SWITCH_TURN > self.last_t {
                d.push(at + SWITCH_TURN);
            }
            d.push(at + SWITCH_SECONDS);
        }
        if let Some(v) = self.vella.as_ref().filter(|v| v.moment) {
            d.push(v.since + VELLA_SECONDS);
        }
        if let Some(p) = &self.pointing {
            match p.fade_from {
                Some(f) => d.push(f + POINT_FADE),
                None if !self.voice.on && self.voice.ended_at.is_none() => d.push(p.since + POINT_ALONE),
                None => {}
            }
        }
        if let Some((_, u)) = &self.expand {
            d.push(*u);
        }
        for (label, since) in &self.held_seen {
            let read = if label.starts_with("problem:") {
                PROBLEM_READ
            } else if label.starts_with("busy:") {
                BUSY_READ
            } else {
                RESTART_READ
            };
            if self.held_active(label) && since + read > self.last_t {
                d.push(since + read);
            }
        }
        d.into_iter().filter(|x| x.is_finite()).reduce(f64::min)
    }

    fn held_active(&self, label: &str) -> bool {
        match self.cues.held() {
            Look::Status { tone: HealthTone::Problem, label: l, .. } => label == format!("problem:{l}"),
            Look::Status { tone: HealthTone::Restarting, label: l, .. } => label == format!("restart:{l}"),
            Look::Status { tone: HealthTone::Busy, .. } => self.cues.busy_key().is_some_and(|k| label == format!("busy:{k}")),
            _ => false,
        }
    }

    // ------------------------------------------------------------------ the scene

    /// The light at `t` (pure).
    pub fn light_now(&self, t: f64) -> Light {
        let flash = self.light.flash_at.map_or(0.0, |f| if t >= f { 0.16 * (1.0 - (t - f) / FLASH_SECONDS).clamp(0.0, 1.0) } else { 0.0 });
        if let Some((tc, r0, o0)) = self.light.close {
            let run = LIGHT.eval(r0, 0.0, 0.0, (t - tc).max(0.0)).0.clamp(0.0, 1.0);
            // Gone exactly once it has run back (the frames after it are identical).
            if run < 0.002 {
                return Light { run: 0.0, opacity: 0.0, flash };
            }
            let opacity = if r0 > 1e-6 { o0 * (run / r0).clamp(0.0, 1.0) } else { 0.0 };
            return Light { run, opacity, flash };
        }
        if let Some(to) = self.light.open_at {
            let tau = (t - to).max(0.0);
            let run = LIGHT.reached(tau);
            let epoch = to + LIGHT.reach_time();
            let opacity = if t < epoch { 1.0 } else { 0.65 + 0.35 * (2.0 * std::f64::consts::PI * (t - epoch) / BREATH_PERIOD).cos() };
            return Light { run, opacity, flash };
        }
        Light { run: 0.0, opacity: 0.0, flash }
    }

    /// The light's loop epoch while it breathes.
    pub fn light_breathing(&self, t: f64) -> Option<f64> {
        let to = self.light.open_at?;
        let epoch = to + LIGHT.reach_time();
        (t >= epoch && self.light.close.is_none()).then_some(epoch)
    }

    /// Whether the light or flash still moves at `t`.
    pub fn light_moving(&self, t: f64) -> bool {
        if self.light.flash_at.is_some_and(|f| t < f + FLASH_SECONDS) {
            return true;
        }
        if let Some((tc, r0, _)) = self.light.close {
            return LIGHT.eval(r0, 0.0, 0.0, (t - tc).max(0.0)).0 >= 0.002;
        }
        self.light.open_at.is_some_and(|to| t < to + LIGHT.reach_time())
    }

    /// The pointing marks' opacity at `t` (pure).
    pub fn pointing_now(&self, t: f64) -> f64 {
        let Some(p) = self.pointing else { return 0.0 };
        // Exact at the ends (the frames after a fade are identical), the same expressions as the deadlines.
        let ramp = |t: f64, from: f64, len: f64| if t >= from + len { 1.0 } else { ((t - from) / len).clamp(0.0, 1.0) };
        let arrive = ramp(t, p.since, POINT_IN);
        let leave = p.fade_from.map_or(1.0, |f| 1.0 - ramp(t, f, POINT_FADE));
        let leave = if p.fade_from.is_none() && !self.voice.on && self.voice.ended_at.is_none() && t >= p.since + POINT_ALONE {
            1.0 - ramp(t, p.since + POINT_ALONE, POINT_FADE)
        } else {
            leave
        };
        (arrive * leave).clamp(0.0, 1.0)
    }

    pub fn pointing_moving(&self, t: f64) -> bool {
        let Some(p) = self.pointing else { return false };
        let alone = p.fade_from.is_none() && !self.voice.on && self.voice.ended_at.is_none() && t >= p.since + POINT_ALONE && t < p.since + POINT_ALONE + POINT_FADE;
        t < p.since + POINT_IN || p.fade_from.is_some_and(|f| t < f + POINT_FADE) || alone
    }

    /// Whether the voice's look is up at `t`, and whether it has stopped (flattened, dimmed).
    fn voice_look(&self, t: f64) -> Option<bool> {
        if self.voice.hushed {
            return None;
        }
        if self.voice.on {
            // The words follow the audio by about a tenth of a second: wait for them (up to `WORDS_WAIT`) so the
            // speaking shape opens once, with its words, never as an empty box that fills late.
            if self.voice.sentences.is_empty() && t < self.voice.on_at + WORDS_WAIT {
                return None;
            }
            return Some(false);
        }
        self.voice.ended_at.filter(|e| t < e + GRACE).map(|_| true)
    }

    /// The moment as a row (icon, words) for a status row or an inline line; marks agent lines and turn statuses
    /// as shown.
    fn moment_row(&mut self, t: f64, mark: bool) -> Option<(Glyph, String)> {
        if let Some(look) = self.cues.on_screen().filter(|m| self.ears_said.as_ref() != Some(&format!("{m:?}"))) {
            if let Some(row) = cue_row(&look) {
                return Some(row);
            }
        }
        if let Some(line) = &mut self.line {
            if mark {
                line.shown_at.get_or_insert(t);
            }
            return Some((kind_glyph(line.kind), line.text.clone()));
        }
        if let Some((g, words, until)) = &mut self.attached {
            if mark {
                until.get_or_insert(t + ATTACHED_SECONDS);
            }
            return Some((g.clone(), words.clone()));
        }
        None
    }

    /// The one thing the island says at `t`. Marks lines, statuses and held states as shown from `t`, so call it
    /// at every event and deadline.
    pub fn scene(&mut self, t: f64) -> Scene {
        self.last_t = t;
        // A tucked question answered, closed or expired is no longer tucked: asked again, it shows.
        let questions = &self.questions.items;
        self.tucked.retain(|id| questions.iter().any(|i| i.notice.id == *id && i.phase == QPhase::Open));
        let s = self.resolve(t);
        let card = match &s {
            Scene::Question { card, .. } => Some(card),
            Scene::Reply { ask: Some(a), .. } => Some(&a.card),
            _ => None,
        };
        let now_ms = self.wall_ms(t);
        self.countdown_until = card.and_then(|c| c.countdown).filter(|c| c.left_ms > 0).map(|c| now_ms + c.left_ms);
        // A pull-down an agent's note opened counts its unattended time only while it is on screen with the pointer
        // away (hidden under a question, the reader or drawing mode, its time starts over when it shows again).
        let shown = matches!(s, Scene::Reply { .. });
        let over = self.pointer;
        if let Some(v) = &mut self.reply_open {
            if shown && !v.owner && !over && !v.hot {
                v.unattended.get_or_insert(t);
            } else {
                v.unattended = None;
            }
        }
        // A held restart or problem shown expanded: remember from when.
        if let Scene::Stack { .. } = &s {
            if let Look::Status { tone, label, .. } = self.cues.held() {
                let key = match tone {
                    HealthTone::Problem => Some(format!("problem:{label}")),
                    HealthTone::Restarting => Some(format!("restart:{label}")),
                    // By its condition, not its words: a new count keeps the read time running.
                    HealthTone::Busy => self.cues.busy_key().map(|k| format!("busy:{k}")),
                    _ => None,
                };
                if let Some(key) = key {
                    self.held_seen.entry(key).or_insert(t);
                }
            }
        }
        self.held_seen.retain(|k, _| {
            let held = self.cues.held();
            matches!(&held, Look::Status { tone: HealthTone::Problem, label, .. } if *k == format!("problem:{label}"))
                || matches!(&held, Look::Status { tone: HealthTone::Restarting, label, .. } if *k == format!("restart:{label}"))
                // Catching up keeps its read while it lasts, so a restart in between does not open it again after.
                || k.strip_prefix("busy:").is_some_and(|key| self.cues.is_busy(key))
        });
        s
    }

    fn resolve(&mut self, t: f64) -> Scene {
        // Drawing mode: the keys hold the island; a moment shows in the ears for a second, Send never moves.
        if self.draw.is_some() {
            let moment = self.cues.on_screen();
            let key = moment.as_ref().map(|m| format!("{m:?}"));
            let d = self.draw.as_mut().expect("checked");
            if let (Some(m), Some(k)) = (&moment, key) {
                self.ears_said = Some(k.clone());
                if d.swapped.as_ref() != Some(&k) {
                    if let Some((l, r)) = cue_ears(m) {
                        d.swap = Some((l, r, t + LOOK_SECONDS));
                    }
                    d.swapped = Some(k);
                }
            }
            let mut ears = d.swap.as_ref().map(|(l, r, _)| (l.clone(), r.clone()));
            if let Some(Scene::Ears { left: Ear::Glyph(l), right: Ear::Glyph(r), .. }) = self.switch_scene(t) {
                ears = Some((l, r));
            }
            let d = self.draw.as_ref().expect("checked");
            return Scene::Drawing { ink: d.ink > 0, ears, send: d.send, close: d.close };
        }
        // Bodies: the reader, a question, the last turn pulled down; a moment rides on top as a status row.
        if self.reader.as_ref().is_some_and(|r| r.pulled.is_none()) {
            let status = self.moment_row(t, true);
            let r = self.reader.as_ref().expect("checked");
            return Scene::Reader { speed: r.wpm(), left: r.left(), face: r.face(), playing: r.playing(), started: r.started(), hot: false, status };
        }
        let question = self.question_scene(false);
        // The user's pulled-down conversation stays; a question docks at its bottom (a tucked one too: it is still his to
        // answer).
        let docked = self.reply_shows() && self.reply_open.is_some_and(|v| v.owner) && matches!(question, Some(Scene::Question { .. }) | None);
        if let (Some(q), false) = (question, docked) {
            let status = self.moment_row(t, true);
            return match q {
                // While Heimdall speaks, his words stay on top and the card docks below them: the card never
                // replaces the speech ("the decision goes down, so … I can always see you talk").
                Scene::Question { id, card, .. } => {
                    let hot = self.ask_hot;
                    let speech = self.voice_look(t).filter(|_| !self.voice.sentences.is_empty()).map(|stopped| super::layout::Speech {
                        sentences: self.voice.sentences.clone(),
                        level: if stopped { 0 } else { (self.voice.level.clamp(0.0, 1.0) * 1000.0).round() as u16 },
                        stopped,
                    });
                    Scene::Question { id, status, card, speech, thinking: self.turn.activity == Some(Activity::Thinking), hot }
                }
                other => other,
            };
        }
        let ask = match self.question_scene(true) {
            Some(Scene::Question { id, card, .. }) => Some(super::layout::Ask { id, card }),
            _ => None,
        };
        // The last turn, pulled down: the voice's ears (live while it speaks), the whole reply, its notes, its lip band.
        if self.reply_shows() {
            let status = self.moment_row(t, true);
            let speaking = self.voice_look(t) == Some(false);
            let level = if speaking { (self.voice.level.clamp(0.0, 1.0) * 1000.0).round() as u16 } else { 0 };
            let v = self.reply_open.expect("checked");
            let notes = self.notes.iter().map(|n| NoteView { id: n.id.clone(), rev: n.rev, icon: n.icon.clone(), title: n.title.clone(), body: n.body }).collect();
            let paragraphs = self.turn_paragraphs();
            // The conversation around the turn only when the user pulled it down: a pull-down a note opened stays the
            // note's (it pulls itself up again).
            let chat = if v.owner { crate::island::chat::split(&self.history, &paragraphs.join("\n"), &self.note_ids()) } else { Default::default() };
            return Scene::Reply { paragraphs, notes, chat, follow: !v.scrolled, tail: v.tail, ask, status, level, flat: !speaking, scroll: v.scroll, hot: v.hot, composer_empty: self.composer_empty, attached: self.composer_attached.clone(), working: v.owner && self.turn.activity == Some(Activity::Thinking), copied: v.copied };
        }
        // The voice: its sentences accumulate below the ears; a moment rides above them as a status row (the
        // words never leave for it).
        if let Some(stopped) = self.voice_look(t) {
            let status = self.moment_row(t, true);
            if status.is_some() && self.cues.on_screen().and_then(|l| cue_row(&l)).is_none() {
                if let Some(line) = &mut self.line {
                    line.inline = true;
                }
            }
            let level = (self.voice.level.clamp(0.0, 1.0) * 1000.0).round() as u16;
            return Scene::Speaking { sentences: self.voice.sentences.clone(), status, stopped, level: if stopped { 0 } else { level } };
        }
        // The voice ended while a line showed inside its row: if the line is nearly read, it ends with the voice
        // (never a grow into a stack and a retraction 0.3 s later); otherwise it moves once to the stack and holds.
        if let Some(line) = &self.line {
            if line.inline && line.shown_at.is_some_and(|s| s + read_seconds(line.kind, &line.text) - t < 1.5) {
                self.line = self.next_line.take();
            }
        }
        // Moments: a failed Send's warning, a command result or health message, a line, then the turn's new status.
        if let Some((g, words, until)) = &mut self.attached {
            until.get_or_insert(t + ATTACHED_SECONDS);
            return Scene::Stack { icon: g.clone(), words: words.clone(), button: None };
        }
        let said = self.cues.on_screen().is_some_and(|m| self.ears_said.as_ref() == Some(&format!("{m:?}")));
        if !said {
            self.ears_said = None;
        }
        if let Some(look) = self.cues.on_screen().filter(|_| !said) {
            if let Some(s) = cue_scene(&look) {
                return s;
            }
        }
        // A switch the user just turned, after a command or health moment (their time runs from arrival; a line waits).
        // The voice, a body or drawing mode's keys keep the island: drawing mode shows it in its ears, the others
        // only show the token after.
        if let Some(s) = self.switch_scene(t) {
            return s;
        }
        // Dictating to Vella, as it starts: its mark over a few words.
        if self.vella.as_ref().is_some_and(|v| v.moment) {
            return Scene::Stack { icon: Glyph::new(super::layout::VELLA), words: VELLA_WORDS.into(), button: None };
        }
        if let Some(line) = &mut self.line {
            line.shown_at.get_or_insert(t);
            let line = line.clone();
            return line_scene(&line);
        }
        if let Some(pop) = &mut self.turn.pop {
            pop.shown_at.get_or_insert(t);
            return match &pop.kind {
                PopKind::Working(words) => Scene::Working { words: words.clone() },
                PopKind::Compacting => Scene::Stack { icon: Glyph::new("rectangle.compress.vertical"), words: "Compacting".into(), button: None },
            };
        }
        let held = self.cues.held();
        // The turn's lip, once the user has been quiet the ack's delay (`Event::Hold`).
        let waiting = t < self.turn.held_until;
        let sent = self.turn.sent_at.is_some() && !waiting;
        let activity = self.turn.activity.filter(|a| !(waiting && *a == Activity::Thinking));
        if !sent && activity.is_none() {
            if let Some(s) = self.nudge_scene(t) {
                return s;
            }
        }
        if sent {
            return Scene::Lip(Lip::Sent);
        }
        match activity {
            Some(Activity::Thinking) => return Scene::Lip(Lip::Dots),
            Some(Activity::Compacting) => return Scene::Lip(Lip::Compress),
            None => {}
        }
        // Held health: expanded for its read time, then a token.
        match &held {
            Look::Status { tone: HealthTone::Restarting, label, .. } => {
                let key = format!("restart:{label}");
                if self.held_seen.get(&key).is_none_or(|s| t < s + RESTART_READ) {
                    return Scene::Stack { icon: Glyph::new("arrow.clockwise").colors(&[Rgba::ORANGE]).fx(Fx::Rotate), words: label.clone(), button: None };
                }
                return Scene::Ears {
                    left: Ear::Glyph(Glyph::new("arrow.clockwise").colors(&[Rgba::ORANGE]).fx(Fx::Rotate)),
                    right: Ear::Glyph(Glyph::new("hourglass").colors(&[Rgba::ORANGE])),
                    target: None,
                };
            }
            // Catching up: its stack and words for their read, then the stack (its layers lighting in turn) and a dim
            // hourglass rest in the ears until it ends.
            Look::Status { tone: HealthTone::Busy, label, .. } => {
                let key = self.cues.busy_key().map(|k| format!("busy:{k}")).unwrap_or_default();
                if self.held_seen.get(&key).is_none_or(|s| t < s + BUSY_READ) {
                    return Scene::Stack { icon: busy_glyph(), words: label.clone(), button: None };
                }
                return busy_token();
            }
            Look::Status { tone: HealthTone::Problem, symbol, label, detail, .. } => {
                let key = format!("problem:{label}");
                let expanded = self.held_seen.get(&key).is_none_or(|s| t < s + PROBLEM_READ) || self.expand.as_ref().is_some_and(|(l, _)| l == label);
                if expanded {
                    return problem_stack(symbol, label, detail.as_deref());
                }
                return problem_token(symbol);
            }
            _ => {}
        }
        if let Look::Pending { kind, .. } = held {
            return heard_ears(kind);
        }
        // Tokens at rest.
        if let Look::Alert { symbol, tone } = held {
            if tone == HealthTone::Problem {
                if let Some((l, _)) = &self.expand {
                    if let Some((_, _, sym, label, action)) = self.cues.problem() {
                        if *l == label {
                            return problem_stack(sym, &label, Some(&action));
                        }
                    }
                }
                return problem_token(symbol);
            }
        }
        if self.reader.as_ref().is_some_and(|r| r.pulled.is_some()) {
            return Scene::Ears {
                left: Ear::Glyph(Glyph::new("smallcircle.filled.circle").colors(&[Rgba::RED, Rgba::WHITE])),
                right: Ear::Glyph(Glyph::new("chevron.down.circle")),
                target: token(Token::Reader),
            };
        }
        if let Look::Alert { symbol, tone: HealthTone::Drop } = held {
            return Scene::Ears {
                left: Ear::Glyph(cue_glyph(symbol).colors(&[Rgba::ORANGE])),
                right: Ear::Glyph(Glyph::new("exclamationmark.circle.fill").colors(&[Rgba::WHITE, Rgba::ORANGE])),
                target: None,
            };
        }
        // Emergency mode: the agent's glyph and the warning triangle, for as long as it lasts. The notch still opens
        // the last turn; the peek lip shows while the pointer is there.
        if self.emergency && !(self.peek && self.notch_opens()) {
            return emergency_token(if self.notch_opens() { token(Token::Reply) } else { None });
        }
        // Nothing else to say and the pointer at the notch with a last turn to open: the peek lip.
        if self.peek && self.notch_opens() {
            return Scene::Lip(Lip::Peek);
        }
        // Dictating to Vella (with Listening on): Vella's mark rests in the left ear, its shortcut in the right; the
        // notch still opens the last turn.
        if let Some(v) = self.vella.as_ref().filter(|_| !self.switches.listening_off) {
            let target = if self.notch_opens() { token(Token::Reply) } else { None };
            let right = match &v.keys {
                Some(k) if k.chars().count() <= VELLA_KEYS_MAX => Ear::Num(k.clone()),
                _ => Ear::Glyph(Glyph::new("keyboard")),
            };
            return Scene::Ears { left: Ear::Glyph(Glyph::new(super::layout::VELLA)), right, target };
        }
        // Listening or Seeing off: the user can see Heimdall is deaf or blind; the notch still opens the last turn.
        if self.switches.listening_off || self.switches.seeing_off {
            let target = if self.notch_opens() { token(Token::Reply) } else { None };
            let (left, right) = (self.switch_ear(super::Switch::Listening, SWITCH_TOKEN_ALPHA), self.switch_ear(super::Switch::Seeing, SWITCH_TOKEN_ALPHA));
            return Scene::Ears { left, right, target };
        }
        Scene::Rest
    }

    /// A switch's ear: slashed while off, whole while on (beside one that is off); dim at rest, full in a moment.
    fn switch_ear(&self, switch: super::Switch, alpha: f64) -> Ear {
        let on = match switch {
            super::Switch::Listening => !self.switches.listening_off,
            super::Switch::Seeing => !self.switches.seeing_off,
            super::Switch::Islands => true,
        };
        Ear::Glyph(Glyph::new(&switch.glyph_name(on)).alpha(alpha))
    }

    /// The switch moment (24 Oct): the switch's glyph in its ear, as it was, then turning into what it is now on the
    /// same image view (the system's magic replace draws or lifts the slash); the other ear shows its own switch, both
    /// at full strength, dimming together into the token. Islands has no token: its glyph turns on the left and a
    /// check answers on the right.
    fn switch_scene(&self, t: f64) -> Option<Scene> {
        let (switch, on, at) = self.switches.moment?;
        if t >= at + SWITCH_SECONDS {
            return None;
        }
        let now = if t < at + SWITCH_TURN { !on } else { on };
        let glyph = Ear::Glyph(Glyph::new(&switch.glyph_name(now)));
        let (left, right) = match switch {
            super::Switch::Listening => (glyph, self.switch_ear(super::Switch::Seeing, 1.0)),
            super::Switch::Seeing => (self.switch_ear(super::Switch::Listening, 1.0), glyph),
            super::Switch::Islands => (glyph, Ear::Glyph(Glyph::new("checkmark").bold().fx(Fx::Bounce))),
        };
        Some(Scene::Ears { left, right, target: None })
    }


    /// Night 1022: the nudge, once nothing else shows (the voice, the pull-down, a question, the reader, drawing
    /// mode, a line, the turn's lip all come first; held health tokens wait for it). One that waited too long goes.
    fn nudge_scene(&mut self, t: f64) -> Option<Scene> {
        let n = self.nudge.as_mut()?;
        if n.shown_at.is_none() && t - n.asked > NUDGE_QUEUE {
            self.nudge = None;
            return None;
        }
        let s = *n.shown_at.get_or_insert(t);
        (t < s + NUDGE_SECONDS).then_some(Scene::Lip(Lip::Nudge { question: n.question }))
    }

    /// Night 1022: whether the pulled-down turn has anything to pull out: its words or its notes (23 Oct: a table
    /// could not be pulled out).
    fn can_pull_out(&self) -> bool {
        !self.notes.is_empty() || self.turn_paragraphs().iter().any(|p| !p.trim().is_empty())
    }

    /// Night 1022: the turn's reply as written leaves as a floating document island; the pull-down goes up.
    fn pull_out(&mut self) {
        if !self.reply_shows() || !self.can_pull_out() {
            return;
        }
        let text = self.turn_paragraphs().join("\n").trim().to_string();
        self.outputs.push(Output::PullOut { text, notes: self.note_ids() });
        self.reply_open = None;
    }

    /// Night 1022: the pulled-down reply left the screen while dragged: the stretch springs back, nothing pulled out.
    pub fn let_go(&mut self, t: f64) {
        if self.pull.is_some() {
            self.release_pull(t);
        }
    }

    fn release_pull(&mut self, t: f64) {
        if self.pull.is_some() {
            let v = self.stretch(t);
            self.pull = None;
            self.pull_back = (v > 0.0).then(|| (v, t, t + PULL_BACK.settle_time(v, 0.0, 0.0, 0.1)));
        }
    }

    /// Night 1022: how far the pulled-down shape is stretched at `t` (the drag's rubber band, then its spring back).
    pub fn stretch(&self, t: f64) -> f64 {
        if let Some((p, from, at)) = self.pull {
            let to = rubber(p);
            // Critically damped, not exponential: 1 - (1 + x)e^-x with x in units of PULL_FOLLOW. It starts at rest and
            // never passes the target, but it is slower at one constant (a quarter of the way, not two thirds). The
            // exponential's full first slope stepped the height 23 pt in 18 ms (random-1965, 5.189 s, I4).
            let x = (t - at).max(0.0) / PULL_FOLLOW;
            return to + (from - to) * (1.0 + x) * (-x).exp();
        }
        match self.pull_back {
            Some((v, t0, end)) if t < end => PULL_BACK.eval(v, 0.0, 0.0, t - t0).0.max(0.0),
            _ => 0.0,
        }
    }

    /// Night 1022: the stretch moves (held: the pointer may move any frame; released: it springs back).
    pub fn stretch_moving(&self, t: f64) -> bool {
        self.pull.is_some() || self.pull_back.is_some_and(|(_, _, end)| t < end)
    }

    /// Night 1022: a nudge waiting or showing (tests).
    pub fn nudge_pending(&self) -> bool {
        self.nudge.is_some()
    }

    /// The digit keys (Option plus a digit) the question card on screen answers now: its options on its current tab,
    /// at most four, while it is open. Zero for anything else, and for a card tucked away (unless it is docked in
    /// the user's pulled-down conversation). A naming card (a voice card's name field) draws no digits and takes none: it
    /// comes on its own, and a stray Option+1 typed in another app must never answer it (Not important forgets a voice).
    pub fn card_keys(&self) -> usize {
        match self.card_item() {
            Some(item) if item.phase == QPhase::Open => item.questions().get(item.tab).map_or(0, |q| if q.field.is_some() { 0 } else { q.options.len().min(4) }),
            _ => 0,
        }
    }

    /// The request ids of the question cards the island holds (answered ones included until they leave).
    pub fn card_ids(&self) -> Vec<String> {
        self.questions.items.iter().filter_map(|i| i.request_id().map(str::to_owned)).collect()
    }

    /// Whether `item` is an open question the user tucked away.
    fn is_tucked(&self, item: &crate::agent_notice::Item) -> bool {
        item.phase == QPhase::Open && self.tucked.contains(&item.notice.id)
    }

    /// The card on screen: docked in the user's pulled-down conversation, the oldest; otherwise the oldest he has not
    /// tucked away.
    fn card_item(&self) -> Option<&crate::agent_notice::Item> {
        if self.reply_open.is_some_and(|v| v.owner) {
            return self.questions.items.first();
        }
        self.questions.items.iter().find(|i| !self.is_tucked(i))
    }

    /// The card as a scene: the oldest the user has not tucked away, or with `tucked` (docked in his conversation) the
    /// oldest of all.
    fn question_scene(&self, tucked: bool) -> Option<Scene> {
        let item = if tucked { self.questions.items.first()? } else { self.questions.items.iter().find(|i| !self.is_tucked(i))? };
        if item.is_connect() {
            let (label, target) = match item.phase {
                QPhase::Connecting => ("Connecting…".to_string(), None),
                _ => ("Connect".to_string(), Some(Target::Connect(item.notice.id.clone()))),
            };
            return Some(Scene::Stack { icon: Glyph::new("terminal").fx(Fx::Bounce), words: item.title(), button: Some((label, target)) });
        }
        let tab = item.tab.min(item.questions().len().saturating_sub(1));
        let labels: Vec<String> = item.questions().get(tab).map(|q| q.options.iter().map(|o| o.label.clone()).collect()).unwrap_or_default();
        let look = match &item.phase {
            QPhase::Open => QLook::Open,
            // The option the user picked on the tab (a click answered it), shown as chosen.
            QPhase::Answered => QLook::Chosen(item.picks.get(tab).and_then(|p| p.labels.first()).and_then(|l| labels.iter().position(|o| o == l)).unwrap_or(usize::MAX)),
            QPhase::Closed { answer } => QLook::Chosen(Notices::option_named(&labels, answer).and_then(|a| labels.iter().position(|o| o == a)).unwrap_or(usize::MAX)),
            QPhase::Expired => QLook::Expired,
            QPhase::Unavailable => QLook::Unavailable,
            QPhase::Connecting => QLook::Connecting,
        };
        // A countdown runs only while the card is open; an answered card's controls are off and its time no longer counts.
        let countdown = item.countdown(self.wall_ms(self.last_t)).filter(|_| look == QLook::Open);
        let card = super::layout::Card { questions: item.questions().to_vec(), tab, picks: item.picks.clone(), look, countdown };
        Some(Scene::Question { id: item.notice.id.clone(), status: None, card, speech: None, thinking: false, hot: false })
    }

    /// Every tucked question shows again.
    pub fn untuck(&mut self) {
        self.tucked.clear();
    }

    /// An open question the user tucked away waits for the notch.
    pub fn tucked_pending(&self) -> bool {
        self.questions.items.iter().any(|i| self.is_tucked(i))
    }

    /// Whether the notch at rest opens something: the last turn, or a question tucked away.
    pub fn notch_opens(&self) -> bool {
        self.reply_available() || self.tucked_pending()
    }

    /// Whether a speaking look exists (for invariants).
    pub fn speaking(&self, t: f64) -> bool {
        self.voice_look(t).is_some()
    }
    /// Whether the notch can be pulled down: the last turn has a reply or a note, or there is a conversation to show.
    pub fn reply_available(&self) -> bool {
        self.turn_available() || !self.history.is_empty()
    }
    /// Whether the last turn has anything to show: its reply or a note.
    fn turn_available(&self) -> bool {
        !self.notes.is_empty() || (self.reply_turn == self.turn_no && (self.written.is_some() || self.reply.as_ref().is_some_and(|r| !r.is_empty())))
    }
    /// The last turn's reply as the pull-down shows it: none while a later turn has only shown notes so far (its
    /// notes never sit under an older turn's words).
    pub fn turn_paragraphs(&self) -> Vec<String> {
        if self.reply_turn == self.turn_no {
            self.reply_paragraphs()
        } else {
            vec![]
        }
    }
    /// The last turn's notes by id (for tests and invariants).
    pub fn note_ids(&self) -> Vec<String> {
        self.notes.iter().map(|n| n.id.clone()).collect()
    }
    /// The last reply as the pull-down shows it: paragraphs (an empty one is a blank line). The reply as written when
    /// the app sent it (it arrives before its speech, so the pull-down holds the whole reply while it is said), else
    /// the last spoken reply's sentences as one paragraph.
    pub fn reply_paragraphs(&self) -> Vec<String> {
        match (&self.written, &self.reply) {
            (Some(w), _) => w.split('\n').map(str::to_string).collect(),
            (None, Some(s)) => vec![s.join(" ")],
            (None, None) => vec![],
        }
    }
    /// The sentences the speaking row holds now (for invariants).
    pub fn spoken(&self) -> &[String] {
        &self.voice.sentences
    }
    /// Whether the pulled-down reply is open.
    pub fn reply_open(&self) -> bool {
        self.reply_open.is_some()
    }
    /// Whether the pulled-down reply shows (open, with a reply to show).
    fn reply_shows(&self) -> bool {
        self.reply_open.is_some_and(|v| self.turn_available() || (v.owner && (v.keys || !self.history.is_empty())))
    }
    /// The pulled-down reply's scroll, kept within its body (the engine knows the body's length once laid out); while
    /// it follows the latest exchange, where that put it (`at`), so the user's first scroll starts from what he sees.
    pub fn clamp_reply_scroll(&mut self, max: f64, at: f64) {
        if let Some(v) = &mut self.reply_open {
            v.scroll = if v.scrolled { v.scroll } else { at }.clamp(0.0, max.max(0.0));
            v.at_end = v.scroll >= max - 1.0;
        }
    }

    /// A new message (a reply, a row of the conversation): the user's open conversation follows it to the end, unless
    /// he scrolled up on purpose (away from the end).
    fn follow_new_message(&mut self) {
        if let Some(v) = self.reply_open.as_mut().filter(|v| v.owner) {
            if !v.scrolled || v.at_end {
                v.scrolled = false;
                v.tail = true;
            }
        }
    }

    /// Whether the open pull-down is the user's (he pulled it down: ⌃⌘H or a click), not one an agent's note opened.
    pub fn owners_reply_open(&self) -> bool {
        self.reply_open.is_some_and(|v| v.owner)
    }
    /// Where a drop goes now (`drop::route`): into the composer while the pull-down shows it and he writes there
    /// (`focused`: the composer has the keyboard, which only the window knows).
    pub fn drop_route(&self, focused: bool) -> crate::island::drop::Route {
        crate::island::drop::route(self.reply_shows(), !self.composer_empty, focused)
    }
    /// What the composer holds that was dropped into it.
    pub fn composer_attached(&self) -> &[crate::island::drop::DropItem] {
        &self.composer_attached
    }
    pub fn voice_on(&self) -> bool {
        self.voice.on
    }
    pub fn drawing(&self) -> bool {
        self.draw.is_some()
    }
    pub fn line_ids(&self) -> Vec<String> {
        self.line.iter().chain(self.next_line.iter()).map(|l| l.id.clone()).collect()
    }
}

fn inverse_light(run: f64) -> f64 {
    // The time on the unit light curve at which it has run `run` (so reopening mid-close continues from there).
    if run <= 0.0 {
        return 0.0;
    }
    let (mut lo, mut hi) = (0.0, LIGHT.reach_time());
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if LIGHT.unit(mid) < run {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// The island's icon for a cue's symbol name.
fn cue_glyph(name: &str) -> Glyph {
    match name {
        crate::island::HELMET => Glyph::new(HELMET),
        crate::island::EYE => Glyph::new("eye"),
        crate::island::CHECK | crate::island::SMILE => Glyph::new("checkmark.circle.fill").colors(&[Rgba::WHITE, Rgba::GREEN]),
        other => Glyph::new(other),
    }
}

fn kind_glyph(kind: NoticeKind) -> Glyph {
    match kind {
        NoticeKind::Info => Glyph::new("info.circle.fill").colors(&[Rgba::WHITE, Rgba::BLUE]).fx(Fx::Bounce),
        NoticeKind::Success => Glyph::new("checkmark.circle.fill").colors(&[Rgba::WHITE, Rgba::GREEN]).fx(Fx::Bounce),
        NoticeKind::Warning => Glyph::new("exclamationmark.triangle.fill").colors(&[Rgba::BLACK, Rgba::ORANGE]).fx(Fx::Bounce),
        NoticeKind::Error => Glyph::new("xmark.circle.fill").colors(&[Rgba::WHITE, Rgba::RED]).fx(Fx::Wiggle),
        NoticeKind::Looking => Glyph::new("eye").fx(Fx::Bounce),
        NoticeKind::Working => Glyph::new("ellipsis"),
    }
}

/// Whether a line reads as lines to read (long and informational: a spoken line with the voice off) rather than an
/// icon over words.
/// Text with its line breaks kept: each line's spaces as one, runs of blank lines as one, none at the ends.
fn keep_breaks(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    lines.join("\n").trim_matches('\n').to_string()
}

fn reads_as_lines(kind: NoticeKind, text: &str) -> bool {
    let long = text.chars().count() > 34 || text.split_whitespace().count() > 5;
    long && matches!(kind, NoticeKind::Info)
}

/// How long a line holds before a different one replaces it: a line to read (a reply shown as text, bug note 22 Oct
/// 15:13: "you've responded twice and I cannot see your last response") its whole read time, so a second reply queues
/// behind the first; otherwise `MIN_LINE` (a flood shows the newest, and a warning or error never waits behind a reply).
fn line_hold(line: &Line, next: &Line) -> f64 {
    if reads_as_lines(line.kind, &line.text) && reads_as_lines(next.kind, &next.text) {
        read_seconds(line.kind, &line.text)
    } else {
        MIN_LINE
    }
}

/// A line: icon over words, or (long, informational) lines to read.
fn line_scene(line: &Line) -> Scene {
    if reads_as_lines(line.kind, &line.text) {
        Scene::Lines { text: line.text.clone() }
    } else {
        Scene::Stack { icon: kind_glyph(line.kind), words: line.text.clone(), button: None }
    }
}

fn result_glyph(kind: CommandKind, tone: Tone) -> Glyph {
    match tone {
        Tone::Failed => Glyph::new("exclamationmark.triangle.fill").colors(&[Rgba::BLACK, Rgba::ORANGE]).fx(Fx::Bounce),
        Tone::Done => match kind {
            CommandKind::Listening => Glyph::new(HELMET),
            k => cue_glyph(k.result_symbol()).fx(Fx::Bounce),
        },
    }
}

fn status_glyph(symbol: &str, tone: HealthTone) -> Glyph {
    match tone {
        HealthTone::Restarting => Glyph::new("arrow.clockwise").colors(&[Rgba::ORANGE]).fx(Fx::Rotate),
        HealthTone::Recovered => Glyph::new("checkmark.circle.fill").colors(&[Rgba::WHITE, Rgba::GREEN]).fx(Fx::Bounce),
        HealthTone::Problem => cue_glyph(symbol).colors(&[Rgba::RED]).fx(Fx::Bounce),
        HealthTone::Drop | HealthTone::Notice => cue_glyph(symbol).fx(Fx::Bounce),
        HealthTone::Busy => busy_glyph(),
    }
}

/// Catching up's stack in soft white, its layers lighting one after another (the system's variable colour).
fn busy_glyph() -> Glyph {
    Glyph::new(crate::island::BUSY_SYMBOL).colors(&[BUSY_WHITE]).fx(Fx::Variable)
}

/// Catching up at rest: the stack in the left ear, a dim hourglass in the right (the restart's token, calm).
fn busy_token() -> Scene {
    Scene::Ears { left: Ear::Glyph(busy_glyph()), right: Ear::Glyph(Glyph::new("hourglass").colors(&[Rgba::white(0.55)])), target: None }
}

/// Catching up's white: a little softer than the words.
const BUSY_WHITE: Rgba = Rgba([0xEB, 0xEB, 0xF0, 255]);

/// A result with its count: "3 screenshots" (DESIGN.md §3: only the numeral changes), else "Label ×3".
fn counted(label: &str, count: usize) -> String {
    if count > 1 && label == "Screenshot" {
        format!("{count} screenshots")
    } else {
        result_text(label, count)
    }
}

fn heard_ears(kind: CommandKind) -> Scene {
    if kind == CommandKind::Listening {
        return Scene::Ears { left: Ear::Glyph(Glyph::new(HELMET)), right: Ear::Spinner, target: None };
    }
    let icon = cue_glyph(kind.symbol());
    Scene::Ears { left: Ear::Glyph(icon.fx(Fx::Bounce)), right: Ear::Glyph(Glyph::new("hourglass")), target: None }
}

/// A command or health moment as a whole scene.
fn cue_scene(look: &Look) -> Option<Scene> {
    Some(match look {
        Look::Heard { kind, .. } => heard_ears(*kind),
        Look::Result { kind: CommandKind::Listening, tone: Tone::Done, .. } => Scene::Listening,
        Look::Result { kind, label, tone, count } => Scene::Stack { icon: result_glyph(*kind, *tone), words: counted(label, *count), button: None },
        Look::Status { symbol, label, detail, tone: HealthTone::Problem, .. } => problem_stack(symbol, label, detail.as_deref()),
        Look::Status { symbol, label, tone, .. } => Scene::Stack { icon: status_glyph(symbol, *tone), words: label.clone(), button: None },
        _ => return None,
    })
}

/// A command or health moment as one row (a status row above a body, a line inside the voice's row).
fn cue_row(look: &Look) -> Option<(Glyph, String)> {
    match look {
        Look::Result { kind, label, tone, count } => Some((result_glyph(*kind, *tone), counted(label, *count))),
        Look::Status { symbol, label, tone, .. } => Some((status_glyph(symbol, *tone), label.clone())),
        _ => None,
    }
}

/// A moment as drawing mode's ears: what happened, and whether it went well.
fn cue_ears(look: &Look) -> Option<(Glyph, Glyph)> {
    let check = Glyph::new("checkmark").bold().fx(Fx::Bounce);
    match look {
        Look::Heard { .. } => None,
        Look::Result { kind, tone: Tone::Done, .. } => Some((result_glyph(*kind, Tone::Done).fx(Fx::Bounce), check)),
        Look::Result { tone: Tone::Failed, .. } => {
            Some((result_glyph(CommandKind::Other, Tone::Failed), Glyph::new("exclamationmark.circle.fill").colors(&[Rgba::WHITE, Rgba::ORANGE]).fx(Fx::Bounce)))
        }
        Look::Status { tone: HealthTone::Busy, .. } => Some((busy_glyph(), Glyph::new("hourglass").colors(&[Rgba::white(0.55)]))),
        Look::Status { symbol, tone, .. } => {
            let c = match tone {
                HealthTone::Problem => Rgba::RED,
                HealthTone::Recovered => Rgba::GREEN,
                _ => Rgba::ORANGE,
            };
            let right = if *tone == HealthTone::Recovered { check } else { Glyph::new("exclamationmark.circle.fill").colors(&[Rgba::WHITE, c]).fx(Fx::Bounce) };
            Some((status_glyph(symbol, *tone).fx(Fx::Bounce), right))
        }
        _ => None,
    }
}

fn problem_stack(symbol: &str, label: &str, action: Option<&str>) -> Scene {
    let button = action.filter(|a| !a.trim().is_empty()).map(|a| (a.trim().to_string(), Some(Target::HealthButton)));
    Scene::Stack { icon: cue_glyph(symbol).colors(&[Rgba::RED]).fx(Fx::Bounce), words: label.to_string(), button }
}

/// Emergency mode at rest: the agent's glyph and the warning triangle in orange (it returns by itself, like a drop).
fn emergency_token(target: Option<Target>) -> Scene {
    Scene::Ears {
        left: Ear::Glyph(cue_glyph(crate::island::Part::Agent.symbol()).colors(&[Rgba::ORANGE])),
        right: Ear::Glyph(Glyph::new(EMERGENCY_SYMBOL).colors(&[Rgba::BLACK, Rgba::ORANGE])),
        target,
    }
}
/// The warning triangle emergency mode rests with.
pub const EMERGENCY_SYMBOL: &str = "exclamationmark.triangle.fill";

fn problem_token(symbol: &str) -> Scene {
    Scene::Ears {
        left: Ear::Glyph(cue_glyph(symbol).colors(&[Rgba::RED])),
        right: Ear::Glyph(Glyph::new("exclamationmark.circle.fill").colors(&[Rgba::WHITE, Rgba::RED])),
        target: token(Token::Problem),
    }
}
