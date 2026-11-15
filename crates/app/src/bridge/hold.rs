//! The user's turn comes first (night pass V1). 19 Oct 18:28–18:51 Heimdall started five replies while the user was still
//! talking ("Got it.", "Noted.", "The desktop"); his speech cut each after a few words and four were never said.
//! Agent speech never starts over him: before its first sample (and before its synthesis, when he is already
//! talking as its line comes up) it looks at his microphone.
//!
//! Night pass 1022 (bug notes 21 Oct 18:41 and its 18:42 correction, which wins): a reply that would play while he
//! is talking is skipped: never said, never shown, never replayed later. The user: "The response should go back to you,
//! and you should know this was skipped, it was never shown to the user, so you can talk to me and actually answer. I
//! don't want to hear old messages." So:
//! - He is talking when a line would start (counted speech on a microphone now): the line is skipped (`Skipped`,
//!   `hold_skipped`). A skip latches: every later line of that reply is skipped too, even once he is quiet, so the
//!   second half of a reply never plays without its first.
//! - He stopped less than `QUIET_AFTER` ago (counted from the speech detector's end of speech, which on his
//!   microphones comes 300 ms after his last voice: 2.5 s of silence in all): the line waits out that pause (`Held`), and is skipped if he takes up his sentence again.
//!   Waiting speech is not "Heimdall speaking": no barge-in arms on it.
//! - The agent is told once that the lines never reached him: inside the wake that carries his words
//!   (`Tools::drop_held_speech`), or, when no wake of his carried it (the call gate, a "wait" verdict), by a wake of
//!   its own once his pause is over (`Tools::after_skip`, night 1022 review), and answers fresh. One such wake per
//!   skipped reply: an answer skipped again is told as context only, until a line reaches him or his words reach
//!   the agent.
//! - The latch is released when his words reach the agent as a wake, when the skipped reply's turn has ended and its
//!   lines are settled (`resume`), or when a new run starts (`Tools::new_run`: the earlier reply's queued lines are
//!   dropped with it). A skipped final reply leaves the island's pull-down (`UiUpdate::WithdrawReply`).
//! - The speaker's pre-buffer runs after the hold said a line may start; the hold is asked again right before the
//!   first sample (`speak::PlaybackStream::with_start_check`).
//!
//! Where a line waits out the pause: before its synthesis starts, and at the speaker when he starts while it is
//! being synthesized. Never inside synthesis: Jarvis answers his sentence checks only between packets, within 4 s,
//! and a synthesizer blocked on a held speaker missed them and moved the reply to the lighter voice. The held speaker
//! takes the synthesizer's audio into its own buffer instead (`speak::PlaybackStream::take_ready`).
//!
//! In a call (bug notes 2026-11-04T20-52-24, 2026-11-05T00-14-40: latency in calls between Heimdall and the user was
//! really bad): the user talks to the call again while Heimdall's answer to them is being made, so the skip
//! above turned most answers into a skip, a second model turn and a held line said minutes later. A line queued in a
//! call (`set_patient`: only his own answers reach the speaker then) waits for the gap instead: while he, or a guest on
//! the call audio (`other_voice`, bug note 2026-11-05T02-32-17: "speak only in a quiet gap"), is talking it is `Held`
//! for up to `PATIENCE`, then skipped as before.
//!
//! His answer waits only for him (bug note 2026-11-13T15-32-42; "When I'm not streaming, it works
//! perfectly. When I'm streaming, it's fucking broken"). At 15:31:57 he asked Heimdall to fact-check a guest (his ⌃⌘\
//! key); the answer waited 10 s for the guests to pause and was skipped. A line queued in a run his address by name
//! (or his key) woke (`set_addressed`, the queue's job id) waits in a call for his own pause, never for the guests':
//! their voices neither hold nor skip it. His talking still holds it for up to `PATIENCE`, and his pause after it
//! (`QUIET_AFTER`) still comes first. Unprompted speech in a call is unchanged.
//!
//! Speech that starts while Heimdall is audible does not hold or skip the next line: it is Heimdall's own voice on
//! the microphone, or the user talking over it, which the barge-in decides (pipeline/barge_in.rs). When the barge-in
//! stops Heimdall, whoever is talking is the user from then on (`claim`).
//!
//! Only confirmed the user speech skips a line (bug note 2026-11-07T04-31-47; the user, 04:31 on 7 Nov: "I wasn't talking, so
//! that was a bug"). At 04:30:56 a 1.2-second onset on the Shure that never held a word (game sound or the room) skipped
//! a reply he wanted to hear. On a microphone the speaker check can judge (`verify`: his primary microphone, with the
//! encoder and a voice print ready), an onset is `Unverified`: a line coming up then waits (`Held`, never `Skipped`)
//! while the barge-in's own window check (pipeline/barge_in.rs) scores the speech, at most `VERIFY_MAX` from its
//! first look. His voice makes it `the user` and the line is skipped as before; another voice or Heimdall's echo makes it
//! `NotUser` (a quiet or unsure window decides nothing) and the line plays (the barge-in still judges it once audible). Speech no check can judge
//! counts as his from its onset, as before. Each skip logs what triggered it: the source, how long it had been
//! speaking, and what made it the user's.
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Mutex},
    time::{Duration, Instant},
};

/// Heimdall's audio is reaching the speaker: from the first sample to the last (speak's speaking flag, set in
/// `attach_speech_level`). Speech that starts meanwhile is not the user's turn by itself.
pub(crate) static AUDIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `AUDIBLE` is maintained (the speaking hook is installed: the app with its island; not unit tests).
pub(crate) static HOOKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn audible() -> bool {
    AUDIBLE.load(Ordering::Acquire)
}
/// Quiet needed after the speech detector's end of speech before Heimdall starts talking. 2.5 s after his last voice
/// in all, as before 11 Nov, when the detector reported his pause at 1.5 s and this was 1 s; it now reports it at
/// 300 ms (Smart Turn ends his turns early, crates/listen/README.md). Only an answer ready that soon waits.
pub(crate) const QUIET_AFTER: Duration = Duration::from_millis(2_200);
/// What a skipped line reports as its failure (the speech queue's `Failed`): it is neither said nor shown, the agent
/// is told, and speech is not "unavailable".
pub(crate) fn hold_skipped() -> String {
    format!("{} was talking when this line would have played", agent::identity::user_start())
}
/// How often waiting speech looks again (only while it waits; nothing polls otherwise).
const POLL: Duration = Duration::from_millis(40);
/// How often a speaker waiting for a line's first audio, with the user quiet, looks at him again (the audio itself wakes
/// it at once).
const FIRST_POLL: Duration = Duration::from_millis(10);
/// How long a dropped line waits for the queue interrupt that follows `drop_held` before it gives up by itself: past
/// his pause (`QUIET_AFTER`), so a pause that ends first never lets it play.
const DROP_GRACE: Duration = Duration::from_millis(QUIET_AFTER.as_millis() as u64 + 1_000);
/// How long a line queued in a call waits for a gap while the user or a guest talks before it is skipped (the retry path,
/// one more model turn). The user talks to the call in turns of a few seconds; ten is past most of them.
pub(crate) const PATIENCE: Duration = Duration::from_secs(10);
/// How long a line waits for the speaker check on an unverified onset before it plays anyway (the first check answers
/// about 1.5 s after the onset, or after the line's first look at speech already going on).
pub(crate) const VERIFY_MAX: Duration = Duration::from_millis(3_000);
/// A guest's voice open longer than this proves nothing more (a detector that missed the end; speech_policy's
/// `OPEN_VOICE_MAX`).
const OTHER_VOICE_MAX: Duration = Duration::from_secs(60);

/// What waiting agent speech does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Floor {
    /// The user is quiet: go on.
    Free,
    /// The user stopped less than `QUIET_AFTER` ago: wait out the pause.
    Held,
    /// He is talking as the line would start, or an earlier line of this reply was skipped: it never plays and is
    /// never shown; the agent is told.
    Skipped,
    /// His words reached the agent (`drop_held`), or the line was cancelled while it waited: it never plays.
    Dropped,
}

