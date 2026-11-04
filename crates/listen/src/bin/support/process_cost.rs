use anyhow::{ensure, Result};
use serde_json::{json, Value};
#[repr(C)]
#[cfg(target_os = "macos")]
struct UsageV6 {
    base: libc::rusage_info_v4,
    flags: u64,
    user_ptime: u64,
    system_ptime: u64,
    pinstructions: u64,
    pcycles: u64,
    energy_nj: u64,
    // Oversized to include every remaining SDK v6 field, never a short FFI buffer.
    tail: [u64; 64],
}
pub fn usage() -> Result<Value> {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) } == 0,
        "getrusage"
    );
    let time = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    #[cfg(target_os = "macos")]
    let counters = {
        let mut v: UsageV6 = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as i32,
                6,
                (&mut v as *mut UsageV6).cast(),
            )
        } == 0;
        // TASK_POWER_INFO_V2: six CPU u64s, four GPU u64s, task energy,
        // performance-core time and pset switches (macOS arm64 SDK).
        let mut power = [0_u64; 13];
        let mut count = 26_u32;
        #[allow(deprecated)]
        let gpu_ok = unsafe {
            libc::task_info(
                libc::mach_task_self(),
                26,
                power.as_mut_ptr().cast(),
                &mut count,
            )
        } == 0
            && count >= 14;
        json!({"energy_nj":if ok {Some(v.energy_nj)} else {None},"billed_energy_raw":if ok {Some(v.base.ri_billed_energy)} else {None},"instructions":if ok {Some(v.base.ri_instructions)} else {None},"gpu_time_ns":if gpu_ok {Some(power[6])} else {None}})
    };
    #[cfg(not(target_os = "macos"))]
    let counters =
        json!({"energy_nj":null,"billed_energy_raw":null,"instructions":null,"gpu_time_ns":null});
    let mut result = counters;
    result["cpu_s"] = json!(time(r.ru_utime) + time(r.ru_stime));
    Ok(result)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn matches_sdk_v6_layout_without_short_buffer() {
        assert_eq!(std::mem::size_of::<libc::rusage_info_v4>(), 296);
        assert_eq!(std::mem::offset_of!(UsageV6, energy_nj), 336);
        assert!(std::mem::size_of::<UsageV6>() >= 464);
        assert!(usage().unwrap()["cpu_s"].as_f64().unwrap() >= 0.);
    }
}

/// System-wide GPU energy (IOReport "Energy Model"/"GPU Energy", nJ, live on M5 Max).
/// Includes every process's GPU work; callers difference it against an idle window.
#[cfg(target_os = "macos")]
pub mod gpu {
    use std::ffi::c_void;
    type Ref = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(a: Ref, s: *const i8, e: u32) -> Ref;
        fn CFDictionaryGetValue(d: Ref, k: Ref) -> Ref;
        fn CFDictionaryCreateMutableCopy(a: Ref, n: isize, d: Ref) -> Ref;
        fn CFDictionarySetValue(d: Ref, k: Ref, v: Ref);
        fn CFArrayGetCount(a: Ref) -> isize;
        fn CFArrayGetValueAtIndex(a: Ref, i: isize) -> Ref;
        fn CFArrayCreateMutable(a: Ref, n: isize, cb: Ref) -> Ref;
        fn CFArrayAppendValue(a: Ref, v: Ref);
        fn CFStringCompare(a: Ref, b: Ref, o: usize) -> isize;
        fn CFRelease(r: Ref);
        static kCFTypeArrayCallBacks: c_void;
    }
    #[link(name = "IOReport")]
    extern "C" {
        fn IOReportCopyChannelsInGroup(g: Ref, s: Ref, a: u64, b: u64, c: u64) -> Ref;
        fn IOReportCreateSubscription(a: Ref, ch: Ref, out: *mut Ref, d: u64, e: Ref) -> Ref;
        fn IOReportCreateSamples(s: Ref, ch: Ref, a: Ref) -> Ref;
        fn IOReportSimpleGetIntegerValue(c: Ref, i: i32) -> i64;
        fn IOReportChannelGetChannelName(c: Ref) -> Ref;
    }
    struct Subscription(Ref, Ref, Ref);
    unsafe impl Send for Subscription {}
    unsafe impl Sync for Subscription {}
    fn cf(s: &std::ffi::CStr) -> Ref {
        unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), 0x0800_0100) }
    }
    fn subscription() -> Option<&'static Subscription> {
        static S: std::sync::OnceLock<Option<Subscription>> = std::sync::OnceLock::new();
        S.get_or_init(|| unsafe {
            let all = IOReportCopyChannelsInGroup(cf(c"Energy Model"), std::ptr::null(), 0, 0, 0);
            if all.is_null() {
                return None;
            }
            let key = cf(c"IOReportChannels");
            let channels = CFDictionaryGetValue(all, key);
            let keep = CFArrayCreateMutable(
                std::ptr::null(),
                1,
                (&raw const kCFTypeArrayCallBacks).cast(),
            );
            let want = cf(c"GPU Energy");
            for i in 0..CFArrayGetCount(channels) {
                let c = CFArrayGetValueAtIndex(channels, i);
                if CFStringCompare(IOReportChannelGetChannelName(c), want, 0) == 0 {
                    CFArrayAppendValue(keep, c);
                }
            }
            if CFArrayGetCount(keep) != 1 {
                return None;
            }
            let dict = CFDictionaryCreateMutableCopy(std::ptr::null(), 0, all);
            CFDictionarySetValue(dict, key, keep);
            let mut subbed: Ref = std::ptr::null();
            let s = IOReportCreateSubscription(
                std::ptr::null(),
                dict,
                &mut subbed,
                0,
                std::ptr::null(),
            );
            (!s.is_null() && !subbed.is_null()).then_some(Subscription(s, subbed, key))
        })
        .as_ref()
    }
    /// Cumulative system GPU energy in nJ, or None where IOReport lacks the channel.
    pub fn energy_nj() -> Option<u64> {
        let s = subscription()?;
        unsafe {
            let sample = IOReportCreateSamples(s.0, s.1, std::ptr::null());
            if sample.is_null() {
                return None;
            }
            let channels = CFDictionaryGetValue(sample, s.2);
            let v = (!channels.is_null() && CFArrayGetCount(channels) == 1).then(|| {
                IOReportSimpleGetIntegerValue(CFArrayGetValueAtIndex(channels, 0), 0) as u64
            });
            CFRelease(sample);
            v
        }
    }
}
