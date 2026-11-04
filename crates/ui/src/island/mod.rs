//! The island domain (see README.md): `object` is the island as one object — one state machine, one layout, one
//! motion clock. This file keeps its shared types and `Cues`, the queue of command results and health moments the
//! object composes with everything else:
//!
//! A command is detected early (partial, window decode or final) and decided only when the user's turn ends,
//! possibly minutes later. So the pill has two moments: `Heard` grows out of the notch with the command's icon and
//! shrinks to a small glyph that stays while the decision is pending; `Done` or `Failed` expands with a 1-3 word
//! label, then retracts. A rejected command (`Dropped`) leaves silently. Several pending commands share one glyph
//! with a count. Results that arrive together are all said (three screenshots in 1.3 s: "it only showed
//! it like for the last one … kind of annoying"): the same result again counts on as it arrives ("Screenshot ×2",
//! only the numeral changes), different results take turns, and a restart or problem goes right after the result on
//! screen, so the island never lags far behind. This file is the platform-neutral state; `mac/island/` draws it.
//!
//! The island also carries Heimdall's health ("okay something is wrong, Heimdall is restarting — the
//! orange thingy with a reload button — and then if everything is fine then an okay and then it should vanish. If
//! not, then it should be red"). A part that restarts shows an orange "Restarting …" pill with the reload glyph
//! until it is back ("… OK", green, briefly) or gives up (red, persistent, with what to do, until fixed or clicked).
//! A part that dropped (a disconnected microphone) says so once and then rests as a small glyph at the notch until it
//! returns. One-off notices (the microphone switched to the built-in one) show briefly. Heimdall catching up on its own
//! work (`Busy`, 31 Oct: the archive locked for minutes and speech stalled with no sign) shows in calm white with what
//! it is doing, then rests as a token until it ends; it never turns anything red. Health is independent of
//! Listening: turning Listening off clears commands, not health.

pub mod agent_notice;
pub mod chat;
pub mod click_source;
pub mod drawing;
pub mod drop;
pub mod object;
pub mod reader;

/// The island window's title. ScreenCaptureKit filters exclude this process's window with this title
/// (`screen::EXCLUDED_WINDOW_TITLE`, kept equal by a test in crates/app), so the pill never appears in a capture.
pub const ISLAND_WINDOW_TITLE: &str = "Heimdall Command Island";

/// What a command does, for its icon and label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Screenshot,
    LookAtThis,
    Cancel,
    /// Not a command: Listening was turned on. The glyph waits while the models load, then "Listening".
    Listening,
    Other,
    /// Not a command: something the user dropped onto the notch (`drop.rs`); its result is what it took ("1 image").
    Drop(drop::DropKind),
}

impl CommandKind {
    /// SF Symbol.
    pub fn symbol(self) -> &'static str {
        match self {
            CommandKind::Screenshot => EYE,
            CommandKind::LookAtThis => EYE,
            CommandKind::Cancel => "stop.circle",
            CommandKind::Listening => HELMET,
            CommandKind::Other => "waveform",
            CommandKind::Drop(k) => k.symbol(),
        }
    }

    /// The icon of its result: a screenshot taken is the viewfinder whichever phrase asked for it, so one word
    /// ("Screenshot") always goes with one icon.
    pub fn result_symbol(self) -> &'static str {
        match self {
            CommandKind::LookAtThis => CommandKind::Screenshot.symbol(),
            other => other.symbol(),
        }
    }

    /// The command's name, for saying what failed among joined results ("Cancel: Agent is off").
    pub fn name(self) -> &'static str {
        match self {
            CommandKind::Screenshot | CommandKind::LookAtThis => "Screenshot",
            CommandKind::Cancel => "Cancel",
            CommandKind::Listening => "Listening",
            CommandKind::Other => "Command",
            CommandKind::Drop(_) => "Drop",
        }
    }

    /// The label once the command ran.
    pub fn done_label(self) -> &'static str {
        match self {
            CommandKind::Screenshot | CommandKind::LookAtThis => "Screenshot",
            CommandKind::Cancel => "Cancelled",
            CommandKind::Listening => "Listening",
            CommandKind::Other => "Done",
            CommandKind::Drop(_) => "Taken",
        }
    }
}

/// Not an SF Symbol: the island draws Heimdall's own helmet glyph for this name.
pub const HELMET: &str = "heimdall.helmet";
/// Not SF Symbols: the island's own animated glyphs. The eye is a screenshot and anything Heimdall looks at (the user:
/// "the eye for the screenshot"); the check snaps into place when something is back to normal.
pub const EYE: &str = "heimdall.eye";
pub const CHECK: &str = "heimdall.check";
/// The smile: something went right (a part back to normal), as an agent's success line.
pub const SMILE: &str = "heimdall.smile";

/// The part of Heimdall a health message is about: its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Heimdall,
    Microphone,
    Transcription,
    Commands,
    Screen,
    Agent,
    Archive,
    Speech,
    Settings,
    InferenceEngine,
}

impl Part {
    /// The part's icon (a notice, or a restart's subject).
    pub fn symbol(self) -> &'static str {
        match self {
            Part::Heimdall => HELMET,
            Part::Microphone => "mic",
            Part::Transcription => "waveform",
            Part::Commands => "command",
            Part::Screen => EYE,
            Part::Agent => "terminal",
            Part::Archive => "externaldrive",
            Part::Speech => "speaker.wave.2",
            Part::Settings => "gearshape",
            Part::InferenceEngine => "server.rack",
        }
    }

    /// The icon of the part when it is down (a drop or a problem).
    pub fn down_symbol(self) -> &'static str {
        match self {
            Part::Heimdall => "exclamationmark.triangle",
            Part::Microphone => "mic.slash",
            Part::Transcription => "waveform.slash",
            Part::Commands => "command",
            Part::Screen => "eye.slash",
            Part::Agent => "terminal",
            Part::Archive => "externaldrive.badge.exclamationmark",
            Part::Speech => "speaker.slash",
            Part::Settings => "gearshape",
            Part::InferenceEngine => "server.rack",
        }
    }

    pub const ALL: [Part; 10] = [
        Part::Heimdall,
        Part::Microphone,
        Part::Transcription,
        Part::Commands,
        Part::Screen,
        Part::Agent,
        Part::Archive,
        Part::Speech,
        Part::Settings,
        Part::InferenceEngine,
    ];
}

/// What clicking a red pill does. The UI handles `OpenSettings`, `OpenMenu` and `OpenAgent` itself; crates/app the
/// rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthControl {
    /// Nothing beyond collapsing the pill to its glyph.
    None,
    /// Relaunch Heimdall (settings, the Agent latch and the archive are kept).
    Relaunch,
    /// System Settings at this permission's privacy pane.
    OpenSettings(crate::model::Permission),
    /// Heimdall's menu (pick another microphone, turn something on).
    OpenMenu,
    /// The Agent window (a delivery to check).
    OpenAgent,
    /// The archive folder in Finder.
    ShowArchive,
}

