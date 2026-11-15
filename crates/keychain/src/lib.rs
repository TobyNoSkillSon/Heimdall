//! Heimdall's one door to the login Keychain. Every secret Heimdall keeps (the T3 pairing, provider keys and Console
//! sign-ins, the ElevenLabs key, the Own Engine endpoint, the friends identity) is read and written here, nowhere else.
//!
//! Why (cleanup night, 13-14 Nov): a bare release binary (`target/release/heimdall --night-replay-smoke`) read the T3
//! pairing; macOS asked for the login password in a dialog nobody could answer, and while it stood every Keychain read
//! of the installed app blocked inside `SecItemCopyMatching` on its live loop. Heimdall stalled from 22:56 to 08:15.
//! Three rules follow, and this crate enforces all three:
//!
//! 1. Who may ask. Only the installed app (`~/Applications/Heimdall.app`, or `/Applications/…` for another user's
//!    install) uses the Keychain. Tests (`cfg(test)`, and any cargo test harness in a `deps` folder) get a private
//!    in-memory store; lab, replay and `--data-dir` runs call `isolate`; `HEIMDALL_SECRET_STORE` (`memory` or an
//!    absolute folder) isolates any process but the installed app, which ignores it. Every other binary gets `Error::Off` at once and nothing is asked.
//! 2. No prompt. The installed app turns Keychain user interaction off for the whole process (the legacy file
//!    keychain's switch) and skips items that need authentication (`kSecUseAuthenticationUISkip`): a read that would
//!    show a dialog fails with errSecInteractionNotAllowed instead.
//! 3. No wait. Each Keychain call runs on its own thread; the caller waits at most `READ_WAIT` (`WRITE_WAIT` for a
//!    write), then gets `Error::Blocked` and the call finishes in the background. Once one call has timed out, later
//!    calls wait only `STUCK_WAIT` (20 ms) until the Keychain answers again, with anything, an error included.
//!    Answers are cached (only Heimdall writes these items), so each item is asked once per run. A refused item is
//!    logged and reported at once through `on_event` (the app shows it on the island); a blocked one only if it still
//!    has not answered `REPORT_AFTER` later, so a launch's slow first reads never flash a problem. Either is reported
//!    until it answers, which is reported too (only when its problem was), and only after a real Keychain answer for
//!    every item of that service that failed.
//!
//! Items another build saved (VM run 14 Nov, upgrade from alpha.1): an ad-hoc signed build cannot read the item an
//! earlier ad-hoc build saved (errSecAuthFailed), yet macOS lets it update that item's data in place, keeping the old
//! build's access list. Saving it again "worked", the cache answered from memory, and every `--provider-token` child
//! (a fresh process, no cache) was refused. So a write never updates an item this build cannot read: it saves the key
//! under the next generation of the account (`account#2`, `#3`, …, numbered after the newest found by one
//! attribute-only listing, no cap), reports saved only after a real read-back, and only then deletes, best effort, the
//! older items it replaces. Reads take the account itself when this build can read it, else the newest generation it
//! can read, so an old unreadable item never shadows the new one and a missing number hides nothing. An item this build
//! can read (a stably signed build's own) is read with one query and updated in place exactly as before; generations
//! exist only after an item another build saved.
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};

/// The longest a caller waits for a read. The live loop's hang report fires after 2.2 s of silence.
pub const READ_WAIT: Duration = Duration::from_millis(1000);
/// The longest a caller waits for a write or delete (a Connect click, a renewed sign-in).
pub const WRITE_WAIT: Duration = Duration::from_secs(3);
/// After a refused read, the item is asked again no sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// A read still running after this long is presumed wedged; one new attempt may start beside it.
const STUCK_AFTER: Duration = Duration::from_secs(600);
/// How long a caller waits while the Keychain is stuck (a call timed out and nothing has answered since).
pub const STUCK_WAIT: Duration = Duration::from_millis(20);
/// A blocked item is reported only if it is still blocked this long after its wait ran out (VM run 14 Nov: at every
/// launch the first read took just over `READ_WAIT`, and five problems opened and closed within a millisecond).
pub const REPORT_AFTER: Duration = Duration::from_secs(2);
/// errSecItemNotFound.
pub const ITEM_NOT_FOUND: i32 = -25300;
/// errSecInteractionNotAllowed: the item needs a password or a prompt, and prompts are off.
pub const INTERACTION_NOT_ALLOWED: i32 = -25308;
/// Isolates any process: `memory`, or an absolute folder for a file store.
pub const ENV: &str = "HEIMDALL_SECRET_STORE";
/// errSecAuthFailed: the item exists, but its access list names another build (an earlier ad-hoc signed one).
pub const AUTH_FAILED: i32 = -25293;

