use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize};

/// A Keychain that never answers: every call waits on a gate that opens only when the test says so (13 Nov: every
/// read of the installed app sat in `SecItemCopyMatching` behind a password dialog nobody could answer).
#[derive(Default)]
struct Wedged {
    open: Mutex<bool>,
    turn: std::sync::Condvar,
    calls: AtomicUsize,
    value: Mutex<Option<Vec<u8>>>,
}
impl Wedged {
    fn wait(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.turn.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.turn.notify_all();
    }
}
impl Backend for Wedged {
    fn read(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        self.wait();
        Ok(self.value.lock().unwrap().clone())
    }
    fn write(&self, _: &str, _: &str, _: &str, b: &[u8]) -> Result<(), Error> {
        self.wait();
        *self.value.lock().unwrap() = Some(b.to_vec());
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        self.wait();
        Ok(())
    }
}

fn events(store: &Store) -> Arc<Mutex<Vec<Event>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    store.on_event(move |e| s.lock().unwrap().push(e.clone()));
    seen
}

#[test]
fn a_keychain_that_never_answers_never_holds_the_loop() {
    let wedged = Arc::new(Wedged::default());
    *wedged.value.lock().unwrap() = Some(b"token".to_vec());
    let store = Store::custom(wedged.clone()).with_waits(Duration::from_millis(100), Duration::from_millis(100)).with_report_after(Duration::from_millis(50));
    let seen = events(&store);
    // A stand-in for the live loop: one heartbeat per turn, and every turn reads two items (as model_options did).
    let beats = Arc::new(AtomicU64::new(0));
    let (b, s) = (beats.clone(), store.clone());
    let lp = std::thread::spawn(move || {
        let mut answers = Vec::new();
        for _ in 0..50 {
            answers.push(s.read("dev.heimdall.t3", "oauth"));
            answers.push(s.read("Heimdall API provider: Anthropic", "anthropic-console"));
            b.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
        }
        answers
    });
    let started = Instant::now();
    let answers = lp.join().unwrap();
    // 50 turns of 5 ms plus one 100 ms wait: the loop never waits on the Keychain again once it is stuck.
    assert!(started.elapsed() < Duration::from_millis(1500), "the loop waited {:?}", started.elapsed());
    assert_eq!(beats.load(Ordering::SeqCst), 50);
    assert!(answers.iter().all(|a| matches!(a, Err(Error::Blocked { .. }))), "{answers:?}");
    // Each item was asked once, not once per turn.
    assert_eq!(wedged.calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.degraded(), ["Heimdall API provider: Anthropic", "dev.heimdall.t3"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(store.problem("dev.heimdall.t3"), Some(Cause::Blocked));
    let reported: Vec<_> = seen.lock().unwrap().clone();
    assert_eq!(reported.len(), 2, "logged and reported once per service: {reported:?}");
    assert!(reported.iter().all(|e| matches!(e, Event::Degraded { .. })));

    // The Keychain answers at last (08:15): the stuck reads finish, fill the cache and report the recovery.
    wedged.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !store.degraded().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(store.degraded().is_empty());
    assert_eq!(store.read("dev.heimdall.t3", "oauth").unwrap().as_deref(), Some(&b"token"[..]));
    assert_eq!(seen.lock().unwrap().iter().filter(|e| matches!(e, Event::Recovered { .. })).count(), 2);
    assert_eq!(wedged.calls.load(Ordering::SeqCst), 2, "the answer came from the cache");
}

#[test]
fn a_blocked_write_returns_in_time_and_lands_later() {
    let wedged = Arc::new(Wedged::default());
    let store = Store::custom(wedged.clone()).with_waits(Duration::from_millis(50), Duration::from_millis(50));
    let t = Instant::now();
    assert!(matches!(store.write("s", "a", "s", b"v"), Err(Error::Blocked { .. })));
    assert!(t.elapsed() < Duration::from_millis(500));
    wedged.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.read("s", "a").ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(store.read("s", "a").unwrap().as_deref(), Some(&b"v"[..]));
}

struct Refusing(AtomicUsize);
impl Backend for Refusing {
    fn read(&self, s: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::Refused { service: s.into(), code: INTERACTION_NOT_ALLOWED })
    }
    fn write(&self, s: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        Err(Error::Refused { service: s.into(), code: INTERACTION_NOT_ALLOWED })
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

#[test]
fn a_refused_read_is_reported_once_and_not_asked_again_at_once() {
    let backend = Arc::new(Refusing(AtomicUsize::new(0)));
    let store = Store::custom(backend.clone());
    let seen = events(&store);
    for _ in 0..10 {
        let e = store.read("Heimdall API provider: ElevenLabs", "elevenlabs").unwrap_err();
        assert!(e.to_string().contains("unlock the login Keychain"), "{e}");
    }
    assert_eq!(backend.0.load(Ordering::SeqCst), 1, "retried only after RETRY_AFTER");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn off_refuses_at_once_and_names_the_rule() {
    let store = Store::off("/x/target/release/heimdall is not the installed Heimdall");
    let t = Instant::now();
    let e = store.read("dev.heimdall.t3", "oauth").unwrap_err();
    assert!(t.elapsed() < Duration::from_millis(200));
    assert!(matches!(e, Error::Off { .. }));
    assert!(e.to_string().contains("Only the installed Heimdall reads the Keychain"), "{e}");
    assert!(store.write("dev.heimdall.t3", "oauth", "l", b"x").is_err());
    assert!(store.degraded().is_empty(), "off is by design, not a degraded feature");
}

#[test]
fn only_the_installed_bundle_gets_the_keychain() {
    let home = Path::new("/Users/t");
    let installed = Path::new("/Users/t/Applications").join(APP_BUNDLE).join("Contents/MacOS/Heimdall");
    assert_eq!(choose(Some(&installed), Some(home), None).0, Kind::Keychain);
    let system = Path::new("/Applications").join(APP_BUNDLE).join("Contents/MacOS/Heimdall");
    assert_eq!(choose(Some(&system), Some(home), None).0, Kind::Keychain);
    // 13 Nov's binary, a worktree build, a bundle copied anywhere else, /tmp: refused, never asked.
    for bare in [
        "/Users/t/Work/Projects/Heimdall/target/release/heimdall",
        "/Users/t/Work/Projects/Heimdall/.claude/worktrees/x/target/release/heimdall",
        "/Users/t/Work/Projects/Heimdall/target/bundle/Heimdall.app/Contents/MacOS/Heimdall",
        "/tmp/Heimdall.app/Contents/MacOS/Heimdall",
    ] {
        let (kind, reason) = choose(Some(Path::new(bare)), Some(home), None);
        assert_eq!(kind, Kind::Off, "{bare}");
        assert!(reason.contains("not the installed Heimdall"), "{reason}");
    }
    // cargo test harnesses: memory.
    assert_eq!(choose(Some(Path::new("/r/target/test/debug/deps/agent-0123abcd")), Some(home), None).0, Kind::Memory);
    // The installed app ignores the variable (review finding 10); any other copy honours it.
    assert_eq!(choose(Some(&installed), Some(home), Some("memory")).0, Kind::Keychain);
    assert_eq!(choose(Some(&installed), Some(home), Some("/tmp/lab/secrets")).0, Kind::Keychain);
    let lab = Path::new("/r/target/release/heimdall");
    assert_eq!(choose(Some(lab), Some(home), Some("memory")).0, Kind::Memory);
    assert_eq!(choose(Some(lab), Some(home), Some("/tmp/lab/secrets")).0, Kind::Files);
    assert_eq!(choose(Some(lab), Some(home), Some("keychain")).0, Kind::Off);
}

#[test]
fn this_test_binary_never_reaches_the_keychain() {
    assert!(!installed_app());
    assert_eq!(kind(), Kind::Memory);
    write("dev.heimdall.keychain-test", "a", "l", b"x").unwrap();
    assert_eq!(read("dev.heimdall.keychain-test", "a").unwrap().as_deref(), Some(&b"x"[..]));
}

#[test]
fn a_file_store_keeps_items_owner_only_across_stores() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("lab-secrets");
    Store::files(&secrets).write("dev.heimdall.t3", "oauth", "l", b"{}").unwrap();
    let again = Store::files(&secrets);
    assert_eq!(again.read("dev.heimdall.t3", "oauth").unwrap().as_deref(), Some(&b"{}"[..]));
    assert_eq!(again.read("dev.heimdall.t3", "other").unwrap(), None);
    std::fs::write(secrets.join(format!("{}.tmp", hex("dev.heimdall.t3"))), b"").unwrap();
    assert_eq!(Files(secrets.clone()).accounts("dev.heimdall.t3").unwrap(), ["oauth"], "one listing, no temporaries");
    std::fs::remove_file(secrets.join(format!("{}.tmp", hex("dev.heimdall.t3")))).unwrap();
    let file = std::fs::read_dir(&secrets).unwrap().next().unwrap().unwrap().path();
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    again.delete("dev.heimdall.t3", "oauth").unwrap();
    assert_eq!(Store::files(&secrets).read("dev.heimdall.t3", "oauth").unwrap(), None);
}

#[test]
fn a_scoped_store_is_this_threads_only() {
    let wedged = Store::off("scoped");
    scoped(&wedged, || assert_eq!(kind(), Kind::Off));
    assert_eq!(kind(), Kind::Memory);
    std::thread::spawn(|| assert_eq!(kind(), Kind::Memory)).join().unwrap();
}

/// Answers only when released, and then with a refusal (a locked Keychain once the dialog closes).
struct WedgedThenRefusing(Wedged);
impl Backend for WedgedThenRefusing {
    fn read(&self, s: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        self.0.wait();
        Err(Error::Refused { service: s.into(), code: INTERACTION_NOT_ALLOWED })
    }
    fn write(&self, _: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Review 14 Nov: only a successful answer cleared `stuck`, so after a refusal every later save waited 20 ms and
/// reported "did not answer" although it had landed.
#[test]
fn a_refusal_after_a_timeout_is_an_answer_and_later_saves_wait_again() {
    let backend = Arc::new(WedgedThenRefusing(Wedged::default()));
    let store = Store::custom(backend.clone()).with_waits(Duration::from_millis(50), Duration::from_millis(500));
    assert!(matches!(store.read("dev.heimdall.t3", "oauth"), Err(Error::Blocked { .. })));
    assert!(store.0.stuck.load(Ordering::SeqCst));
    backend.0.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.0.stuck.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!store.0.stuck.load(Ordering::SeqCst), "a refusal is an answer");
    // The save gets the Keychain's answer (here a refusal: it cannot be read back), not "did not answer".
    let saved = store.write("dev.heimdall.t3", "oauth", "l", b"v");
    assert!(matches!(saved, Err(Error::Refused { .. })), "{saved:?}");
}

struct Panicking;
impl Backend for Panicking {
    fn read(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        panic!("backend bug")
    }
    fn write(&self, _: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        panic!("backend bug")
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Review 14 Nov: a panicking backend looked like a timeout (stuck set, slot Pending for ten minutes).
#[test]
fn a_panicking_backend_fails_without_marking_the_keychain_stuck() {
    let store = Store::custom(Arc::new(Panicking));
    assert!(matches!(store.read("s", "a"), Err(Error::Failed { .. })));
    assert!(matches!(store.write("s", "a", "l", b"v"), Err(Error::Failed { .. })));
    assert!(!store.0.stuck.load(Ordering::SeqCst));
    assert!(matches!(store.read("s", "a"), Err(Error::Failed { .. })), "the failure settled the slot, not Pending");
}

/// Review 14 Nov: `with_extension("tmp")` gave every account of a service the same temporary file.
#[test]
fn two_accounts_of_one_service_write_at_once_without_mixing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::files(dir.path().join("lab-secrets"));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let s = store.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    s.write("svc", &format!("acct{}", i % 2), "l", format!("value{}", i % 2).as_bytes()).unwrap();
                }
            })
        })
        .collect();
    handles.into_iter().for_each(|h| h.join().unwrap());
    for i in 0..2 {
        let fresh = Store::files(dir.path().join("lab-secrets"));
        assert_eq!(fresh.read("svc", &format!("acct{i}")).unwrap(), Some(format!("value{i}").into_bytes()));
    }
}