/// Whose voice a microphone's open speech is, as far as the hold knows (bug note 2026-11-07T04-31-47).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Heard {
    /// The speaker check has not answered yet.
    Unverified,
    /// The user's, and why the hold believes it.
    Owner(&'static str),
    /// Another voice, Heimdall's echo, or too quiet to be speech.
    NotUser(&'static str),
}
/// Why an onset on a microphone no check judges counts as his.
const UNCHECKED: &str = "speech onset (no speaker check on this microphone)";
/// One microphone's open speech.
struct Talk {
    /// It counts as the user's turn (it did not start over Heimdall, or the barge-in claimed it).
    counted: bool,
    heard: Heard,
    since: Instant,
    /// When a waiting line first looked at it while it was unverified (`VERIFY_MAX` counts from here).
    looked: Option<Instant>,
}
impl Talk {
    fn user(&self) -> bool {
        self.counted && matches!(self.heard, Heard::Owner(_))
    }
}
#[derive(Default)]
struct State {
    /// Microphones in speech now: whether it counts as the user's turn, and whose voice it is.
    talking: HashMap<String, Talk>,
    /// Microphones whose speech the speaker check judges (`verify`).
    verified: std::collections::HashSet<String>,
    /// `VERIFY_MAX`, unless a test shortens it.
    verify_max: Option<Duration>,
    /// When the user's last counted speech ended.
    ended: Option<Instant>,
    /// Agent speech waiting for him (each waiting synthesizer or speaker counts once).
    waiting: usize,
    /// Bumped by `drop_held`: lines waiting then never play.
    dropped: u64,
    /// A line was skipped because he was talking: the rest of that reply is skipped too, until `resume`.
    skipping: bool,
    /// `QUIET_AFTER`, unless a test lengthens it.
    quiet: Option<Duration>,
    /// Lines queued now are in a call: they wait for a gap while anyone talks, up to `PATIENCE` (`set_patient`).
    patient: bool,
    /// Call sources with a guest's voice open, and since when (`other_voice`).
    others: HashMap<String, Instant>,
    /// `PATIENCE`, unless a test shortens it.
    patience: Option<Duration>,
    /// Queued speech jobs the user asked for by name (or his key), not settled yet (`set_addressed`): in a call they wait
    /// for his pause only, not the guests'.
    addressed: std::collections::HashSet<u64>,
    /// Of those, the jobs the agent marked to say now (`set_urgent`, the `speak` tool's `now`): they never wait for the
    /// guests. The others wait for a gap in the guests' talk, `PATIENCE` at most, then play over it (night 1115).
    urgent: std::collections::HashSet<u64>,
    /// When his answer first waited for the guests' gap: one `PATIENCE` for the whole answer, across synthesis,
    /// output and every line, not one per waiter (night 1115 review S1). Cleared when no answer of his is queued.
    asked_since: Option<Instant>,
}
impl State {
    /// Confirmed the user speech on a microphone now.
    fn talking(&self) -> bool {
        self.talking.values().any(Talk::user)
    }
    /// Counted speech waiting for the speaker check, within `VERIFY_MAX` of the first look at it.
    fn unverified(&self, now: Instant) -> bool {
        let max = self.verify_max.unwrap_or(VERIFY_MAX);
        self.talking.values().any(|t| t.counted && t.heard == Heard::Unverified && t.looked.is_none_or(|at| now.saturating_duration_since(at) < max))
    }
    /// What made the hold count the user as talking, for its log line.
    fn trigger(&self, now: Instant) -> String {
        self.talking.iter().filter(|(_, t)| t.user()).map(|(source, t)| {
            let why = match t.heard { Heard::Owner(why) => why, _ => "" };
            format!("{source}, speaking for {:.1} s, {why}", now.saturating_duration_since(t.since).as_secs_f64())
        }).collect::<Vec<_>>().join("; ")
    }
    fn holds(&self, now: Instant) -> bool {
        self.talking() || self.unverified(now) || self.ended.is_some_and(|at| now.saturating_duration_since(at) < self.quiet.unwrap_or(QUIET_AFTER))
    }
    /// A guest is talking on the call audio.
    fn others_talking(&self, now: Instant) -> bool {
        self.others.values().any(|from| now.saturating_duration_since(*from) <= OTHER_VOICE_MAX)
    }
}
#[derive(Default)]
pub(crate) struct OwnerVoice {
    state: Mutex<State>,
}
impl OwnerVoice {
    /// A microphone's speech started (`on`) or ended. `over_heimdall`: Heimdall was speaking when it started.
    pub(crate) fn set(&self, source: &str, on: bool, over_heimdall: bool) {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        if on {
            // The worker thread and the pipeline both report an onset; the first report decides, unless a later one
            // finds Heimdall quiet.
            let heard = if state.verified.contains(source) { Heard::Unverified } else { Heard::Owner(UNCHECKED) };
            let talk = state.talking.entry(source.into()).or_insert(Talk { counted: false, heard, since: now, looked: None });
            talk.counted |= !over_heimdall;
        } else if let Some(talk) = state.talking.remove(source) {
            // Speech that was not his voice starts no pause; unverified speech does (it may have been him).
            if talk.counted && !matches!(talk.heard, Heard::NotUser(_)) {
                state.ended = Some(now);
            }
        }
    }
    /// Whether the speaker check judges speech on `source` (the pipeline says so at each onset): its onsets wait for
    /// the check instead of counting as his at once.
    pub(crate) fn verify(&self, source: &str, on: bool) {
        let mut state = self.state.lock().unwrap();
        if on { state.verified.insert(source.into()); } else { state.verified.remove(source); }
        // The worker thread may have reported this onset a moment before the pipeline said whether it is checked.
        if let Some(talk) = state.talking.get_mut(source) {
            match talk.heard {
                Heard::Owner(UNCHECKED) if on => talk.heard = Heard::Unverified,
                Heard::Unverified if !on => talk.heard = Heard::Owner(UNCHECKED),
                _ => {}
            }
        }
    }
    /// The speaker check's verdict on the open speech on `source`: `Some(true)` his voice, `Some(false)` not (another
    /// voice, Heimdall's echo, too quiet), `None` unsure (it stays as it was). Speech confirmed as his stays his.
    pub(crate) fn heard(&self, source: &str, owner: Option<bool>, why: &'static str) {
        let mut state = self.state.lock().unwrap();
        let waiting = state.waiting;
        let Some(talk) = state.talking.get_mut(source) else { return };
        match (owner, talk.heard) {
            (Some(true), Heard::Unverified | Heard::NotUser(_)) => talk.heard = Heard::Owner(why),
            (Some(false), Heard::Unverified) => {
                talk.heard = Heard::NotUser(why);
                if waiting > 0 && talk.counted {
                    eprintln!("Hold: speech on {source} is not User's ({why}); the reply waiting for it plays");
                }
            }
            _ => {}
        }
    }
    /// The open speech on `source` waits for the speaker check (the barge-in's checks run for it while agent speech
    /// is queued).
    pub(crate) fn unverified(&self, source: &str) -> bool {
        self.state.lock().unwrap().talking.get(source).is_some_and(|t| t.counted && t.heard == Heard::Unverified)
    }
    #[cfg(test)]
    pub(crate) fn set_verify_max(&self, max: Duration) {
        self.state.lock().unwrap().verify_max = Some(max);
    }
    /// A guest's voice on call source `source` opened (`on`) or ended (bridge/speech_policy.rs `set_call_voice_hook`).
    pub(crate) fn other_voice(&self, source: &str, on: bool) {
        let mut state = self.state.lock().unwrap();
        if on { state.others.insert(source.into(), Instant::now()); } else { state.others.remove(source); }
    }
    /// Lines queued from now are in a call (`true`): they wait for a gap while the user or a guest talks, up to
    /// `PATIENCE`, instead of being skipped at once (`Tools::speak` sets it for every line it queues).
    pub(crate) fn set_patient(&self, patient: bool) {
        self.state.lock().unwrap().patient = patient;
    }
    /// Speech job `job` was queued in a run the user woke by addressing Heimdall by name or with his key (`on`), or it
    /// settled (played, skipped, cancelled). While such a job is queued, a line in a call waits for his pause only:
    /// the guests' voices do not hold it. In a call only his answers reach the queue (speech_policy.rs `speech_held`),
    /// so the line looking is one of them.
    pub(crate) fn set_addressed(&self, job: u64, on: bool) {
        let mut state = self.state.lock().unwrap();
        if on { state.addressed.insert(job); } else { state.addressed.remove(&job); state.urgent.remove(&job); }
    }
    /// Addressed job `job` is to be said now (the agent's `speak` with `now`: he asked in the middle of the talk, or it
    /// matters now): in a call it waits only for his own pause, never for the guests'. Cleared with `set_addressed`.
    pub(crate) fn set_urgent(&self, job: u64) {
        let mut state = self.state.lock().unwrap();
        state.addressed.insert(job);
        state.urgent.insert(job);
    }
    #[cfg(test)]
    pub(crate) fn set_patience(&self, patience: Duration) {
        self.state.lock().unwrap().patience = Some(patience);
    }
    /// The barge-in stopped Heimdall for this speech: it is the user's turn now.
    pub(crate) fn claim(&self) {
        let mut state = self.state.lock().unwrap();
        for talk in state.talking.values_mut() {
            talk.counted = true;
            talk.heard = Heard::Owner("the barge-in stopped Heimdall for his voice");
        }
    }
    /// The user is talking, or stopped less than `QUIET_AFTER` ago.
    pub(crate) fn holds(&self) -> bool {
        self.state.lock().unwrap().holds(Instant::now())
    }
    /// Agent speech is waiting for him.
    pub(crate) fn waiting(&self) -> bool {
        self.state.lock().unwrap().waiting > 0
    }
    /// Agent speech is not audible yet and will wait for him before its first sample: it is not Heimdall speaking, so
    /// no barge-in judges (and cuts) a line he has not heard while it is still being synthesized.
    pub(crate) fn defers(&self) -> bool {
        self.defers_given(HOOKED.load(Ordering::Acquire), audible())
    }
    fn defers_given(&self, hooked: bool, audible: bool) -> bool {
        self.waiting() || (hooked && !audible && self.holds())
    }
    /// One waiting line's view of the hold (`Waiter::floor`).
    pub(crate) fn waiter(&self) -> Waiter<'_> {
        Waiter { owner: self, counted: false, dropped: self.state.lock().unwrap().dropped, patient_from: None }
    }
    /// Blocks until the user's pause is over (`Free`), the line is skipped because he is talking (`Skipped`), or it is
    /// dropped or `cancelled` says to give up (Stop, Speaking off, a newer interrupt) (`Dropped`). Never `Held`.
    pub(crate) fn wait(&self, cancelled: &dyn Fn() -> bool) -> Floor {
        let mut waiter = self.waiter();
        loop {
            if cancelled() {
                return Floor::Dropped;
            }
            match waiter.floor() {
                Floor::Held => std::thread::sleep(POLL),
                floor => return floor,
            }
        }
    }
    /// A wake carrying the user's words is being delivered: the lines waiting for him now never play (the caller
    /// cancels them next). Whether any was waiting; a line already playing is the barge-in's.
    pub(crate) fn drop_held(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.waiting == 0 {
            return false;
        }
        state.dropped += 1;
        true
    }
    /// A line has been skipped since the last `resume`: later lines of the reply are skipped too.
    pub(crate) fn skipping(&self) -> bool {
        self.state.lock().unwrap().skipping
    }
    /// The skipped reply is over (his words reached the agent, or its turn ended and its lines settled): lines play
    /// again, by the same rules.
    pub(crate) fn resume(&self) {
        self.state.lock().unwrap().skipping = false;
    }
    #[cfg(test)]
    pub(crate) fn set_quiet(&self, quiet: Duration) {
        self.state.lock().unwrap().quiet = Some(quiet);
    }
}

/// A waiting line, counted in `OwnerVoice::waiting` while it is held (and dropped, until its interrupt arrives).
pub(crate) struct Waiter<'a> {
    owner: &'a OwnerVoice,
    counted: bool,
    /// `State::dropped` when it began waiting.
    dropped: u64,
    /// When this line began waiting for a gap in a call (`PATIENCE` counts from it).
    patient_from: Option<Instant>,
}
impl Waiter<'_> {
    /// Looks at the hold now. Free and the count are decided under one lock, so `drop_held` never drops a line that
    /// has started.
    pub(crate) fn floor(&mut self) -> Floor {
        look(self.owner, &mut self.counted, self.dropped, &mut self.patient_from)
    }
}
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.owner.state.lock().unwrap().waiting -= 1;
        }
    }
}
/// One waiting line's look at the hold (`Waiter::floor`, `StartWaiter::floor`).
fn look(owner: &OwnerVoice, counted: &mut bool, dropped: u64, patient_from: &mut Option<Instant>) -> Floor {
    let mut state = owner.state.lock().unwrap();
    let now = Instant::now();
    // In a call, anyone talking makes the line wait for the gap, for `PATIENCE` at most.
    // In a call his unverified onset holds the line for the gap too, as his speech did before the check (no
    // `VERIFY_MAX` there: `PATIENCE` bounds it).
    // Night 1115 ("you will wait for talking other people if it's a low priority, but if I will ask
    // you something in the core of the conversation, you should be able to … barge in … you decide when"): his answer
    // (`set_addressed`) waits for a gap in the guests' talk, `PATIENCE` at most, and then plays over it rather than
    // being skipped; one the agent marked to say now (`set_urgent`) never waits for the guests (bug note
    // 2026-11-13T15-32-42). His own talk holds both, as before.
    let asked = state.patient && !state.addressed.is_empty();
    let urgent = state.patient && !state.urgent.is_empty();
    let owner_busy = state.talking() || (state.patient && state.talking.values().any(|t| t.counted && t.heard == Heard::Unverified));
    let guests = !urgent && state.others_talking(now);
    let busy = owner_busy || guests;
    // Unverified speech this line now waits on: `VERIFY_MAX` counts from this first look.
    for talk in state.talking.values_mut() {
        if talk.counted && talk.heard == Heard::Unverified && talk.looked.is_none() { talk.looked = Some(now); }
    }
    let own_start = *patient_from.get_or_insert(now);
    if !asked { state.asked_since = None; }
    let start = if asked && !owner_busy && guests { *state.asked_since.get_or_insert(now) } else { own_start };
    let patient = state.patient && busy && now.saturating_duration_since(start) < state.patience.unwrap_or(PATIENCE);
    let floor = if state.dropped != dropped {
        Floor::Dropped
    } else if state.skipping {
        Floor::Skipped
    } else if patient {
        Floor::Held
    } else if asked && !owner_busy && guests {
        // His answer waited out the guests' patience: it plays over the call now, never vanishes.
        if state.holds(now) { Floor::Held } else {
            eprintln!("Hold: someone on the call kept talking for {} s while User's answer waited for a gap; it plays now", state.patience.unwrap_or(PATIENCE).as_secs());
            Floor::Free
        }
    } else if state.talking() || (state.patient && busy) {
        if state.patient && asked { eprintln!("Hold: User kept talking on the call for {} s while his answer waited for his pause; it is skipped unheard, shown on the island, and the agent is told ({})", state.patience.unwrap_or(PATIENCE).as_secs(), state.trigger(now)); }
        else if state.patient { eprintln!("Hold: someone on the call kept talking for {} s while a reply waited for a gap; it is skipped unheard, shown on the island, and the agent is told", state.patience.unwrap_or(PATIENCE).as_secs()); }
        else { eprintln!("Hold: User was talking when a reply would have played ({}); it is skipped unheard and unshown, and the agent is told", state.trigger(now)); }
        state.skipping = true;
        Floor::Skipped
    } else if state.holds(now) {
        Floor::Held
    } else {
        Floor::Free
    };
    let waits = matches!(floor, Floor::Held | Floor::Dropped);
    if waits != *counted {
        if waits { state.waiting += 1 } else { state.waiting -= 1 }
        *counted = waits;
    }
    floor
}
/// The same look, owned by the stream (`speak::PlaybackStream::with_start_check`): the output asks it right before
/// its first sample, after its pre-buffer (night 1022 review: up to 1.2 s passed after the hold said Free).
struct StartWaiter {
    owner: std::sync::Arc<OwnerVoice>,
    counted: bool,
    dropped: u64,
    /// Carried over from the line's first wait, so the pre-buffer never restarts `PATIENCE`.
    patient_from: Option<Instant>,
}
impl StartWaiter {
    fn may_start(&mut self) -> speak::MayStart {
        match look(&self.owner, &mut self.counted, self.dropped, &mut self.patient_from) {
            Floor::Free => speak::MayStart::Start,
            Floor::Held => speak::MayStart::Wait,
            Floor::Skipped => speak::MayStart::Skip(anyhow::anyhow!(hold_skipped())),
            Floor::Dropped => speak::MayStart::Drop,
        }
    }
}
impl Drop for StartWaiter {
    fn drop(&mut self) {
        if self.counted {
            self.owner.state.lock().unwrap().waiting -= 1;
        }
    }
}

/// Agent speech output that starts no sample while the user holds the floor. Meanwhile it takes the synthesizer's audio
/// into the stream's own buffer, so synthesis never waits on it.
pub(super) struct HeldOutput<O> {
    pub(super) inner: O,
    pub(super) owner: std::sync::Arc<OwnerVoice>,
}
impl<O: speak::AudioOutput> speak::AudioOutput for HeldOutput<O> {
    fn output(&mut self, audio: &speak::Audio, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
        match self.owner.wait(cancel) {
            Floor::Free => self.inner.output(audio, cancel),
            Floor::Skipped => Err(anyhow::anyhow!(hold_skipped())),
            Floor::Held | Floor::Dropped => Ok(()),
        }
    }
    fn output_stream(&mut self, mut chunks: speak::PlaybackStream, cancel: &dyn Fn() -> bool, signal: speak::Cancellation) -> anyhow::Result<()> {
        let stop = || cancel() || signal.cancelled();
        let mut waiter = self.owner.waiter();
        // The producer is still sending.
        let mut open = true;
        let mut dropped = None;
        // Until the first audio is here and the user is quiet. Dropping the stream unplayed ends its producer; the queue
        // reports the job cancelled (Stop, a drop) or failed (`hold_skipped`), unheard.
        loop {
            if stop() {
                return Ok(());
            }
            match waiter.floor() {
                Floor::Free if chunks.has_audio() || !open => break,
                Floor::Free => open = chunks.take_ready(FIRST_POLL),
                Floor::Held if open => open = chunks.take_ready(POLL),
                Floor::Held => std::thread::sleep(POLL),
                Floor::Skipped => return Err(anyhow::anyhow!(hold_skipped())),
                // The queue interrupt that cancels it follows at once; until then it neither plays nor blocks.
                Floor::Dropped => {
                    if dropped.get_or_insert_with(Instant::now).elapsed() > DROP_GRACE {
                        return Ok(());
                    }
                    if open { open = chunks.take_ready(POLL) } else { std::thread::sleep(POLL) }
                }
            }
        }
        let (dropped, patient_from) = (waiter.dropped, waiter.patient_from);
        drop(waiter);
        // Asked again right before the first sample, after the output's pre-buffer.
        let mut start = StartWaiter { owner: self.owner.clone(), counted: false, dropped, patient_from };
        self.inner.output_stream(chunks.with_start_check(move || start.may_start()), cancel, signal)
    }
}