/// crates/app → the island: Heimdall's health. `key` names one condition ("listen-worker", "mic", "agent").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthCue {
    /// Orange with the reload glyph ("Restarting transcription…") until `Recovered`, `Problem` or `Clear` for `key`.
    Restarting { key: String, part: Part, label: String },
    /// Green, briefly ("Transcription OK"), then gone; ends `key`.
    Recovered { key: String, part: Part, label: String },
    /// Red and persistent: what is wrong and what to do; the menu-bar helmet turns red until it ends or is fixed.
    Problem { key: String, part: Part, label: String, action: String, control: HealthControl },
    /// A part dropped ("AirPods Pro disconnected"): said once, then a small glyph at the notch until it ends.
    Drop { key: String, part: Part, label: String },
    /// One-off information ("AirPods Pro → MacBook Pro Microphone"), briefly.
    Notice { part: Part, label: String },
    /// Heimdall finishing its own work ("Catching up · 6,489 notes to save"): calm, in soft white, until `Recovered`
    /// or `Clear` for `key` (which end it silently). The same key again with new words updates them in place, with no
    /// new arrival (the app resends it every few seconds with a falling count). Never alerting.
    Busy { key: String, part: Part, label: String },
    /// `key` ended without a message.
    Clear { key: String },
}

impl HealthCue {
    /// The cue with its words in the configured names (`identity::named`): the app writes them for Heimdall and the user.
    pub fn named(self) -> Self {
        self.named_with(&crate::identity::current())
    }

    pub fn named_with(self, identity: &crate::identity::Identity) -> Self {
        let n = |s: String| identity.named(&s);
        match self {
            HealthCue::Restarting { key, part, label } => HealthCue::Restarting { key, part, label: n(label) },
            HealthCue::Recovered { key, part, label } => HealthCue::Recovered { key, part, label: n(label) },
            HealthCue::Problem { key, part, label, action, control } => HealthCue::Problem { key, part, label: n(label), action: n(action), control },
            HealthCue::Drop { key, part, label } => HealthCue::Drop { key, part, label: n(label) },
            HealthCue::Notice { part, label } => HealthCue::Notice { part, label: n(label) },
            HealthCue::Busy { key, part, label } => HealthCue::Busy { key, part, label: n(label) },
            HealthCue::Clear { key } => HealthCue::Clear { key },
        }
    }
}

/// The colour a health look carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthTone {
    /// Orange: restarting on its own.
    Restarting,
    /// Green: back.
    Recovered,
    /// Red: needs the user.
    Problem,
    /// Orange: a part is down and returns by itself.
    Drop,
    /// The accent colour: information.
    Notice,
    /// Soft white: Heimdall catching up on its own work (not broken, nothing for the user to do).
    Busy,
}

/// crates/app → the island. `id` ties a command's later outcome to its first detection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandCue {
    /// First detection; the decision is pending.
    Heard { id: u64, kind: CommandKind },
    /// Approved and executed. `id` is `None` for an outcome with no pending detection.
    Done { id: Option<u64>, kind: CommandKind, label: String },
    /// Approved (or due) but it could not run; `reason` in a few words ("Seeing is off").
    Failed { id: Option<u64>, kind: CommandKind, reason: String },
    /// Rejected, a false alarm, a quotation, or abandoned: the pending glyph leaves silently.
    Dropped { id: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Done,
    Failed,
}

/// What the pill shows now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    Hidden,
    /// The small glyph at the notch: the newest pending command's icon, and the count when there are several.
    Pending { kind: CommandKind, count: usize },
    /// Just heard: wider wings, full-strength icon.
    Heard { kind: CommandKind, count: usize },
    /// Expanded below the notch with a label.
    /// `count` of the same result: the numeral beside the label ("×2") when more than one.
    Result { kind: CommandKind, label: String, tone: Tone, count: usize },
    /// Health, expanded below the notch: `symbol` in the tone's colour, the label, and a second line (what to do).
    /// `pulse` marks its arrival (the outline pulse runs once).
    Status { symbol: &'static str, label: String, detail: Option<String>, tone: HealthTone, pulse: bool },
    /// Health at rest: a small glyph at the notch in the tone's colour while a part is down.
    Alert { symbol: &'static str, tone: HealthTone },
}

/// What the agent is doing while its turn runs ("whenever you are inferencing … some kind of
/// animation that … you're working", and "whenever you are compacting, there is actually an animation").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Thinking,
    Compacting,
}

impl Activity {
    /// What the agent's run state shows on the island: a turn at work thinks (Pi's or the relay's,
    /// docs/claude-relay.md), a compaction compacts, anything else rests.
    pub fn of(state: &crate::AgentRunState) -> Option<Activity> {
        match state {
            crate::AgentRunState::Working => Some(Activity::Thinking),
            crate::AgentRunState::Compacting | crate::AgentRunState::Stopping { compaction: true } => Some(Activity::Compacting),
            _ => None,
        }
    }
}

/// The drawing island's Send capsule: at rest, under the pointer, or held down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendState {
    Rest,
    Hover,
    Pressed,
}

/// An agent line's kind (icon and colour).
pub use crate::agent_notice::NoticeKind;

/// How long each moment stays, in seconds (the grow/shrink motion is part of it).
pub const HEARD_SECONDS: f64 = 1.1;
pub const DONE_SECONDS: f64 = 1.7;
pub const FAILED_SECONDS: f64 = 2.6;
/// A result on screen while a different one (or a restart or problem) waits stays this long after its last change
/// (readable, then the next), so a burst never lags far.
pub const QUEUED_DONE_SECONDS: f64 = 1.3;
pub const QUEUED_FAILED_SECONDS: f64 = 2.0;
/// The same result again counts on the one on screen as it arrives ("Screenshot ×2"): only the numeral changes.
/// A result reads alone at least `FIRST_COUNT_SECONDS` before its first count, and each count at least
/// `COUNT_SECONDS` before the next; arrivals in between fold into the next count, which shows the total.
pub const FIRST_COUNT_SECONDS: f64 = 0.6;
pub const COUNT_SECONDS: f64 = 0.5;
/// Different results waiting at most; a further one joins the newest waiting one ("Screenshot · Cancelled").
const MAX_WAITING_RESULTS: usize = 3;
/// The same result again within this long of the last one's end counts on: a second screenshot during the first's
/// retraction never reads as the first one bouncing back.
const RESULT_RUN_GAP: f64 = 1.0;
/// Health moments: "… OK", a drop's first showing, a notice; a restart's or problem's arrival pulse.
pub const RECOVERED_SECONDS: f64 = 1.7;
pub const DROP_SECONDS: f64 = 2.6;
pub const NOTICE_SECONDS: f64 = 3.2;
pub const ARRIVAL_SECONDS: f64 = 0.8;
/// A health moment is on screen at least this long before another one (about something else) replaces it; the newer
/// one waits its turn. One about the same condition replaces it at once.
pub const MIN_READ_SECONDS: f64 = 1.2;
/// The reload glyph of a restart, and the check of a recovery.
pub const RESTART_SYMBOL: &str = "arrow.clockwise";
pub const RECOVERED_SYMBOL: &str = SMILE;
/// Catching up: a stack whose layers light in turn (variable colour), notes going into storage.
pub const BUSY_SYMBOL: &str = "square.stack.3d.up.fill";
/// A bound on remembered pending commands, should an outcome ever be lost.
const MAX_PENDING: usize = 32;
/// A bound on health conditions held at once.
const MAX_HEALTH: usize = 32;
/// A bound on health moments waiting for the island.
const MAX_QUEUED: usize = 8;