/// The Keychain account of generation `n` (1 is the account itself; later ones exist only after an item another build
/// saved, which this one cannot read). Numbers have no cap.
pub fn generation(account: &str, n: u32) -> String {
    if n <= 1 { account.to_owned() } else { format!("{account}#{n}") }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// This process may not use the Keychain (not the installed app, and no isolated store): nothing was asked.
    Off { service: String, reason: String },
    /// The Keychain did not answer in time (a password dialog or a locked Keychain holds it): the call goes on in the
    /// background, and the feature waits.
    Blocked { service: String },
    /// The Keychain answered with an error (locked, needs a password, denied).
    Refused { service: String, code: i32 },
    /// A file store could not be read or written.
    Failed { service: String, message: String },
}
impl Error {
    pub fn service(&self) -> &str {
        match self {
            Self::Off { service, .. } | Self::Blocked { service } | Self::Refused { service, .. } | Self::Failed { service, .. } => service,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off { service, reason } => write!(
                f,
                "Keychain off for this process ({reason}); \"{service}\" was not read. Only the installed Heimdall reads the \
                 Keychain; lab runs pass --data-dir or {ENV}=memory"
            ),
            Self::Blocked { service } => write!(f, "The Keychain did not answer for \"{service}\" (a password prompt or a locked Keychain holds it)"),
            Self::Refused { service, code: INTERACTION_NOT_ALLOWED } => {
                write!(f, "The Keychain wants a password for \"{service}\"; unlock the login Keychain")
            }
            Self::Refused { service, code } => write!(f, "The Keychain refused \"{service}\" (error {code})"),
            Self::Failed { service, message } => write!(f, "Secret store: \"{service}\": {message}"),
        }
    }
}
impl std::error::Error for Error {}

/// Where secrets live. The Keychain is one; tests and lab runs use the others.
pub trait Backend: Send + Sync {
    /// `Ok(None)` only when the item does not exist.
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, Error>;
    fn write(&self, service: &str, account: &str, label: &str, bytes: &[u8]) -> Result<(), Error>;
    /// A missing item is not an error.
    fn delete(&self, service: &str, account: &str) -> Result<(), Error>;
    /// Every account saved in `service`, from attributes alone (no item is opened, so no access list is asked). A
    /// backend without generations lists none.
    fn accounts(&self, _service: &str) -> Result<Vec<String>, Error> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keychain,
    Memory,
    Files,
    Off,
    Custom,
}

/// What `on_event` hears: an item stopped answering, or answers again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Degraded { service: String, reason: String, cause: Cause },
    Recovered { service: String },
}

/// Why an item does not answer, as the user can act on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cause {
    /// No answer in time: a password prompt or a locked Keychain holds it.
    Blocked,
    /// errSecInteractionNotAllowed: the Keychain wants a password (locked, or the item asks before this app may read it).
    Locked,
    /// Any other refusal (errSecAuthFailed, no access): the item exists but this app may not read it, as when an
    /// earlier build saved it and macOS no longer trusts the new signature. Unlocking does not help; saving it again
    /// does.
    Unreadable,
    /// A file store failed.
    Failed,
}
impl Cause {
    pub fn of(e: &Error) -> Self {
        match e {
            Error::Blocked { .. } => Self::Blocked,
            Error::Refused { code: INTERACTION_NOT_ALLOWED, .. } => Self::Locked,
            Error::Refused { .. } => Self::Unreadable,
            Error::Off { .. } | Error::Failed { .. } => Self::Failed,
        }
    }
}

type Notify = Arc<dyn Fn(&Event) + Send + Sync>;
type Key = (String, String);

enum Slot {
    Known(Option<Vec<u8>>),
    Pending(Instant),
    Failed(Error, Instant),
}

#[derive(Default)]
struct State {
    slots: HashMap<Key, Slot>,
    /// Services blocked or refused now, and why.
    degraded: HashMap<String, Cause>,
    /// Of those, the ones reported through `on_event` (a blocked one waits `report_after` first), with the cause told.
    reported: HashMap<String, Cause>,
    logged_off: HashSet<String>,
}

struct Shared {
    kind: Kind,
    backend: Arc<dyn Backend>,
    state: Mutex<State>,
    notify: RwLock<Option<Notify>>,
    read_wait: Duration,
    write_wait: Duration,
    report_after: Duration,
    /// A call timed out and the Keychain has not answered since: callers wait only `STUCK_WAIT`.
    stuck: AtomicBool,
    /// Writes and deletes run one at a time, in the order they were asked, so the cache settles as the Keychain does.
    changes: Mutex<()>,
    /// Held from a report's state change through its event, so listeners hear problems and recoveries in the order
    /// they happened (a recovery never overtakes its problem). Listeners must not call the store.
    events: Mutex<()>,
}