/// Agent speech synthesis that starts only once the user's pause is over, and never when he is talking as its line comes
/// up (the line is skipped unsynthesized). Once started it never waits for him: the speaker does (`HeldOutput`).
pub(super) struct HeldSynth<S> {
    pub(super) inner: S,
    pub(super) owner: std::sync::Arc<OwnerVoice>,
}
impl<S> HeldSynth<S> {
    fn turn(&self, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
        match self.owner.wait(cancel) {
            Floor::Free => Ok(()),
            Floor::Skipped => Err(anyhow::anyhow!(hold_skipped())),
            Floor::Held | Floor::Dropped => anyhow::bail!("speech interrupted"),
        }
    }
}
impl<S: speak::Synthesizer> speak::Synthesizer for HeldSynth<S> {
    // voice (night 1022): the speed reaches playback's pre-buffer through every wrapper.
    fn pace(&self) -> f32 { self.inner.pace() }
    fn synthesize(&mut self, text: &str, lang: &str, cancel: &dyn Fn() -> bool) -> anyhow::Result<speak::Audio> {
        crate::latency::tts_started(); // Latency map (lab/latency).
        self.inner.synthesize(text, lang, cancel)
    }
    fn stream(&mut self, text: &str, lang: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&speak::Audio) -> anyhow::Result<()>) -> anyhow::Result<speak::Audio> {
        self.turn(cancel)?;
        crate::latency::tts_started(); // Latency map (lab/latency): synthesis starts after the reply hold.
        self.inner.stream(text, lang, cancel, emit)
    }
    fn stream_packets(&mut self, text: &str, lang: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(speak::SpeechPacket) -> anyhow::Result<()>) -> anyhow::Result<speak::Audio> {
        self.turn(cancel)?;
        crate::latency::tts_started(); // Latency map (lab/latency): synthesis starts after the reply hold.
        self.inner.stream_packets(text, lang, cancel, emit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use speak::AudioOutput;
    use std::sync::{atomic::{AtomicBool, AtomicU64}, mpsc, Arc};

    /// Records when the first sample reached the speaker.
    struct Speaker(Arc<Mutex<Option<Instant>>>);
    impl speak::AudioOutput for Speaker {
        fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> anyhow::Result<()> {
            self.0.lock().unwrap().get_or_insert_with(Instant::now);
            Ok(())
        }
    }
    fn stream() -> speak::PlaybackStream {
        let (tx, rx) = mpsc::channel();
        tx.send(speak::PlaybackChunk { audio: speak::Audio { samples: vec![0.1; 64], sample_rate: 44_100, first_audio_ms: 0., synthesis_ms: 0. }, generation: 0, label: None }).unwrap();
        speak::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0)))
    }
    fn play(owner: &Arc<OwnerVoice>, cancel: Arc<AtomicBool>) -> (std::thread::JoinHandle<()>, Arc<Mutex<Option<Instant>>>) {
        let (thread, heard) = try_play(owner, cancel);
        (std::thread::spawn(move || thread.join().unwrap().unwrap()), heard)
    }
    /// `play`, with the speaker's result (a line given up at the limit fails).
    fn try_play(owner: &Arc<OwnerVoice>, cancel: Arc<AtomicBool>) -> (std::thread::JoinHandle<anyhow::Result<()>>, Arc<Mutex<Option<Instant>>>) {
        let heard = Arc::new(Mutex::new(None));
        let mut output = HeldOutput { inner: Speaker(heard.clone()), owner: owner.clone() };
        let thread = std::thread::spawn(move || {
            let c = cancel.clone();
            output.output_stream(stream(), &|| c.load(Ordering::Acquire), speak::Cancellation::new(move || cancel.load(Ordering::Acquire)))
        });
        (thread, heard)
    }

    /// The user's pause in these tests (`OwnerVoice::set_quiet`), so they do not sleep out the real `QUIET_AFTER` (2.2 s)
    /// again and again. `DROP_GRACE` stays derived from the real one, so a dropped line's grace still outlasts it.
    const QUIET: Duration = Duration::from_millis(300);
    /// A hold whose pause is `QUIET`.
    fn quick() -> Arc<OwnerVoice> {
        let owner = Arc::new(OwnerVoice::default());
        owner.set_quiet(QUIET);
        owner
    }

    /// The user, in his pause (`set` on then off): the window in which a line waits instead of being skipped.
    fn pause(owner: &OwnerVoice) {
        owner.set("mic:1", true, false);
        owner.set("mic:1", false, false);
    }

    /// Bug note 21 Oct 18:42: a reply that would start while the user is talking is skipped, not held for later: it never
    /// reaches the speaker, fails with `hold_skipped` at once, and nothing waits.
    #[test]
    fn a_reply_that_would_start_while_owner_talks_is_skipped() {
        let owner = Arc::new(OwnerVoice::default());
        owner.set("mic:1", true, false);
        let began = Instant::now();
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        let error = thread.join().unwrap().expect_err("skipped, not played");
        assert_eq!(error.to_string(), hold_skipped());
        assert!(began.elapsed() < Duration::from_millis(200), "skipped at once, not held: {:?}", began.elapsed());
        assert!(heard.lock().unwrap().is_none(), "never over him");
        assert!(!owner.waiting() && owner.skipping());
        // Synthesis that has not started is skipped the same way (`HeldSynth`).
        assert_eq!(owner.wait(&|| false), Floor::Skipped);
    }
    /// Bug note 2026-11-07T04-31-47: on a microphone the speaker check judges, an onset alone never skips a reply. The
    /// line waits for the check: another voice (game sound, the room) lets it play while that speech goes on, his
    /// voice skips it, and with no answer it plays after `VERIFY_MAX`. Unverified speech that ends still leaves his
    /// pause; speech that was not his leaves none.
    #[test]
    fn an_onset_the_speaker_check_has_not_confirmed_never_skips_a_reply() {
        // Another voice: the waiting line plays, with the speech still open.
        let owner = Arc::new(OwnerVoice::default());
        owner.verify("mic:1", true);
        owner.set("mic:1", true, false);
        assert!(owner.unverified("mic:1") && owner.holds());
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(150));
        assert!(heard.lock().unwrap().is_none() && owner.waiting() && !owner.skipping(), "held for the check, not skipped");
        owner.heard("mic:1", Some(false), "another voice by speaker check");
        thread.join().unwrap().expect("played");
        assert!(heard.lock().unwrap().is_some() && !owner.skipping());
        owner.set("mic:1", false, false);
        assert!(!owner.holds(), "speech that was not his starts no pause");
        // His voice: the waiting line is skipped.
        let owner = Arc::new(OwnerVoice::default());
        owner.verify("mic:1", true);
        owner.set("mic:1", true, false);
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(100));
        owner.heard("mic:1", None, "");
        std::thread::sleep(Duration::from_millis(100));
        assert!(owner.waiting(), "an unsure window decides nothing");
        owner.heard("mic:1", Some(true), "his voice by speaker check");
        assert_eq!(thread.join().unwrap().expect_err("skipped").to_string(), hold_skipped());
        assert!(heard.lock().unwrap().is_none() && owner.skipping());
        owner.heard("mic:1", Some(false), "another voice by speaker check");
        assert!(owner.holds(), "confirmed speech stays his");
        // No answer: it plays after `VERIFY_MAX` from its first look.
        let owner = Arc::new(OwnerVoice::default());
        owner.set_verify_max(Duration::from_millis(300));
        owner.verify("mic:1", true);
        owner.set("mic:1", true, false);
        let began = Instant::now();
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        let at = heard.lock().unwrap().expect("played");
        assert!(at.duration_since(began) >= Duration::from_millis(300) && !owner.skipping());
        // Unverified speech that ends leaves his pause.
        owner.set("mic:1", false, false);
        assert!(owner.holds());
        // The pipeline says a moment after the worker's onset whether the check runs; no check: his at once.
        let owner = OwnerVoice::default();
        owner.set("mic:2", true, false);
        owner.verify("mic:2", true);
        assert!(owner.unverified("mic:2"));
        owner.verify("mic:2", false);
        assert!(!owner.unverified("mic:2") && owner.state.lock().unwrap().talking());
        // In a call his unverified onset holds the line for the gap, past `VERIFY_MAX`, as his speech did before.
        let owner = Arc::new(OwnerVoice::default());
        owner.set_verify_max(Duration::from_millis(100));
        owner.set_patient(true);
        owner.set_patience(Duration::from_millis(400));
        owner.verify("mic:1", true);
        owner.set("mic:1", true, false);
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(250));
        assert!(heard.lock().unwrap().is_none() && owner.waiting(), "waiting for the gap");
        assert_eq!(thread.join().unwrap().expect_err("skipped after the patience").to_string(), hold_skipped());
    }
    /// The skip latches: the reply's next line is skipped even once he is quiet (never the second half of a reply
    /// without its first), until `resume`; then lines play again.
    #[test]
    fn a_skip_covers_the_rest_of_the_reply_until_resumed() {
        let owner = quick();
        owner.set("mic:1", true, false);
        assert!(try_play(&owner, Arc::new(AtomicBool::new(false))).0.join().unwrap().is_err());
        owner.set("mic:1", false, false);
        std::thread::sleep(QUIET + POLL);
        assert!(!owner.holds());
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        assert_eq!(thread.join().unwrap().expect_err("still the skipped reply").to_string(), hold_skipped());
        assert!(heard.lock().unwrap().is_none());
        owner.resume();
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some(), "a new reply plays");
    }
    /// He has just stopped: the line waits out the pause (not Heimdall speaking meanwhile) and plays after it.
    #[test]
    fn a_reply_ready_in_his_pause_waits_for_the_end_of_it() {
        // Unless a test shortens it, his pause is `QUIET_AFTER` from the end of his speech.
        let real = OwnerVoice::default();
        pause(&real);
        let state = real.state.lock().unwrap();
        let end = state.ended.expect("his speech ended");
        assert!(state.holds(end + QUIET_AFTER - Duration::from_millis(1)) && !state.holds(end + QUIET_AFTER));
        drop(state);
        let owner = quick();
        pause(&owner);
        let ended = Instant::now();
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(150));
        assert!(heard.lock().unwrap().is_none(), "not inside his pause");
        assert!(owner.waiting(), "waiting speech is not Heimdall speaking");
        thread.join().unwrap();
        let started = heard.lock().unwrap().expect("played after the pause");
        assert!(started.duration_since(ended) >= QUIET, "a pause shorter than the turn end is not the end");
        assert!(started.duration_since(ended) < QUIET + Duration::from_millis(300));
        assert!(!owner.waiting() && !owner.skipping());
    }
    /// He takes up his sentence again inside the pause: the waiting line is skipped, never played afterwards.
    #[test]
    fn he_resumes_within_the_pause_and_the_waiting_reply_is_skipped() {
        let owner = quick();
        pause(&owner);
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(QUIET / 2);
        owner.set("mic:1", true, false);
        assert_eq!(thread.join().unwrap().expect_err("skipped").to_string(), hold_skipped());
        owner.set("mic:1", false, false);
        std::thread::sleep(QUIET + POLL);
        assert!(heard.lock().unwrap().is_none(), "never replayed later");
        assert!(!owner.waiting());
    }
    /// Bug notes 2026-11-04T20-52-24, 2026-11-05T00-14-40 (23:44:09 name wake, 23:44:54 skipped because he was
    /// talking to the call): in a call his answer waits for his pause instead of being skipped, and plays after it.
    #[test]
    fn in_a_call_his_answer_waits_for_his_pause_instead_of_being_skipped() {
        let owner = Arc::new(OwnerVoice::default());
        owner.set_quiet(Duration::from_millis(100));
        owner.set_patient(true);
        owner.set("mic:1", true, false);
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(300));
        assert!(heard.lock().unwrap().is_none() && owner.waiting() && !owner.skipping(), "waiting, not skipped, not Heimdall speaking");
        owner.set("mic:1", false, false);
        let ended = Instant::now();
        thread.join().unwrap();
        let at = heard.lock().unwrap().expect("played in the gap");
        assert!(at.duration_since(ended) >= Duration::from_millis(100), "after his pause");
        assert!(!owner.waiting() && !owner.skipping());
    }
    /// The wait is bounded: past `PATIENCE` of talk the line is skipped as before (the retry answers).
    #[test]
    fn in_a_call_a_line_waits_at_most_the_patience_then_is_skipped() {
        assert_eq!(PATIENCE, Duration::from_secs(10));
        let owner = Arc::new(OwnerVoice::default());
        owner.set_patient(true);
        owner.set_patience(Duration::from_millis(300));
        owner.set("mic:1", true, false);
        let began = Instant::now();
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        assert_eq!(thread.join().unwrap().expect_err("skipped").to_string(), hold_skipped());
        assert!(began.elapsed() >= Duration::from_millis(300), "waited the patience first: {:?}", began.elapsed());
        assert!(heard.lock().unwrap().is_none() && owner.skipping() && !owner.waiting());
    }
    /// Night 1115: his answer without the agent's `now` waits for a gap in the guests' talk; when
    /// they talk on past the patience it plays over them, never skipped (11:56: his answers vanished on a call).
    #[test]
    fn in_a_call_his_answer_waits_for_the_guests_gap_then_plays_over_them() {
        let owner = Arc::new(OwnerVoice::default());
        owner.set_patient(true);
        owner.set_quiet(Duration::from_millis(100));
        owner.set_patience(Duration::from_millis(400));
        owner.other_voice("process:discord", true);
        owner.set_addressed(9, true);
        let began = Instant::now();
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(200));
        assert!(heard.lock().unwrap().is_none() && owner.waiting(), "a low-priority answer waits for the guest's gap");
        thread.join().unwrap();
        let at = heard.lock().unwrap().expect("played over the guest after the patience");
        assert!(at.duration_since(began) >= Duration::from_millis(400), "{:?}", at.duration_since(began));
        assert!(!owner.skipping());
        // With a gap inside the patience it plays in the gap.
        owner.set_addressed(10, true);
        let began = Instant::now();
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(100));
        owner.other_voice("process:discord", false);
        thread.join().unwrap();
        let at = heard.lock().unwrap().expect("played in the gap");
        assert!(at.duration_since(began) < Duration::from_millis(400), "{:?}", at.duration_since(began));
    }
    /// Night 1115 review S1: the guests' patience is one wait for his whole answer. A second waiter for the same answer
    /// (synthesis, then output, or the next line) does not start the wait again while the guest keeps talking.
    #[test]
    fn in_a_call_his_answer_waits_for_the_guests_once_not_per_line() {
        let owner = Arc::new(OwnerVoice::default());
        owner.set_patient(true);
        owner.set_quiet(Duration::from_millis(100));
        owner.set_patience(Duration::from_millis(400));
        owner.other_voice("process:discord", true);
        owner.set_addressed(9, true);
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some(), "first line played over the guest after the patience");
        let began = Instant::now();
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        let at = heard.lock().unwrap().expect("the next line of the same answer plays");
        assert!(at.duration_since(began) < Duration::from_millis(300), "waited again: {:?}", at.duration_since(began));
    }
    /// Bug note 2026-11-05T02-32-17 (the user, 02:31: "speak only in a quiet gap ... guests included"): in a call a guest
    /// talking on the call audio holds the line too, until their voice ends; outside a call guests change nothing.
    #[test]
    fn in_a_call_a_guest_talking_holds_the_line_until_the_gap() {
        let owner = Arc::new(OwnerVoice::default());
        owner.other_voice("process:discord", true);
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some(), "not in a call: a guest voice holds nothing");
        owner.set_patient(true);
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(200));
        assert!(heard.lock().unwrap().is_none() && owner.waiting(), "the guest is talking");
        owner.other_voice("process:discord", false);
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some() && !owner.skipping(), "played in the gap");
    }
    /// Bug note 2026-11-13T15-32-42 and An answer the agent marked to say now (`set_urgent`) waits in
    /// a call for his own pause only; a guest talking on neither holds nor skips it. A line he did not ask for keeps
    /// the guests' gap, and is skipped after the patience as before.
    #[test]
    fn in_a_call_an_urgent_answer_waits_for_his_pause_not_the_guests() {
        let owner = Arc::new(OwnerVoice::default());
        owner.set_patient(true);
        owner.set_quiet(Duration::from_millis(150));
        owner.set_patience(Duration::from_millis(400));
        owner.other_voice("process:discord", true);
        owner.set_urgent(7);
        // He is asking; the guest talks over him and on.
        owner.set("mic:1", true, false);
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        std::thread::sleep(Duration::from_millis(250));
        assert!(heard.lock().unwrap().is_none() && owner.waiting(), "not over him");
        owner.set("mic:1", false, false);
        let paused = Instant::now();
        thread.join().unwrap();
        let at = heard.lock().unwrap().expect("played while the guest talks");
        assert!(at.duration_since(paused) >= Duration::from_millis(150), "after his pause");
        assert!(at.duration_since(paused) < Duration::from_millis(400), "{:?}", at.duration_since(paused));
        assert!(!owner.skipping());
        // His answer settled: the next line is not his answer and waits for the guest, then is skipped.
        owner.set_addressed(7, false);
        std::thread::sleep(Duration::from_millis(200));
        let (thread, heard) = try_play(&owner, Arc::new(AtomicBool::new(false)));
        assert_eq!(thread.join().unwrap().expect_err("skipped after the patience").to_string(), hold_skipped());
        assert!(heard.lock().unwrap().is_none());
        // Out of a call the mark changes nothing.
        owner.resume();
        owner.set_patient(false);
        owner.set_addressed(8, true);
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some());
    }
    #[test]
    fn quiet_microphone_plays_at_once_and_stop_while_waiting_plays_nothing() {
        let owner = Arc::new(OwnerVoice::default());
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some(), "nobody talking: no delay");
        pause(&owner);
        let cancel = Arc::new(AtomicBool::new(false));
        let (thread, heard) = play(&owner, cancel.clone());
        std::thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Release);
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_none(), "Stop while waiting: nothing plays");
        assert!(!owner.waiting());
    }
    #[test]
    fn speech_over_heimdall_holds_nothing_until_the_barge_in_claims_it() {
        let owner = Arc::new(OwnerVoice::default());
        // Heimdall's own voice on the microphone (or the user over it): the next line is not held or skipped.
        owner.set("mic:1", true, true);
        assert!(!owner.holds());
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some() && !owner.skipping());
        // A second report of the same onset after Heimdall went quiet counts it.
        owner.set("mic:1", true, false);
        assert!(owner.holds());
        let claimed = OwnerVoice::default();
        claimed.set("mic:2", true, true);
        assert!(!claimed.holds());
        claimed.claim();
        assert!(claimed.holds(), "the barge-in stopped Heimdall for him: his turn");
        claimed.set("mic:2", false, false);
        assert!(claimed.holds(), "and the pause after it");
        // In the app (speaking flag maintained): his turn, nothing audible yet, so a line still being synthesized is
        // not Heimdall speaking; once audible it is.
        assert!(claimed.defers_given(true, false) && !claimed.defers_given(true, true) && !claimed.defers_given(false, false));
        // An uncounted end never starts a pause.
        let fresh = OwnerVoice::default();
        fresh.set("mic:1", true, true);
        fresh.set("mic:1", false, false);
        assert!(!fresh.holds());
    }

    /// Night review 20 Oct (1): the held speaker takes the synthesizer's audio as it comes, so a synthesizer far ahead
    /// of the speech queue's bounded channel never waits on the user's pause; all of it plays, in order, after it.
    #[test]
    fn a_held_line_never_blocks_its_synthesizer() {
        struct Chunks(Arc<Mutex<Vec<f32>>>);
        impl speak::AudioOutput for Chunks {
            fn output(&mut self, audio: &speak::Audio, _: &dyn Fn() -> bool) -> anyhow::Result<()> {
                self.0.lock().unwrap().push(audio.samples[0]);
                Ok(())
            }
        }
        let owner = Arc::new(OwnerVoice::default());
        // Longer than the 400 ms the synthesizer may take below, so its audio is all in before the pause ends.
        owner.set_quiet(Duration::from_millis(800));
        pause(&owner);
        // The speech queue's channel: eight chunks.
        let (tx, rx) = mpsc::sync_channel(8);
        let synthesizer = std::thread::spawn(move || {
            let began = Instant::now();
            for n in 0..60 {
                let audio = speak::Audio { samples: vec![n as f32; 32], sample_rate: 44_100, first_audio_ms: 0., synthesis_ms: 0. };
                tx.send(speak::PlaybackChunk { audio, generation: 0, label: None }).unwrap();
            }
            began.elapsed()
        });
        let heard = Arc::new(Mutex::new(vec![]));
        let mut output = HeldOutput { inner: Chunks(heard.clone()), owner: owner.clone() };
        let speaker = std::thread::spawn(move || {
            output.output_stream(speak::PlaybackStream::new(rx, Arc::new(AtomicU64::new(0))), &|| false, speak::Cancellation::new(|| false))
        });
        let sent = synthesizer.join().unwrap();
        assert!(sent < Duration::from_millis(400), "the synthesizer waited on User: {sent:?}");
        assert!(heard.lock().unwrap().is_empty(), "nothing inside his pause");
        speaker.join().unwrap().unwrap();
        assert_eq!(*heard.lock().unwrap(), (0..60).map(|n| n as f32).collect::<Vec<_>>());
    }

    /// Night review 20 Oct (6): his words reached the agent, so the line waiting out his pause is dropped. It never
    /// plays, not even when the pause ends before the queue interrupt arrives; with nothing waiting, nothing is dropped.
    #[test]
    fn a_dropped_line_never_plays() {
        let owner = quick();
        assert!(!owner.drop_held(), "nothing waiting");
        pause(&owner);
        let cancel = Arc::new(AtomicBool::new(false));
        let (thread, heard) = play(&owner, cancel.clone());
        std::thread::sleep(Duration::from_millis(100));
        assert!(owner.drop_held());
        // His pause ends long before `DROP_GRACE`, with the queue interrupt still to come.
        std::thread::sleep(QUIET + Duration::from_millis(200));
        assert!(heard.lock().unwrap().is_none(), "dropped: never plays");
        assert!(owner.waiting(), "still not Heimdall speaking until it is cancelled");
        cancel.store(true, Ordering::Release);
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_none() && !owner.waiting());
        // A later line is not affected.
        let (thread, heard) = play(&owner, Arc::new(AtomicBool::new(false)));
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_some());
        assert!(!owner.drop_held(), "a line that is playing or done is not dropped");
    }

    /// Night 1022 review (2): the speaker's pre-buffer (up to 1.2 s) runs after the hold said the line may start.
    /// The user starting during it skips the line before its first sample; the user pausing during it makes it wait out his
    /// pause, counted as waiting.
    #[test]
    fn owner_starting_during_the_pre_buffer_skips_the_line_before_its_first_sample() {
        /// Gathers (blocked on `go`), then asks the start check as `CoreAudioOutput` does.
        struct Prebuffering { gathering: mpsc::Sender<()>, go: mpsc::Receiver<()>, heard: Arc<Mutex<Option<Instant>>> }
        impl speak::AudioOutput for Prebuffering {
            fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> anyhow::Result<()> { unreachable!() }
            fn output_stream(&mut self, mut chunks: speak::PlaybackStream, _: &dyn Fn() -> bool, _: speak::Cancellation) -> anyhow::Result<()> {
                self.gathering.send(()).unwrap();
                self.go.recv().unwrap();
                loop {
                    match chunks.may_start() {
                        speak::MayStart::Start => { self.heard.lock().unwrap().get_or_insert_with(Instant::now); return Ok(()) }
                        speak::MayStart::Wait => std::thread::sleep(Duration::from_millis(5)),
                        speak::MayStart::Skip(e) => return Err(e),
                        speak::MayStart::Drop => return Ok(()),
                    }
                }
            }
        }
        let run = |owner: &Arc<OwnerVoice>| {
            let (gathering, gathered) = mpsc::channel();
            let (go, wait) = mpsc::channel();
            let heard = Arc::new(Mutex::new(None));
            let mut output = HeldOutput { inner: Prebuffering { gathering, go: wait, heard: heard.clone() }, owner: owner.clone() };
            let thread = std::thread::spawn(move || output.output_stream(stream(), &|| false, speak::Cancellation::new(|| false)));
            gathered.recv_timeout(Duration::from_secs(2)).unwrap();
            (thread, go, heard)
        };
        // He starts talking during the pre-buffer: skipped, never heard, and the rest of the reply with it.
        let owner = Arc::new(OwnerVoice::default());
        let (thread, go, heard) = run(&owner);
        owner.set("mic:1", true, false);
        go.send(()).unwrap();
        let error = thread.join().unwrap().unwrap_err();
        assert_eq!(error.to_string(), hold_skipped());
        assert!(heard.lock().unwrap().is_none() && owner.skipping());
        // He speaks and stops during the pre-buffer: it waits out his pause, then plays.
        let owner = Arc::new(OwnerVoice::default());
        owner.set_quiet(Duration::from_millis(200));
        let (thread, go, heard) = run(&owner);
        pause(&owner);
        let ended = Instant::now();
        go.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert!(owner.waiting(), "waiting for him, so his words can still drop it");
        thread.join().unwrap().unwrap();
        let at = heard.lock().unwrap().expect("played after his pause");
        assert!(at.duration_since(ended) >= Duration::from_millis(200));
        assert!(!owner.waiting() && !owner.skipping());
    }

    // ---- with the real Tools ----

    struct Synth;
    impl speak::Synthesizer for Synth {
        fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> anyhow::Result<speak::Audio> {
            Ok(speak::Audio { samples: vec![0.1; 160], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
        }
    }
    /// Plays until interrupted, reporting its first sample.
    struct Blocking(std::sync::mpsc::Sender<()>);
    impl speak::AudioOutput for Blocking {
        fn output(&mut self, _: &speak::Audio, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
            let _ = self.0.send(());
            let deadline = Instant::now() + Duration::from_secs(3);
            while !cancel() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(2)); }
            Ok(())
        }
    }
    fn notices(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>) -> Vec<(String, String)> {
        let mut out = vec![];
        while let Ok(command) = rx.try_recv() {
            if let crate::bridge::Command::Notice { id, text, .. } = command { out.push((id, text)); }
        }
        out
    }

    /// The notices told so far, waiting up to `within` for at least `count` of them.
    fn notices_within(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>, count: usize, within: Duration) -> Vec<(String, String)> {
        let deadline = Instant::now() + within;
        let mut out = notices(rx);
        while out.len() < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            out.extend(notices(rx));
        }
        out
    }
    fn wakes(told: &(String, String)) -> bool {
        agent::notice::NoticeKind::classify(&told.0, &told.1).wakes()
    }

    /// Night pass V1: a line the barge-in cuts reaches the agent as context ("the user talked over you…", a context
    /// notice, no turn); a line Stop cuts does not.
    #[test]
    fn a_talked_over_line_is_told_to_the_agent_and_a_stopped_one_is_not() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let tools = crate::bridge::Tools::new(dir.path(), dir.path());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *tools.agent_commands.lock().unwrap() = Some(tx);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, Blocking(started));
        let permit = tools.agent_enabled.permit()?;
        agent::mcp::ToolHost::speak(&tools, "Your dashboard is open in the browser.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        // The user starts talking over it while it is audible (what `Tools::voice` reports with speak's speaking flag up);
        // the barge-in decides he is talking to Heimdall.
        tools.owner.set("mic:1", true, true);
        assert!(!tools.owner.holds(), "speech over Heimdall is the barge-in's to judge");
        tools.talked_over();
        tools.test_finish_speech()?;
        let told = notices(&mut rx);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("unheard-speech:cut:"));
        assert_eq!(told[0].1, "User talked over you; he heard only the start of: Your dashboard is open in the browser.");
        assert_eq!(agent::notice::NoticeKind::classify(&told[0].0, &told[0].1), agent::notice::NoticeKind::Context, "context, never a turn");
        assert!(tools.owner.holds(), "the barge-in made it his turn: the next line waits");
        tools.voice("mic:1", false);
        // Stop: nothing told.
        agent::mcp::ToolHost::speak(&tools, "Done.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.interrupt();
        tools.test_finish_speech()?;
        assert!(notices(&mut rx).is_empty());
        Ok(())
    }

    /// Agent speech waiting out the user's pause is not Heimdall speaking (no barge-in arms on it) and plays after it.
    #[test]
    fn waiting_speech_is_not_heimdall_speaking() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let tools = crate::bridge::Tools::new(dir.path(), dir.path());
        tools.owner.set_quiet(QUIET);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        tools.voice("mic:1", true);
        tools.voice("mic:1", false);
        tools.test_speak("Got it. Small jobs go to Haiku.")?;
        std::thread::sleep(Duration::from_millis(150));
        assert!(playing.try_recv().is_err(), "not inside his pause");
        assert!(tools.agent_speech_busy() && !tools.heimdall_speaking());
        playing.recv_timeout(Duration::from_secs(3))?;
        assert!(tools.heimdall_speaking(), "playing now");
        tools.interrupt();
        tools.test_finish_speech()?;
        Ok(())
    }

    /// The user starts talking while the line is still being synthesized: it is skipped at the speaker, and never plays
    /// after he is done.
    #[test]
    fn speech_that_starts_during_synthesis_skips_the_line() -> anyhow::Result<()> {
        struct Slow(std::sync::mpsc::Receiver<()>);
        impl speak::Synthesizer for Slow {
            fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> anyhow::Result<speak::Audio> {
                self.0.recv_timeout(Duration::from_secs(3))?;
                Ok(speak::Audio { samples: vec![0.1; 160], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
            }
        }
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(QUIET);
        let (ready, synthesized) = std::sync::mpsc::channel();
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(HeldSynth { inner: Slow(synthesized), owner: tools.owner.clone() }, HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        agent::mcp::ToolHost::speak(&tools, "The desktop widget is ready.", "en", &tools.agent_enabled.permit()?)?;
        std::thread::sleep(Duration::from_millis(50));
        tools.voice("mic:1", true);
        ready.send(())?;
        std::thread::sleep(Duration::from_millis(150));
        assert!(!tools.heimdall_speaking(), "not speaking: no barge-in arms");
        tools.voice("mic:1", false);
        tools.test_finish_speech()?;
        assert!(playing.recv_timeout(QUIET + Duration::from_millis(300)).is_err(), "never played, not even after him");
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("skipped-reply:") && wakes(&told[0]), "after his pause the agent is woken to answer fresh");
        assert!(told[0].1.ends_with("with his new words: The desktop widget is ready."), "{}", told[0].1);
        Ok(())
    }

    fn heimdall(tools: &crate::bridge::Tools) -> Vec<String> {
        tools.send_context.lock().unwrap().snapshot().turns.iter().filter(|t| t.speaker == "Heimdall").map(|t| t.text.clone()).collect()
    }
    fn tools_with_notices(dir: &std::path::Path) -> anyhow::Result<(crate::bridge::Tools, tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>)> {
        let tools = crate::bridge::Tools::new(dir, dir);
        tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(())))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *tools.agent_commands.lock().unwrap() = Some(tx);
        Ok((tools, rx))
    }

    /// Night review 20 Oct (minor): talking over a reply sent one note per cut line; now one note for all of them.
    #[test]
    fn the_lines_owner_talked_over_reach_the_agent_in_one_note() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, Blocking(started));
        let permit = tools.agent_enabled.permit()?;
        agent::mcp::ToolHost::speak(&tools, "Your dashboard is open.", "en", &permit)?;
        agent::mcp::ToolHost::speak(&tools, "The chart shows last week.", "en", &permit)?;
        agent::mcp::ToolHost::speak(&tools, "Ask if you want more.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.owner.set("mic:1", true, true);
        tools.talked_over();
        for _ in 0..3 { tools.test_finish_speech()?; }
        let told = notices(&mut rx);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("unheard-speech:cut:"));
        assert_eq!(told[0].1, "User talked over you; he heard only the start of: Your dashboard is open. He did not hear: The chart shows last week. Ask if you want more.");
        assert_eq!(heimdall(&tools), ["Your dashboard is open."], "only the line he heard the start of joins the conversation");
        Ok(())
    }

    /// Night review 20 Oct (1), with Jarvis's real line logic (`speak_line`, `Service::stream`) and a fake worker that
    /// gives each sentence check 0.3 s (the real one 4 s). The hold blocked Jarvis's packets inside synthesis while the user
    /// talked, his checks missed their deadline, and the reply moved to the lighter voice mid-sentence. Now: the user
    /// starting during synthesis, or already talking when the line comes up, never costs Jarvis a sentence, and nothing
    /// plays until his turn is over.
    #[test]
    fn jarvis_keeps_his_voice_when_owner_talks_through_synthesis() -> anyhow::Result<()> {
        const WORKER: &str = r#"import sys,json,base64,os,re,time,threading,queue
log=open(os.path.join(os.path.dirname(os.path.abspath(__file__)),'requests.log'),'a')
print(json.dumps({'event':'ready'}),flush=True)
data=base64.b64encode(b'\xcd\xcc\x4c\x3e'*256).decode()
def out(**k): print(json.dumps(k),flush=True)
requests=queue.Queue();verdicts=queue.Queue();cancel=threading.Event()
def reader():
 while True:
  line=sys.stdin.readline()
  if not line: os._exit(0)
  r=json.loads(line)
  if r['op']=='speak': cancel.clear();requests.put(r)
  elif r['op']=='verified': verdicts.put(r)
  elif r['op']=='cancel': cancel.set()
threading.Thread(target=reader,daemon=True).start()
while True:
 r=requests.get();i=r['id']
 log.write(r['text']+'\n');log.flush()
 parts=[p.strip() for p in re.findall(r'(?:[0-9]\.(?=[0-9])|[^.?!])+[.?!]*|[.?!]+',r['text']) if p.strip()]
 time.sleep(0.2)
 for k,p in enumerate(parts):
  for _ in range(3):
   time.sleep(0.04);out(event='pcm',id=i,rate=44100,pcm=data,sentence=k)
  if k>0:
   out(event='sentence_check',id=i,sentence=k,attempt=0)
   try: v=verdicts.get(timeout=0.3)
   except queue.Empty: v=None
   if not v or not v['accepted']:
    out(event='fallback',id=i,remaining_sentence=k,synthesis_ms=10);break
  if cancel.is_set(): out(event='cancelled',id=i);break
  out(event='sentence_done',id=i,sentence=k)
 else: out(event='done',id=i,synthesis_ms=10)
"#;
        struct LineVoices<'a>(&'a Mutex<Vec<String>>);
        impl speak::drama3::Voices for LineVoices<'_> {
            fn jarvis(&mut self, service: &speak::drama3::Service, written: &str, _: bool, cancel: &dyn Fn() -> bool,
                emit: &mut dyn FnMut(speak::SpeechPacket) -> anyhow::Result<()>) -> (anyhow::Result<speak::Audio>, bool) {
                let mut refused = false;
                let result = service.stream(written, cancel, &mut |packet| {
                    let result = emit(packet);
                    refused |= result.is_err();
                    result
                });
                (result, refused)
            }
            fn lighter(&mut self, sentence: &str, _: &dyn Fn() -> bool) -> anyhow::Result<speak::Audio> {
                self.0.lock().unwrap().push(sentence.into());
                Ok(speak::Audio { samples: vec![0.8; 64], sample_rate: 44_100, first_audio_ms: 0., synthesis_ms: 0. })
            }
        }
        struct Jarvis { service: Arc<speak::drama3::Service>, lighter: Arc<Mutex<Vec<String>>> }
        impl speak::Synthesizer for Jarvis {
            fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> anyhow::Result<speak::Audio> {
                anyhow::bail!("Jarvis streams")
            }
            fn stream_packets(&mut self, text: &str, _: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(speak::SpeechPacket) -> anyhow::Result<()>) -> anyhow::Result<speak::Audio> {
                let lighter = self.lighter.clone();
                speak::drama3::speak_line(&self.service, &mut LineVoices(&lighter), text, cancel, emit)
            }
        }
        /// Each chunk's arrival at the speaker and its first sample (Jarvis 0.2, the lighter voice 0.8).
        struct Heard(Arc<Mutex<Vec<(Instant, f32)>>>);
        impl speak::AudioOutput for Heard {
            fn output(&mut self, audio: &speak::Audio, _: &dyn Fn() -> bool) -> anyhow::Result<()> {
                self.0.lock().unwrap().push((Instant::now(), audio.samples[0]));
                Ok(())
            }
        }
        let assets = tempfile::tempdir()?;
        std::fs::create_dir_all(assets.path().join("runtime/bin"))?;
        std::os::unix::fs::symlink("/usr/bin/python3", assets.path().join("runtime/bin/python3"))?;
        std::fs::write(assets.path().join("sidecar.py"), WORKER)?;
        let requests = || std::fs::read_to_string(assets.path().join("requests.log")).unwrap_or_default().lines().count();
        let service = speak::drama3::Service::new(assets.path().into());
        service.set_verifier(Some(Arc::new(|_, _, _| Ok(serde_json::json!({"accepted": true})))));
        service.configure(true);
        anyhow::ensure!(service.wait_ready(Duration::from_secs(10), &|| false), "fake Jarvis did not start: {:?}", service.events());
        let dir = tempfile::tempdir()?;
        let tools = crate::bridge::Tools::new(dir.path(), dir.path());
        // His pause, three sentence deadlines long: packets held inside synthesis for it would miss their checks.
        let quiet = Duration::from_millis(1_000);
        tools.owner.set_quiet(quiet);
        let (lighter, heard) = (Arc::new(Mutex::new(Vec::<String>::new())), Arc::new(Mutex::new(Vec::new())));
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Jarvis { service: service.clone(), lighter: lighter.clone() }, owner: tools.owner.clone() },
            HeldOutput { inner: Heard(heard.clone()), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        let line = "One is fine. Two is fine. Three is fine.";
        let arrived = || {
            let began = Instant::now();
            while heard.lock().unwrap().is_empty() && began.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(20)); }
        };
        let check = |ended: Instant| {
            let heard = std::mem::take(&mut *heard.lock().unwrap());
            assert_eq!(heard.len(), 9, "every chunk of every sentence: {heard:?}");
            assert!(heard.iter().all(|(_, voice)| *voice > 0.19 && *voice < 0.21), "Jarvis throughout: {heard:?}");
            let first = heard[0].0.duration_since(ended);
            assert!(first + Duration::from_millis(20) >= quiet, "played {first:?} after his turn: before it was over");
            assert!(lighter.lock().unwrap().is_empty(), "lighter voice used: {:?}", lighter.lock().unwrap());
            let switches: Vec<_> = service.events().into_iter().filter(|e| e["event"] == "voice_switch").collect();
            assert!(switches.is_empty(), "{switches:?}");
        };
        // He finishes while the line is being synthesized, before its first audio (the worker waits 0.2 s after the
        // request). He starts only once Jarvis has the request: a fixed wait here let a loaded machine reach his onset
        // before the line had passed the hold, which then skipped it (the 13 Nov flake: nothing heard).
        agent::mcp::ToolHost::speak(&tools, line, "en", &permit)?;
        let began = Instant::now();
        while requests() == 0 && began.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(2)); }
        anyhow::ensure!(requests() == 1, "Jarvis was never asked");
        tools.voice("mic:1", true);
        tools.voice("mic:1", false);
        let ended = Instant::now();
        std::thread::sleep(Duration::from_millis(600));
        assert!(heard.lock().unwrap().is_empty(), "never inside his pause");
        arrived();
        tools.test_finish_speech()?;
        check(ended);
        assert_eq!(heimdall(&tools), [line], "said in full");
        // He has just stopped when the line comes up: Jarvis is not even asked until his pause is over.
        std::thread::sleep(quiet);
        tools.voice("mic:1", true);
        tools.voice("mic:1", false);
        let ended = Instant::now();
        agent::mcp::ToolHost::speak(&tools, line, "en", &permit)?;
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(requests(), 1, "synthesis waits for his pause to end");
        arrived();
        tools.test_finish_speech()?;
        check(ended);
        assert_eq!(requests(), 2);
        // He is talking when the line comes up: Jarvis is never asked, nothing plays.
        std::thread::sleep(quiet);
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, line, "en", &permit)?;
        tools.test_finish_speech()?;
        tools.voice("mic:1", false);
        std::thread::sleep(quiet + Duration::from_millis(200));
        assert_eq!(requests(), 2, "a skipped line is never synthesized");
        assert!(heard.lock().unwrap().is_empty());
        service.stop();
        Ok(())
    }

    /// Bug note 21 Oct 18:42, with the real Tools: lines that would play while the user talks are skipped, the reply's
    /// later lines too even once he is quiet; speech is not reported unavailable, and the agent gets one note once its
    /// turn is over. The final reply of that turn is skipped with them. Bug note 2026-10-26T18-35-49 ("you didn't say
    /// it"): the final reply stays in the pull-down, and once the reply is over its lines show as one island line,
    /// which joins the decider's conversation like any line shown as text. After that, a new reply plays.
    #[test]
    fn a_reply_skipped_while_owner_talks_is_shown_on_the_island_and_told_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(QUIET);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.agent_working(true)?;
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, "The build is green.", "en", &permit)?;
        tools.test_finish_speech()?;
        // Bug note 2026-11-05T02-32-37: on the island at once, as quiet text, not when the reply is over.
        assert_eq!(tools.skipped_notice.lock().unwrap().as_ref().map(|(_, t)| t.clone()).as_deref(), Some("The build is green."));
        tools.voice("mic:1", false);
        std::thread::sleep(QUIET + POLL);
        // He is quiet now, but this is still the skipped reply.
        agent::mcp::ToolHost::speak(&tools, "All tests passed.", "en", &permit)?;
        tools.test_finish_speech()?;
        assert_eq!(tools.skipped_notice.lock().unwrap().as_ref().map(|(_, t)| t.clone()).as_deref(), Some("The build is green. All tests passed."), "the same island line grows");
        assert!(heimdall(&tools).is_empty(), "it joins the conversation once, when the reply is over");
        assert!(playing.try_recv().is_err(), "never said");
        assert!(notices(&mut rx).is_empty(), "one note, once the reply is over");
        tools.agent_final_answer("Want me to merge it?", "stop")?;
        assert!(playing.recv_timeout(Duration::from_millis(300)).is_err(), "the final reply is skipped too");
        assert_eq!(*tools.test_island.lock().unwrap(), [ui::UiUpdate::LastReply("Want me to merge it?".into())], "in the pull-down");
        assert_eq!(tools.speech_status(), None, "speech is fine");
        assert_eq!(heimdall(&tools), ["The build is green. All tests passed. Want me to merge it?"], "shown once, as one line");
        assert!(tools.skipped_notice.lock().unwrap().is_none(), "the reply is over: its island line is complete");
        // He is quiet: once the retry's grace has passed, the agent is woken (once) to answer fresh.
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("skipped-reply:"));
        assert_eq!(told[0].1, crate::bridge::tools::skipped_note(&["The build is green.".into(), "All tests passed.".into(), "Want me to merge it?".into()]));
        assert!(told[0].1.contains("was not said") && told[0].1.contains("shown on the island"));
        assert_eq!(agent::notice::NoticeKind::classify(&told[0].0, &told[0].1), agent::notice::NoticeKind::Event, "a wake");
        // The next reply plays.
        assert!(!tools.owner.skipping());
        agent::mcp::ToolHost::speak(&tools, "Done.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.interrupt();
        tools.test_finish_speech()?;
        Ok(())
    }

    /// The skip never outlives the skipped reply: a final line skipped after its turn already ended releases it once
    /// settled (no stuck silence when his talk never becomes a wake).
    #[test]
    fn a_skip_after_the_turn_ended_releases_itself() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(QUIET);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, "Your reminder is set.", "en", &permit)?;
        tools.test_finish_speech()?;
        assert!(!tools.owner.skipping(), "settled with the agent idle: released");
        std::thread::sleep(Duration::from_millis(300));
        assert!(notices(&mut rx).is_empty(), "the retry waits for the end of his pause");
        tools.voice("mic:1", false);
        std::thread::sleep(QUIET + POLL);
        agent::mcp::ToolHost::speak(&tools, "Done.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.interrupt();
        tools.test_finish_speech()?;
        let told = notices_within(&mut rx, 1, Duration::from_secs(2));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("skipped-reply:"));
        Ok(())
    }

    /// Night review 20 Oct (6), with the real Tools: when the user's words reach the agent, the line waiting for him and
    /// the one queued behind it are dropped unheard, and the wake's note says what he did not hear; they never play
    /// and nothing else is told. A line he is hearing is not dropped.
    #[test]
    fn owner_s_words_drop_the_reply_waiting_for_him() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(QUIET);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        assert_eq!(tools.drop_held_speech(), None, "nothing waiting");
        tools.voice("mic:1", true);
        tools.voice("mic:1", false);
        agent::mcp::ToolHost::speak(&tools, "The widget thread is started.", "en", &permit)?;
        agent::mcp::ToolHost::speak(&tools, "It runs on Haiku.", "en", &permit)?;
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(tools.drop_held_speech(), Some(crate::bridge::tools::dropped_note(&["The widget thread is started.".into(), "It runs on Haiku.".into()])));
        assert!(playing.recv_timeout(QUIET + Duration::from_millis(400)).is_err(), "dropped lines never play");
        tools.test_finish_speech()?;
        tools.test_finish_speech()?;
        assert!(notices(&mut rx).is_empty(), "the wake carries the note; nothing else is told");
        assert!(heimdall(&tools).is_empty(), "never said");
        // A line he is hearing is the barge-in's.
        agent::mcp::ToolHost::speak(&tools, "Done.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        assert_eq!(tools.drop_held_speech(), None);
        assert!(tools.heimdall_speaking(), "still playing");
        tools.interrupt();
        tools.test_finish_speech()?;
        Ok(())
    }

    /// Bug note 21 Oct 18:42: lines skipped while he talked, then his words reach the agent as a wake (a steer into
    /// the running turn): the wake carries the note, no separate note follows, and the skip is released so the
    /// agent's answer to his words plays.
    #[test]
    fn owner_s_wake_carries_the_skipped_reply_and_releases_the_skip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(QUIET);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.agent_working(true)?;
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, "Three files changed.", "en", &permit)?;
        tools.test_finish_speech()?;
        tools.voice("mic:1", false);
        assert!(tools.owner.skipping());
        assert_eq!(tools.drop_held_speech(), Some(crate::bridge::tools::skipped_note(&["Three files changed.".into()])));
        assert!(!tools.owner.skipping(), "released for the answer to his words");
        std::thread::sleep(QUIET + POLL);
        agent::mcp::ToolHost::speak(&tools, "Yes, the login one too.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.interrupt();
        tools.test_finish_speech()?;
        tools.agent_working(false)?;
        tools.speech_status();
        assert!(notices(&mut rx).is_empty(), "the wake carried it; nothing else is told");
        Ok(())
    }

    /// Night 1022 review (1): a skipped reply whose note no wake of the user's carried (the call gate, a "wait" verdict)
    /// wakes the agent once his pause is over, so he gets an answer. Exactly once: an answer skipped again is told as
    /// context only, until his words reach the agent (or a line reaches him); then a skip may retry once more.
    #[test]
    fn a_skipped_reply_wakes_the_agent_once_after_owner_s_pause() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(Duration::from_millis(200));
        let (started, _playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        let skip = |line: &str| -> anyhow::Result<()> {
            tools.voice("mic:1", true);
            agent::mcp::ToolHost::speak(&tools, line, "en", &permit)?;
            tools.test_finish_speech()?;
            assert!(!tools.owner.skipping(), "the agent is idle: settled at once");
            Ok(())
        };
        skip("Your reminder is set.")?;
        std::thread::sleep(Duration::from_millis(400));
        assert!(notices(&mut rx).is_empty(), "nothing while he talks");
        tools.voice("mic:1", false);
        let ended = Instant::now();
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(ended.elapsed() >= Duration::from_millis(200), "after his pause");
        assert!(told[0].0.starts_with("skipped-reply:") && wakes(&told[0]));
        assert_eq!(told[0].1, crate::bridge::tools::skipped_note(&["Your reminder is set.".into()]));
        std::thread::sleep(Duration::from_millis(900));
        assert!(notices(&mut rx).is_empty(), "exactly once");
        // The retry's answer is skipped too: told as context, no second wake.
        skip("It is set for nine.")?;
        tools.voice("mic:1", false);
        std::thread::sleep(Duration::from_millis(1_000));
        let told = notices(&mut rx);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("unheard-speech:skipped:") && !wakes(&told[0]), "context only: no loop");
        // His words reach the agent: the next skipped reply may retry once again.
        assert_eq!(tools.drop_held_speech(), None);
        skip("Nine it is.")?;
        tools.voice("mic:1", false);
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("skipped-reply:") && wakes(&told[0]));
        Ok(())
    }

    // ---- quiet hold (lane S, night 1113) ----
    fn held_quiet(tools: &crate::bridge::Tools) -> Vec<(String, bool)> {
        tools.speech_policy.test_held().into_iter().map(|l| (l.text, l.quiet)).collect()
    }
    /// A rig whose speaker reports each line it starts, the user's pause 200 ms.
    fn quiet_rig(dir: &std::path::Path) -> anyhow::Result<(crate::bridge::Tools, tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>, std::sync::mpsc::Receiver<()>)> {
        let (tools, rx) = tools_with_notices(dir)?;
        tools.owner.set_quiet(Duration::from_millis(200));
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        Ok((tools, rx, playing))
    }
    /// ("Yeah"): a task result that would play while he is talking is held, shown on the island at
    /// once, and said once he has been quiet a moment; the agent gets no "This was not said" note and no retry wake.
    #[test]
    fn a_notice_reply_he_talks_over_is_said_once_he_is_quiet() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx, playing) = quiet_rig(dir.path())?;
        let permit = tools.agent_enabled.permit()?;
        tools.voice("mic:1", true);
        // A T3 completion's run, no words of his in it: its `speak` call.
        assert!(tools.mcp_actions.speak(&tools, "The Vella build finished.", "en", "speak-notice", &permit)?.starts_with("Speech queued"));
        tools.test_finish_speech()?;
        assert!(playing.try_recv().is_err(), "played over him");
        assert_eq!(held_quiet(&tools), [("The Vella build finished.".to_string(), true)]);
        assert!(heimdall(&tools).iter().any(|t| t == "The Vella build finished."), "shown on the island at once");
        tools.mcp_actions.replay_held(&tools);
        assert!(playing.recv_timeout(Duration::from_millis(300)).is_err(), "said while he still talks");
        tools.voice("mic:1", false);
        std::thread::sleep(Duration::from_millis(250));
        tools.mcp_actions.replay_held(&tools);
        playing.recv_timeout(Duration::from_secs(2))?;
        assert!(held_quiet(&tools).is_empty(), "said once");
        tools.interrupt();
        tools.test_finish_speech()?;
        std::thread::sleep(Duration::from_millis(800));
        let told = notices(&mut rx);
        assert!(!told.iter().any(|(id, _)| id.starts_with("skipped-reply:") || id.starts_with("unheard-speech:")), "{told:?}");
        Ok(())
    }
    /// The held notice goes into his wake's note when his words reach the agent first (one answer to both), and goes
    /// stale unsaid a minute after it was due (the agent is told it stayed text). A reply to his own words that he
    /// talked over is skipped as before, never held.
    #[test]
    fn a_held_notice_joins_his_wake_or_goes_stale_and_his_own_reply_is_still_skipped() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx, _playing) = quiet_rig(dir.path())?;
        let permit = tools.agent_enabled.permit()?;
        tools.voice("mic:1", true);
        tools.mcp_actions.speak(&tools, "The Vella build finished.", "en", "speak-1", &permit)?;
        tools.test_finish_speech()?;
        tools.voice("mic:1", false);
        assert_eq!(tools.drop_held_speech(), Some(crate::bridge::tools::skipped_note(&["The Vella build finished.".into()])));
        assert!(held_quiet(&tools).is_empty());

        tools.voice("mic:1", true);
        tools.mcp_actions.speak(&tools, "Tests passed.", "en", "speak-2", &permit)?;
        tools.test_finish_speech()?;
        tools.speech_policy.test_age_held(crate::bridge::speech_policy::HELD_FOR.as_nanos() as u64 + 1_000_000_000);
        tools.voice("mic:1", false);
        std::thread::sleep(Duration::from_millis(250));
        let _ = notices(&mut rx);
        tools.mcp_actions.replay_held(&tools);
        assert!(held_quiet(&tools).is_empty());
        let told = notices(&mut rx);
        assert_eq!(told.iter().filter(|(id, _)| id.starts_with("unheard-speech:quiet-stale:")).map(|(_, t)| t.clone()).collect::<Vec<_>>(),
            [crate::bridge::speech_policy::quiet_dropped_note(&["Tests passed.".into()])], "{told:?}");

        // His words woke this run: talked over, it is skipped and answered fresh, not held.
        tools.speech_policy.owner_woke("Heimdall, is the build done?", capture::host_now_ns());
        tools.voice("mic:1", true);
        tools.mcp_actions.speak(&tools, "Yes, it finished.", "en", "speak-3", &permit)?;
        tools.test_finish_speech()?;
        assert!(held_quiet(&tools).is_empty(), "a reply to his words was held");
        tools.voice("mic:1", false);
        assert_eq!(tools.drop_held_speech(), Some(crate::bridge::tools::skipped_note(&["Yes, it finished.".into()])));
        Ok(())
    }
    // ---- end quiet hold ----

    /// Night 1022 review (1): when his words do wake the agent before his pause is over, that wake carries the
    /// skipped reply and no retry follows.
    #[test]
    fn owner_s_wake_takes_the_skipped_reply_before_the_retry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(Duration::from_millis(200));
        let (started, _playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, "The tests are green.", "en", &tools.agent_enabled.permit()?)?;
        tools.test_finish_speech()?;
        tools.voice("mic:1", false);
        assert_eq!(tools.drop_held_speech(), Some(crate::bridge::tools::skipped_note(&["The tests are green.".into()])));
        std::thread::sleep(Duration::from_millis(1_200));
        assert!(notices(&mut rx).is_empty(), "his wake carried it");
        Ok(())
    }

    /// Night 1022 review (3, 4): the final reply is queued, then skipped at the speaker because the user started talking;
    /// a T3 completion starts the next run right after, before anything settled the skip. The new run's speech plays
    /// (the latch belonged to the earlier reply). The skipped final reply stays in the pull-down and shows (bug note
    /// 2026-10-26T18-35-49).
    #[test]
    fn a_new_run_right_after_a_skip_is_not_silenced() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(Duration::from_millis(200));
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.agent_working(true)?;
        // His words woke this run (a notice run's reply would be held for his silence instead: quiet hold, lane S).
        tools.speech_policy.owner_woke("Heimdall, is the branch ready?", capture::host_now_ns());
        tools.voice("mic:1", true);
        tools.agent_final_answer("Want me to merge it?", "stop")?;
        assert_eq!(*tools.test_island.lock().unwrap(), [ui::UiUpdate::LastReply("Want me to merge it?".into())]);
        std::thread::sleep(Duration::from_millis(150));
        assert!(tools.owner.skipping(), "skipped at the speaker; its event not drained yet");
        // The run completes and a T3 completion starts the next one at once (supervisor: Complete, then the loop's
        // `agent_working`), with no speech poll in between.
        tools.agent_working(false)?;
        tools.agent_working(true)?;
        assert!(!tools.owner.skipping(), "released by the new run");
        assert_eq!(*tools.test_island.lock().unwrap(), [ui::UiUpdate::LastReply("Want me to merge it?".into())], "not withdrawn");
        assert_eq!(heimdall(&tools), ["Want me to merge it?"], "shown as text");
        tools.voice("mic:1", false);
        std::thread::sleep(Duration::from_millis(200) + POLL * 2);
        agent::mcp::ToolHost::speak(&tools, "The T3 run finished.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.interrupt();
        tools.test_finish_speech()?;
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].1, crate::bridge::tools::skipped_note(&["Want me to merge it?".into()]));
        Ok(())
    }

    /// Bug note 22 Oct 15:13: a reply shown as text (Speaking off) that the user has probably not read stays in the
    /// pull-down above the next reply. When that next reply is skipped at the speaker (he was talking), both stay
    /// (bug note 2026-10-26T18-35-49); the agent was told he probably missed the first.
    #[test]
    fn a_skipped_reply_stays_in_the_pull_down_below_the_unread_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(Duration::from_millis(200));
        let (started, _playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let first = "The Mistral demo runs three stages at once: transcription, reasoning and speech.";
        tools.enabled.store(false, std::sync::atomic::Ordering::Release);
        tools.agent_final_answer(first, "stop")?;
        tools.enabled.store(true, std::sync::atomic::Ordering::Release);
        tools.agent_working(true)?;
        tools.voice("mic:1", true);
        tools.agent_final_answer("Want me to merge it?", "stop")?;
        std::thread::sleep(Duration::from_millis(150));
        assert!(tools.owner.skipping(), "skipped at the speaker");
        tools.agent_working(false)?;
        tools.agent_working(true)?;
        let both = format!("{first}\n\nWant me to merge it?");
        assert_eq!(*tools.test_island.lock().unwrap(), [
            ui::UiUpdate::LastReply(first.into()),
            ui::UiUpdate::LastReply(both),
        ]);
        let told = notices(&mut rx);
        assert!(told.iter().any(|(id, text)| id.starts_with("unheard-speech:unread:") && text.contains("It began: \"The Mistral demo")), "{told:?}");
        tools.voice("mic:1", false);
        Ok(())
    }

    // ---- in a call (5 Nov bug round) ----

    /// Bug note 2026-11-13T15-32-42, replayed: on a Discord call a guest (Morgan) talks the whole time; the user asks
    /// "Heimdall, answer what Morgan said. Fact check him." (his ⌃⌘\ key, an addressed wake) and stops; the run settles
    /// while its answer waits (15:32:09). A question in the core of the conversation: the agent says it with `now`
    ///, so with the real `PATIENCE` the answer plays after his pause, while the guest is still
    /// talking; before 13 Nov it waited for the guest for 10 s and was skipped (15:32:19).
    #[test]
    fn in_a_call_his_named_question_is_answered_after_his_pause_while_guests_talk_on() -> anyhow::Result<()> {
        assert_eq!(PATIENCE, Duration::from_secs(10));
        let dir = tempfile::tempdir()?;
        let (tools, _rx) = tools_with_notices(dir.path())?;
        let tools = Arc::new(tools);
        crate::bridge::speech::install_call_voice_hook(&Arc::downgrade(&tools));
        tools.owner.set_quiet(Duration::from_millis(300));
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_held_speech_queue(Synth, Blocking(started));
        let permit = tools.agent_enabled.permit()?;
        tools.speech_policy.other_line(capture::host_now_ns());
        tools.speech_policy.other_voice("process:discord", Some(capture::host_now_ns()));
        let guest_from = Instant::now();
        tools.voice("mic:1", true);
        tools.speech_policy.addressed_wake();
        crate::bridge::speech_policy::NEXT_URGENT.with(|n| n.set(true));
        agent::mcp::ToolHost::speak(&*tools, "Morgan's figure is off: the study he means found the opposite.", "en", &permit)?;
        tools.speech_policy.run_settled();
        assert!(playing.recv_timeout(Duration::from_millis(600)).is_err(), "never over User");
        tools.voice("mic:1", false);
        let paused = Instant::now();
        playing.recv_timeout(Duration::from_secs(3))?;
        let after = paused.elapsed();
        assert!(after >= Duration::from_millis(300) && after < Duration::from_millis(1_000), "after his pause: {after:?}");
        assert!(tools.speech_policy.voice_open_since("process:discord").is_some() && guest_from.elapsed() < PATIENCE, "the guest still talking");
        assert!(!tools.owner.skipping());
        tools.speech_policy.other_voice("process:discord", None);
        tools.interrupt();
        tools.test_finish_speech()?;
        assert!(!tools.owner.state.lock().unwrap().addressed.iter().any(|_| true), "the mark goes with the job");
        Ok(())
    }

    /// Fresh review S1 (night 1114), bug note 2026-11-13T15-32-42: once his named answer plays on the call, Morgan's
    /// next sentences (a new voice opening every ~0.7 s, each longer than `GUEST_CUT_AFTER`) never cut it; it plays to
    /// its end. The user's own talk-over still stops it. Unprompted speech is still cut by a guest (the 5 Nov rule).
    #[test]
    fn in_a_call_his_named_answer_plays_on_over_guest_sentences_and_only_he_stops_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, _rx) = tools_with_notices(dir.path())?;
        let tools = Arc::new(tools);
        crate::bridge::speech::install_call_voice_hook(&Arc::downgrade(&tools));
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, HeldOutput { inner: LongBlocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.speech_policy.other_line(capture::host_now_ns());
        // Unprompted speech (a notice's run): the first guest sentence cuts it.
        agent::mcp::ToolHost::speak(&*tools, "Your T3 task finished.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.test_audible.store(true, std::sync::atomic::Ordering::Release);
        assert!(!tools.answer_playing());
        tools.speech_policy.other_voice("process:discord", Some(capture::host_now_ns()));
        let from = Instant::now();
        while !tools.cut_by_guest.load(std::sync::atomic::Ordering::Acquire) {
            assert!(from.elapsed() < GUEST_CUT + Duration::from_millis(500), "a guest cuts unprompted speech");
            std::thread::sleep(Duration::from_millis(2));
        }
        tools.test_finish_speech()?;
        tools.speech_policy.other_voice("process:discord", None);
        tools.test_audible.store(false, std::sync::atomic::Ordering::Release);
        tools.cut_by_guest.store(false, std::sync::atomic::Ordering::Release);
        // His answer: plays to its end over five guest sentences.
        tools.speech_policy.addressed_wake();
        agent::mcp::ToolHost::speak(&*tools, "Morgan's figure is off: the study he means found the opposite.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.test_audible.store(true, std::sync::atomic::Ordering::Release);
        assert!(tools.answer_playing());
        for _ in 0..5 {
            tools.speech_policy.other_voice("process:discord", Some(capture::host_now_ns()));
            std::thread::sleep(GUEST_CUT + Duration::from_millis(250));
            tools.speech_policy.other_voice("process:discord", None);
            std::thread::sleep(Duration::from_millis(50));
            assert!(!tools.cut_by_guest.load(std::sync::atomic::Ordering::Acquire), "a guest never cuts his answer");
            assert!(tools.heimdall_speaking(), "still answering");
        }
        // The user talks over it: that stops it.
        tools.talked_over();
        tools.test_finish_speech()?;
        assert!(!tools.heimdall_speaking(), "his own voice stops it");
        tools.speech_policy.run_settled();
        tools.test_audible.store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Bug notes 2026-11-04T20-52-24 and 2026-11-05T00-14-40 (23:44:09 name wake, 23:44:54 skipped, 23:45:16 retry,
    /// 23:47:23 held, 23:57:23 dropped stale): the user addressed Heimdall by name in a call, kept talking to the call past
    /// the patience, the answer was skipped and the run settled. The retry that wakes the agent carries his address,
    /// so its fresh answer is not held in the call.
    #[test]
    fn the_retry_of_a_skipped_answer_to_his_name_may_speak_in_the_call() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        tools.owner.set_quiet(Duration::from_millis(200));
        tools.owner.set_patience(Duration::from_millis(200));
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(
            HeldSynth { inner: Synth, owner: tools.owner.clone() },
            HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.speech_policy.other_line(capture::host_now_ns());
        tools.speech_policy.addressed_wake();
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, "It's nine in the evening.", "en", &permit)?;
        // The run settles while the answer waits for him; he talks past the patience.
        tools.speech_policy.run_settled();
        tools.test_finish_speech()?;
        assert!(playing.try_recv().is_err(), "skipped, never over him");
        assert!(tools.speech_policy.speech_held(capture::host_now_ns()), "the settled run's address is gone");
        tools.voice("mic:1", false);
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("skipped-reply:") && wakes(&told[0]));
        assert!(tools.speech_policy.addressed_now() && !tools.speech_policy.speech_held(capture::host_now_ns()),
            "the retry carries his address: its answer is spoken in the call");
        // A skipped line that was not his answer (a notice's run) retries without it.
        tools.speech_policy.run_settled();
        assert!(tools.drop_held_speech().is_none());
        tools.voice("mic:1", true);
        agent::mcp::ToolHost::speak(&tools, "Your T3 task finished.", "en", &permit)?;
        tools.test_finish_speech()?;
        tools.voice("mic:1", false);
        let told = notices_within(&mut rx, 1, Duration::from_secs(3));
        assert!(told.len() == 1 && told[0].0.starts_with("skipped-reply:"), "{told:?}");
        assert!(!tools.speech_policy.addressed_now(), "no address to carry");
        Ok(())
    }

    /// Bug note 2026-11-05T02-32-17 item 5 (stop as soon as anyone talks, guests included): a guest's
    /// voice on the call audio, still open `GUEST_CUT_AFTER` later, stops Heimdall's audible speech; the agent hears
    /// it was a guest. A voice that already ended (a cough) stops nothing. Through the app's hook (bridge/speech.rs).
    #[test]
    fn a_guest_talking_on_the_call_stops_heimdall() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        let tools = Arc::new(tools);
        crate::bridge::speech::install_call_voice_hook(&Arc::downgrade(&tools));
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, HeldOutput { inner: Blocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        let now = capture::host_now_ns();
        tools.speech_policy.other_line(now);
        agent::mcp::ToolHost::speak(&*tools, "The deploy finished and the site is live.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.test_audible.store(true, std::sync::atomic::Ordering::Release);
        // A cough: open and closed before the check.
        tools.speech_policy.other_voice("process:discord", Some(now + 1));
        tools.speech_policy.other_voice("process:discord", None);
        std::thread::sleep(crate::bridge::speech_policy::GUEST_CUT_AFTER + Duration::from_millis(200));
        assert!(tools.heimdall_speaking(), "a cough stops nothing");
        // Robin starts talking.
        tools.speech_policy.other_voice("process:discord", Some(capture::host_now_ns()));
        // (`test_finish_speech` holds the queue while it waits: the cut must come first.)
        std::thread::sleep(crate::bridge::speech_policy::GUEST_CUT_AFTER + Duration::from_millis(300));
        tools.test_finish_speech()?;
        assert!(!tools.heimdall_speaking(), "stopped");
        let told = notices_within(&mut rx, 1, Duration::from_secs(2));
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("unheard-speech:cut:"));
        assert_eq!(told[0].1, "Someone on the call started talking, so you stopped; he heard only the start of: The deploy finished and the site is live.");
        // While Robin talks, his next answer waits for the gap.
        agent::mcp::ToolHost::speak(&*tools, "Anything else?", "en", &permit)?;
        assert!(playing.recv_timeout(Duration::from_millis(300)).is_err(), "not over Robin");
        tools.speech_policy.other_voice("process:discord", None);
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.test_audible.store(false, std::sync::atomic::Ordering::Release);
        tools.interrupt();
        tools.test_finish_speech()?;
        Ok(())
    }

    // ---- listening mode: only the user and the listened guest may interrupt (bridge/call_interrupt.rs) ----

    /// Plays until interrupted (at most 8 s), reporting its first sample.
    struct LongBlocking(std::sync::mpsc::Sender<()>);
    impl speak::AudioOutput for LongBlocking {
        fn output(&mut self, _: &speak::Audio, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
            let _ = self.0.send(());
            let deadline = Instant::now() + Duration::from_secs(8);
            while !cancel() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(2)); }
            Ok(())
        }
    }
    /// What happened to Heimdall's speech when a voice opened on the call while he talked.
    struct CallOutcome {
        /// From the voice's start to the cut, if he was stopped within the watch.
        stopped_after: Option<Duration>,
        tools: Arc<crate::bridge::Tools>,
        rx: tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>,
        _dir: tempfile::TempDir,
    }
    /// Heimdall speaks in a call (listening to a guest when `listening`); a voice opens on the Discord tap and the
    /// speaker tracker's verdicts arrive at the given offsets after it began (`voice_attributed`, as the pipeline sends
    /// them). Watches for `watch` whether and when the speech is cut. The user's microphone is not involved.
    fn call_voice_scenario(listening: bool, verdicts: Vec<(Duration, crate::bridge::call_interrupt::CallVoiceWho)>, watch: Duration) -> anyhow::Result<CallOutcome> {
        let dir = tempfile::tempdir()?;
        let (tools, rx) = tools_with_notices(dir.path())?;
        let tools = Arc::new(tools);
        crate::bridge::speech::install_call_voice_hook(&Arc::downgrade(&tools));
        tools.speech_policy.set_listening_guests(listening);
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, HeldOutput { inner: LongBlocking(started), owner: tools.owner.clone() });
        let permit = tools.agent_enabled.permit()?;
        tools.speech_policy.other_line(capture::host_now_ns());
        agent::mcp::ToolHost::speak(&*tools, "Alex, the server is back up and the logs look clean.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.test_audible.store(true, std::sync::atomic::Ordering::Release);
        let begun = Instant::now();
        let at = capture::host_now_ns();
        tools.speech_policy.other_voice("process:discord", Some(at));
        let mut verdicts = verdicts.into_iter().peekable();
        let mut stopped_after = None;
        while begun.elapsed() < watch {
            while verdicts.peek().is_some_and(|(after, _)| begun.elapsed() >= *after) {
                let (_, who) = verdicts.next().unwrap();
                tools.speech_policy.voice_attributed("process:discord", at, who);
            }
            if tools.cut_by_guest.load(std::sync::atomic::Ordering::Acquire) {
                stopped_after = Some(begun.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if stopped_after.is_some() { tools.test_finish_speech()?; }
        Ok(CallOutcome { stopped_after, tools, rx, _dir: dir })
    }
    impl CallOutcome {
        /// Ends the scenario: the voice closes and anything still playing is stopped.
        fn end(mut self) -> anyhow::Result<Vec<(String, String)>> {
            self.tools.speech_policy.other_voice("process:discord", None);
            let told = notices_within(&mut self.rx, usize::from(self.stopped_after.is_some()), Duration::from_secs(2));
            if self.stopped_after.is_none() {
                self.tools.test_audible.store(false, std::sync::atomic::Ordering::Release);
                self.tools.interrupt();
                self.tools.test_finish_speech()?;
            }
            Ok(told)
        }
    }
    use crate::bridge::call_interrupt::{CallVoiceWho, NOISE, PENDING_MAX, POLL, UNLABELLED};
    const GUEST_CUT: Duration = crate::bridge::speech_policy::GUEST_CUT_AFTER;

    /// ("Build it so only you and the listened-to guest can interrupt me"): while he listens to Alex, Alex
    /// talking on the call stops Heimdall once the tracker attributes the voice to him, and the agent hears it was a
    /// guest. A verdict landing after `GUEST_CUT_AFTER` cuts within one `POLL` of it.
    #[test]
    fn listening_the_listened_guest_talking_stops_heimdall() -> anyhow::Result<()> {
        let verdict_at = Duration::from_millis(700);
        let o = call_voice_scenario(true, vec![(Duration::from_millis(100), CallVoiceWho::Pending), (verdict_at, CallVoiceWho::MayInterrupt("Alex".into()))], Duration::from_secs(3))?;
        let after = o.stopped_after.expect("Alex stops him");
        assert!(after >= verdict_at && after < verdict_at + POLL + Duration::from_millis(250), "{after:?}");
        let told = o.end()?;
        assert!(told.len() == 1 && told[0].0.starts_with("unheard-speech:cut:"), "{told:?}");
        assert!(told[0].1.starts_with("Someone on the call started talking, so you stopped"), "{}", told[0].1);
        Ok(())
    }

    /// A verdict already in before `GUEST_CUT_AFTER` still waits for it: a click or a cough never stops him.
    #[test]
    fn listening_an_early_verdict_still_waits_out_a_click() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::ZERO, CallVoiceWho::MayInterrupt("Alex".into()))], Duration::from_secs(2))?;
        let after = o.stopped_after.expect("stopped");
        assert!(after >= GUEST_CUT && after < GUEST_CUT + Duration::from_millis(250), "{after:?}");
        o.end()?;
        Ok(())
    }

    /// Another guest talking, however long, does not stop Heimdall while he listens to Alex (it only keeps a new reply
    /// from starting, as before).
    #[test]
    fn listening_another_guest_talking_does_not_stop_heimdall() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::from_millis(300), CallVoiceWho::Pending), (Duration::from_millis(800), CallVoiceWho::Bystander("Robin".into()))], Duration::from_millis(3_200))?;
        assert_eq!(o.stopped_after, None, "Robin never stops him");
        assert!(o.tools.heimdall_speaking(), "still talking");
        assert!(o.end()?.is_empty(), "nothing cut, nothing told");
        Ok(())
    }

    /// Noise (a voice with no tracker track) does not stop him either.
    #[test]
    fn listening_noise_does_not_stop_heimdall() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::from_millis(900), CallVoiceWho::Bystander(NOISE.into()))], Duration::from_millis(3_000))?;
        assert_eq!(o.stopped_after, None);
        o.end()?;
        Ok(())
    }

    /// A voice the tracker has no confident label for yet (a new guest needs ~12 s of clean speech) may be the
    /// listened guest: it stops him, the safe side.
    #[test]
    fn listening_an_unlabelled_voice_stops_heimdall() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::from_millis(500), CallVoiceWho::MayInterrupt(UNLABELLED.into()))], Duration::from_secs(2))?;
        assert!(o.stopped_after.is_some_and(|a| a >= Duration::from_millis(500)), "{:?}", o.stopped_after);
        o.end()?;
        Ok(())
    }

    /// Alex starting over another guest within the same open voice: the verdict turns, and he stops Heimdall.
    #[test]
    fn listening_the_listened_guest_joining_another_voice_stops_heimdall() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::from_millis(700), CallVoiceWho::Bystander("Robin".into())), (Duration::from_millis(1_600), CallVoiceWho::MayInterrupt("Alex".into()))], Duration::from_secs(3))?;
        let after = o.stopped_after.expect("Alex stops him");
        assert!(after >= Duration::from_millis(1_600), "not on Robin: {after:?}");
        o.end()?;
        Ok(())
    }

    /// The tracker gives no verdict (reloading, failed or behind): the voice is treated as possibly the listened guest
    /// and stops him at `PENDING_MAX`.
    #[test]
    fn listening_no_verdict_in_time_stops_heimdall_at_the_deadline() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![], Duration::from_secs(4))?;
        let after = o.stopped_after.expect("stopped at the deadline");
        assert!(after >= PENDING_MAX && after < PENDING_MAX + Duration::from_millis(300), "{after:?}");
        o.end()?;
        Ok(())
    }

    /// Outside listening mode the user's 5 Nov rule stands: any voice still open `GUEST_CUT_AFTER` later stops Heimdall,
    /// whatever the tracker says about it.
    #[test]
    fn not_listening_any_voice_stops_heimdall_as_before() -> anyhow::Result<()> {
        let o = call_voice_scenario(false, vec![(Duration::from_millis(100), CallVoiceWho::Bystander("Robin".into()))], Duration::from_secs(2))?;
        let after = o.stopped_after.expect("stopped");
        assert!(after >= GUEST_CUT && after < GUEST_CUT + Duration::from_millis(250), "{after:?}");
        o.end()?;
        Ok(())
    }

    /// Listening turned off while another guest's voice is open and kept: the user's 5 Nov rule applies again at once.
    #[test]
    fn listening_turned_off_mid_voice_falls_back_to_the_old_rule() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::from_millis(100), CallVoiceWho::Bystander("Robin".into()))], Duration::from_millis(800))?;
        assert_eq!(o.stopped_after, None);
        o.tools.speech_policy.set_listening_guests(false);
        let off = Instant::now();
        while !o.tools.cut_by_guest.load(std::sync::atomic::Ordering::Acquire) {
            assert!(off.elapsed() < Duration::from_millis(500), "the old rule cuts within a poll");
            std::thread::sleep(Duration::from_millis(2));
        }
        o.tools.test_finish_speech()?;
        o.tools.speech_policy.other_voice("process:discord", None);
        Ok(())
    }

    /// The user's own voice still stops Heimdall at once while he listens to a guest and another guest is talking: his
    /// microphone's barge-in (pipeline/barge_in.rs, `talked_over`) does not wait for the call's tracker.
    #[test]
    fn listening_owner_talking_still_stops_heimdall() -> anyhow::Result<()> {
        let o = call_voice_scenario(true, vec![(Duration::from_millis(100), CallVoiceWho::Bystander("Robin".into()))], Duration::from_millis(900))?;
        assert_eq!(o.stopped_after, None, "Robin does not");
        o.tools.talked_over();
        o.tools.test_finish_speech()?;
        assert!(!o.tools.heimdall_speaking(), "User does");
        o.tools.speech_policy.other_voice("process:discord", None);
        Ok(())
    }

    /// Bug notes 2026-11-05T00-10-37 ("we need some kind of stop button") and 2026-11-05T02-32-17 item 4: Stop cuts
    /// what plays and what is queued, and tells the agent this moment was not for it; with nothing playing it does
    /// nothing.
    #[test]
    fn stop_cuts_speech_and_tells_the_agent_the_moment_was_not_for_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools_with_notices(dir.path())?;
        let (started, playing) = std::sync::mpsc::channel();
        tools.test_guarded_speech_queue(Synth, Blocking(started));
        let permit = tools.agent_enabled.permit()?;
        tools.stop_pressed();
        assert!(notices(&mut rx).is_empty(), "nothing playing: nothing told");
        agent::mcp::ToolHost::speak(&tools, "Here is the long answer.", "en", &permit)?;
        agent::mcp::ToolHost::speak(&tools, "And more of it.", "en", &permit)?;
        playing.recv_timeout(Duration::from_secs(2))?;
        tools.stop_pressed();
        tools.test_finish_speech()?;
        tools.test_finish_speech()?;
        assert!(!tools.agent_speech_busy());
        assert!(playing.recv_timeout(Duration::from_millis(300)).is_err(), "the queued line never plays");
        let told = notices(&mut rx);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("unheard-speech:stopped:") && !wakes(&told[0]), "context, no turn");
        Ok(())
    }
}