#[derive(Clone, Debug, PartialEq)]
enum Condition {
    Restarting { label: String },
    Problem { label: String, action: String, control: HealthControl, acknowledged: bool },
    Drop { label: String },
    Busy { label: String },
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    key: String,
    part: Part,
    condition: Condition,
}

#[derive(Clone, Debug, PartialEq)]
enum Shown {
    Nothing,
    Heard { kind: CommandKind, until: f64 },
    /// A command result; its end depends on what waits behind it (`Island::shown_until`).
    Result(OnScreen),
    /// A health moment (`look` is a `Look::Status`); `key` is the condition it is about, `since` when it showed.
    Health { look: Look, key: Option<String>, since: f64, until: f64 },
}

/// A health moment waiting for the island: shown in turn once the command or the moment on screen ends.
#[derive(Clone, Debug, PartialEq)]
struct Queued {
    look: Look,
    key: Option<String>,
    seconds: f64,
    /// For the reports; `None` once it has been shown (a moment a command interrupted, coming back).
    track: Option<Track>,
    /// A restart or problem: it goes right after the result on screen, before waiting results and notices. Any
    /// moment that has waited `HEALTH_MAX_WAIT` becomes one, so a stream of results never starves a notice.
    urgent: bool,
    queued_at: f64,
}

/// The longest a health moment waits behind results before it goes next.
const HEALTH_MAX_WAIT: f64 = 6.0;
/// A waiting health moment shown with more behind it stays this long (readable), then the next.
const QUEUED_HEALTH_SECONDS: f64 = 1.6;

/// What the island did with a cue, for the diagnostics record (every unexpected behaviour logged).
/// Transitions only: one `Shown` per cue moment, one `Cut` per moment that was not readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IslandReport {
    /// The cue's moment is on screen: the island took it (state), or its first visible frame was drawn (the live
    /// window holds `Shown` until then). `waited_ms` runs from the cue's arrival.
    Shown { id: Option<u64>, phase: &'static str, kind: String, label: String, waited_ms: u64 },
    /// The cue left unreadable: never on screen (`shown_ms` 0), or on screen shorter than `minimum_ms`.
    Cut { id: Option<u64>, phase: &'static str, kind: String, label: String, shown_ms: u64, minimum_ms: u64, why: &'static str },
    /// A heard command's glyph left the island without a result: its verdict dropped it ("dropped"), Listening went
    /// off ("listening off"), or the pending bound evicted it. `shown_ms` runs from its arrival. Every heard cue ends
    /// in a result or one of these, so a report can name what was still visible (night review I1, 20 Oct 18:39:46).
    Left { id: Option<u64>, phase: &'static str, kind: String, shown_ms: u64, why: &'static str },
}

impl IslandReport {
    /// A `Shown` drawn `seconds` after the island took it: its first visible frame.
    pub fn drawn_after(self, seconds: f64) -> IslandReport {
        match self {
            IslandReport::Shown { id, phase, kind, label, waited_ms } => IslandReport::Shown { id, phase, kind, label, waited_ms: waited_ms + ms(seconds) },
            cut => cut,
        }
    }
    /// A `Shown` whose moment left before any frame showed it.
    pub fn never_drawn(self, why: &'static str) -> IslandReport {
        match self {
            IslandReport::Shown { id, phase, kind, label, .. } => IslandReport::Cut { id, phase, kind, label, shown_ms: 0, minimum_ms: 0, why },
            cut => cut,
        }
    }
    /// A command cue the island never received (the UI shows commands only while Listening).
    pub fn not_taken(cue: &CommandCue, why: &'static str) -> Option<IslandReport> {
        let (id, phase, kind, label) = match cue {
            CommandCue::Heard { id, kind } => (Some(*id), "heard", *kind, String::new()),
            CommandCue::Done { id, kind, label } => (*id, "done", *kind, label.clone()),
            CommandCue::Failed { id, kind, reason } => (*id, "failed", *kind, reason.clone()),
            CommandCue::Dropped { .. } => return None,
        };
        Some(IslandReport::Cut { id, phase, kind: format!("{kind:?}"), label, shown_ms: 0, minimum_ms: 0, why })
    }
}

/// Reports waiting to be taken; a bound for state nobody drains (tests, frame-strip runs).
const MAX_REPORTS: usize = 64;