/// A secret store with the cache, the waits and the reporting. Cheap to clone.
#[derive(Clone)]
pub struct Store(Arc<Shared>);

impl Store {
    fn build(kind: Kind, backend: Arc<dyn Backend>) -> Self {
        Self(Arc::new(Shared {
            kind,
            backend,
            state: Mutex::new(State::default()),
            notify: RwLock::new(None),
            read_wait: READ_WAIT,
            write_wait: WRITE_WAIT,
            report_after: REPORT_AFTER,
            stuck: AtomicBool::new(false),
            changes: Mutex::new(()),
            events: Mutex::new(()),
        }))
    }
    /// A private store for this process's lifetime.
    pub fn memory() -> Self {
        Self::build(Kind::Memory, Arc::new(Memory::default()))
    }
    /// One owner-only file per item in `dir` (a lab run's `--data-dir`), kept across runs of that lab profile.
    pub fn files(dir: impl Into<PathBuf>) -> Self {
        Self::build(Kind::Files, Arc::new(Files(dir.into())))
    }
    /// Refuses everything at once with `Error::Off`.
    pub fn off(reason: impl Into<String>) -> Self {
        Self::build(Kind::Off, Arc::new(Off(reason.into())))
    }
    /// Any backend (tests: a Keychain that never answers).
    pub fn custom(backend: Arc<dyn Backend>) -> Self {
        Self::build(Kind::Custom, backend)
    }
    #[cfg(target_os = "macos")]
    fn keychain() -> Self {
        Self::build(Kind::Keychain, Arc::new(mac::Keychain::new()))
    }
    #[cfg(not(target_os = "macos"))]
    fn keychain() -> Self {
        Self::off("no Keychain on this system")
    }
    /// The same store with other waits (tests keep them short).
    pub fn with_waits(self, read: Duration, write: Duration) -> Self {
        let s = Arc::try_unwrap(self.0).unwrap_or_else(|_| panic!("with_waits before the store is shared"));
        Self(Arc::new(Shared { read_wait: read, write_wait: write, ..s }))
    }
    /// The same store with another grace before a blocked item is reported (tests keep it short).
    pub fn with_report_after(self, after: Duration) -> Self {
        let s = Arc::try_unwrap(self.0).unwrap_or_else(|_| panic!("with_report_after before the store is shared"));
        Self(Arc::new(Shared { report_after: after, ..s }))
    }
    /// This store's own listener (the global store's is `on_event`).
    pub fn on_event(&self, f: impl Fn(&Event) + Send + Sync + 'static) {
        *self.0.notify.write().unwrap() = Some(Arc::new(f));
    }
    pub fn kind(&self) -> Kind {
        self.0.kind
    }
    /// Services that are blocked or refused right now.
    pub fn degraded(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.state.lock().unwrap().degraded.keys().cloned().collect();
        v.sort();
        v
    }
    /// Why `service` does not answer, once that has been reported (a blocked item only after `REPORT_AFTER`).
    pub fn problem(&self, service: &str) -> Option<Cause> {
        let st = self.0.state.lock().unwrap();
        st.reported.contains_key(service).then(|| st.degraded.get(service).copied()).flatten()
    }

    pub fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, Error> {
        let key: Key = (service.into(), account.into());
        {
            let mut st = self.0.state.lock().unwrap();
            match st.slots.get(&key) {
                Some(Slot::Known(v)) => return Ok(v.clone()),
                Some(Slot::Pending(since)) if since.elapsed() < STUCK_AFTER => {
                    drop(st);
                    return Err(Shared::degrade(&self.0, Error::Blocked { service: service.into() }));
                }
                Some(Slot::Failed(e, at)) if at.elapsed() < RETRY_AFTER => return Err(e.clone()),
                _ => {}
            }
            st.slots.insert(key.clone(), Slot::Pending(Instant::now()));
        }
        let shared = self.0.clone();
        Shared::run(&self.0, service, self.0.read_wait, move || {
            let result = guarded(&key.0, || read_newest(&*shared.backend, &key.0, &key.1));
            Shared::settle_read(&shared, &key, &result);
            result
        })
    }

    pub fn write(&self, service: &str, account: &str, label: &str, bytes: &[u8]) -> Result<(), Error> {
        let (shared, key, label, bytes) = (self.0.clone(), (service.to_owned(), account.to_owned()), label.to_owned(), bytes.to_vec());
        Shared::run(&self.0, service, self.0.write_wait, move || {
            let _one_at_a_time = shared.changes.lock().unwrap_or_else(|p| p.into_inner());
            let result = guarded(&key.0, || write_verified(&*shared.backend, &key.0, &key.1, &label, &bytes));
            shared.settle_change(&key, &result, Some(bytes));
            result
        })
    }

