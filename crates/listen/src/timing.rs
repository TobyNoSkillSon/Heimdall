//! When each final utterance was decided and transcribed, on the host clock (mach_absolute_time ns, the clock capture
//! stamps audio with), for the app's per-request latency line. The engine marks; the app takes the marks of a final
//! when it receives that final. Process-global and bounded by the number of sources: display data, never journalled.
use std::{collections::HashMap, sync::Mutex};

/// Host-clock marks of one final utterance. `end_ns` is the utterance's audio end (the `FinalUtterance` field), so a
/// consumer can check the marks belong to the final it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FinalTiming {
    pub end_ns: u64,
    /// The engine decided the speech had paused long enough to ask Smart Turn (or hit the forced-end limit).
    pub endpoint_ns: u64,
    /// Smart Turn answered.
    pub turn_ns: u64,
    /// The final Parakeet decode finished; the `FinalUtterance` is emitted right after.
    pub decoded_ns: u64,
    /// Silero's last voiced audio (the `SpeechEnd` time, what the ack and the reply hold count quiet from); 0 unknown.
    /// `end_ns` is later when the recognizer's last word ends after it (26 Oct: up to 1.4 s).
    pub voice_end_ns: u64,
}

static MARKS: Mutex<Option<HashMap<String, FinalTiming>>> = Mutex::new(None);
const MAX_SOURCES: usize = 64;

fn with<T>(f: impl FnOnce(&mut HashMap<String, FinalTiming>) -> T) -> T {
    let mut guard = MARKS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() > MAX_SOURCES {
        map.clear();
    }
    f(map)
}
pub(crate) fn endpoint(source: &str, voice_end_ns: u64) {
    let now = host_now_ns();
    with(|m| {
        let t = m.entry(source.into()).or_default();
        *t = FinalTiming { endpoint_ns: now, turn_ns: now, voice_end_ns, ..Default::default() };
    });
}
pub(crate) fn turn(source: &str) {
    let now = host_now_ns();
    with(|m| m.entry(source.into()).or_default().turn_ns = now);
}
pub(crate) fn decoded(source: &str, end_ns: u64) {
    let now = host_now_ns();
    with(|m| {
        let t = m.entry(source.into()).or_default();
        if t.endpoint_ns == 0 {
            t.endpoint_ns = now;
            t.turn_ns = now;
        }
        t.decoded_ns = now;
        t.end_ns = end_ns;
    });
}
/// The marks of the final that ended at `end_ns` on `source`, once.
pub fn take(source: &str, end_ns: u64) -> Option<FinalTiming> {
    with(|m| match m.get(source) {
        Some(t) if t.end_ns == end_ns && t.decoded_ns != 0 => m.remove(source),
        _ => None,
    })
}

/// Same clock as `capture::host_now_ns` (duplicated: listen does not depend on capture).
#[cfg(target_os = "macos")]
pub fn host_now_ns() -> u64 {
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    static SCALE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let &(n, d) = SCALE.get_or_init(|| {
        let mut t = Timebase { numer: 0, denom: 0 };
        assert_eq!(unsafe { mach_timebase_info(&mut t) }, 0);
        (t.numer, t.denom)
    });
    ((unsafe { mach_absolute_time() } as u128 * n as u128) / d as u128) as u64
}
#[cfg(not(target_os = "macos"))]
pub fn host_now_ns() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marks_are_ordered_and_taken_once_for_the_matching_final() {
        endpoint("timing-test-a", 40);
        turn("timing-test-a");
        decoded("timing-test-a", 42);
        assert_eq!(take("timing-test-a", 41), None, "another final's end never takes these marks");
        let t = take("timing-test-a", 42).unwrap();
        assert!(t.endpoint_ns <= t.turn_ns && t.turn_ns <= t.decoded_ns && t.endpoint_ns > 0);
        assert_eq!(t.voice_end_ns, 40, "the last voiced audio rides with the marks");
        assert_eq!(take("timing-test-a", 42), None);
    }
    #[test]
    fn a_final_without_an_endpoint_mark_gets_its_decode_time() {
        decoded("timing-test-b", 7);
        let t = take("timing-test-b", 7).unwrap();
        assert_eq!((t.endpoint_ns, t.turn_ns), (t.decoded_ns, t.decoded_ns));
    }
    #[test]
    fn a_resumed_pause_restarts_the_marks() {
        endpoint("timing-test-c", 1);
        let first = take_raw("timing-test-c");
        std::thread::sleep(std::time::Duration::from_millis(2));
        endpoint("timing-test-c", 1);
        decoded("timing-test-c", 9);
        let t = take("timing-test-c", 9).unwrap();
        assert!(t.endpoint_ns > first.endpoint_ns);
    }
    fn take_raw(source: &str) -> FinalTiming {
        with(|m| *m.get(source).unwrap())
    }
    #[test]
    fn host_clock_is_monotonic() {
        let a = host_now_ns();
        let b = host_now_ns();
        assert!(b >= a && a > 0);
    }
}
