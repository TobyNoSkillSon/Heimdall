//! Single asynchronous d1 owner. Commands precede queued send work; the suggest question follows each send answer.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};

pub(super) type Answer = Result<(decide::send::Fitted, decide::send::Prediction)>;
pub(super) type Completion = Box<dyn FnOnce(&mut Pipeline) -> Result<()> + Send>;
pub struct PendingSend {
    pub(super) final_: AgentFinal,
    pub(super) snapshot: decide::send::Snapshot,
    pub(super) explicit: Option<decide::send::Trigger>,
    /// A trigger heard during a call that waits for d1's verdict (night review D3; decide::send::call_trigger_waits).
    pub(super) call_trigger: Option<decide::send::Trigger>,
    pub(super) permit: Option<agent::delivery::DeliveryPermit>,
    pub(super) audio: serde_json::Value,
    pub(super) admitted: Instant,
}
pub struct CompletedSend {
    pub(super) job: PendingSend,
    pub(super) result: Answer,
    pub(super) prompt: Option<serde_json::Value>,
}
struct Ticket(Arc<AtomicUsize>);
impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
const CAPACITY: usize = 32;
// Attempts own neither admission tickets nor the callback. Retrying preserves
// the original sentence; a late abandoned attempt cannot consume its callback.
type RepeatTask = Arc<dyn Fn(&mut OwnerState) -> Completion + Send + Sync>;
struct Work {
    task: RepeatTask,
    tokens: usize,
    _ticket: Ticket,
    _lane: Ticket,
}
pub(super) struct OwnerState {
    decider: Option<decide::LocalDecider>,
    load: Arc<dyn Fn() -> Result<decide::LocalDecider> + Send + Sync>,
    loading: Arc<AtomicBool>,
    retry: Option<Instant>,
    failures: u32,
    events: crossbeam_channel::Sender<Internal>,
    pub(super) urgent: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    errors: u32,
    /// Successful loads, read by the watchdog to give the next requests the cold allowance.
    loads: Arc<AtomicUsize>,
    /// A replacement for an owner that stopped answering: "Commands OK" waits for its first model answer, not its load.
    recovering: bool,
    /// Owner threads still running, abandoned ones included (`LiveOwner`).
    alive: Arc<AtomicUsize>,
    /// The precision the loader loads, and the one loaded (`decider_swap`).
    pub(super) choice: Arc<super::decider_swap::Choice>,
}
/// Owner threads that may hold a loaded decider at once: the live one and two abandoned ones still inside a model
/// call. Past this a replacement does not load another copy; it waits for an abandoned one to finish.
const MAX_OWNERS: usize = 3;
struct LiveOwner(Arc<AtomicUsize>);
impl Drop for LiveOwner {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
/// The pause before a replacement owner loads again, by failures in the last 10 minutes: none for the first two,
/// then 30 s doubling to 5 min. The decider never takes Heimdall down with it (bug 2026-10-26T15-49-44: three
/// owner timeouts relaunched the app mid-answer); while paused its requests fail at once, as an unloaded
/// decider's do, and the agent stays reachable by name.
pub(super) fn owner_pause(attempt: u32) -> Option<std::time::Duration> {
    (attempt >= 3).then(|| std::time::Duration::from_secs((30u64 << (attempt - 3).min(4)).min(300)))
}
impl OwnerState {
    /// A decider was just loaded: its first requests run cold (`COLD_REQUESTS`).
    pub(super) fn mark_loaded(&self) {
        self.loads.fetch_add(1, Ordering::AcqRel);
    }
    pub(super) fn outcome(&mut self, ok: bool) {
        if ok {
            self.errors = 0;
            if std::mem::take(&mut self.recovering) {
                // The replacement answered: commands are back.
                let stopped = self.stop.clone();
                let _ = self.events.send(Internal::Inference(Box::new(move |p| {
                    if !stopped.load(Ordering::Acquire) {
                        p.decider_loaded(Ok(()));
                    }
                    Ok(())
                })));
            }
            return;
        }
        if self.decider.is_none() {
            return;
        }
        self.errors += 1;
        if self.errors >= 3 {
            self.errors = 0;
            self.decider.take();
            let stopped = self.stop.clone();
            let _ = self.events.send(Internal::Inference(Box::new(move |p| {
                if !stopped.load(Ordering::Acquire) {
                    let _ = p.index.evidence.event(
                        "decider_reloaded",
                        "heimdall",
                        &[],
                        capture::host_now_ns(),
                        serde_json::json!({"after_errors":3}),
                    );
                }
                Ok(())
            })));
        }
    }
    /// A decider swap on this owner thread (`decider_swap`): the decider in memory dropped, `to` loaded and asked a
    /// short decision, the previous one back if that fails.
    /// `from` is the live loop's: a swap retried on a replacement owner keeps the real previous one.
    pub(super) fn swap_to(&mut self, from: decide::precision::Pick, to: decide::precision::Pick) -> super::decider_swap::Outcome {
        // As `decider()`: past the stuck-owner bound no further copy loads; nothing changes.
        let owners = self.alive.load(Ordering::Acquire);
        if owners > MAX_OWNERS {
            let reason = format!("{} didn\u{2019}t load: {} earlier decider threads are still stuck", to.label(), owners - 1);
            return match self.decider { Some(_) => super::decider_swap::Outcome::RolledBack { reason, used: from }, None => super::decider_swap::Outcome::Failed { reason } };
        }
        let load = self.load.clone();
        let out = swap_on_owner(&self.choice, &self.loading, &self.loads, &self.stop, &mut self.decider, from, to, || load(), probe);
        self.after_swap(&out);
        out
    }
    /// A swap that left a decider loaded clears the reload backoff; one that left none lets the next request retry.
    pub(super) fn after_swap(&mut self, out: &super::decider_swap::Outcome) {
        self.errors = 0;
        if !matches!(out, super::decider_swap::Outcome::Failed { .. }) {
            self.retry = None;
            self.failures = 0;
        }
    }
    pub(super) fn decider(&mut self) -> Result<&mut decide::LocalDecider> {
        if self.decider.is_none() {
            if let Some(at) = self.retry.filter(|at| Instant::now() < *at) {
                anyhow::bail!(
                    "decider not loaded; next try in {} s",
                    at.duration_since(Instant::now()).as_secs() + 1
                );
            }
            let owners = self.alive.load(Ordering::Acquire);
            if owners > MAX_OWNERS {
                self.retry = Some(Instant::now() + std::time::Duration::from_secs(60));
                anyhow::bail!("decider not loaded: {} earlier owners are still stuck; next try in 60 s", owners - 1);
            }
            self.loading.store(true, Ordering::Release);
            let loaded = (self.load)();
            self.loading.store(false, Ordering::Release);
            match loaded {
                Ok(d) => {
                    self.decider = Some(d);
                    self.mark_loaded();
                    self.retry = None;
                    self.failures = 0;
                    if self.recovering {
                        return Ok(self.decider.as_mut().unwrap());
                    }
                    let stopped = self.stop.clone();
                    let _ = self.events.send(Internal::Inference(Box::new(move |p| {
                        if !stopped.load(Ordering::Acquire) {
                            p.decider_loaded(Ok(()));
                        }
                        Ok(())
                    })));
                }
                Err(e) => {
                    self.failures += 1;
                    let wait = (5u64 << self.failures.min(6)).min(300);
                    self.retry = Some(Instant::now() + std::time::Duration::from_secs(wait));
                    let detail = format!("{e:#}");
                    let attempt = self.failures;
                    let stopped = self.stop.clone();
                    let _ = self.events.send(Internal::Inference(Box::new(move |p| {
                        if !stopped.load(Ordering::Acquire) {
                            p.decider_loaded(Err((detail, attempt, wait)));
                        }
                        Ok(())
                    })));
                    return Err(e);
                }
            }
        }
        Ok(self.decider.as_mut().unwrap())
    }
}

/// The log line naming the decider that loaded, at launch and after every swap (the integrator's check after an
/// install): "d1-2B-heimdall-v6-4bit (Decider V6, 4-bit) loaded in 1.9 s".
pub(super) fn loaded_line(used: decide::precision::Pick, took: std::time::Duration) -> String {
    format!("{} loaded in {:.1} s", used.describe(), took.as_secs_f64())
}

/// Asks a freshly loaded decider one short send decision: it has answered once this returns Ok.
fn probe(d: &mut decide::LocalDecider) -> Result<()> {
    let mut c = decide::send::Conversation::default();
    c.append("User", "Okay.", true);
    d.send_decision_interruptible(&c.snapshot(), &AtomicUsize::new(0)).map(|_| ())
}

/// The swap's owner-thread steps for any decider type (the fakes in the tests too): the precision the loader reads set
/// for each attempt, the loading flag up (the watchdog's load allowance, the drop included) and a load counted (the
/// cold allowance for the probe that follows). The chosen precision ends as what holds: `to`, or `from` after a rollback
/// or a double failure. An owner the watchdog retired (`retired`) loads nothing more and leaves the shared choice to its
/// replacement, which runs the same swap again.
#[allow(clippy::too_many_arguments)]
pub(super) fn swap_on_owner<D>(
    choice: &super::decider_swap::Choice,
    loading: &AtomicBool,
    loads: &AtomicUsize,
    retired: &AtomicBool,
    held: &mut Option<D>,
    from: decide::precision::Pick,
    to: decide::precision::Pick,
    load: impl Fn() -> Result<D>,
    probe: impl FnMut(&mut D) -> Result<()>,
) -> super::decider_swap::Outcome {
    loading.store(true, Ordering::Release);
    let out = super::decider_swap::swap(held, from, to, |p| {
        anyhow::ensure!(!retired.load(Ordering::Acquire), "the decider thread was replaced");
        choice.set_chosen(p);
        loading.store(true, Ordering::Release);
        let loaded = load();
        loading.store(false, Ordering::Release);
        let d = loaded?;
        loads.fetch_add(1, Ordering::AcqRel);
        Ok((d, choice.running().unwrap_or(p)))
    }, probe);
    loading.store(false, Ordering::Release);
    if retired.load(Ordering::Acquire) { return out; }
    if !matches!(out, super::decider_swap::Outcome::Swapped { .. }) { choice.set_chosen(from); }
    if held.is_none() { choice.set_running(None); }
    out
}

pub(super) struct InferenceOwner {
    tx: [crossbeam_channel::Sender<Work>; 4],
    pub(super) outstanding: Arc<AtomicUsize>,
    lanes: [Arc<AtomicUsize>; 4],
    stop: Arc<AtomicBool>,
    urgent: Arc<AtomicUsize>,
    wake: crossbeam_channel::Sender<()>,
    held: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The precision this owner loads (`decider_swap::Choice`).
    pub(super) choice: Arc<super::decider_swap::Choice>,
}
pub(super) struct Hold {
    held: Arc<AtomicUsize>,
    wake: crossbeam_channel::Sender<()>,
}
impl Drop for Hold {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
        let _ = self.wake.try_send(());
    }
}
static RETIRED: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());
pub(super) fn join_retired() {
    for thread in std::mem::take(&mut *RETIRED.lock().unwrap()) {
        let _ = thread.join();
    }
}
impl Drop for InferenceOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.urgent.fetch_add(1, Ordering::AcqRel);
        let _ = self.wake.try_send(());
        if let Some(thread) = self.thread.take() {
            RETIRED.lock().unwrap().push(thread);
        }
    }
}
// Full 2k forwards take tens of ms. Allow scheduling and fitting, with a
// size-proportional deadline; loading gets its own 30-second allowance.
// The first requests after a load run cold (kernels compile, caches fill, often
// beside the voice's own warm-up) and get `COLD`'s 10 s: at 02:32 on 24 Oct
// the request after the lazy load and each reload's first request missed
// 1843 ms, and three restarts relaunched Heimdall (bug 2026-10-24T04-30-49).
const LOADING: usize = usize::MAX;
const COLD: usize = usize::MAX - 1;
/// Requests after each load that get the cold allowance, and how long after the load it lasts at most.
const COLD_REQUESTS: u32 = 3;
const COLD_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
fn request_deadline(tokens: usize) -> std::time::Duration {
    match tokens {
        LOADING => std::time::Duration::from_secs(30),
        COLD => std::time::Duration::from_secs(10),
        _ => std::time::Duration::from_millis(1000 + tokens.min(decide::send::WINDOW) as u64),
    }
}
/// One owner thread's cold allowance: armed by each load, spent by requests, gone after `COLD_WINDOW`.
#[derive(Default)]
struct Cold {
    seen: usize,
    left: u32,
    until: Option<Instant>,
}
impl Cold {
    fn watch(&mut self, loads: usize, now: Instant) {
        if loads != self.seen {
            self.seen = loads;
            self.left = COLD_REQUESTS;
            self.until = Some(now + COLD_WINDOW);
        }
    }
    fn applies(&self, now: Instant) -> bool {
        self.left > 0 && self.until.is_some_and(|u| now < u)
    }
}
const WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(100);
struct Worker {
    tx: crossbeam_channel::Sender<RepeatTask>,
    rx: crossbeam_channel::Receiver<std::thread::Result<Completion>>,
    stop: Arc<AtomicBool>,
    loading: Arc<AtomicBool>,
    loads: Arc<AtomicUsize>,
    thread: std::thread::JoinHandle<()>,
}
impl Worker {
    /// `paused_until`: no load before then (`owner_pause`); `recovering`: it replaces an owner that failed.
    fn start(
        load: Arc<dyn Fn() -> Result<decide::LocalDecider> + Send + Sync>,
        events: crossbeam_channel::Sender<Internal>,
        urgent: Arc<AtomicUsize>,
        alive: Arc<AtomicUsize>,
        paused_until: Option<Instant>,
        recovering: bool,
        choice: Arc<super::decider_swap::Choice>,
    ) -> Result<Self> {
        let (tx, requests) = crossbeam_channel::unbounded::<RepeatTask>();
        let (responses, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let loading = Arc::new(AtomicBool::new(false));
        let phase = loading.clone();
        let loads = Arc::new(AtomicUsize::new(0));
        let loaded = loads.clone();
        alive.fetch_add(1, Ordering::AcqRel);
        let owner = LiveOwner(alive.clone());
        let thread = std::thread::Builder::new()
            .name("heimdall-d1-owner".into())
            .spawn(move || {
                let _owner = owner;
                // No Rust lifetime mutex: an abandoned thread must not hold the
                // replacement hostage. Native handles retain native isolation.
                let mut state = OwnerState {
                    decider: None,
                    load,
                    loading: phase,
                    retry: paused_until,
                    failures: 0,
                    events,
                    urgent,
                    stop: stopping.clone(),
                    errors: 0,
                    loads: loaded,
                    recovering,
                    alive,
                    choice,
                };
                while let Ok(task) = requests.recv() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(&mut state)));
                    let failed = result.is_err();
                    if responses.send(result).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            rx,
            stop,
            loading,
            loads,
            thread,
        })
    }
    fn retire(self) {
        self.stop.store(true, Ordering::Release);
        // Rust cannot kill a stuck FFI call. Do not join one on the live loop
        // or Quit. Stuck owners are bounded by `MAX_OWNERS`, never by a relaunch.
        drop(self.tx);
        if self.thread.is_finished() {
            let _ = self.thread.join();
        }
    }
}
impl InferenceOwner {
    pub(super) fn start(
        models: PathBuf,
        archive: PathBuf,
        session: PathBuf,
        writer: heimdall_archive::records::Writer,
        events: crossbeam_channel::Sender<Internal>,
        pick: decide::precision::Pick,
    ) -> Result<Self> {
        // Training rows are written on their own thread, outside the request deadline (`DecisionArchive`).
        let rows = super::DecisionArchive::start(session, writer)?;
        let choice = Arc::new(super::decider_swap::Choice::new(pick));
        let loader = choice.clone();
        Self::supervised_with(events, choice, Arc::new(move || {
            // The chosen decider, or the nearest one on this Mac when its folder is missing (the log and menu say so).
            let resolved = decide::precision::resolve(loader.chosen(), |p| decide::precision::present(&models, p));
            if let Some(why) = resolved.reason() { crate::diagnostics::line("decider", &why); }
            let started = Instant::now();
            // The log names the folder; the error stays bare, as the menu's swap reason already names the decider.
            let mut d = decide::LocalDecider::load_pick(&models, &archive, resolved.used)
                .inspect_err(|e| crate::diagnostics::line("decider", &format!("{} did not load: {e:#}", resolved.used.name())))?;
            crate::diagnostics::line("decider", &loaded_line(resolved.used, started.elapsed()));
            loader.set_running(Some(resolved.used));
            let rows = rows.clone();
            d.set_archive_sink(Box::new(move |input, decision, error, outputs| {
                rows.queue(input, decision, error, outputs);
                Ok(())
            }));
            Ok(d)
        }), request_deadline, owner_pause)
    }
    #[cfg(test)]
    fn supervised(
        events: crossbeam_channel::Sender<Internal>,
        load: Arc<dyn Fn() -> Result<decide::LocalDecider> + Send + Sync>,
        deadline: fn(usize) -> std::time::Duration,
        pause_after: fn(u32) -> Option<std::time::Duration>,
    ) -> Result<Self> {
        Self::supervised_with(events, Arc::new(super::decider_swap::Choice::new(decide::precision::Pick::DEFAULT)), load, deadline, pause_after)
    }
    pub(super) fn supervised_with(
        events: crossbeam_channel::Sender<Internal>,
        choice: Arc<super::decider_swap::Choice>,
        load: Arc<dyn Fn() -> Result<decide::LocalDecider> + Send + Sync>,
        deadline: fn(usize) -> std::time::Duration,
        pause_after: fn(u32) -> Option<std::time::Duration>,
    ) -> Result<Self> {
        let owner_choice = choice.clone();
        let pairs: [_; 4] = std::array::from_fn(|_| crossbeam_channel::bounded::<Work>(CAPACITY));
        let tx = std::array::from_fn(|i| pairs[i].0.clone());
        let rx: Vec<_> = pairs.into_iter().map(|p| p.1).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let urgent = Arc::new(AtomicUsize::new(0));
        let urgency = urgent.clone();
        let held = Arc::new(AtomicUsize::new(0));
        let holding = held.clone();
        let (wake, notified) = crossbeam_channel::bounded(1);
        let alive = Arc::new(AtomicUsize::new(0));
        let mut worker = Worker::start(load.clone(), events.clone(), urgency.clone(), alive.clone(), None, false, choice.clone())?;
        let thread = std::thread::Builder::new()
            .name("heimdall-d1-watchdog".into())
            .spawn(move || {
                let mut restarts = VecDeque::<Instant>::new();
                let mut retry: Option<(usize, Work)> = None;
                // Per owner thread: a replacement starts with none and arms it at its own load.
                let mut cold = Cold::default();
                while !stopping.load(Ordering::Acquire) {
                    if holding.load(Ordering::Acquire) > 0 {
                        let _ = notified.recv_timeout(WATCH_TICK);
                        continue;
                    }
                    let job = (0..4).rev().find_map(|i| {
                        if retry.as_ref().is_some_and(|(priority, _)| *priority == i) {
                            retry.take()
                        } else {
                            rx[i].try_recv().ok().map(|w| (i, w))
                        }
                    });
                    let Some((priority, work)) = job else {
                        let _ = notified.recv_timeout(WATCH_TICK);
                        continue;
                    };
                    if priority >= 2 {
                        urgency.fetch_sub(1, Ordering::AcqRel);
                    }
                    let mut phase_began = Instant::now();
                    let mut was_loading = false;
                    let mut was_cold = false;
                    let submitted = worker.tx.send(work.task.clone()).is_ok();
                    let result = loop {
                        if stopping.load(Ordering::Acquire) {
                            worker.retire();
                            return;
                        }
                        if !submitted {
                            break Err("owner exited".to_string());
                        }
                        match worker.rx.recv_timeout(WATCH_TICK) {
                            Ok(Ok(completion)) => break Ok(completion),
                            Ok(Err(panic)) => {
                                let reason = panic
                                    .downcast_ref::<&str>()
                                    .copied()
                                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                                    .unwrap_or("unknown panic");
                                break Err(format!("owner panicked: {reason}"));
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                break Err("owner exited".into())
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        }
                        let loading = worker.loading.load(Ordering::Acquire);
                        if loading != was_loading {
                            phase_began = Instant::now();
                            was_loading = loading;
                        }
                        let now = Instant::now();
                        cold.watch(worker.loads.load(Ordering::Acquire), now);
                        let allowed = if loading {
                            deadline(LOADING)
                        } else if was_cold || cold.applies(now) {
                            // Once cold, a request keeps the allowance to its end (the window may close meanwhile).
                            was_cold = true;
                            deadline(work.tokens).max(deadline(COLD))
                        } else {
                            deadline(work.tokens)
                        };
                        if phase_began.elapsed() > allowed {
                            break Err(format!(
                                "owner hung: no answer within {} ms for <= {} tokens",
                                allowed.as_millis(),
                                work.tokens
                            ));
                        }
                    };
                    cold.watch(worker.loads.load(Ordering::Acquire), Instant::now());
                    if was_cold || cold.applies(Instant::now()) {
                        cold.left = cold.left.saturating_sub(1);
                    }
                    match result {
                        Ok(completion) => {
                            let global = stopping.clone();
                            let _ = events.send(Internal::Inference(Box::new(move |p| {
                                drop(work);
                                if !global.load(Ordering::Acquire) {
                                    completion(p)?;
                                }
                                Ok(())
                            })));
                        }
                        Err(error) => {
                            worker.retire();
                            let now = Instant::now();
                            while restarts.front().is_some_and(|t| {
                                now.duration_since(*t) > std::time::Duration::from_secs(600)
                            }) {
                                restarts.pop_front();
                            }
                            restarts.push_back(now);
                            let attempt = restarts.len() as u32;
                            let pause = pause_after(attempt);
                            let global = stopping.clone();
                            let _ = events.send(Internal::Inference(Box::new(move |p| {
                                if !global.load(Ordering::Acquire) {
                                    p.decider_owner_failed(error, attempt, pause.map(|d| d.as_secs()));
                                }
                                Ok(())
                            })));
                            // Always a replacement, paused past the second failure: the in-flight request and the
                            // queue get answers (errors while paused), never a relaunch.
                            match Worker::start(load.clone(), events.clone(), urgency.clone(), alive.clone(), pause.map(|d| now + d), true, owner_choice.clone()) {
                                Ok(next) => {
                                    worker = next;
                                    cold = Cold::default();
                                }
                                Err(e) => {
                                    let global = stopping.clone();
                                    let _ = events.send(Internal::Inference(Box::new(move |p| {
                                        if !global.load(Ordering::Acquire) {
                                            p.decider_owner_stopped(format!("replacement could not start: {e}"));
                                        }
                                        Ok(())
                                    })));
                                    // Keep trying: queued requests keep their callbacks and run once one starts.
                                    loop {
                                        let _ = notified.recv_timeout(std::time::Duration::from_secs(5));
                                        if stopping.load(Ordering::Acquire) {
                                            return;
                                        }
                                        if let Ok(next) = Worker::start(load.clone(), events.clone(), urgency.clone(), alive.clone(), None, true, owner_choice.clone()) {
                                            worker = next;
                                            cold = Cold::default();
                                            break;
                                        }
                                    }
                                }
                            }
                            if priority >= 2 {
                                urgency.fetch_add(1, Ordering::AcqRel);
                            }
                            retry = Some((priority, work));
                        }
                    }
                }
                worker.retire();
            })?;
        Ok(Self {
            tx,
            outstanding: Arc::new(AtomicUsize::new(0)),
            lanes: std::array::from_fn(|_| Arc::new(AtomicUsize::new(0))),
            stop,
            urgent,
            wake,
            held,
            thread: Some(thread),
            choice,
        })
    }
    /// Swaps the decider to `to` on the owner thread, after the decision in flight and ahead of queued sends (the top
    /// lane); `done` gets the outcome on the live loop (`decider_swap`).
    pub(super) fn swap(
        &self,
        from: decide::precision::Pick,
        to: decide::precision::Pick,
        done: impl FnOnce(&mut Pipeline, super::decider_swap::Outcome) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.request(3, 0, move |state| state.swap_to(from, to), done)
    }
    pub(super) fn hold(&self) -> Hold {
        self.held.fetch_add(1, Ordering::AcqRel);
        Hold {
            held: self.held.clone(),
            wake: self.wake.clone(),
        }
    }
    // One-shot coordination barriers only. Product inference uses request().
    #[cfg(test)]
    pub(super) fn task(
        &self,
        priority: usize,
        task: impl FnOnce(&mut OwnerState) -> Completion + Send + 'static,
    ) -> Result<()> {
        let task = std::sync::Mutex::new(Some(task));
        self.enqueue(
            priority,
            decide::send::WINDOW,
            Arc::new(move |state| {
                let task = task.lock().unwrap().take();
                match task {
                    Some(task) => task(state),
                    None => Box::new(|_| Ok(())),
                }
            }),
        )
    }
    pub(super) fn request<T: Send + 'static>(
        &self,
        priority: usize,
        tokens: usize,
        infer: impl Fn(&mut OwnerState) -> T + Send + Sync + 'static,
        complete: impl FnOnce(&mut Pipeline, T) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let complete = Arc::new(std::sync::Mutex::new(Some(complete)));
        self.enqueue(
            priority,
            tokens,
            Arc::new(move |state| {
                let result = infer(state);
                let complete = complete.clone();
                Box::new(move |p| {
                    let callback = complete.lock().unwrap().take();
                    match callback {
                        Some(done) => done(p, result),
                        None => Ok(()),
                    }
                })
            }),
        )
    }
    fn enqueue(&self, priority: usize, tokens: usize, task: RepeatTask) -> Result<()> {
        self.lanes[priority]
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < CAPACITY).then_some(n + 1)
            })
            .map_err(|_| anyhow::anyhow!("d1 backlog full; input retained"))?;
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        let work = Work {
            task,
            tokens,
            _ticket: Ticket(self.outstanding.clone()),
            _lane: Ticket(self.lanes[priority].clone()),
        };
        if priority >= 2 {
            self.urgent.fetch_add(1, Ordering::AcqRel);
        }
        if self.tx[priority].try_send(work).is_err() {
            if priority >= 2 {
                self.urgent.fetch_sub(1, Ordering::AcqRel);
            }
            anyhow::bail!("d1 owner stopped");
        }
        let _ = self.wake.try_send(());
        Ok(())
    }
    /// Sentences waiting for or in a send decision: the suggest question waits for an idle send lane.
    pub(super) fn send_busy(&self) -> bool {
        self.lanes[1].load(Ordering::Acquire) > 0
    }
    pub(super) fn send_full(&self) -> bool {
        self.lanes[1].load(Ordering::Acquire) >= CAPACITY
    }
    pub(super) fn submit(&self, job: PendingSend) -> Result<()> {
        let snapshot = job.snapshot.clone();
        self.request(
            1,
            snapshot.token_bound(),
            move |state| {
                let urgent = state.urgent.clone();
                let result = state
                    .decider()
                    .and_then(|d| d.send_decision_interruptible(&snapshot, &urgent));
                let prompt = state
                    .decider
                    .as_ref()
                    .and_then(|d| d.last_send_request())
                    .map(|f| f.request.clone());
                if !result
                    .as_ref()
                    .is_err_and(|e| e.to_string().contains("preempted"))
                {
                    state.outcome(result.is_ok());
                }
                (result, prompt)
            },
            move |p, (result, prompt)| {
                p.complete_send(CompletedSend {
                    job,
                    result,
                    prompt,
                })
            },
        )
    }
}
impl Pipeline {
    /// A call is going on at `end` (host ns): a call line within `CALL_ACTIVE_NS` and no goodbye since.
    pub(super) fn in_call_at(&self, end: u64) -> bool {
        decide::send::call_active(self.last_call_line_ns, self.call_farewell_ns, end)
    }
    /// A goodbye in the call ended at `end_ns`: the call and the conversation hold are over.
    pub(super) fn call_goodbye(&mut self, end_ns: u64) {
        self.call_farewell_ns = Some(self.call_farewell_ns.map_or(end_ns, |t| t.max(end_ns)));
        self.tools.speech_policy.farewell(end_ns);
    }
    /// `speaker`: whose line the decision sent (`agent::Speaker`, night review 9).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch_pending(
        &mut self,
        source: &str,
        text: &str,
        through: u64,
        observation: &str,
        speaker: agent::Speaker,
        permit: Option<agent::delivery::DeliveryPermit>,
        for_heimdall: bool,
        ack: super::ack::AckPlan,
    ) -> Result<()> {
        let Some(permit) = permit else {
            return Ok(());
        };
        if permit.check().is_err()
            || self.agent.is_none()
            || self.dispatched_through >= through
            || !self
                .tools
                .send_context
                .lock()
                .unwrap()
                .has_pending_through(through)
        {
            return Ok(());
        }
        // Speech d1, his name or "send it" sent to Heimdall may close a question card at this wake (bridge/questions.rs).
        if for_heimdall { self.tools.spoke_to_heimdall(through); }
        // Who this wake carries, for `listen_to` (pipeline/listen_to.rs): the user's own lines riding it, or a guest's.
        if speaker == agent::Speaker::Owner {
            self.owner_wake(through, text);
        } else {
            self.tools.speech_policy.guest_woke();
        }
        self.expire_attachments(capture::host_now_ns());
        let mut images = vec![];
        let mut kept = vec![];
        let mut stamps = vec![];
        for (image, stamp) in std::mem::take(&mut self.attachment_batch)
            .into_iter()
            .zip(std::mem::take(&mut self.attachment_stamps))
        {
            if stamp.0 <= through {
                images.push(image);
            } else {
                kept.push(image);
                stamps.push(stamp);
            }
        }
        self.attachment_batch = kept;
        self.attachment_stamps = stamps;
        // The instant acknowledgement (pipeline/ack.rs), before the wake so the wake can say it was said.
        let later = ack == super::ack::AckPlan::Later;
        // The island's arrow and dots wait for the same moment ("That is an animation, but also speech").
        if speaker == agent::Speaker::Owner && !self.settings.dry_run { self.hold_island(source); }
        let acked = speaker == agent::Speaker::Owner && self.instant_ack_with(source, through, observation, text, ack);
        let result = self
            .agent
            .as_ref()
            .context("agent stopped before send")?
            .wake_pending_through_with_permit(
                images,
                false,
                through,
                Some(observation.to_owned()),
                speaker,
                permit.clone(),
            );
        if acked && (result.is_err() || permit.check().is_err()) {
            // The wake did not go out: its ack line is never told to a later one.
            self.tools.ack_said.lock().unwrap().take();
        }
        if permit.check().is_ok() {
            result?;
            self.dispatched_through = self.dispatched_through.max(through);
            self.pending_text.clear();
            // speech-policy (night 3): a spoken wake went to the agent: the wake sound (bridge/speech_policy.rs).
            // Agent off sends nothing, so it stays quiet.
            // An acknowledgement already says it was sent: no second sound.
            // Decider v3, a wake sent before d1 answered: the sound waits for d1's line or its moment (pipeline/ack.rs).
            let deferred = later && speaker == agent::Speaker::Owner && !self.settings.dry_run
                && self.defer_ack(source, through, observation, text);
            if !self.settings.dry_run && !acked && !deferred { self.tools.speech_policy.wake_sent(capture::host_now_ns()); }
            // The send arrow comes from the agent thread once the wake is actually delivered (bridge/).
        }
        Ok(())
    }
    pub(super) fn complete_send(&mut self, completed: CompletedSend) -> Result<()> {
        let preempted = completed
            .result
            .as_ref()
            .is_err_and(|e| e.to_string().contains("preempted"));
        if preempted {
            let job = &completed.job;
            let retry = job.explicit.is_none()
                && job.permit.as_ref().is_some_and(|p| p.check().is_ok())
                && self.dispatched_through < job.snapshot.through
                && self
                    .tools
                    .send_context
                    .lock()
                    .unwrap()
                    .has_pending_through(job.snapshot.through);
            self.index.evidence.event(
                "send_decision_preempted", &job.final_.source,
                std::slice::from_ref(&job.final_.observation.id), job.final_.end,
                serde_json::json!({"through":job.snapshot.through,
                    "outcome":if retry {"retry"} else if job.explicit.is_some() {"already_sent"} else {"superseded_by_barrier"},
                    "prompt":completed.prompt,"latency_ms":job.admitted.elapsed().as_secs_f64()*1000.}),
            )?;
            if retry {
                // Same snapshot, sentence and permission; never reacquire authority
                // or append the observation again. Foreground lanes still win.
                self.inference()?.submit(completed.job)?;
                return Ok(());
            }
            // Finalize superseded/explicit rows too: no perpetual decision_pending
            // status, and the original explicit trigger remains in the journal.
        }
        let CompletedSend {
            job,
            result,
            prompt,
        } = completed;
        let PendingSend {
            final_,
            snapshot,
            explicit,
            call_trigger,
            permit,
            audio,
            admitted,
            ..
        } = job;
        let AgentFinal {
            audio_error: _,
            observation,
            source,
            start,
            end,
            turn,
            audio_spans,
            forced,
            end_of_turn_probability,
            ..
        } = final_;
        let mut trigger = explicit;
        // Decider v3: the verdict, and the ack line it picked with its send (pipeline/ack.rs `AckPlan`).
        let mut decided_line: Option<Option<String>> = None;
        let mut call_gate = None;
        let mut d1_gate = None;
        // The user's own line (mic, labelled the user): while a question card is up, d1's verdict follows bridge/questions.rs.
        let card = if self.owner_line(&source, &observation.id) && snapshot.pending.iter().any(|t| t.seq == snapshot.through && t.speaker == "User") {
            self.tools.card_state(start, end)
        } else {
            crate::bridge::questions::CardState::None
        };
        let mut record = match result {
            Ok((fitted, prediction)) => {
                self.tools
                    .send_context
                    .lock()
                    .unwrap()
                    .trim_history(&snapshot, fitted.dropped_turns);
                self.component_status("decision", None);
                self.decisions += 1;
                decided_line = Some(prediction.ack.clone());
                // Night review D3: a trigger heard during a call wakes unless d1 says strongly "not for Heimdall".
                if let Some(t) = call_trigger {
                    let not_for = prediction.probabilities.get("not_for_heimdall").copied();
                    let wakes = decide::send::call_trigger_holds(not_for);
                    call_gate = Some(serde_json::json!({"trigger":t,"not_for_heimdall":not_for,"bar":decide::send::CALL_NOT_FOR,"wakes":wakes}));
                    if wakes { trigger = Some(t); }
                }
                if trigger.is_none() {
                    let text = snapshot.pending.iter().find(|t| t.seq == snapshot.through).map_or("", |t| t.text.as_str());
                    // Night pass 1022: call and app-audio lines never wake by d1 alone, nor the user's lines during a call
                    // unless he answers Heimdall's card; a near-tie outside a call sends (decide::send::d1_gate).
                    // His own line, or a listened guest's (listen_to.rs); never another voice on his microphone
                    // (mic_voice.rs).
                    let owner = self.like_owner(&source, &observation.id);
                    let in_call = owner && self.in_call_at(end);
                    let argmax_send = prediction.choice == decide::send::Choice::Send;
                    use crate::bridge::questions::CardState;
                    let gate = decide::send::d1_gate(owner, in_call, card == CardState::Answering, card == CardState::None,
                        &prediction.probabilities, argmax_send);
                    trigger = match gate {
                        decide::send::D1Gate::Open => crate::bridge::questions::card_verdict(card, text, argmax_send,
                            prediction.probabilities.get("send").copied().unwrap_or(0.)),
                        decide::send::D1Gate::NearTie => Some(decide::send::Trigger::D1),
                        decide::send::D1Gate::NotOwner | decide::send::D1Gate::InCall => None,
                    };
                    if gate != decide::send::D1Gate::Open {
                        d1_gate = Some(serde_json::json!({"gate":gate,"argmax_send":argmax_send,"near_tie":decide::send::NEAR_TIE}));
                    }
                }
                serde_json::json!({"schema":1,"prompt":fitted.request,"context_tokens":fitted.context_tokens,
                    "input_tokens":prediction.input_tokens,"probabilities":prediction.probabilities,"choice":prediction.choice,
                    "dropped_turns":fitted.dropped_turns,"pending_truncated":fitted.pending_truncated,"model_output":prediction.output,
                    "ack_line":prediction.ack})
            }
            Err(e) => {
                // No verdict: a waiting call trigger wakes, as it did before the call rule.
                if let Some(t) = call_trigger {
                    trigger = Some(t);
                    call_gate = Some(serde_json::json!({"trigger":t,"wakes":true,"reason":"no decision"}));
                }
                if !preempted {
                    self.errors += 1;
                    self.fail(&format!("Decision failed; transcript retained: {e}"));
                }
                serde_json::json!({"schema":1,"error":format!("{e:#}"),"prompt":prompt,"context":snapshot.turns,"pending":snapshot.pending})
            }
        };
        let would_wake = trigger.is_some() && !self.own_audio(&source);
        record["trigger"] = serde_json::json!(trigger);
        if let Some(gate) = call_gate { record["call_gate"] = gate; }
        let held_by_gate = d1_gate.as_ref().and_then(|g| g["gate"].as_str().map(str::to_owned)).filter(|g| g != "near_tie");
        if let Some(gate) = d1_gate { record["d1_gate"] = gate; }
        if card != crate::bridge::questions::CardState::None { record["question_card"] = serde_json::json!(format!("{card:?}").to_lowercase()); }
        record["outcome"] = serde_json::json!(if would_wake {
            "send"
        } else if let (Some(gate), true) = (held_by_gate.as_deref(), record["choice"] == "send") {
            // Night pass 1022: d1 said send, the gate kept it as context.
            if gate == "not_owner" { "held_not_owner" } else { "held_in_call" }
        } else if trigger.is_none() && record["choice"] == "send" {
            // d1 sent a filler while a question card was up: held (bridge/questions.rs).
            "filler_held"
        } else {
            record["choice"].as_str().unwrap_or("wait")
        });
        record["latency_ms"] = serde_json::json!(admitted.elapsed().as_secs_f64() * 1000.);
        if !self.replay { crate::latency::decided(&turn.id, record["trigger"].as_str()); } // Latency map (lab/latency).
        record["audio"] = audio;
        self.index.evidence.event(
            "send_decision",
            &source,
            std::slice::from_ref(&observation.id),
            end,
            record.clone(),
        )?;
        record["would_wake"] = serde_json::json!(would_wake);
        record["given_to_agent"] = serde_json::json!(true);
        if !audio_spans.is_empty() {
            record["agent_audio_spans"] = serde_json::json!(audio_spans);
        }
        self.index.decision(&observation.id, &record)?;
        // ⌃⌘\ and ⌃⌘' label what V5 saw for this line (hotkeys.rs).
        self.hotkey_decided(&source, &snapshot, &observation.id, &record);
        crate::diagnostics::event_stream(&serde_json::json!({"event":"final","dry_run":self.settings.dry_run,"id":observation.id,"source":observation.source,"speaker":observation.speaker,"text":observation.text,"forced":forced,"end_of_turn_probability":end_of_turn_probability,"decision":record,"would_wake":would_wake}));
        if explicit.is_some() {
            // Decider v3: these words already woke the agent (his name, "send it", a drop or a guest); d1's verdict
            // picks their ack, or nothing (pipeline/ack.rs `late_ack`).
            self.late_ack(snapshot.through, decided_line.as_ref().map(Option::as_deref));
        }
        if would_wake && explicit.is_none() {
            // d1 sent it: for Heimdall, so it may close a card (bridge/questions.rs).
            let speaker = if self.owner_line(&source, &observation.id) { agent::Speaker::Owner } else { agent::Speaker::Other };
            let ack = self.ack_plan(decided_line.as_ref().map(Option::as_deref));
            self.dispatch_pending(&source, &observation.text, snapshot.through, &turn.id, speaker, permit, true, ack)?;
        }
        // Decider V5 learned three jobs (wake, suggest, screen commands): no fixed-command or end-of-turn reply-line
        // question follows the send any more (29 Oct). Suggest asks V5 next (pipeline/suggest.rs).
        if !preempted {
            self.suggest_ask(&source, &snapshot, &record);
        }
        // A turn of his that woke Heimdall, outside a call, with Heimdall silent since it began, is a sample of the
        // user's voice for barge-in (pipeline/heimdall_voice.rs `UserVoice`).
        let in_call = self.in_call_at(end);
        let heimdall_heard = self.tools.heimdall_speaking() || self.tools.send_context.lock().unwrap().finished_ns.is_some_and(|f| f >= start);
        if would_wake && self.primary_mic(&source) && self.owner_line(&source, &observation.id) && !in_call && !heimdall_heard {
            self.user_voice_heard(&turn.audio.samples, false);
        } else if self.line_voice(&observation.id) == Some(&super::mic_voice::Line::Unknown) && record["choice"] == "send"
            && self.primary_mic(&source) && !in_call && !heimdall_heard && decide::send::addressed_by_name(&observation.text) {
            // A line addressing Heimdall by name that d1 would have sent but his print did not match: it only counts
            // toward starting his print again (five that agree with each other: his microphone or voice changed), never
            // as a sample of him. Talk through the speakers rarely addresses Heimdall by name (review 30 Oct).
            self.user_voice_heard(&turn.audio.samples, true);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "send_worker_tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "send_worker_tests/admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
#[path = "send_worker_tests/watchdog_tests.rs"]
mod watchdog_tests;
#[cfg(test)]
#[path = "send_worker_tests/decider_swap_tests.rs"]
mod decider_swap_tests;
#[cfg(test)]
#[path = "send_worker_tests/bounded_foreground_tests.rs"]
mod bounded_foreground_tests;
