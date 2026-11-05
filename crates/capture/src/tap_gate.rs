//! System audio's permission, apart from the microphone.
//!
//! macOS asks for System Audio Recording when a process makes its first process tap, and that call blocks until the
//! prompt is answered: unanswered, it waited about three minutes in the VM runs of 5 Nov 06:56-07:16, and the capture's
//! HAL thread, which opened microphones and taps in one pass, never reported ready, so the microphone never opened
//! either. Now the first tap is asked for here, once per process, on a thread of its own (`TapGate::access`), and the
//! HAL thread makes per-process taps only once that ask has come back (`opens`). Microphones never wait for it: they
//! open on the first pass and stay open when taps join later.
//!
//! The ask answered with a tap made is `Open` (a denial macOS reports only as silent taps, which the app reads from the
//! audio); an error is `Off` with why, asked again only after a new grant (`ask_again`); still waiting is `Waiting`.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Whether per-process taps may be made now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapAccess {
    /// The ask came back: taps are made (and are silent if the user said no).
    Open,
    /// The ask is out (the prompt is up, or the call is slow): taps wait, microphones don't.
    Waiting,
    /// The ask failed: no taps until a new grant asks again.
    Off(String),
}

enum State {
    Unasked,
    Asking(Instant),
    Open,
    Off(String),
}

type Ask = Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

pub struct TapGate {
    state: Mutex<State>,
    ask: Ask,
    /// HAL threads to wake when the ask comes back.
    wakers: Mutex<Vec<std::thread::Thread>>,
}

impl TapGate {
    pub fn new(ask: impl Fn() -> anyhow::Result<()> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::Unasked), ask: Box::new(ask), wakers: Mutex::new(Vec::new()) })
    }

    /// Where the ask is; the first call sends it, on its own thread, and returns at once.
    pub fn access(self: &Arc<Self>) -> TapAccess {
        let mut state = self.state.lock().unwrap();
        match &*state {
            State::Open => TapAccess::Open,
            State::Off(why) => TapAccess::Off(why.clone()),
            State::Asking(_) => TapAccess::Waiting,
            State::Unasked => {
                *state = State::Asking(Instant::now());
                drop(state);
                let gate = self.clone();
                let spawned = std::thread::Builder::new().name("heimdall-system-audio-ask".into()).spawn(move || {
                    let result = (gate.ask)();
                    gate.finish(result);
                });
                if let Err(e) = spawned { self.finish(Err(anyhow::anyhow!("the ask could not start: {e}"))); return self.access(); }
                TapAccess::Waiting
            }
        }
    }

    /// Where the ask is, without sending it: `None` before it is sent.
    pub fn peek(&self) -> Option<TapAccess> {
        match &*self.state.lock().unwrap() {
            State::Unasked => None,
            State::Asking(_) => Some(TapAccess::Waiting),
            State::Open => Some(TapAccess::Open),
            State::Off(why) => Some(TapAccess::Off(why.clone())),
        }
    }

    /// How long the ask has been out, while it is.
    pub fn waiting_for(&self) -> Option<Duration> {
        match &*self.state.lock().unwrap() { State::Asking(since) => Some(since.elapsed()), _ => None }
    }

    /// A new grant (the menu's or Setup's Allow): a failed ask is sent again at the next `access`.
    pub fn ask_again(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Off(_)) { *state = State::Unasked; }
    }

    /// `thread` is woken when the ask comes back (a HAL thread parks between passes).
    pub fn wake_on(&self, thread: std::thread::Thread) {
        let mut wakers = self.wakers.lock().unwrap();
        wakers.retain(|t| t.id() != thread.id());
        wakers.push(thread);
    }

    fn finish(&self, result: anyhow::Result<()>) {
        *self.state.lock().unwrap() = match result { Ok(()) => State::Open, Err(e) => State::Off(format!("{e:#}")) };
        for t in self.wakers.lock().unwrap().iter() { t.unpark(); }
    }
}

/// The process's gate: its ask is a private global tap made and destroyed (`macos::tap::ask_system_audio`).
pub fn global() -> &'static Arc<TapGate> {
    static GATE: OnceLock<Arc<TapGate>> = OnceLock::new();
    GATE.get_or_init(|| {
        #[cfg(target_os = "macos")]
        { TapGate::new(crate::macos::ask_system_audio) }
        #[cfg(not(target_os = "macos"))]
        { TapGate::new(|| Ok(())) }
    })
}

/// Whether a candidate opens this pass: a microphone when it is granted and selected, whatever system audio does; a
/// process tap only when it is granted and the ask came back.
pub(crate) fn opens(microphone: bool, selected: bool, grants: u32, taps: Option<&TapAccess>) -> bool {
    if microphone { grants & 1 != 0 && selected } else { grants & 2 != 0 && taps == Some(&TapAccess::Open) }
}

/// Whether an open source stops this pass: sleep, a route change or a wake restarts everything; otherwise only a source
/// no longer wanted, or wanted differently. Taps joining (a discovery pass) never touch an open microphone.
pub(crate) fn stops(sleeping: bool, route_or_wake: bool, still_wanted_as_is: bool) -> bool {
    sleeping || route_or_wake || !still_wanted_as_is
}

