/// Core Audio host time is mach_absolute_time, shared across devices/processes.
/// Convert to nanoseconds without resetting the origin for individual sources.
#[cfg(target_os = "macos")]
pub fn host_now_ns() -> u64 {
    ticks_to_ns(unsafe { mach_absolute_time() })
}
#[cfg(target_os = "macos")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}
#[cfg(target_os = "macos")]
pub(crate) fn ticks_to_ns(ticks: u64) -> u64 {
    static SCALE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let &(n, d) = SCALE.get_or_init(|| {
        let mut t = Timebase { numer: 0, denom: 0 };
        assert_eq!(unsafe { mach_timebase_info(&mut t) }, 0);
        (t.numer, t.denom)
    });
    ((ticks as u128 * n as u128) / d as u128) as u64
}
#[cfg(not(target_os = "macos"))]
pub fn host_now_ns() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}
#[derive(Default, Debug)]
pub struct ClockMapper {
    last_end: Option<u64>,
    last_start: Option<u64>,
}
impl ClockMapper {
    /// Clamp a regressing native clock and mark discontinuities. No per-source
    /// zero origin; comparable timestamps survive output chunking and restarts.
    pub fn stamp(&mut self, host_ns: u64, frames: usize, rate: u32) -> (u64, bool) {
        let start = self
            .last_start
            .map(|previous| host_ns.max(previous.saturating_add(1)))
            .unwrap_or(host_ns);
        let discontinuity = self.last_start.is_some_and(|previous| host_ns <= previous)
            || self
                .last_end
                .is_some_and(|end| host_ns.abs_diff(end) > 5_000_000);
        self.last_start = Some(start);
        self.last_end =
            Some(start.saturating_add((frames as u128 * 1_000_000_000 / rate as u128) as u64));
        (start, discontinuity)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_device_clock_drift_is_not_a_discontinuity() {
        let mut c = ClockMapper::default();
        for i in 0..1000 {
            let ns = 10_000_000_000 + i * 10_001_000;
            assert_eq!(c.stamp(ns, 480, 48000), (ns, false));
        }
    }
    #[test]
    fn clocks_share_origin_and_do_not_regress() {
        let mut a = ClockMapper::default();
        let mut b = ClockMapper::default();
        assert_eq!(
            a.stamp(10_000_000_000, 480, 48000).0,
            b.stamp(10_000_000_000, 160, 16000).0
        );
        assert_eq!(a.stamp(9_000_000_000, 480, 48000), (10_000_000_001, true));
        assert_eq!(a.stamp(11_000_000_000, 480, 48000), (11_000_000_000, true));
    }
}