fn ms(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

/// One cue as reports name it: `phase` "heard", "done", "failed" or "health".
#[derive(Clone, Debug, PartialEq)]
struct Track {
    id: Option<u64>,
    phase: &'static str,
    kind: String,
    label: String,
    arrived: f64,
}

impl Track {
    fn result(id: Option<u64>, kind: CommandKind, label: &str, tone: Tone, now: f64) -> Track {
        Track { id, phase: if tone == Tone::Failed { "failed" } else { "done" }, kind: format!("{kind:?}"), label: label.into(), arrived: now }
    }
    fn health(look: &Look, now: f64) -> Track {
        let (kind, label) = match look {
            Look::Status { tone, label, .. } => (format!("{tone:?}"), label.clone()),
            other => (format!("{other:?}"), String::new()),
        };
        Track { id: None, phase: "health", kind, label, arrived: now }
    }
    fn shown(&self, now: f64) -> IslandReport {
        IslandReport::Shown { id: self.id, phase: self.phase, kind: self.kind.clone(), label: self.label.clone(), waited_ms: ms(now - self.arrived) }
    }
    fn cut(&self, shown: f64, minimum: f64, why: &'static str) -> IslandReport {
        IslandReport::Cut { id: self.id, phase: self.phase, kind: self.kind.clone(), label: self.label.clone(), shown_ms: ms(shown), minimum_ms: ms(minimum), why }
    }
}

/// Joined results named in a label; the rest are counted ("+3 more"). A label never runs longer than this.
const MAX_PARTS: usize = 2;
const MAX_LABEL_CHARS: usize = 96;

/// Cues one result keeps for its reports; a longer burst still counts every one.
const MAX_CUES: usize = 32;

/// Adds `new` cues to a result's, up to `MAX_CUES`; the rest are reported at once as counted without their own report.
fn add_cues(cues: &mut Vec<Track>, new: Vec<Track>) -> Vec<IslandReport> {
    let mut out = vec![];
    for cue in new {
        if cues.len() < MAX_CUES {
            cues.push(cue);
        } else {
            out.push(cue.cut(0.0, 0.0, "counted in a burst, not reported on its own"));
        }
    }
    out
}

/// A command result for the island: on screen or waiting; `count` of the same result (one cue each).
#[derive(Clone, Debug, PartialEq)]
struct Waiting {
    kind: CommandKind,
    label: String,
    tone: Tone,
    count: usize,
    /// Its full time on screen after its last change when nothing waits behind it.
    seconds: f64,
    /// The cues it shows, oldest first.
    cues: Vec<Track>,
    /// Different results joined because too many waited: each part's kind, label, tone and count. `label` then
    /// says them all ("Screenshot ×2 · Cancel: Agent is off").
    parts: Vec<(CommandKind, String, Tone, usize)>,
    /// Joined results beyond the parts named ("+3 more").
    more: usize,
}

impl Waiting {
    fn new(id: Option<u64>, kind: CommandKind, label: String, tone: Tone, seconds: f64, now: f64) -> Waiting {
        let cues = vec![Track::result(id, kind, &label, tone, now)];
        Waiting { kind, label, tone, count: 1, seconds, cues, parts: vec![], more: 0 }
    }
    fn same(&self, kind: CommandKind, label: &str, tone: Tone) -> bool {
        self.parts.is_empty() && self.kind.result_symbol() == kind.result_symbol() && self.label == label && self.tone == tone
    }
    /// Joins a different result (too many wait): the label names every part, a failure with what failed.
    fn join(&mut self, other: Waiting) -> Vec<IslandReport> {
        if self.parts.is_empty() {
            self.parts.push((self.kind, std::mem::take(&mut self.label), self.tone, self.count));
        }
        let room = self.parts.len() < MAX_PARTS;
        match self.parts.iter_mut().find(|(k, l, t, _)| k.result_symbol() == other.kind.result_symbol() && *l == other.label && *t == other.tone) {
            Some(part) => part.3 += other.count,
            None if room => self.parts.push((other.kind, other.label, other.tone, other.count)),
            // Beyond the parts named: counted. A failure is named in place of the newest success, so the label
            // always says what failed.
            None => match self.parts.iter().rposition(|p| p.2 == Tone::Done) {
                Some(i) if other.tone == Tone::Failed => {
                    self.more += self.parts[i].3;
                    self.parts[i] = (other.kind, other.label, other.tone, other.count);
                }
                _ => self.more += other.count,
            },
        }
        let part = |(k, l, t, n): &(CommandKind, String, Tone, usize)| {
            let text = if *t == Tone::Failed { format!("{}: {l}", k.name()) } else { l.clone() };
            result_text(&text, *n)
        };
        let mut named: Vec<String> = self.parts.iter().map(part).collect();
        if self.more > 0 {
            named.push(format!("+{} more", self.more));
        }
        self.label = named.join(" · ");
        // A reason is short; the bound keeps a joined label short whatever arrives.
        if self.label.chars().count() > MAX_LABEL_CHARS {
            self.label = self.label.chars().take(MAX_LABEL_CHARS - 1).collect::<String>() + "…";
        }
        if self.parts.iter().any(|p| p.2 == Tone::Failed) {
            self.tone = Tone::Failed;
        }
        self.count = 1;
        self.seconds = self.seconds.max(other.seconds);
        add_cues(&mut self.cues, other.cues)
    }
}

/// A result's text with its count: "Screenshot", "Screenshot ×2". The island draws the numeral on its own so a
/// count changes only the numeral.
pub fn result_text(label: &str, count: usize) -> String {
    if count > 1 { format!("{label} ×{count}") } else { label.into() }
}

/// The result on screen: `count` arrived, `showing` of them said; `changed` when the label last changed.
#[derive(Clone, Debug, PartialEq)]
struct OnScreen {
    r: Waiting,
    showing: usize,
    since: f64,
    changed: f64,
    /// It is not cut before this (when something began to wait behind it).
    floor: f64,
    /// Cues reported shown (the rest arrived after the label last changed).
    said: usize,
    /// When the newest count arrived: a count shows no earlier than its arrival.
    arrived: f64,
}

impl OnScreen {
    /// When the next count shows, if more arrived than are said.
    fn count_due(&self) -> Option<f64> {
        (self.r.count > self.showing).then(|| self.changed + if self.showing <= 1 { FIRST_COUNT_SECONDS } else { COUNT_SECONDS })
    }
    fn readable(&self) -> f64 {
        if self.r.tone == Tone::Failed { QUEUED_FAILED_SECONDS } else { QUEUED_DONE_SECONDS }
    }
}

/// Commands and health: what is pending, the result or health moment on screen and what waits behind it, the
/// conditions held. `object::Island` owns one and asks it `on_screen` and `held`. Time is seconds on
/// any monotonic clock.
#[derive(Clone, Debug)]
pub struct Cues {
    pending: Vec<(u64, CommandKind)>,
    shown: Shown,
    /// Health conditions, oldest first; the newest of a kind is the one shown.
    health: Vec<Entry>,
    /// Health moments that arrived while a command or another moment held the island ("every problem
    /// logged and registered"; a notice must never vanish unseen).
    queued: std::collections::VecDeque<Queued>,
    /// Command results waiting behind the one on screen, oldest first; they go before waiting health moments.
    results: std::collections::VecDeque<Waiting>,
    /// The last result shown (its total) and when it ended: the next same result counts on from it.
    last_result: Option<(Waiting, f64)>,
    /// Heard cues not yet on screen (a result held the island), with their arrival.
    heard_unseen: Vec<(u64, f64)>,
    /// Every pending heard cue's arrival, for the report of its exit.
    heard_at: Vec<(u64, f64)>,
    reports: Vec<IslandReport>,
}

impl Cues {
    /// A command result: the same as the one on screen counts on it; a different one waits its turn, and the one on
    /// screen then keeps only a readable time.
    fn result(&mut self, id: Option<u64>, kind: CommandKind, label: String, tone: Tone, seconds: f64, now: f64) {
        let w = Waiting::new(id, kind, label, tone, seconds, now);
        // Once a restart or problem waits, the result on screen stops counting on (each count would extend it); the
        // same result queues behind the health moment and counts there.
        let urgent = self.queued.iter().any(|q| q.urgent);
        let waited = self.others_wait();
        let Shown::Result(on) = &mut self.shown else {
            self.requeue_health(now);
            self.shown = Shown::Nothing;
            return self.start_result(w, now);
        };
        let overflow = if on.r.same(kind, &w.label, tone) && !urgent {
            on.r.count += 1;
            on.arrived = now;
            add_cues(&mut on.r.cues, w.cues)
        } else {
            // Its readable time runs from when something first waited behind it, not from each later arrival.
            if !waited {
                on.floor = now;
            }
            if let Some(same) = self.results.iter_mut().find(|q| q.same(kind, &w.label, tone)) {
                same.count += 1;
                add_cues(&mut same.cues, w.cues)
            } else if self.results.len() < MAX_WAITING_RESULTS {
                self.results.push_back(w);
                vec![]
            } else {
                self.results.back_mut().expect("full").join(w)
            }
        };
        overflow.into_iter().for_each(|r| self.report(r));
        self.expire(now);
    }

    /// Puts `w` on screen (the island is free), saying its whole count; the same result as the last one, just ended,
    /// counts on from it.
    fn start_result(&mut self, mut w: Waiting, now: f64) {
        if let Some((last, end)) = &self.last_result {
            if last.same(w.kind, &w.label, w.tone) && now - end <= RESULT_RUN_GAP {
                w.count += last.count;
            }
        }
        let shown: Vec<_> = w.cues.iter().map(|c| c.shown(now)).collect();
        shown.into_iter().for_each(|r| self.report(r));
        self.shown = Shown::Result(OnScreen { showing: w.count, said: w.cues.len(), r: w, since: now, changed: now, floor: now, arrived: now });
    }

    /// The result on screen ends at `now` (or was replaced): the next same result may count on from it.
    fn end_result(&mut self, now: f64) {
        if let Shown::Result(on) = &self.shown {
            self.last_result = Some((Waiting { cues: vec![], ..on.r.clone() }, now));
        }
    }

    /// Something waits behind the result on screen: a different result, or a restart or problem arriving.
    fn others_wait(&self) -> bool {
        !self.results.is_empty() || self.queued.iter().any(|q| q.urgent)
    }

    /// The result on screen says its next count.
    fn show_count(&mut self, due: f64) {
        let Shown::Result(on) = &mut self.shown else { return };
        on.showing = on.r.count;
        on.changed = due.max(on.arrived);
        let due = on.changed;
        let shown: Vec<_> = on.r.cues[on.said..].iter().map(|c| c.shown(due)).collect();
        on.said = on.r.cues.len();
        shown.into_iter().for_each(|r| self.report(r));
    }

    fn report(&mut self, report: IslandReport) {
        if self.reports.len() == MAX_REPORTS {
            self.reports.remove(0);
        }
        self.reports.push(report);
    }

    /// What happened to cues since the last call, oldest first.
    pub fn take_reports(&mut self) -> Vec<IslandReport> {
        std::mem::take(&mut self.reports)
    }

    /// Heard cues whose glyph is on screen now are shown.
    fn observe(&mut self, now: f64) {
        if self.heard_unseen.is_empty() || !matches!(self.look(), Look::Heard { .. } | Look::Pending { .. }) {
            return;
        }
        let (seen, unseen): (Vec<_>, Vec<_>) = std::mem::take(&mut self.heard_unseen).into_iter().partition(|(id, _)| self.pending.iter().any(|(p, _)| p == id));
        self.heard_unseen = unseen;
        for (id, at) in seen {
            let kind = self.pending.iter().find(|(p, _)| *p == id).map_or(CommandKind::Other, |(_, k)| *k);
            let track = Track { id: Some(id), phase: "heard", kind: format!("{kind:?}"), label: String::new(), arrived: at };
            self.report(track.shown(now));
        }
    }


}

impl Default for Cues {
    fn default() -> Self {
        Cues {
            pending: Vec::new(),
            shown: Shown::Nothing,
            health: Vec::new(),
            queued: Default::default(),
            results: Default::default(),
            last_result: None,
            heard_unseen: Vec::new(),
            heard_at: Vec::new(),
            reports: Vec::new(),
        }
    }
}

fn status(symbol: &'static str, label: &str, detail: Option<&str>, tone: HealthTone, pulse: bool) -> Look {
    Look::Status { symbol, label: label.into(), detail: detail.map(Into::into), tone, pulse }
}

impl Cues {
    pub fn cue(&mut self, cue: CommandCue, now: f64) -> Look {
        self.expire(now);
        match cue {
            CommandCue::Heard { id, kind } => {
                if self.pending.iter().any(|(p, _)| *p == id) {
                    return self.look();
                }
                if self.pending.len() == MAX_PENDING {
                    let (old, _) = self.pending[0];
                    self.left(old, "pending bound", now);
                    self.pending.remove(0);
                }
                self.pending.push((id, kind));
                self.heard_at.push((id, now));
                self.heard_unseen.retain(|(p, _)| self.pending.iter().any(|(q, _)| q == p));
                self.heard_unseen.push((id, now));
                // A result on screen keeps its time; the glyph (with the new count) follows it. A health moment
                // gives way and waits its turn.
                if !matches!(self.shown, Shown::Result(_)) {
                    self.requeue_health(now);
                    let kind = self.pending.first().map_or(kind, |(_, k)| *k);
                    self.shown = Shown::Heard { kind, until: now + HEARD_SECONDS };
                }
            }
            CommandCue::Done { id, kind, label } => {
                self.forget(id);
                self.result(id, kind, label, Tone::Done, DONE_SECONDS, now);
            }
            CommandCue::Failed { id, kind, reason } => {
                self.forget(id);
                self.result(id, kind, reason, Tone::Failed, FAILED_SECONDS, now);
            }
            CommandCue::Dropped { id } => {
                self.left(id, "dropped", now);
                self.forget(Some(id));
                if self.pending.is_empty() && matches!(self.shown, Shown::Heard { .. }) {
                    // Whatever waited behind the heard glyph shows now.
                    self.shown = Shown::Nothing;
                    self.next_queued(now);
                }
            }
        }
        self.observe(now);
        self.look()
    }

    /// Ends a moment whose time is up; called at `deadline`.
    pub fn tick(&mut self, now: f64) -> Look {
        self.expire(now);
        self.observe(now);
        self.look()
    }

    /// When the look changes next on its own; `None` at rest (nothing showing, or only the pending glyph).
    pub fn deadline(&self) -> Option<f64> {
        self.shown_until()
    }

    /// When what is on screen changes next: a moment's end, or a result's next count.
    fn shown_until(&self) -> Option<f64> {
        match &self.shown {
            Shown::Nothing => None,
            Shown::Heard { until, .. } | Shown::Health { until, .. } => Some(*until),
            Shown::Result(on) => Some(on.count_due().unwrap_or_else(|| {
                let dwell = if self.others_wait() { on.readable() } else { on.r.seconds };
                (on.changed + dwell).max(on.floor)
            })),
        }
    }

    /// Listening off: no command pending or shown. Health stays: it is about Heimdall, not about Listening.
    pub fn clear(&mut self, now: f64) {
        self.expire(now);
        for (id, _) in self.pending.clone() {
            self.left(id, "listening off", now);
        }
        self.pending.clear();
        self.heard_at.clear();
        self.heard_unseen.clear();
        // Results cut short or never shown: Listening off is the user's, but a result he did not get to read is logged.
        if let Shown::Result(on) = &self.shown {
            let (minimum, shown) = (on.readable(), now - on.since);
            let said = on.said;
            let mut cut: Vec<_> = if shown < minimum { on.r.cues[..said].iter().map(|c| c.cut(shown, minimum, "listening off")).collect() } else { vec![] };
            cut.extend(on.r.cues[said..].iter().map(|c| c.cut(0.0, COUNT_SECONDS, "listening off before its count")));
            cut.into_iter().for_each(|r| self.report(r));
        }
        for w in std::mem::take(&mut self.results) {
            let minimum = if w.tone == Tone::Failed { QUEUED_FAILED_SECONDS } else { QUEUED_DONE_SECONDS };
            w.cues.iter().for_each(|c| self.report(c.cut(0.0, minimum, "listening off before its turn")));
        }
        self.last_result = None;
        if matches!(self.shown, Shown::Heard { .. } | Shown::Result(_)) {
            self.shown = Shown::Nothing;
            self.next_queued(now);
        }
    }

    /// A health cue. A restart or problem arrives with an outline pulse and then stays; a drop, a recovery and a
    /// notice are moments. A command result on screen keeps its time; health takes the island after it.
    pub fn health(&mut self, cue: HealthCue, now: f64) -> Look {
        self.expire(now);
        let moment = |s: &mut Self, look: Look, key: Option<String>, seconds: f64| s.moment(look, key, seconds, now);
        match cue {
            HealthCue::Restarting { key, part, label } => {
                let changed = self.set(&key, part, Condition::Restarting { label: label.clone() });
                if changed {
                    moment(self, status(RESTART_SYMBOL, &label, None, HealthTone::Restarting, true), Some(key), ARRIVAL_SECONDS);
                }
            }
            HealthCue::Problem { key, part, label, action, control } => {
                // The same problem again keeps the user's acknowledgement (no new red pill for a repeated report).
                let acknowledged = self.entry(&key).is_some_and(|e| {
                    matches!(&e.condition, Condition::Problem { label: l, action: a, acknowledged: true, .. } if *l == label && *a == action)
                });
                let changed = self.set(&key, part, Condition::Problem { label: label.clone(), action: action.clone(), control, acknowledged });
                if changed && !acknowledged {
                    moment(self, status(part.down_symbol(), &label, Some(&action), HealthTone::Problem, true), Some(key), ARRIVAL_SECONDS);
                }
            }
            HealthCue::Drop { key, part, label } => {
                if self.set(&key, part, Condition::Drop { label: label.clone() }) {
                    moment(self, status(part.down_symbol(), &label, None, HealthTone::Drop, true), Some(key), DROP_SECONDS);
                }
            }
            // Back: the restart (or drop) simply leaves; no success mark after it.
            HealthCue::Recovered { key, .. } => {
                self.remove(&key);
                self.end_health_moment(&key, now);
            }
            HealthCue::Notice { part, label } => {
                moment(self, status(part.symbol(), &label, None, HealthTone::Notice, true), None, NOTICE_SECONDS);
            }
            // Catching up: arrives once; the same key again only changes its words, where they are (the condition
            // keeps its place among the others, the moment on screen or waiting keeps its time).
            HealthCue::Busy { key, part, label } => {
                if let Some(e) = self.health.iter_mut().find(|e| e.key == key && matches!(e.condition, Condition::Busy { .. })) {
                    e.part = part;
                    e.condition = Condition::Busy { label: label.clone() };
                    self.relabel_busy(&key, &label);
                } else if self.set(&key, part, Condition::Busy { label: label.clone() }) {
                    moment(self, status(BUSY_SYMBOL, &label, None, HealthTone::Busy, true), Some(key), ARRIVAL_SECONDS);
                }
            }
            HealthCue::Clear { key } => {
                if self.remove(&key) {
                    self.end_health_moment(&key, now);
                }
            }
        }
        self.observe(now);
        self.look()
    }

    /// The user clicked the red pill: it collapses to its glyph (the menu-bar helmet stays red until the problem ends).
    /// Returns the clicked problem's key and control.
    pub fn acknowledge(&mut self, now: f64) -> Option<(String, HealthControl)> {
        // The problem on screen, which queued arrivals can make other than the newest. Resolve it before expiring:
        // an overdue moment is still what the user sees and clicked until the next redraw.
        let on_screen = self.displayed_problem().map(|e| e.key.clone());
        self.expire(now);
        let key = match on_screen {
            Some(key) => key,
            None => self.displayed_problem()?.key.clone(),
        };
        let entry = self.health.iter_mut().find(|e| e.key == key)?;
        let Condition::Problem { control, acknowledged, .. } = &mut entry.condition else { return None };
        *acknowledged = true;
        let clicked = (entry.key.clone(), *control);
        if matches!(&self.shown, Shown::Health { look: Look::Status { tone: HealthTone::Problem, .. }, .. }) {
            self.shown = Shown::Nothing;
        }
        // Its own arrival, still waiting behind a command, is moot now.
        let key = clicked.0.clone();
        self.queued.retain(|q| !(q.key.as_deref() == Some(key.as_str()) && matches!(q.look, Look::Status { tone: HealthTone::Problem, .. })));
        self.next_queued(now);
        self.observe(now);
        Some(clicked)
    }

    /// The red pill's control while it is on screen (unacknowledged and not covered by another moment).
    pub fn control(&self) -> Option<HealthControl> {
        match &self.displayed_problem()?.condition {
            Condition::Problem { control, .. } => Some(*control),
            _ => None,
        }
    }

    /// The unacknowledged problem whose red pill is on screen: the health moment's own condition while one shows
    /// (a queued arrival can make it other than the newest), else the one `look` selects at rest. `None` while
    /// anything else holds the island.
    fn displayed_problem(&self) -> Option<&Entry> {
        let unacknowledged = |e: &&Entry| matches!(e.condition, Condition::Problem { acknowledged: false, .. });
        match &self.shown {
            Shown::Health { look: Look::Status { tone: HealthTone::Problem, .. }, key: Some(key), .. } => self.entry(key).filter(unacknowledged),
            Shown::Nothing if matches!(self.look(), Look::Status { tone: HealthTone::Problem, .. }) => self.health.iter().rev().find(unacknowledged),
            _ => None,
        }
    }

    /// Something needs the user: the menu-bar helmet is red.
    pub fn alerting(&self) -> bool {
        self.health.iter().any(|e| matches!(e.condition, Condition::Problem { .. }))
    }

    pub fn look(&self) -> Look {
        let count = self.pending.len();
        match &self.shown {
            Shown::Heard { kind, .. } => return Look::Heard { kind: *kind, count },
            Shown::Result(on) => return Look::Result { kind: on.r.kind, label: on.r.label.clone(), tone: on.r.tone, count: on.showing },
            Shown::Health { look, .. } => return look.clone(),
            Shown::Nothing => {}
        }
        // At rest: a restart in progress, then an unacknowledged problem, then catching up, then the pending command
        // glyph, then the glyph of anything still down.
        let newest = |f: &dyn Fn(&Condition) -> bool| self.health.iter().rev().find(|e| f(&e.condition));
        if let Some(Entry { condition: Condition::Restarting { label }, .. }) = newest(&|c| matches!(c, Condition::Restarting { .. })) {
            return status(RESTART_SYMBOL, label, None, HealthTone::Restarting, false);
        }
        if let Some(Entry { part, condition: Condition::Problem { label, action, .. }, .. }) =
            newest(&|c| matches!(c, Condition::Problem { acknowledged: false, .. }))
        {
            return status(part.down_symbol(), label, Some(action), HealthTone::Problem, false);
        }
        if let Some(Entry { condition: Condition::Busy { label }, .. }) = newest(&|c| matches!(c, Condition::Busy { .. })) {
            return status(BUSY_SYMBOL, label, None, HealthTone::Busy, false);
        }
        // The glyph keeps the first pending command's icon while more arrive (the count says how many), so a
        // provisional guess heard in between never flashes a different icon.
        if let Some((_, kind)) = self.pending.first() {
            return Look::Pending { kind: *kind, count };
        }
        if let Some(e) = newest(&|c| matches!(c, Condition::Problem { .. })) {
            return Look::Alert { symbol: e.part.down_symbol(), tone: HealthTone::Problem };
        }
        if let Some(e) = newest(&|c| matches!(c, Condition::Drop { .. })) {
            return Look::Alert { symbol: e.part.down_symbol(), tone: HealthTone::Drop };
        }
        Look::Hidden
    }

    /// The moment on screen, if any: a heard command, a result, or a health message (`object::Island` composes it
    /// with everything else).
    pub fn on_screen(&self) -> Option<Look> {
        let count = self.pending.len();
        match &self.shown {
            Shown::Heard { kind, .. } => Some(Look::Heard { kind: *kind, count }),
            Shown::Result(on) => Some(Look::Result { kind: on.r.kind, label: on.r.label.clone(), tone: on.r.tone, count: on.showing }),
            Shown::Health { look, .. } => Some(look.clone()),
            Shown::Nothing => None,
        }
    }

    /// What holds when no moment shows: a restart in progress, an unacknowledged problem, catching up, the
    /// pending command glyph, the glyph of anything still down; else `Hidden`.
    pub fn held(&self) -> Look {
        let newest = |f: &dyn Fn(&Condition) -> bool| self.health.iter().rev().find(|e| f(&e.condition));
        if let Some(Entry { condition: Condition::Restarting { label }, .. }) = newest(&|c| matches!(c, Condition::Restarting { .. })) {
            return status(RESTART_SYMBOL, label, None, HealthTone::Restarting, false);
        }
        if let Some(Entry { part, condition: Condition::Problem { label, action, .. }, .. }) =
            newest(&|c| matches!(c, Condition::Problem { acknowledged: false, .. }))
        {
            return status(part.down_symbol(), label, Some(action), HealthTone::Problem, false);
        }
        if let Some(Entry { condition: Condition::Busy { label }, .. }) = newest(&|c| matches!(c, Condition::Busy { .. })) {
            return status(BUSY_SYMBOL, label, None, HealthTone::Busy, false);
        }
        if let Some((_, kind)) = self.pending.first() {
            return Look::Pending { kind: *kind, count: self.pending.len() };
        }
        if let Some(e) = newest(&|c| matches!(c, Condition::Problem { .. })) {
            return Look::Alert { symbol: e.part.down_symbol(), tone: HealthTone::Problem };
        }
        if let Some(e) = newest(&|c| matches!(c, Condition::Drop { .. })) {
            return Look::Alert { symbol: e.part.down_symbol(), tone: HealthTone::Drop };
        }
        Look::Hidden
    }

    /// The newest problem, acknowledged or not: its key, control, icon, words and fix (the fix button and a token's
    /// reopening show it).
    pub fn problem(&self) -> Option<(String, HealthControl, &'static str, String, String)> {
        self.health.iter().rev().find_map(|e| match &e.condition {
            Condition::Problem { label, action, control, .. } => Some((e.key.clone(), *control, e.part.down_symbol(), label.clone(), action.clone())),
            _ => None,
        })
    }

    /// The key of the catching-up condition `held` shows when it shows one (the newest), so the scene can time its
    /// read by the condition, not by its words (which change every few seconds).
    pub fn busy_key(&self) -> Option<&str> {
        self.health.iter().rev().find(|e| matches!(e.condition, Condition::Busy { .. })).map(|e| e.key.as_str())
    }

    /// Whether `key` is still catching up.
    pub fn is_busy(&self, key: &str) -> bool {
        self.entry(key).is_some_and(|e| matches!(e.condition, Condition::Busy { .. }))
    }

    /// New words for a catching-up moment on screen or waiting: changed in place, its time and arrival kept.
    fn relabel_busy(&mut self, key: &str, words: &str) {
        let ours = |look: &Look, k: &Option<String>| k.as_deref() == Some(key) && matches!(look, Look::Status { tone: HealthTone::Busy, .. });
        if let Shown::Health { look, key: k, .. } = &mut self.shown {
            if ours(look, k) {
                if let Look::Status { label, .. } = look {
                    *label = words.into();
                }
            }
        }
        for q in self.queued.iter_mut().filter(|q| ours(&q.look, &q.key)) {
            if let Look::Status { label, .. } = &mut q.look {
                *label = words.into();
            }
        }
    }

    fn entry(&self, key: &str) -> Option<&Entry> {
        self.health.iter().find(|e| e.key == key)
    }

    /// Records `condition` for `key` (newest last). Returns whether anything changed.
    fn set(&mut self, key: &str, part: Part, condition: Condition) -> bool {
        if let Some(i) = self.health.iter().position(|e| e.key == key) {
            if self.health[i].condition == condition && self.health[i].part == part {
                return false;
            }
            self.health.remove(i);
        }
        if self.health.len() == MAX_HEALTH {
            self.health.remove(0);
        }
        self.health.push(Entry { key: key.into(), part, condition });
        true
    }

    fn remove(&mut self, key: &str) -> bool {
        let before = self.health.len();
        self.health.retain(|e| e.key != key);
        self.health.len() != before
    }

    /// A health moment about `key`, a condition that just ended, gives way at once; so do its queued arrivals.
    fn end_health_moment(&mut self, key: &str, now: f64) {
        let stale = |look: &Look, k: &Option<String>| {
            k.as_deref() == Some(key) && matches!(look, Look::Status { tone: HealthTone::Restarting | HealthTone::Problem | HealthTone::Drop | HealthTone::Busy, .. })
        };
        let (gone, kept) = std::mem::take(&mut self.queued).into_iter().partition(|q| stale(&q.look, &q.key));
        self.queued = kept;
        self.cut_queued(gone, "condition ended before its turn");
        if let Shown::Health { look, key: k, since, .. } = &self.shown {
            if !stale(look, k) {
                return;
            }
            if now - since < MIN_READ_SECONDS {
                let report = Track::health(look, *since).cut(now - since, MIN_READ_SECONDS, "condition ended");
                self.report(report);
            }
            self.shown = Shown::Nothing;
            self.next_queued(now);
        }
    }

    /// Shows a health moment now, or queues it: a command on screen keeps the island, and a moment about another
    /// condition stays until it has been readable for `MIN_READ_SECONDS`. One about the same condition replaces it.
    fn moment(&mut self, look: Look, key: Option<String>, seconds: f64, now: f64) {
        let wait = match &self.shown {
            Shown::Heard { .. } | Shown::Result(_) => true,
            Shown::Health { key: k, since, .. } => !(key.is_some() && *k == key) && now - since < MIN_READ_SECONDS,
            Shown::Nothing => false,
        };
        if wait {
            // A newer arrival about the same condition replaces the one still waiting.
            if key.is_some() {
                let (gone, kept) = std::mem::take(&mut self.queued).into_iter().partition(|q| q.key == key);
                self.queued = kept;
                self.cut_queued(gone, "superseded while waiting");
            }
            if self.queued.len() == MAX_QUEUED {
                let gone: Vec<_> = self.queued.pop_front().into_iter().collect();
                self.cut_queued(gone, "health queue full");
            }
            let track = Some(Track::health(&look, now));
            let urgent = matches!(look, Look::Status { tone: HealthTone::Restarting | HealthTone::Problem, .. });
            let waited = self.others_wait();
            if let (true, false, Shown::Result(on)) = (urgent, waited, &mut self.shown) {
                on.floor = now;
            }
            self.queued.push_back(Queued { look, key, seconds, track, urgent, queued_at: now });
            self.expire(now);
        } else {
            // One about the same condition replaces the one on screen at once.
            if let Shown::Health { look: old, since, .. } = &self.shown {
                if now - since < MIN_READ_SECONDS {
                    let report = Track::health(old, *since).cut(now - since, MIN_READ_SECONDS, "replaced by the same condition");
                    self.report(report);
                }
            }
            let report = Track::health(&look, now).shown(now);
            self.report(report);
            self.shown = Shown::Health { look, key, since: now, until: now + seconds };
        }
    }

    fn cut_queued(&mut self, gone: impl IntoIterator<Item = Queued>, why: &'static str) {
        for q in gone {
            let track = q.track.unwrap_or_else(|| Track::health(&q.look, 0.0));
            self.report(track.cut(0.0, MIN_READ_SECONDS, why));
        }
    }

    /// A command takes the island: the health moment on screen goes back to the front of the queue, unless the user has
    /// had time to read it.
    fn requeue_health(&mut self, now: f64) {
        if let Shown::Health { look, key, since, until } = &self.shown {
            if now - since < MIN_READ_SECONDS {
                let seconds = (until - since).max(MIN_READ_SECONDS);
                let urgent = matches!(look, Look::Status { tone: HealthTone::Restarting | HealthTone::Problem, .. });
                self.queued.push_front(Queued { look: look.clone(), key: key.clone(), seconds, track: None, urgent, queued_at: now });
                let gone: Vec<_> = self.queued.drain(MAX_QUEUED.min(self.queued.len())..).collect();
                self.cut_queued(gone, "health queue full");
            }
            self.shown = Shown::Nothing;
        }
    }

    /// The island is free: the next waiting result shows, else the next waiting health moment.
    fn next_queued(&mut self, now: f64) {
        if !matches!(self.shown, Shown::Nothing) {
            return;
        }
        // A restart or problem (or a moment that waited too long, oldest first), then the results, then the rest.
        let urgent = self.queued.iter().enumerate().filter(|(_, q)| q.urgent).min_by(|a, b| a.1.queued_at.total_cmp(&b.1.queued_at)).map(|(i, _)| i);
        if urgent.is_none() {
            if let Some(w) = self.results.pop_front() {
                return self.start_result(w, now);
            }
        }
        if let Some(q) = self.queued.remove(urgent.unwrap_or(0)) {
            if let Some(track) = &q.track {
                let report = track.shown(now);
                self.report(report);
            }
            // With more waiting behind it, a moment stays readable, then gives way.
            let seconds = if self.queued.is_empty() && self.results.is_empty() { q.seconds } else { q.seconds.min(QUEUED_HEALTH_SECONDS) };
            self.shown = Shown::Health { look: q.look, key: q.key, since: now, until: now + seconds };
        }
    }

    fn forget(&mut self, id: Option<u64>) {
        if let Some(id) = id {
            self.pending.retain(|(p, _)| *p != id);
            self.heard_unseen.retain(|(p, _)| *p != id);
            self.heard_at.retain(|(p, _)| *p != id);
        }
    }

    /// Reports a pending heard cue leaving without a result (nothing for an id that is not pending).
    fn left(&mut self, id: u64, why: &'static str, now: f64) {
        let Some(&(_, kind)) = self.pending.iter().find(|(p, _)| *p == id) else { return };
        let since = self.heard_at.iter().find(|(p, _)| *p == id).map_or(now, |(_, at)| *at);
        self.report(IslandReport::Left { id: Some(id), phase: "heard", kind: format!("{kind:?}"), shown_ms: ms(now - since), why });
    }

    fn expire(&mut self, now: f64) {
        // A moment that has waited long enough goes next; the result on screen then keeps only a readable time.
        let starved = self.queued.iter().any(|q| !q.urgent && now - q.queued_at >= HEALTH_MAX_WAIT);
        if starved {
            if let (false, Shown::Result(on)) = (self.others_wait(), &mut self.shown) {
                on.floor = now;
            }
            self.queued.iter_mut().filter(|q| now - q.queued_at >= HEALTH_MAX_WAIT).for_each(|q| q.urgent = true);
        }
        // A count due first: the result keeps counting before anything ends it.
        if let Some(due) = match &self.shown { Shown::Result(on) => on.count_due().filter(|d| *d <= now), _ => None } {
            self.show_count(due);
            return self.expire(now);
        }
        if self.shown_until().is_some_and(|until| until <= now) {
            // The next waiting moment starts when this one ended (a late tick does not shorten it).
            let ended = self.shown_until().unwrap_or(now);
            self.end_result(ended);
            self.shown = Shown::Nothing;
            self.next_queued(ended);
            if self.deadline().is_some_and(|until| until <= now) {
                self.expire(now);
            }
        }
    }
}

#[cfg(test)]
mod tests;