    pub fn delete(&self, service: &str, account: &str) -> Result<(), Error> {
        let (shared, key) = (self.0.clone(), (service.to_owned(), account.to_owned()));
        Shared::run(&self.0, service, self.0.write_wait, move || {
            let _one_at_a_time = shared.changes.lock().unwrap_or_else(|p| p.into_inner());
            let result = guarded(&key.0, || delete_every(&*shared.backend, &key.0, &key.1));
            shared.settle_change(&key, &result, None);
            result
        })
    }
}

impl Shared {
    /// Runs `op` on its own thread and waits for it at most `wait` (not at all while the Keychain is stuck).
    fn run<T: Send + 'static>(this: &Arc<Self>, service: &str, wait: Duration, op: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, Error> {
        let (tx, rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new().name("keychain".into()).spawn(move || {
            let _ = tx.send(op());
        });
        if let Err(e) = spawned {
            return Err(Error::Failed { service: service.into(), message: format!("no thread for the Keychain: {e}") });
        }
        let wait = if this.stuck.load(Ordering::Acquire) { STUCK_WAIT } else { wait };
        match rx.recv_timeout(wait) {
            Ok(result) => this.got(result),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::Failed { service: service.into(), message: "the Keychain call ended without an answer".into() })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                this.stuck.store(true, Ordering::Release);
                // The answer may have landed between the timeout and the flag: then it counts, and the flag clears.
                match rx.try_recv() {
                    Ok(result) => this.got(result),
                    Err(_) => Err(Self::degrade(this, Error::Blocked { service: service.into() })),
                }
            }
        }
    }

    /// Any answer from the backend, an error included, means it is not stuck.
    fn got<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        self.stuck.store(false, Ordering::Release);
        result
    }

    fn settle_read(this: &Arc<Self>, key: &Key, result: &Result<Option<Vec<u8>>, Error>) {
        this.stuck.store(false, Ordering::Release);
        let mut st = this.state.lock().unwrap();
        // A write that landed while this read was out wins.
        if !matches!(st.slots.get(key), Some(Slot::Pending(_)) | None) {
            return;
        }
        match result {
            Ok(v) => {
                st.slots.insert(key.clone(), Slot::Known(v.clone()));
                let others = Self::others_failing(&st, key);
                drop(st);
                if !others {
                    this.answered(&key.0);
                }
            }
            Err(e) => {
                st.slots.insert(key.clone(), Slot::Failed(e.clone(), Instant::now()));
                drop(st);
                Self::degrade(this, e.clone());
            }
        }
    }

    fn settle_change(&self, key: &Key, result: &Result<(), Error>, value: Option<Vec<u8>>) {
        self.stuck.store(false, Ordering::Release);
        let mut st = self.state.lock().unwrap();
        match result {
            // A write is Ok only after a real read-back (`write_verified`), so the cache holds what the Keychain does.
            Ok(()) => {
                st.slots.insert(key.clone(), Slot::Known(value));
                let others = Self::others_failing(&st, key);
                drop(st);
                if !others {
                    self.answered(&key.0);
                }
            }
            // Unknown now: the next read asks again. The error goes back to whoever clicked.
            Err(_) => {
                st.slots.remove(key);
            }
        }
    }

    /// Whether another account of `key`'s service is still refused or unanswered (VM run 14 Nov: a missing sign-in
    /// item read fine and logged "answers again" while the saved key beside it stayed unreadable).
    fn others_failing(st: &State, key: &Key) -> bool {
        st.slots.iter().any(|(k, slot)| k.0 == key.0 && k.1 != key.1 && matches!(slot, Slot::Pending(_) | Slot::Failed(..)))
    }

    /// The Keychain answered for `service`: report the recovery once, if its problem was reported.
    fn answered(&self, service: &str) {
        self.stuck.store(false, Ordering::Release);
        let _order = self.events.lock().unwrap_or_else(|p| p.into_inner());
        let reported = {
            let mut st = self.state.lock().unwrap();
            st.degraded.remove(service).is_some() && st.reported.remove(service).is_some()
        };
        if reported {
            eprintln!("keychain: \"{service}\" answers again");
            self.emit(&Event::Recovered { service: service.into() });
        }
    }

    /// Logs and reports a failure once per service until the item answers again: a refusal at once, a timeout only if
    /// the item still has not answered `report_after` later (a slow first read is no problem).
    fn degrade(this: &Arc<Self>, e: Error) -> Error {
        let service = e.service().to_owned();
        if matches!(e, Error::Off { .. }) {
            if this.state.lock().unwrap().logged_off.insert(service) {
                eprintln!("keychain: {e}");
            }
            return e;
        }
        let cause = Cause::of(&e);
        {
            let mut st = this.state.lock().unwrap();
            match st.degraded.get(&service) {
                // Already known; a refusal after a timeout changes the cause, and is reported below (a pill that
                // said "not answering" is replaced by one with the refusal's fix).
                Some(c) if *c == cause || cause == Cause::Blocked => return e,
                _ => {}
            }
            st.degraded.insert(service.clone(), cause);
        }
        if cause == Cause::Blocked && !this.report_after.is_zero() {
            let (shared, reason, after) = (this.clone(), e.to_string(), this.report_after);
            let spawned = std::thread::Builder::new().name("keychain-report".into()).spawn(move || {
                std::thread::sleep(after);
                shared.report(&service, cause, &reason);
            });
            if spawned.is_err() {
                this.report(e.service(), cause, &e.to_string());
            }
        } else {
            this.report(&service, cause, &e.to_string());
        }
        e
    }

    /// Reports `service` with `cause`, if that is still its state and not told yet. A cause that changed since is
    /// reported by whoever changed it.
    fn report(&self, service: &str, cause: Cause, reason: &str) {
        let _order = self.events.lock().unwrap_or_else(|p| p.into_inner());
        {
            let mut st = self.state.lock().unwrap();
            if st.degraded.get(service) != Some(&cause) || st.reported.get(service) == Some(&cause) {
                return;
            }
            st.reported.insert(service.to_owned(), cause);
        }
        eprintln!("keychain: {reason}; that feature waits until it answers");
        self.emit(&Event::Degraded { service: service.into(), reason: reason.into(), cause });
    }

    fn emit(&self, event: &Event) {
        let f = self.notify.read().unwrap().clone().or_else(|| GLOBAL_NOTIFY.read().unwrap().clone());
        if let Some(f) = f {
            f(event);
        }
    }
}

