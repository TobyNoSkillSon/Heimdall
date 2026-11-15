//! The compute flag's contract (bug note 2026-11-09T12-43-19): the reader's rule on fixed files, and the writer's
//! red, refresh, linger and green on a temporary folder. The same fixtures are checked by Arsenal's
//! `ComputeFlagTests` (Projects/Arsenal-public, Tests/ArsenalCoreTests).
use super::*;

const NOW: f64 = 1_800_000_000.;

fn at(secs: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(secs)
}

fn flag(state: &str, since: f64, expires: f64, pid: u32) -> String {
    format!(r#"{{"version":1,"state":"{state}","seq":7,"holder":"heimdall","pid":{pid},"reason":"Jarvis speech","since":{since},"expires":{expires}}}"#)
}

#[test]
fn the_reader_sees_red_only_for_a_live_fresh_red_from_a_living_heimdall() {
    let alive = |_| true;
    assert!(is_red(&flag("red", NOW - 1., NOW + 9., 42), at(NOW), &alive));
    // Green, expired, too long, or a dead writer: green.
    assert!(!is_red(&flag("green", NOW - 1., NOW + 9., 42), at(NOW), &alive));
    assert!(!is_red(&flag("red", NOW - 11., NOW, 42), at(NOW), &alive), "expires is exclusive");
    assert!(!is_red(&flag("red", NOW - 181., NOW + 9., 42), at(NOW), &alive), "a red never lasts past MAX_RED");
    assert!(is_red(&flag("red", NOW - 180., NOW + 9., 42), at(NOW), &alive));
    assert!(!is_red(&flag("red", NOW - 1., NOW + 9., 42), at(NOW), &|_| false), "Heimdall crashed: its red lapses");
    // Unreadable or another version: green, never a stop on a file nobody understands.
    for text in ["", "{}", "not json", &flag("red", NOW - 1., NOW + 9., 42).replace("\"version\":1", "\"version\":2"),
        &flag("amber", NOW - 1., NOW + 9., 42)] {
        assert!(!is_red(text, at(NOW), &alive), "{text}");
    }
}

#[test]
fn pid_alive_knows_this_process_and_not_a_free_pid() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(0));
    assert!(!pid_alive(u32::MAX));
}

fn read(path: &Path) -> FlagFile {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done()
}

#[test]
fn red_is_written_before_raise_returns_refreshed_while_up_and_green_after_the_linger() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DIR).join(FLAG_FILE);
    let timing = Timing { ttl: Duration::from_millis(400), refresh: Duration::from_millis(60), linger: Duration::from_millis(120) };
    let flag = Flag::start_with(path.clone(), timing);
    assert_eq!(read(&path).state, State::Green, "a new Heimdall clears a red a crashed one left");
    let red = flag.raise("Jarvis speech");
    let first = read(&path);
    assert_eq!((first.state, first.seq, first.pid, first.version), (State::Red, 1, std::process::id(), VERSION));
    assert!(is_red(&std::fs::read_to_string(&path).unwrap(), SystemTime::now(), &pid_alive));
    // Refreshed: `expires` moves on while `since` and `seq` stay.
    assert!(wait_for(Duration::from_secs(2), || read(&path).expires > first.expires));
    let later = read(&path);
    assert_eq!((later.since, later.seq), (first.since, first.seq));
    // A second raise inside the first is the same red.
    let inner = flag.raise("Jarvis speech");
    drop(inner);
    drop(red);
    // Still red inside the linger, green after it.
    let lowered = Instant::now();
    assert_eq!(read(&path).state, State::Red);
    assert!(wait_for(Duration::from_secs(2), || read(&path).state == State::Green));
    assert!(lowered.elapsed() >= Duration::from_millis(100), "green waited out the linger");
    assert!(!flag.red());
}

#[test]
fn a_line_that_follows_within_the_linger_keeps_the_same_red() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FLAG_FILE);
    let timing = Timing { ttl: Duration::from_secs(5), refresh: Duration::from_secs(1), linger: Duration::from_millis(300) };
    let flag = Flag::start_with(path.clone(), timing);
    drop(flag.raise("Jarvis speech"));
    std::thread::sleep(Duration::from_millis(50));
    let again = flag.raise("Jarvis speech");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!((read(&path).state, read(&path).seq), (State::Red, 1), "no green between the sentences of a reply");
    drop(again);
    assert!(wait_for(Duration::from_secs(2), || read(&path).state == State::Green));
    // The next reply is a new red.
    let _next = flag.raise("Jarvis speech");
    assert_eq!((read(&path).state, read(&path).seq), (State::Red, 2));
}

#[test]
fn a_red_left_by_a_crash_lapses_within_the_ttl() {
    // The writer stops refreshing (a hung Heimdall): readers see green once `expires` passes.
    let text = flag("red", NOW, NOW + TTL.as_secs_f64(), std::process::id());
    assert!(is_red(&text, at(NOW + 9.9), &pid_alive));
    assert!(!is_red(&text, at(NOW + 10.), &pid_alive));
}

#[test]
fn shutdown_writes_green() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FLAG_FILE);
    let flag = Flag::start_with(path.clone(), Timing::default());
    let red = flag.raise("Jarvis speech");
    flag.shutdown();
    assert_eq!(read(&path).state, State::Green);
    drop(red);
}

#[test]
fn the_yield_lets_go_of_jarvis_and_embeddings_and_never_the_listening_models() {
    assert_eq!(yield_state(false, true, true), "none");
    assert_eq!(yield_state(true, true, false), "yielding");
    assert_eq!(yield_state(true, false, true), "yielding");
    assert_eq!(yield_state(true, true, true), "yielded");
    let file = yield_file(vec!["Arsenal FLUX.2 (arsenal-flux.json)".into()], true, true, at(NOW));
    assert_eq!(file.state, "yielded");
    assert_eq!(file.released, YIELDS.to_vec());
    assert_eq!(file.keeps, KEEPS.to_vec());
    // These four are never unloaded.
    for kept in ["listener", "smart-turn", "Parakeet", "decider"] {
        assert!(KEEPS.iter().any(|k| k.contains(kept)), "{kept}");
        assert!(!YIELDS.iter().any(|y| y.contains(kept)), "{kept} must never be yielded");
    }
}