/// Answers every read after `delay`, with nothing (a fresh profile has no saved items).
struct Slow(Duration);
impl Backend for Slow {
    fn read(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        std::thread::sleep(self.0);
        Ok(None)
    }
    fn write(&self, _: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// VM run 14 Nov (fresh install, guest log 15:48:47): the launch's first read took just over the wait, every read
/// after it got the stuck wait, and five "Keychain not answering" pills opened and closed within a millisecond. A read
/// that answers within the report grace, even after its wait ran out, raises nothing.
#[test]
fn a_slow_first_read_at_launch_raises_no_problem() {
    let store = Store::custom(Arc::new(Slow(Duration::from_millis(80)))).with_waits(Duration::from_millis(30), Duration::from_millis(30)).with_report_after(Duration::from_millis(400));
    let seen = events(&store);
    let services = ["Heimdall Alpha API provider: ChatGPT", "Heimdall Alpha API provider: OpenCode Go", "Heimdall Alpha API provider: Anthropic", "Heimdall Alpha API provider: OpenAI-Compatible", "dev.heimdall.t3"];
    for s in services {
        assert!(matches!(store.read(s, "a"), Err(Error::Blocked { .. })));
        assert!(matches!(store.read(s, "a"), Err(Error::Blocked { .. })), "a second read of a pending item");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !store.degraded().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(store.degraded().is_empty());
    // Past the grace: nothing was reported, not even a recovery.
    std::thread::sleep(Duration::from_millis(500));
    assert!(seen.lock().unwrap().is_empty(), "{:?}", seen.lock().unwrap());
    for s in services {
        assert_eq!(store.read(s, "a").unwrap(), None);
        assert_eq!(store.problem(s), None);
    }
}

struct AuthFailed;
impl Backend for AuthFailed {
    fn read(&self, s: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        Err(Error::Refused { service: s.into(), code: -25293 })
    }
    fn write(&self, _: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// VM run 14 Nov (upgrade): the item alpha.1 saved answered errSecAuthFailed to the new build. That is reported at once,
/// as unreadable (save it again), not as a locked Keychain; a password refusal is `Locked`.
#[test]
fn a_refusal_is_reported_at_once_with_its_cause() {
    let store = Store::custom(Arc::new(AuthFailed));
    let seen = events(&store);
    let service = "Heimdall Alpha API provider: OpenAI-Compatible";
    assert!(store.read(service, "a").is_err());
    assert_eq!(store.problem(service), Some(Cause::Unreadable));
    assert!(matches!(&seen.lock().unwrap()[..], [Event::Degraded { cause: Cause::Unreadable, .. }]));
    assert_eq!(Cause::of(&Error::Refused { service: "s".into(), code: INTERACTION_NOT_ALLOWED }), Cause::Locked);
    assert_eq!(Cause::of(&Error::Blocked { service: "s".into() }), Cause::Blocked);
    // A save this build still cannot read back is no recovery (VM run 14 Nov: the cache said "answers again").
    assert!(store.write(service, "a", "l", b"new").is_err());
    assert_eq!(store.problem(service), Some(Cause::Unreadable));
    assert!(!seen.lock().unwrap().iter().any(|e| matches!(e, Event::Recovered { .. })));
}

/// Answers only when released, and then errSecAuthFailed (an item an earlier build saved).
struct WedgedThenUnreadable(Wedged);
impl Backend for WedgedThenUnreadable {
    fn read(&self, s: &str, _: &str) -> Result<Option<Vec<u8>>, Error> {
        self.0.wait();
        Err(Error::Refused { service: s.into(), code: -25293 })
    }
    fn write(&self, _: &str, _: &str, _: &str, _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Review 14 Nov: a refusal that came after the blocked read was already reported changed only the stored cause, so
/// the pill kept "close its password prompt" for an item no unlock can fix. The refusal is reported too, in order.
#[test]
fn a_refusal_after_a_reported_timeout_replaces_the_problem() {
    let backend = Arc::new(WedgedThenUnreadable(Wedged::default()));
    let store = Store::custom(backend.clone()).with_waits(Duration::from_millis(20), Duration::from_millis(20)).with_report_after(Duration::from_millis(30));
    let seen = events(&store);
    let service = "Heimdall Alpha API provider: OpenAI-Compatible";
    assert!(matches!(store.read(service, "a"), Err(Error::Blocked { .. })));
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.problem(service).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(store.problem(service), Some(Cause::Blocked));
    backend.0.release();
    while store.problem(service) != Some(Cause::Unreadable) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let causes: Vec<Cause> = seen.lock().unwrap().iter().filter_map(|e| match e { Event::Degraded { cause, .. } => Some(*cause), _ => None }).collect();
    assert_eq!(causes, [Cause::Blocked, Cause::Unreadable]);
}

/// The login Keychain as builds see it: each item belongs to the build that added it; another build's read is refused
/// (errSecAuthFailed), but its update of the data in place succeeds and keeps the owner (SecItemUpdate needs only the
/// "encrypt" right), which is what alpha.1's item did to the re-entered key. Whether another build may delete an item
/// is a switch: macOS's answer there is not known, so both are tested.
#[derive(Default)]
struct Login {
    items: Mutex<HashMap<Key, (u32, Vec<u8>)>>,
    foreign_delete: AtomicBool,
    reads: AtomicUsize,
    /// The login Keychain is locked: every item read or delete answers errSecInteractionNotAllowed.
    locked: AtomicBool,
    /// Adding a new generation fails (a full disk, a Keychain error).
    generations_fail: AtomicBool,
}
struct Build(Arc<Login>, u32);
impl Backend for Build {
    fn read(&self, s: &str, a: &str) -> Result<Option<Vec<u8>>, Error> {
        self.0.reads.fetch_add(1, Ordering::SeqCst);
        if self.0.locked.load(Ordering::SeqCst) {
            return Err(Error::Refused { service: s.into(), code: INTERACTION_NOT_ALLOWED });
        }
        match self.0.items.lock().unwrap().get(&(s.into(), a.into())) {
            None => Ok(None),
            Some((owner, _)) if *owner != self.1 => Err(Error::Refused { service: s.into(), code: AUTH_FAILED }),
            Some((_, v)) => Ok(Some(v.clone())),
        }
    }
    fn write(&self, s: &str, a: &str, _: &str, b: &[u8]) -> Result<(), Error> {
        if a.contains('#') && self.0.generations_fail.load(Ordering::SeqCst) {
            return Err(Error::Failed { service: s.into(), message: "disk full".into() });
        }
        let mut items = self.0.items.lock().unwrap();
        let owner = items.get(&(s.into(), a.into())).map_or(self.1, |(o, _)| *o);
        items.insert((s.into(), a.into()), (owner, b.to_vec()));
        Ok(())
    }
    fn delete(&self, s: &str, a: &str) -> Result<(), Error> {
        if self.0.locked.load(Ordering::SeqCst) {
            return Err(Error::Refused { service: s.into(), code: INTERACTION_NOT_ALLOWED });
        }
        let mut items = self.0.items.lock().unwrap();
        match items.get(&(s.into(), a.into())) {
            Some((owner, _)) if *owner != self.1 && !self.0.foreign_delete.load(Ordering::SeqCst) => Err(Error::Refused { service: s.into(), code: AUTH_FAILED }),
            _ => {
                items.remove(&(s.into(), a.into()));
                Ok(())
            }
        }
    }
    /// Attributes only: answers for every build, locked or not.
    fn accounts(&self, s: &str) -> Result<Vec<String>, Error> {
        Ok(self.0.items.lock().unwrap().keys().filter(|k| k.0 == s).map(|k| k.1.clone()).collect())
    }
}
impl Login {
    fn store(self: &Arc<Self>, build: u32) -> Store {
        Store::custom(Arc::new(Build(self.clone(), build)))
    }
    fn owner(&self, s: &str, a: &str) -> Option<u32> {
        self.items.lock().unwrap().get(&(s.into(), a.into())).map(|(o, _)| *o)
    }
}
const PROVIDER: &str = "Heimdall Alpha API provider: OpenAI-Compatible";
const ALPHA1: u32 = 1;
const UPGRADE: u32 = 2;

/// The upgrade: alpha.1's key is unreadable to the new build, which reports it; re-entering the key saves an item
/// the new build owns, and only a real read-back says so.
fn upgrade_and_reenter(login: &Arc<Login>) -> (Store, Arc<Mutex<Vec<Event>>>) {
    login.store(ALPHA1).write(PROVIDER, "openai-compatible", PROVIDER, b"sk-old").unwrap();
    let app = login.store(UPGRADE);
    let seen = events(&app);
    assert!(matches!(app.read(PROVIDER, "openai-compatible"), Err(Error::Refused { code: AUTH_FAILED, .. })));
    // Its sign-in account is missing and reads fine: that is no recovery while the key beside it is unreadable.
    assert_eq!(app.read(PROVIDER, "openai-compatible-console").unwrap(), None);
    assert_eq!(app.problem(PROVIDER), Some(Cause::Unreadable));
    assert!(!seen.lock().unwrap().iter().any(|e| matches!(e, Event::Recovered { .. })), "{:?}", seen.lock().unwrap());
    app.write(PROVIDER, "openai-compatible", PROVIDER, b"sk-new").unwrap();
    assert_eq!(app.problem(PROVIDER), None);
    assert!(matches!(seen.lock().unwrap().last(), Some(Event::Recovered { .. })));
    (app, seen)
}

/// VM run 14 Nov (b): after the re-entry every `--provider-token` child (a fresh process: a fresh store, no cache) was
/// refused. Here macOS lets the new build delete alpha.1's item: it is replaced in place by one the new build owns.
#[test]
fn a_reentered_key_after_an_upgrade_is_readable_by_the_token_child() {
    let login = Arc::new(Login::default());
    login.foreign_delete.store(true, Ordering::SeqCst);
    upgrade_and_reenter(&login);
    // Saved as a new item the new build owns (never alpha.1's updated in place), and only then alpha.1's removed.
    assert_eq!(login.owner(PROVIDER, "openai-compatible#2"), Some(UPGRADE));
    assert_eq!(login.owner(PROVIDER, "openai-compatible"), None);
    let child = login.store(UPGRADE);
    assert_eq!(child.read(PROVIDER, "openai-compatible").unwrap().as_deref(), Some(&b"sk-new"[..]));
}

/// The same when macOS refuses the delete: the key goes to the next generation, which reads prefer over the old item
/// they cannot read; the next ad-hoc build's re-entry takes the one after.
#[test]
fn an_undeletable_old_item_never_shadows_the_reentered_key() {
    let login = Arc::new(Login::default());
    upgrade_and_reenter(&login);
    assert_eq!(login.owner(PROVIDER, "openai-compatible"), Some(ALPHA1), "left as it was");
    assert_eq!(login.owner(PROVIDER, "openai-compatible#2"), Some(UPGRADE));
    assert_eq!(login.store(UPGRADE).read(PROVIDER, "openai-compatible").unwrap().as_deref(), Some(&b"sk-new"[..]));
    // alpha.1 itself still reads its own item.
    assert_eq!(login.store(ALPHA1).read(PROVIDER, "openai-compatible").unwrap().as_deref(), Some(&b"sk-old"[..]));
    // A later ad-hoc build: both are unreadable to it, reported; its re-entry lands in generation 3.
    let next = login.store(3);
    assert!(matches!(next.read(PROVIDER, "openai-compatible"), Err(Error::Refused { code: AUTH_FAILED, .. })));
    next.write(PROVIDER, "openai-compatible", PROVIDER, b"sk-3").unwrap();
    assert_eq!(login.owner(PROVIDER, "openai-compatible#3"), Some(3));
    assert_eq!(login.store(3).read(PROVIDER, "openai-compatible").unwrap().as_deref(), Some(&b"sk-3"[..]));
    // Disconnect: the build's own item goes; the ones it cannot read or delete stay unreadable to it, so it reads none.
    next.delete(PROVIDER, "openai-compatible").unwrap();
    assert_eq!(login.store(3).read(PROVIDER, "openai-compatible").unwrap_err(), Error::Refused { service: PROVIDER.into(), code: AUTH_FAILED });
    assert_eq!(login.owner(PROVIDER, "openai-compatible#3"), None);
}

/// The user's stably signed build owns its items: a read asks one item, a save updates it in place, no generation appears.
#[test]
fn a_builds_own_items_read_and_update_as_before() {
    let login = Arc::new(Login::default());
    login.store(7).write(PROVIDER, "openai-compatible", PROVIDER, b"sk-a").unwrap();
    let before = login.reads.load(Ordering::SeqCst);
    assert_eq!(login.store(7).read(PROVIDER, "openai-compatible").unwrap().as_deref(), Some(&b"sk-a"[..]));
    assert_eq!(login.reads.load(Ordering::SeqCst) - before, 1, "one Keychain read, as before");
    let app = login.store(7);
    app.write(PROVIDER, "openai-compatible", PROVIDER, b"sk-b").unwrap();
    assert_eq!(login.items.lock().unwrap().len(), 1, "updated in place");
    assert_eq!(login.store(7).read(PROVIDER, "openai-compatible").unwrap().as_deref(), Some(&b"sk-b"[..]));
    app.delete(PROVIDER, "openai-compatible").unwrap();
    assert!(login.items.lock().unwrap().is_empty());
}

/// Review of 543ad43c: a fixed cap of eight generations let seven ad-hoc upgrades use up an account for good.
#[test]
fn every_upgrade_can_save_again_without_a_cap() {
    let login = Arc::new(Login::default());
    login.store(1).write(PROVIDER, "k", PROVIDER, b"v1").unwrap();
    for build in 2..=20 {
        let key = format!("v{build}");
        login.store(build).write(PROVIDER, "k", PROVIDER, key.as_bytes()).unwrap();
        assert_eq!(login.store(build).read(PROVIDER, "k").unwrap(), Some(key.into_bytes()), "build {build}");
    }
    assert_eq!(login.owner(PROVIDER, "k#20"), Some(20));
    // The build's own generation is updated in place, not a new number per save.
    login.store(20).write(PROVIDER, "k", PROVIDER, b"again").unwrap();
    assert_eq!(login.owner(PROVIDER, "k#21"), None);
    assert_eq!(login.store(20).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"again"[..]));
}

/// Review of 543ad43c: reads stopped at the first missing number, so with the account itself gone the app's cache and
/// a `--provider-token` child disagreed. The listing finds the generation wherever it is.
#[test]
fn a_missing_number_never_hides_a_later_generation() {
    let login = Arc::new(Login::default());
    login.items.lock().unwrap().insert((PROVIDER.into(), "k#3".into()), (UPGRADE, b"sk".to_vec()));
    login.items.lock().unwrap().insert((PROVIDER.into(), "k#2".into()), (ALPHA1, b"old".to_vec()));
    assert_eq!(login.store(UPGRADE).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"sk"[..]));
    login.items.lock().unwrap().remove(&(PROVIDER.into(), "k#2".into()));
    assert_eq!(login.store(UPGRADE).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"sk"[..]));
    // Look-alike accounts are not generations.
    login.items.lock().unwrap().insert((PROVIDER.into(), "k#x".into()), (UPGRADE, b"no".to_vec()));
    login.items.lock().unwrap().insert((PROVIDER.into(), "k-console#9".into()), (UPGRADE, b"no".to_vec()));
    assert_eq!(login.store(UPGRADE).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"sk"[..]));
}

/// Review of 543ad43c: a delete swallowed every refusal, a locked Keychain's included, and said Ok for the build's
/// own item. Only another build's item (errSecAuthFailed) is passed over.
#[test]
fn a_delete_refused_for_the_builds_own_item_is_an_error() {
    let login = Arc::new(Login::default());
    login.store(7).write(PROVIDER, "k", PROVIDER, b"sk").unwrap();
    login.locked.store(true, Ordering::SeqCst);
    let e = login.store(7).delete(PROVIDER, "k").unwrap_err();
    assert_eq!(e, Error::Refused { service: PROVIDER.into(), code: INTERACTION_NOT_ALLOWED });
    login.locked.store(false, Ordering::SeqCst);
    assert_eq!(login.owner(PROVIDER, "k"), Some(7), "still there");
    // Another build's undeletable item beside the build's own: the own one goes, the other is passed over.
    login.items.lock().unwrap().insert((PROVIDER.into(), "k#2".into()), (ALPHA1, b"old".to_vec()));
    login.store(7).delete(PROVIDER, "k").unwrap();
    assert_eq!(login.owner(PROVIDER, "k"), None);
    assert_eq!(login.owner(PROVIDER, "k#2"), Some(ALPHA1));
}

/// Review of 543ad43c: the old item was deleted before the new key was safe. If the new item cannot be saved, the old
/// one stays (still there for the build that can read it) and the save fails.
#[test]
fn a_failed_save_deletes_nothing() {
    let login = Arc::new(Login::default());
    login.foreign_delete.store(true, Ordering::SeqCst);
    login.store(ALPHA1).write(PROVIDER, "k", PROVIDER, b"sk-old").unwrap();
    login.generations_fail.store(true, Ordering::SeqCst);
    assert!(login.store(UPGRADE).write(PROVIDER, "k", PROVIDER, b"sk-new").is_err());
    assert_eq!(login.store(ALPHA1).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"sk-old"[..]));
}

/// Review of 543ad43c: clearing after a save must not delete a newer generation this build cannot read (another
/// build's key), only this build's own stale copies and older items.
#[test]
fn a_save_keeps_another_builds_newer_generation() {
    let login = Arc::new(Login::default());
    login.foreign_delete.store(true, Ordering::SeqCst);
    login.store(7).write(PROVIDER, "k", PROVIDER, b"mine").unwrap();
    login.items.lock().unwrap().insert((PROVIDER.into(), "k#3".into()), (9, b"theirs".to_vec()));
    login.items.lock().unwrap().insert((PROVIDER.into(), "k#2".into()), (7, b"stale".to_vec()));
    login.store(7).write(PROVIDER, "k", PROVIDER, b"mine2").unwrap();
    assert_eq!(login.owner(PROVIDER, "k#3"), Some(9), "another build's newer item stays");
    assert_eq!(login.owner(PROVIDER, "k#2"), None, "this build's stale copy goes");
    assert_eq!(login.store(7).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"mine2"[..]));
    assert_eq!(login.store(9).read(PROVIDER, "k").unwrap().as_deref(), Some(&b"theirs"[..]));
}
