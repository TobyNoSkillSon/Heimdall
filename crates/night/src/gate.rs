//! Fail-closed gate. No Accessibility or capture permission is requested.
use anyhow::{bail, ensure, Result};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug)]
pub struct State {
    pub on_ac: bool,
    pub idle_seconds: f64,
}
pub const IDLE_SECONDS: f64 = 300.;
impl State {
    pub fn eligible(self) -> bool {
        self.on_ac && self.idle_seconds.is_finite() && self.idle_seconds >= IDLE_SECONDS
    }
}
pub fn bounded_output(command: &mut Command) -> Result<String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("system probe timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success() && output.stdout.len() < 65536,
        "system probe failed"
    );
    Ok(String::from_utf8(output.stdout)?)
}
#[cfg(target_os = "macos")]
pub fn state() -> Result<State> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event: u32) -> f64;
    }
    let idle_seconds = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
    use std::{
        ffi::{c_void, CStr},
        ptr,
    };
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> *const c_void;
        fn IOPSGetProvidingPowerSourceType(info: *const c_void) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
        fn CFStringGetCString(
            value: *const c_void,
            buffer: *mut i8,
            size: isize,
            encoding: u32,
        ) -> bool;
    }
    let info = unsafe { IOPSCopyPowerSourcesInfo() };
    ensure!(!info.is_null(), "power snapshot unavailable");
    let kind = unsafe { IOPSGetProvidingPowerSourceType(info) };
    let mut buffer = [0i8; 64];
    let valid = kind != ptr::null()
        && unsafe { CFStringGetCString(kind, buffer.as_mut_ptr(), 64, 0x08000100) };
    unsafe {
        CFRelease(info);
    }
    ensure!(valid, "power source unavailable");
    let kind = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str()?;
    ensure!(
        ["AC Power", "Battery Power", "UPS Power"].contains(&kind),
        "unknown power source"
    );
    Ok(State {
        on_ac: kind == "AC Power",
        idle_seconds,
    })
}
#[cfg(not(target_os = "macos"))]
pub fn state() -> Result<State> {
    bail!("night gate requires macOS; no assumed idle/power state")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn power_and_idle_fail_closed() {
        for s in [
            State {
                on_ac: false,
                idle_seconds: 900.,
            },
            State {
                on_ac: true,
                idle_seconds: 299.9,
            },
            State {
                on_ac: true,
                idle_seconds: f64::NAN,
            },
        ] {
            assert!(!s.eligible());
        }
        assert!(State {
            on_ac: true,
            idle_seconds: 300.
        }
        .eligible());
    }
}

/// Only background scheduler/worker threads call this; live QoS is untouched.
#[cfg(target_os = "macos")]
pub fn background_thread() -> Result<()> {
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;
    }
    ensure!(
        unsafe { pthread_set_qos_class_self_np(0x09, 0) } == 0,
        "background QoS unavailable"
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub fn background_thread() -> Result<()> {
    Ok(())
}
pub fn background() -> Result<()> {
    background_thread()?;
    // An unprivileged process cannot raise its priority back to 10 once it runs
    // nicer (a parent started under `nice -n 19`); that is already background.
    let applied = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) } == 0;
    ensure!(
        applied || unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) } >= 10,
        "background nice unavailable"
    );
    Ok(())
}
/// Bug note 2026-11-08T11-30-09: the speech-split helper ran at 127% CPU during a Discord call and the call's audio
/// glitched. Only its main thread was background; ONNX Runtime's worker thread is the library's own. This puts the
/// whole process in Darwin background (what `taskpolicy -b` sets): every thread, also ones a library starts later,
/// runs on efficiency cores with throttled I/O, below coreaudiod and the call app. Only a dedicated helper process
/// calls it; never Heimdall itself.
#[cfg(target_os = "macos")]
pub fn background_process() -> Result<()> {
    background()?;
    ensure!(
        unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG) } == 0,
        "Darwin background unavailable"
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub fn background_process() -> Result<()> {
    background()
}
/// What `background_process` set, read back: (process in Darwin background, this thread's QoS class).
#[cfg(target_os = "macos")]
pub fn background_state() -> (bool, u32) {
    unsafe extern "C" {
        fn qos_class_self() -> u32;
    }
    // Reads back 1, not PRIO_DARWIN_BG, once set (measured 9 Nov).
    let darwin_bg = unsafe { libc::getpriority(libc::PRIO_DARWIN_PROCESS, 0) } > 0;
    (darwin_bg, unsafe { qos_class_self() })
}

#[cfg(all(test, target_os = "macos"))]
mod background_process_tests {
    const CHILD: &str = "HEIMDALL_BACKGROUND_PROCESS_CHILD";
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    /// Runs in a re-executed copy of the test binary, so the test runner itself is never demoted.
    #[test]
    fn background_process_child() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        assert_eq!(super::background_state().0, false, "a fresh process starts outside Darwin background");
        super::background_process().unwrap();
        assert_eq!(super::background_state(), (true, QOS_CLASS_BACKGROUND));
        assert!(unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) } >= 10);
        // A thread started afterwards (as ONNX Runtime's pool is) does not inherit the QoS class (measured 9 Nov: it
        // starts at default, 0x15), which is how the helper's second thread ran at full priority. The process-wide
        // Darwin background covers it: a spinning later thread measured scheduler priority 4, against 31 without.
        let later = std::thread::spawn(super::background_state).join().unwrap();
        assert!(later.0, "later thread outside Darwin background: {later:?}");
        println!("background-process-child-ok");
    }
    #[test]
    fn helper_process_runs_at_background_qos_with_its_later_threads() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gate::background_process_tests::background_process_child", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("background-process-child-ok"), "{text}");
    }
}
