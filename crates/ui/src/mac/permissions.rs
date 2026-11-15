//! The macOS permission calls behind Setup. `status` never prompts. `request_all` prompts: it runs only from Setup's
//! Allow All button, and never when the UI runs with `Options::fake_permissions` (the demo). `request_at_launch` asks
//! again for a grant macOS has no answer for once Setup was allowed (an update's new signature drops the old grant;
//! crates/app/src/launch_permissions.rs decides), never with `fake_permissions` either.
//!
//! System Audio Recording Only has no public status or request call (E-macos-feasibility.md): macOS asks when the
//! first audio tap starts, so `request_all` hands that step to the app through a callback, and the app reports the
//! state it observes (`PermissionState::Unconfirmed` until audio flows).

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_app_kit::NSWorkspace;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL, NSUserDefaults};

use crate::model::{Permission, PermissionState, PermissionRequests};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

fn key(p: Permission) -> objc2::rc::Retained<NSString> { NSString::from_str(&format!("permissionRequested.{}", p.title())) }
fn ledger() -> PermissionRequests {
    let defaults = NSUserDefaults::standardUserDefaults();
    PermissionRequests { asked: Permission::ALL.map(|p| defaults.boolForKey(&key(p))), ..Default::default() }
}
static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Migrate the prior app's persisted Allow All consent into the once-only ledger.
pub fn restore_setup_requested() {
    let defaults = NSUserDefaults::standardUserDefaults();
    for p in Permission::ALL { defaults.setBool_forKey(true, &key(p)); }
    defaults.synchronize();
}
pub fn system_audio() -> PermissionState {
    if ledger().asked[1] { PermissionState::Unconfirmed } else { PermissionState::NotDetermined }
}
pub fn request_in_flight() -> bool { IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) }

/// Current states, without prompting. System Audio is absent: only the app can tell.
pub fn status() -> Vec<(Permission, PermissionState)> {
    vec![
        (Permission::Microphone, microphone()),
        (Permission::ScreenRecording, screen_recording()),
        (Permission::Accessibility, accessibility()),
    ]
}

/// Records the grants in `states` for `granted_once`. Only the installed app calls it (lab and test runs would record
/// their own process's grants).
pub fn remember_grants(states: &[(Permission, PermissionState)]) {
    let defaults = NSUserDefaults::standardUserDefaults();
    for (p, s) in states {
        if *s == PermissionState::Granted && !defaults.boolForKey(&granted_key(*p)) { defaults.setBool_forKey(true, &granted_key(*p)); }
    }
}

fn granted_key(p: Permission) -> objc2::rc::Retained<NSString> { NSString::from_str(&format!("permissionGranted.{}", p.title())) }
/// Whether this app (its bundle ID, any build) was seen holding `p` (`remember_grants`): an optional grant that is gone
/// now was lost (an update's new signature), not declined.
pub fn granted_once(p: Permission) -> bool { NSUserDefaults::standardUserDefaults().boolForKey(&granted_key(p)) }

pub fn microphone() -> PermissionState {
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return PermissionState::NotDetermined };
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) } {
        AVAuthorizationStatus::Authorized => PermissionState::Granted,
        AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => PermissionState::Denied,
        _ => PermissionState::NotDetermined,
    }
}

pub fn screen_recording() -> PermissionState {
    if objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        PermissionState::Granted
    } else {
        ledger().observed(Permission::ScreenRecording, false)
    }
}

pub fn accessibility() -> PermissionState {
    if unsafe { AXIsProcessTrusted() } {
        PermissionState::Granted
    } else {
        ledger().observed(Permission::Accessibility, false)
    }
}

/// Asks for everything in one pass: Microphone, then System Audio (the app's tap), Screen Recording and
/// Accessibility. Already-granted permissions are skipped; macOS shows each undecided one once.
pub(super) fn request_all(request_system_audio: impl Fn() + Send + Sync + 'static) {
    use std::sync::atomic::Ordering;
    if IN_FLIGHT.swap(true, Ordering::AcqRel) { return; }
    let mut ledger = ledger();
    let Some(pending) = ledger.begin(&status()) else {
        IN_FLIGHT.store(false, Ordering::Release);
        // Recover a crash after the ledger was saved but before app consent was saved.
        request_system_audio();
        return;
    };
    let defaults = NSUserDefaults::standardUserDefaults();
    for p in &pending { defaults.setBool_forKey(true, &key(*p)); }
    // Flush the once-only ledger before TCC can display a modal or the app exits.
    defaults.synchronize();
    let mic = pending.contains(&Permission::Microphone);
    // Screen Recording's and Accessibility's prompts come up after the request below and hand activation back when
    // they close (`setup::refocus_after`); without them Setup takes it back here.
    let prompts_follow = pending.iter().any(|p| matches!(p, Permission::ScreenRecording | Permission::Accessibility));
    let rest = move || {
        dispatch2::DispatchQueue::main().exec_async(move || {
            request_system_audio();
            if pending.contains(&Permission::ScreenRecording) { objc2_core_graphics::CGRequestScreenCaptureAccess(); }
            if pending.contains(&Permission::Accessibility) {
                let key = NSString::from_str("AXTrustedCheckOptionPrompt");
                let yes = NSNumber::numberWithBool(true);
                let options = NSDictionary::from_slices(&[&*key], &[&*yes]);
                unsafe { AXIsProcessTrustedWithOptions(&*options as *const _ as *const std::ffi::c_void) };
            }
            IN_FLIGHT.store(false, Ordering::Release);
            super::with_state(move |s| {
                s.poll_permissions();
                // The request itself finished even if TCC grants did not change.
                if let Some(w) = s.setup.as_mut() {
                    w.show(&s.permissions);
                    if !prompts_follow { w.refocus(); }
                }
            });
        });
    };
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return rest() };
    if mic && microphone() == PermissionState::NotDetermined {
        let rest = std::sync::Mutex::new(Some(rest));
        let block = RcBlock::new(move |_granted: Bool| {
            if let Some(rest) = rest.lock().ok().and_then(|mut r| r.take()) { rest(); }
        });
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &block) };
    } else { rest(); }
}

/// Asks macOS again at launch, after Setup was allowed: the microphone (`AVCaptureDevice`, only while not determined)
/// and Screen Recording (`CGRequestScreenCaptureAccess`, which macOS shows only to a build it has no answer for). The
/// once-only ledger is not consulted: it remembers Setup's ask, not this build's grant. When the microphone answers,
/// the states are read again and the app hears `PermissionsChanged`.
pub fn request_at_launch(mic: bool, screen: bool) {
    super::with_state(move |s| {
        if s.options.fake_permissions || s.options.render_dir.is_some() { return; }
        if screen && !objc2_core_graphics::CGPreflightScreenCaptureAccess() { objc2_core_graphics::CGRequestScreenCaptureAccess(); }
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return };
        if mic && microphone() == PermissionState::NotDetermined {
            let block = RcBlock::new(move |_granted: Bool| {
                dispatch2::DispatchQueue::main().exec_async(|| super::with_state(|s| s.poll_permissions()));
            });
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &block) };
        }
    });
}

/// Opens the permission's Privacy pane for any unresolved grant.
pub fn open_system_settings(permission: Permission) {
    let anchor = match permission {
        Permission::Microphone => "Privacy_Microphone",
        Permission::SystemAudio => "Privacy_AudioCapture",
        Permission::ScreenRecording => "Privacy_ScreenCapture",
        Permission::Accessibility => "Privacy_Accessibility",
    };
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(&url)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}