/// A backend that panics answers with `Error::Failed`, so its slot settles instead of staying Pending.
fn guarded<T>(service: &str, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(Error::Failed { service: service.into(), message: "the secret store panicked".into() }))
}

// ---------------------------------------------------------------------------------------------------------
// Generations: what a read, write or delete asks the backend (see the module notes).

/// An item another build saved: this one may not read it (and may not be allowed to delete it).
fn other_builds(e: &Error) -> bool {
    matches!(e, Error::Refused { code: AUTH_FAILED, .. })
}

/// The later generations of `a` in `s`, newest first, from one attribute-only listing.
fn generations(b: &dyn Backend, s: &str, a: &str) -> Result<Vec<(u32, String)>, Error> {
    let prefix = format!("{a}#");
    let mut found: Vec<(u32, String)> = b
        .accounts(s)?
        .into_iter()
        .filter_map(|account| {
            let n: u32 = account.strip_prefix(&prefix)?.parse().ok()?;
            (n >= 2 && generation(a, n) == account).then_some((n, account))
        })
        .collect();
    found.sort_by(|x, y| y.0.cmp(&x.0));
    found.dedup();
    Ok(found)
}

/// The account itself if this build can read it (one query, as before generations), else the newest generation it
/// can read, found by listing, so a missing number never hides a later one. Nothing readable: the first refusal, else
/// none.
fn read_newest(b: &dyn Backend, s: &str, a: &str) -> Result<Option<Vec<u8>>, Error> {
    let mut refused = match b.read(s, a) {
        Ok(Some(v)) => return Ok(Some(v)),
        Ok(None) => None,
        Err(e @ Error::Refused { .. }) => Some(e),
        Err(e) => return Err(e),
    };
    let later = match generations(b, s, a) {
        Ok(later) => later,
        Err(_) => return refused.map_or(Ok(None), Err),
    };
    for (_, account) in later {
        match b.read(s, &account) {
            Ok(Some(v)) => return Ok(Some(v)),
            Ok(None) => {}
            Err(e @ Error::Refused { .. }) => {
                refused.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
    }
    refused.map_or(Ok(None), Err)
}

/// Saves `bytes` where this build can read them back, and says Ok only after it did (a real read, no cache). An item
/// another build saved is never updated in place (that would keep its access list) and is never deleted before the
/// new item reads back.
fn write_verified(b: &dyn Backend, s: &str, a: &str, label: &str, bytes: &[u8]) -> Result<(), Error> {
    match b.read(s, a) {
        // Missing, or this build's own: add or update in place (a stably signed build's items always land here).
        Ok(found) => {
            b.write(s, a, label, bytes)?;
            match read_back(b, s, a, bytes) {
                Ok(()) => {
                    retire(b, s, a, 1);
                    return Ok(());
                }
                // A new item this build cannot read: a generation would fare no better.
                Err(e) if found.is_none() => return Err(e),
                Err(_) => {}
            }
        }
        Err(Error::Refused { .. }) => {}
        Err(e) => return Err(e),
    }
    // The account belongs to another build. This build's own generation, if it has one, is updated in place;
    // otherwise the key goes to the number after the newest.
    let later = generations(b, s, a)?;
    let own = later.iter().find(|(_, account)| matches!(b.read(s, account), Ok(Some(_)))).map(|(n, _)| *n);
    let n = own.unwrap_or_else(|| later.first().map_or(2, |(newest, _)| newest + 1));
    let account = generation(a, n);
    b.write(s, &account, label, bytes)?;
    read_back(b, s, &account, bytes)?;
    retire(b, s, a, n);
    Ok(())
}

fn read_back(b: &dyn Backend, s: &str, account: &str, bytes: &[u8]) -> Result<(), Error> {
    match b.read(s, account)? {
        Some(v) if v == bytes => Ok(()),
        _ => Err(Error::Failed { service: s.into(), message: "saved, but reading it back gave something else".into() }),
    }
}

/// Once generation `kept` reads back, removes best effort the items it replaces: this build's own stale copies, and
/// other builds' items older than `kept` (if macOS lets this build delete them). Another build's newer item stays.
fn retire(b: &dyn Backend, s: &str, a: &str, kept: u32) {
    let mut all = vec![(1, a.to_owned())];
    all.extend(generations(b, s, a).unwrap_or_default());
    for (n, account) in all.into_iter().filter(|(n, _)| *n != kept) {
        match b.read(s, &account) {
            Ok(Some(_)) => {
                let _ = b.delete(s, &account);
            }
            Err(e) if other_builds(&e) && n < kept => {
                let _ = b.delete(s, &account);
            }
            _ => {}
        }
    }
}

/// Deletes the account and every generation. A refusal fails the delete as before, except for another build's item
/// (errSecAuthFailed) that macOS will not let this build delete: this build cannot read it anyway.
fn delete_every(b: &dyn Backend, s: &str, a: &str) -> Result<(), Error> {
    let mut all = vec![a.to_owned()];
    all.extend(generations(b, s, a)?.into_iter().map(|(_, account)| account));
    for account in all {
        if let Err(e) = b.delete(s, &account) {
            match b.read(s, &account) {
                Err(r) if other_builds(&r) => {}
                _ => return Err(e),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// The process's store.

static CHOSEN: RwLock<Option<Store>> = RwLock::new(None);
static GLOBAL_NOTIFY: RwLock<Option<Notify>> = RwLock::new(None);
thread_local! {
    static SCOPED: RefCell<Option<Store>> = const { RefCell::new(None) };
}

/// An isolated store for a lab, replay or `--data-dir` run. Call it before anything reads a secret; it replaces the
/// process's store, so it can only take the Keychain away, never give it.
pub enum Isolation {
    Memory,
    Dir(PathBuf),
}
pub fn isolate(isolation: Isolation) -> Store {
    let store = match isolation {
        Isolation::Memory => Store::memory(),
        Isolation::Dir(dir) => Store::files(dir),
    };
    *CHOSEN.write().unwrap() = Some(store.clone());
    store
}

/// The store this thread uses: a `scoped` one, else the process's (chosen on first use, see the module notes).
pub fn store() -> Store {
    if let Some(s) = SCOPED.with(|s| s.borrow().clone()) {
        return s;
    }
    if let Some(s) = CHOSEN.read().unwrap().clone() {
        return s;
    }
    CHOSEN.write().unwrap().get_or_insert_with(automatic).clone()
}

/// Runs `f` with `store` as this thread's store (tests: a Keychain that never answers, without touching other tests).
pub fn scoped<R>(store: &Store, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Store>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            SCOPED.with(|s| *s.borrow_mut() = previous);
        }
    }
    let _restore = Restore(SCOPED.with(|s| s.borrow_mut().replace(store.clone())));
    f()
}

/// The listener for the process's stores (the app: an island notice per blocked feature).
pub fn on_event(f: impl Fn(&Event) + Send + Sync + 'static) {
    *GLOBAL_NOTIFY.write().unwrap() = Some(Arc::new(f));
}

pub fn read(service: &str, account: &str) -> Result<Option<Vec<u8>>, Error> {
    store().read(service, account)
}
pub fn write(service: &str, account: &str, label: &str, bytes: &[u8]) -> Result<(), Error> {
    store().write(service, account, label, bytes)
}
pub fn delete(service: &str, account: &str) -> Result<(), Error> {
    store().delete(service, account)
}
pub fn kind() -> Kind {
    store().kind()
}

/// The installed app's bundle: Heimdall.app (personal) or Heimdall Alpha.app (public, `scripts/bundle.sh`).
pub const APP_BUNDLE: &str = if cfg!(feature = "public") { "Heimdall Alpha.app" } else { "Heimdall.app" };

/// Whether this process is the installed app (by its path; see `installed_at`).
pub fn installed_app() -> bool {
    std::env::current_exe().is_ok_and(|p| installed_at(&p, home().as_deref()))
}
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The installed app runs from `~/Applications` (the user's install, the night pass) or `/Applications` (another user's
/// install of the public edition); a build, test or lab copy runs from anywhere else.
pub fn installed_at(exe: &Path, home: Option<&Path>) -> bool {
    let macos = Path::new(APP_BUNDLE).join("Contents/MacOS");
    exe.starts_with(Path::new("/Applications").join(&macos)) || home.is_some_and(|h| exe.starts_with(h.join("Applications").join(&macos)))
}

/// Which store a process gets when nothing isolated it: pure, so the rule itself is tested.
pub fn choose(exe: Option<&Path>, home: Option<&Path>, env: Option<&str>) -> (Kind, String) {
    // The installed app always uses the login Keychain: a stray HEIMDALL_SECRET_STORE in its environment must not make
    // it keep the user's keys in memory or a file (keychain-safe review finding 10). Lab runs use other copies.
    if exe.is_some_and(|e| installed_at(e, home)) {
        return (Kind::Keychain, String::new());
    }
    if let Some(v) = env {
        return match v {
            "memory" => (Kind::Memory, String::new()),
            dir if Path::new(dir).is_absolute() => (Kind::Files, dir.into()),
            other => (Kind::Off, format!("{ENV}={other:?} is neither \"memory\" nor an absolute folder")),
        };
    }
    match exe {
        Some(e) if installed_at(e, home) => (Kind::Keychain, String::new()),
        // cargo's test harnesses (target/<profile>/deps/<crate>-<hash>): every crate's tests, not only this one's.
        Some(e) if e.parent().and_then(Path::file_name).is_some_and(|n| n == "deps") => (Kind::Memory, String::new()),
        Some(e) => (Kind::Off, format!("{} is not the installed Heimdall", e.display())),
        None => (Kind::Off, "its own path is unknown".into()),
    }
}

fn automatic() -> Store {
    if cfg!(test) {
        return Store::memory();
    }
    let exe = std::env::current_exe().ok();
    let env = std::env::var(ENV).ok();
    match choose(exe.as_deref(), home().as_deref(), env.as_deref()) {
        (Kind::Keychain, _) => Store::keychain(),
        (Kind::Memory, _) => Store::memory(),
        (Kind::Files, dir) => Store::files(dir),
        (_, reason) => Store::off(reason),
    }
}

// ---------------------------------------------------------------------------------------------------------
// Backends.

#[derive(Default)]
struct Memory(Mutex<HashMap<Key, Vec<u8>>>);
impl Backend for Memory {
    fn read(&self, s: &str, a: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.lock().unwrap().get(&(s.to_owned(), a.to_owned())).cloned())
    }
    fn write(&self, s: &str, a: &str, _label: &str, bytes: &[u8]) -> Result<(), Error> {
        self.0.lock().unwrap().insert((s.into(), a.into()), bytes.to_vec());
        Ok(())
    }
    fn delete(&self, s: &str, a: &str) -> Result<(), Error> {
        self.0.lock().unwrap().remove(&(s.to_owned(), a.to_owned()));
        Ok(())
    }
    fn accounts(&self, s: &str) -> Result<Vec<String>, Error> {
        Ok(self.0.lock().unwrap().keys().filter(|k| k.0 == s).map(|k| k.1.clone()).collect())
    }
}

struct Off(String);
impl Off {
    fn err(&self, s: &str) -> Error {
        Error::Off { service: s.into(), reason: self.0.clone() }
    }
}
impl Backend for Off {
    fn read(&self, s: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        Err(self.err(s))
    }
    fn write(&self, s: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        Err(self.err(s))
    }
    fn delete(&self, s: &str, _: &str) -> Result<(), Error> {
        Err(self.err(s))
    }
    fn accounts(&self, s: &str) -> Result<Vec<String>, Error> {
        Err(self.err(s))
    }
}

struct Files(PathBuf);
impl Files {
    fn path(&self, s: &str, a: &str) -> PathBuf {
        self.0.join(format!("{}.{}", hex(s), hex(a)))
    }
}
fn failed(s: &str, e: std::io::Error) -> Error {
    Error::Failed { service: s.into(), message: e.to_string() }
}
impl Backend for Files {
    fn read(&self, s: &str, a: &str) -> Result<Option<Vec<u8>>, Error> {
        match std::fs::read(self.path(s, a)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed(s, e)),
        }
    }
    fn write(&self, s: &str, a: &str, _label: &str, bytes: &[u8]) -> Result<(), Error> {
        use std::{io::Write, os::unix::fs::{DirBuilderExt, OpenOptionsExt}};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.0).map_err(|e| failed(s, e))?;
        let path = self.path(s, a);
        // One temporary per item (`with_extension` would share one per service across accounts).
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| failed(s, e))?;
        f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| failed(s, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| failed(s, e))
    }
    fn delete(&self, s: &str, a: &str) -> Result<(), Error> {
        match std::fs::remove_file(self.path(s, a)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(failed(s, e)),
        }
    }
    fn accounts(&self, s: &str) -> Result<Vec<String>, Error> {
        let prefix = format!("{}.", hex(s));
        let entries = match std::fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(failed(s, e)),
        };
        Ok(entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|name| unhex(name.strip_prefix(&prefix)?))
            .collect())
    }
}
fn hex(t: &str) -> String {
    t.bytes().map(|b| format!("{b:02x}")).collect()
}
/// The account from a file name's hex half; a temporary (`….tmp`) or foreign name is none.
fn unhex(h: &str) -> Option<String> {
    if h.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{Backend, Error, ITEM_NOT_FOUND};
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};

    pub(super) struct Keychain;
    /// Prompts are off for this process. Then a protected item answers errSecInteractionNotAllowed (a refusal the
    /// feature reports), which is better than skipping it: a skipped item reads as missing, and T3 would pair again.
    static PROMPTS_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    fn skip() -> bool {
        !PROMPTS_OFF.load(std::sync::atomic::Ordering::Acquire)
    }
    impl Keychain {
        /// Turns Keychain user interaction off for this process, once and for good: from here on a Keychain call that
        /// would show a dialog (an ACL prompt, an unlock prompt) fails with errSecInteractionNotAllowed instead.
        pub(super) fn new() -> Self {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| match security_framework::os::macos::keychain::SecKeychain::disable_user_interaction() {
                // The lock turns interaction back on when dropped; it never is.
                Ok(lock) => {
                    std::mem::forget(lock);
                    PROMPTS_OFF.store(true, std::sync::atomic::Ordering::Release);
                }
                Err(e) => eprintln!("keychain: could not turn Keychain prompts off ({e}); reads skip items that need one"),
            });
            Self
        }
    }
    fn refused(s: &str, e: security_framework::base::Error) -> Error {
        Error::Refused { service: s.into(), code: e.code() }
    }
    impl Backend for Keychain {
        fn read(&self, s: &str, a: &str) -> Result<Option<Vec<u8>>, Error> {
            let found = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(s)
                .account(a)
                .load_data(true)
                .skip_authenticated_items(skip())
                .search();
            match found {
                Ok(items) => Ok(items.into_iter().find_map(|i| match i {
                    SearchResult::Data(bytes) => Some(bytes),
                    _ => None,
                })),
                Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
                Err(e) => Err(refused(s, e)),
            }
        }
        /// Add, or update in place (the item is never gone between two steps). The store calls this only for an item
        /// this build can read or a missing one (`write_verified`): an update in place of another build's item would
        /// keep that build's access list, and this one still could not read it.
        fn write(&self, s: &str, a: &str, label: &str, bytes: &[u8]) -> Result<(), Error> {
            let mut options = security_framework::passwords::PasswordOptions::new_generic_password(s, a);
            options.set_label(label);
            security_framework::passwords::set_generic_password_options(bytes, options).map_err(|e| refused(s, e))
        }
        /// kSecReturnAttributes with kSecMatchLimitAll and no data: the accounts only, so no item's access list is
        /// asked (and prompts are off besides).
        fn accounts(&self, s: &str) -> Result<Vec<String>, Error> {
            let found = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(s)
                .load_attributes(true)
                .limit(Limit::All)
                .skip_authenticated_items(skip())
                .search();
            match found {
                Ok(items) => Ok(items.iter().filter_map(|i| i.simplify_dict()?.remove("acct")).collect()),
                Err(e) if e.code() == ITEM_NOT_FOUND => Ok(Vec::new()),
                Err(e) => Err(refused(s, e)),
            }
        }
        fn delete(&self, s: &str, a: &str) -> Result<(), Error> {
            match ItemSearchOptions::new().class(ItemClass::generic_password()).service(s).account(a).skip_authenticated_items(skip()).delete() {
                Ok(()) => Ok(()),
                Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
                Err(e) => Err(refused(s, e)),
            }
        }
    }
}

#[cfg(test)]
mod tests;