/// The capture's note for system audio, when it says something new: off with why, or still waiting after a while.
pub(crate) fn note(access: &TapAccess, waiting_for: Option<Duration>) -> Option<String> {
    match access {
        TapAccess::Off(why) => Some(format!("System audio off: {why}; the microphone runs without it")),
        TapAccess::Waiting if waiting_for.is_some_and(|d| d >= NOTE_AFTER) => Some("System audio waits for its permission; the microphone runs without it".into()),
        _ => None,
    }
}

/// How long the ask may be out before the capture says it waits.
pub(crate) const NOTE_AFTER: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;

    /// One pass of the HAL thread's choices (`macos::run`), on fake candidates: which open, and which open ones stop.
    fn pass(gate: &Arc<TapGate>, open: &mut Vec<&'static str>, route: bool) {
        let candidates = [("mic:BlackHole2ch_UID", true), ("pid:1432", false)];
        let taps = Some(gate.access());
        let wanted: Vec<&str> = candidates.iter().filter(|(_, mic)| opens(*mic, true, 3, taps.as_ref())).map(|(k, _)| *k).collect();
        open.retain(|k| !stops(false, route, wanted.contains(k)));
        for k in wanted { if !open.contains(&k) { open.push(k); } }
    }

    /// VM runs 5 Nov 06:56-07:16: the first tap's ask blocked and the microphone never opened. With the ask hanging,
    /// the microphone opens on the first pass at once; when the ask comes back, the tap joins and the microphone is
    /// never stopped or reopened.
    #[test]
    fn the_microphone_opens_while_system_audio_is_asked_and_stays_open_when_taps_join() {
        let (release, held) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        let held = Mutex::new(held);
        let gate = TapGate::new(move || held.lock().unwrap().recv().unwrap());
        gate.wake_on(std::thread::current());
        let mut open = vec![];
        let started = Instant::now();
        pass(&gate, &mut open, false);
        assert!(started.elapsed() < Duration::from_millis(200), "the ask never waits on the pass: {:?}", started.elapsed());
        assert_eq!(open, ["mic:BlackHole2ch_UID"], "the microphone alone, while the prompt is up");
        pass(&gate, &mut open, false);
        assert_eq!(open, ["mic:BlackHole2ch_UID"], "still waiting: nothing reopened");
        assert!(gate.waiting_for().is_some());
        release.send(Ok(())).unwrap();
        std::thread::park_timeout(Duration::from_secs(2));
        let deadline = Instant::now() + Duration::from_secs(2);
        while gate.access() != TapAccess::Open && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
        pass(&gate, &mut open, false);
        assert_eq!(open, ["mic:BlackHole2ch_UID", "pid:1432"], "the tap joined; the microphone was never stopped");
    }

    /// A failed ask (macOS refused the tap, or it errored): no taps, a note says so, the microphone runs; a new grant
    /// asks again.
    #[test]
    fn a_failed_ask_leaves_the_microphone_running_and_is_asked_again_after_a_grant() {
        let tries = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counted = tries.clone();
        let gate = TapGate::new(move || {
            if counted.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 { anyhow::bail!("not allowed") } else { Ok(()) }
        });
        let mut open = vec![];
        pass(&gate, &mut open, false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(gate.access(), TapAccess::Waiting) && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
        assert_eq!(gate.access(), TapAccess::Off("not allowed".into()));
        assert_eq!(note(&gate.access(), None).as_deref(), Some("System audio off: not allowed; the microphone runs without it"));
        pass(&gate, &mut open, false);
        assert_eq!(open, ["mic:BlackHole2ch_UID"]);
        gate.ask_again();
        pass(&gate, &mut open, false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while gate.access() != TapAccess::Open && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
        pass(&gate, &mut open, false);
        assert_eq!(open, ["mic:BlackHole2ch_UID", "pid:1432"]);
        assert_eq!(tries.load(std::sync::atomic::Ordering::Acquire), 2, "asked once per grant, never per pass");
    }

    /// The rules the HAL thread applies: a microphone never depends on system audio; a route change still restarts
    /// every source; the note waits a while before saying the ask is still out.
    #[test]
    fn microphones_never_depend_on_system_audio() {
        for taps in [None, Some(TapAccess::Waiting), Some(TapAccess::Off("x".into())), Some(TapAccess::Open)] {
            assert!(opens(true, true, 1, taps.as_ref()), "{taps:?}");
            assert!(!opens(true, false, 3, taps.as_ref()), "an unselected mic stays shut");
        }
        assert!(!opens(false, true, 2, Some(&TapAccess::Waiting)) && !opens(false, true, 1, Some(&TapAccess::Open)));
        assert!(opens(false, true, 2, Some(&TapAccess::Open)));
        assert!(stops(false, true, true) && stops(true, false, true) && !stops(false, false, true));
        assert_eq!(note(&TapAccess::Waiting, Some(Duration::from_secs(1))), None);
        assert!(note(&TapAccess::Waiting, Some(NOTE_AFTER)).is_some());
        assert_eq!(note(&TapAccess::Open, None), None);
    }
}
