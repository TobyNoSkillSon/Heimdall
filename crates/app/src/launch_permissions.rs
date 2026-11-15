//! The permissions Heimdall needs, checked at every launch once Setup was allowed (VM run 14 Nov, upgrade from
//! alpha.1): macOS ties the microphone, Screen Recording and Accessibility grants to the app's signature, an ad-hoc
//! build's signature changes with every build, and the grant stays with the old one. Setup's Allow page asks once and
//! remembers that it asked, so after the upgrade nothing asked again, even after `tccutil reset Microphone`, and the
//! Microphone row said "No sound from …" for good.
//!
//! At launch: the microphone (required) is asked for when macOS has no answer for it (the prompt is the user's own;
//! Setup already got their consent), and is a red pill that opens its System Settings pane when denied, until
//! `permission_health` sees it back. Screen Recording and Accessibility are optional in Setup, so they count as lost
//! only when this app was once seen holding them (`ui::permissions::granted_once`; a record alpha.1 did not keep).
//! Only for parts that are on: the microphone while Listening is on, Screen Recording while Seeing is on. System Audio
//! Recording has no status call; its denial is read from silent taps (`system_audio_health`).
use ui::{Permission, PermissionState};

/// One red pill: (health key, part, permission, label, action). The keys are `permission_health`'s, so the grant coming
/// back clears the pill.
pub(crate) type Pill = (&'static str, ui::Part, Permission, &'static str, &'static str);

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Ask macOS for the microphone (`AVCaptureDevice` says not determined).
    pub request_mic: bool,
    /// Ask macOS for Screen Recording (`CGRequestScreenCaptureAccess` shows its prompt only when macOS has no answer
    /// for this build).
    pub request_screen: bool,
    pub pills: Vec<Pill>,
}

pub(crate) const MIC_LOST: Pill = (
    "permission:mic",
    ui::Part::Microphone,
    Permission::Microphone,
    "Microphone access lost",
    "macOS can drop it after an update. Click to open System Settings \u{25b8} Privacy & Security \u{25b8} Microphone and turn Heimdall on",
);
pub(crate) const SCREEN_LOST: Pill = (
    "permission:screen",
    ui::Part::Screen,
    Permission::ScreenRecording,
    "Screen Recording access lost",
    "macOS can drop it after an update. Click to open System Settings \u{25b8} Privacy & Security \u{25b8} Screen Recording and turn Heimdall on",
);
pub(crate) const ACCESSIBILITY_LOST: Pill = (
    "permission:accessibility",
    ui::Part::Heimdall,
    Permission::Accessibility,
    "Accessibility access lost",
    "macOS can drop it after an update. Click to open System Settings \u{25b8} Privacy & Security \u{25b8} Accessibility and turn Heimdall on",
);

fn state(states: &[(Permission, PermissionState)], p: Permission) -> Option<PermissionState> {
    states.iter().find(|(q, _)| *q == p).map(|(_, s)| *s)
}

/// What the launch does about each permission. `setup_allowed`: Setup's Allow All was pressed (`setup_requested`);
/// before that Setup's own Allow page asks. `granted_once`: the permissions this app was ever seen holding.
pub(crate) fn plan(setup_allowed: bool, listening_on: bool, seeing_on: bool, states: &[(Permission, PermissionState)], granted_once: &[Permission]) -> Plan {
    let mut plan = Plan::default();
    if !setup_allowed {
        return plan;
    }
    match state(states, Permission::Microphone) {
        Some(PermissionState::NotDetermined) if listening_on => plan.request_mic = true,
        Some(PermissionState::Denied) if listening_on => plan.pills.push(MIC_LOST),
        _ => {}
    }
    // Screen Recording has no "not asked" state of its own (Setup's ledger says it asked): a grant once held and gone is
    // asked for again, which macOS shows only to a build it has no answer for, and shown as lost.
    let lost = |p| granted_once.contains(&p) && state(states, p).is_some_and(|s| s != PermissionState::Granted);
    if seeing_on && lost(Permission::ScreenRecording) {
        plan.request_screen = true;
        plan.pills.push(SCREEN_LOST);
    }
    if lost(Permission::Accessibility) {
        plan.pills.push(ACCESSIBILITY_LOST);
    }
    plan
}

/// Whether macOS denies the microphone now (Getting ready's Microphone row, `setup_ready::microphone_access`).
pub(crate) fn mic_denied(states: &[(Permission, PermissionState)]) -> bool {
    state(states, Permission::Microphone) == Some(PermissionState::Denied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionState::*;

    fn states(mic: PermissionState, screen: PermissionState, ax: PermissionState) -> Vec<(Permission, PermissionState)> {
        vec![(Permission::Microphone, mic), (Permission::ScreenRecording, screen), (Permission::Accessibility, ax)]
    }
    const ALL: &[Permission] = &[Permission::Microphone, Permission::ScreenRecording, Permission::Accessibility];
    const NONE: &[Permission] = &[];

    /// VM run 14 Nov, phase 3: after `tccutil reset Microphone` the microphone was not determined and nothing asked.
    #[test]
    fn an_unanswered_microphone_is_asked_for_at_launch_after_setup() {
        let p = plan(true, true, true, &states(NotDetermined, Granted, Granted), NONE);
        assert_eq!(p, Plan { request_mic: true, ..Default::default() });
        // Before Setup's Allow All, Setup's Allow page asks; nothing at launch.
        assert_eq!(plan(false, true, true, &states(NotDetermined, Denied, Denied), ALL), Plan::default());
        // Listening off: no prompt for a part that is off.
        assert!(!plan(true, false, true, &states(NotDetermined, Granted, Granted), NONE).request_mic);
    }

    /// The upgrade: the grant stayed with alpha.1's signature. Denied is a pill that opens the Microphone pane, under the
    /// key `permission_health` clears when the grant comes back.
    #[test]
    fn a_denied_microphone_is_a_pill_to_system_settings() {
        let p = plan(true, true, false, &states(Denied, Denied, Granted), ALL);
        assert!(!p.request_mic);
        assert_eq!(p.pills, vec![MIC_LOST]);
        let (key, _, permission, label, action) = MIC_LOST;
        assert_eq!((key, permission), ("permission:mic", Permission::Microphone));
        assert_eq!(label, "Microphone access lost");
        assert!(action.contains("Privacy & Security \u{25b8} Microphone") && action.contains("update"), "{action}");
        assert!(mic_denied(&states(Denied, Granted, Granted)));
        assert!(!mic_denied(&states(NotDetermined, Granted, Granted)));
        // Everything granted: nothing to do.
        assert_eq!(plan(true, true, true, &states(Granted, Granted, Granted), ALL), Plan::default());
    }

    /// Screen Recording and Accessibility are optional: lost only when this app once held them, and Screen Recording
    /// only while Seeing is on. A user who skipped them gets no pill and no prompt at every launch.
    #[test]
    fn optional_grants_count_as_lost_only_when_once_held() {
        let p = plan(true, true, true, &states(Granted, Denied, Granted), ALL);
        assert!(p.request_screen && !p.request_mic);
        assert_eq!(p.pills, vec![SCREEN_LOST]);
        assert_eq!(plan(true, true, false, &states(Granted, Denied, Granted), ALL), Plan::default());
        assert_eq!(plan(true, true, true, &states(Granted, Granted, Denied), ALL).pills, vec![ACCESSIBILITY_LOST]);
        assert_eq!(plan(true, true, true, &states(Granted, Denied, Denied), NONE), Plan::default(), "never granted: declined, not lost");
    }
}
