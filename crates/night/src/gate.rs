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
pub fn parse_power(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|line| line.contains("'AC Power'"))
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
        assert!(parse_power(
            "Now drawing from 'AC Power'\n -InternalBattery 85%; charging"
        ));
        assert!(!parse_power(
            "Now drawing from 'Battery Power'\n -InternalBattery 100%; charged"
        ));
        assert!(!parse_power("unknown AC Power"));
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
