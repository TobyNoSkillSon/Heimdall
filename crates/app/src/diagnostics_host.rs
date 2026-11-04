//! Bounded native macOS probes. No subprocesses, network, GPU work or model locks.
use serde_json::{Value, json};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};
static DATA: OnceLock<std::path::PathBuf> = OnceLock::new();
pub(super) fn initialize(data: &std::path::Path) {
    let _ = DATA.set(data.into());
}
pub(super) fn commit() -> &'static str {
    static COMMIT: OnceLock<String> = OnceLock::new();
    COMMIT.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("../Resources/source-commit.txt")))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|| "unbundled".into())
    })
}
#[cfg(target_os = "macos")]
unsafe fn sysctl<T: Copy + Default>(name: &std::ffi::CStr) -> Option<T> {
    let mut v = T::default();
    let mut n = std::mem::size_of::<T>();
    (unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut v as *mut T).cast(),
            &mut n,
            std::ptr::null_mut(),
            0,
        )
    } == 0
        && n == std::mem::size_of::<T>())
    .then_some(v)
}
#[cfg(target_os = "macos")]
pub(super) fn sample() -> Value {
    unsafe {
        unsafe extern "C" {
            fn mach_host_self() -> u32;
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
            static mach_task_self_: u32;
            fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
            fn host_page_size(host: u32, size: *mut u32) -> i32;
            fn proc_listallpids(buffer: *mut libc::c_void, size: i32) -> i32;
            fn proc_pidpath(pid: i32, buffer: *mut libc::c_void, size: u32) -> i32;
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut libc::c_void,
                size: i32,
            ) -> i32;
        }
        let host = mach_host_self();
        let mut vm = [0u64; 64];
        let mut count = 128;
        let mut page = 0u32;
        let vm_ok = host_statistics64(host, 4, vm.as_mut_ptr().cast(), &mut count) == 0;
        host_page_size(host, &mut page);
        mach_port_deallocate(mach_task_self_, host);
        let words = std::slice::from_raw_parts(vm.as_ptr().cast::<u32>(), 128);
        let bytes = |i: usize| {
            if vm_ok {
                Some(words[i] as u64 * page as u64)
            } else {
                None
            }
        };
        #[derive(Copy, Clone, Default)]
        #[repr(C)]
        struct Swap {
            total: u64,
            available: u64,
            used: u64,
            page: u32,
            encrypted: i32,
        }
        let swap = sysctl::<Swap>(c"vm.swapusage");
        static PREVIOUS: AtomicU64 = AtomicU64::new(u64::MAX);
        let delta = swap.and_then(|s| {
            let previous = PREVIOUS.swap(s.used, Ordering::Relaxed);
            (previous != u64::MAX).then(|| s.used as i64 - previous as i64)
        });
        let mut load = [0f64; 3];
        let load_ok = libc::getloadavg(load.as_mut_ptr(), 3) == 3;
        let splash_pid = DATA
            .get()
            .and_then(|d| std::fs::File::open(d.join("splash/splash.pid")).ok())
            .and_then(|f| {
                use std::io::Read;
                let mut text = String::new();
                f.take(64).read_to_string(&mut text).ok()?;
                text.trim().parse::<i32>().ok()
            });
        let mut processes = vec![];
        let mut pids = [0i32; 4096];
        let n = proc_listallpids(
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as i32,
        )
        .clamp(0, 4096);
        for pid in &pids[..n as usize] {
            let mut path = [0u8; 4096];
            if proc_pidpath(*pid, path.as_mut_ptr().cast(), 4096) <= 0 {
                continue;
            }
            let path = std::ffi::CStr::from_ptr(path.as_ptr().cast()).to_string_lossy();
            let name = path.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
            let mut drama = false;
            if name.contains("python") || name.contains("drama") {
                // Inspect only our own direct child, never another application's Python arguments.
                let mut bsd = [0u64; 17];
                let n = proc_pidinfo(*pid, 3, 0, bsd.as_mut_ptr().cast(), 136);
                let parent = std::slice::from_raw_parts(bsd.as_ptr().cast::<u32>(), 34)[4];
                if n == 136 && parent == std::process::id() {
                    drama = name.contains("drama");
                    let mut args = [0u8; 32768];
                    let mut size = args.len();
                    let mut mib = [1, 49, *pid];
                    if libc::sysctl(
                        mib.as_mut_ptr(),
                        3,
                        args.as_mut_ptr().cast(),
                        &mut size,
                        std::ptr::null_mut(),
                        0,
                    ) == 0
                    {
                        drama |= args[..size.min(args.len())]
                            .windows(10)
                            .any(|w| w == b"sidecar.py");
                    }
                }
            }
            if Some(*pid) == splash_pid || drama || *pid == std::process::id() as i32 {
                let mut task = [0u64; 12];
                let size = proc_pidinfo(*pid, 4, 0, task.as_mut_ptr().cast(), 96);
                processes.push(json!({"pid":pid,"component":if Some(*pid)==splash_pid{"Splash"}else if drama{"Drama3"}else{"Heimdall"},"rss_bytes":if size==96{Some(task[1])}else{None},"cpu_user_ns":if size==96{Some(task[2])}else{None},"cpu_system_ns":if size==96{Some(task[3])}else{None}}));
                if processes.len() == 16 {
                    break;
                }
            }
        }
        let thermal: i64 = {
            use objc2::{class, msg_send};
            let info: *mut objc2::runtime::AnyObject =
                msg_send![class!(NSProcessInfo), processInfo];
            msg_send![info, thermalState]
        };
        json!({"sample_wall_ns":heimdall_archive::wall_ns(),"app_commit":commit(),"memory":{"free_bytes":bytes(0),"available_estimate_bytes":bytes(0).zip(bytes(2)).zip(bytes(22)).map(|((f,i),p)|f+i+p),"available_method":"free + inactive + purgeable (estimate)","wired_bytes":bytes(3),"compressed_bytes":bytes(32),"swap_used_bytes":swap.map(|s|s.used),"swap_delta_bytes_since_previous_problem":delta,"pressure_level":sysctl::<i32>(c"kern.memorystatus_vm_pressure_level")},"cpu":{"load_average_1m":load_ok.then_some(load[0]),"load_average_5m":load_ok.then_some(load[1]),"thermal_state":thermal},"power":power(),"processes":processes,"process_scan_truncated":n==4096,"process_scan_method":"Splash PID receipt and own Drama3 sidecar child; inaccessible RSS is null"})
    }
}
#[cfg(target_os = "macos")]
unsafe fn power() -> Value {
    unsafe {
        use libc::c_void;
        type CF = *const c_void;
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOPSCopyPowerSourcesInfo() -> CF;
            fn IOPSCopyPowerSourcesList(info: CF) -> CF;
            fn IOPSGetPowerSourceDescription(info: CF, source: CF) -> CF;
            fn IOPSGetProvidingPowerSourceType(info: CF) -> CF;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFRelease(x: CF);
            fn CFArrayGetCount(x: CF) -> isize;
            fn CFArrayGetValueAtIndex(x: CF, i: isize) -> CF;
            fn CFStringGetCString(x: CF, b: *mut i8, n: isize, encoding: u32) -> bool;
            fn CFStringCreateWithCString(a: CF, s: *const i8, encoding: u32) -> CF;
            fn CFDictionaryGetValue(d: CF, key: CF) -> CF;
            fn CFNumberGetValue(n: CF, t: i32, v: *mut c_void) -> bool;
        }
        fn text(x: CF) -> Option<String> {
            if x.is_null() {
                return None;
            }
            let mut b = [0i8; 128];
            unsafe {
                CFStringGetCString(x, b.as_mut_ptr(), 128, 0x08000100).then(|| {
                    std::ffi::CStr::from_ptr(b.as_ptr())
                        .to_string_lossy()
                        .into_owned()
                })
            }
        }
        let info = IOPSCopyPowerSourcesInfo();
        if info.is_null() {
            return json!({"unavailable":"IOPSCopyPowerSourcesInfo"});
        }
        let source = text(IOPSGetProvidingPowerSourceType(info));
        let list = IOPSCopyPowerSourcesList(info);
        let mut capacity = None;
        if !list.is_null() {
            if CFArrayGetCount(list) > 0 {
                let d = IOPSGetPowerSourceDescription(info, CFArrayGetValueAtIndex(list, 0));
                let key = CFStringCreateWithCString(
                    std::ptr::null(),
                    c"Current Capacity".as_ptr(),
                    0x08000100,
                );
                if !d.is_null() && !key.is_null() {
                    let n = CFDictionaryGetValue(d, key);
                    let mut value = 0i64;
                    if !n.is_null() && CFNumberGetValue(n, 4, (&mut value as *mut i64).cast()) {
                        capacity = Some(value);
                    }
                }
                if !key.is_null() {
                    CFRelease(key);
                }
            }
            CFRelease(list);
        }
        CFRelease(info);
        json!({"source":source,"battery_percent":capacity})
    }
}
#[cfg(not(target_os = "macos"))]
pub(super) fn sample() -> Value {
    json!({"sample_wall_ns":heimdall_archive::wall_ns(),"app_commit":commit(),"unavailable":"macOS probes"})
}
#[cfg(test)]
mod tests {
    #[test]
    fn host_probe_is_bounded_and_has_no_shell_dependencies() {
        let s = super::sample();
        assert!(s["sample_wall_ns"].is_u64());
        assert!(serde_json::to_vec(&s).unwrap().len() < 16384);
        #[cfg(target_os = "macos")]
        assert!(s["memory"]["free_bytes"].is_u64());
    }
}
