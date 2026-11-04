//! Whether a click on the island came from the user's hand (4 Nov evening: approval grants switched on). A card option
//! that carries a grant (`agent_notice::GrantItem`) approves an exact delete or login-item removal for the deny-list
//! hook, so it counts only when a real pointer pressed it: a left mouse-down and mouse-up the HID system delivered to
//! the island's click target, both on that option (`mac/island/host.rs`, `Event::HardwareClick`).
//!
//! What never counts: an accessibility press (the island has no accessibility tree, so an AXPress or System Events
//! `click` reaches no option at all), Option plus a digit, Return, his voice (`Event::AskSet`), typed Other, the
//! scenario checks' `sendEvent` taps, and any event another process posted (`CGEventPost`, `CGEventPostToPid`,
//! `cliclick`, `osascript`, screen sharing).
//!
//! How (each field read from the NSEvent's underlying `CGEvent`):
//! - `kCGEventSourceUnixProcessID` is 0. A posted event carries its poster's pid, and the window server stamps it
//!   from the posting connection: a probe on this Mac (macOS 27, 4 Nov) set the field to 0 on a `CGEvent` and posted
//!   it to its own pid, and it arrived with the poster's pid. An event made in this process (`NSEvent` or `CGEvent`
//!   constructors) carries this process's pid. Hardware events carry 0: measured on this Mac (macOS 27, 4 Nov, a
//!   passive global monitor over seven of the user's trackpad clicks, down and up: pid 0, state 1, user data 0), as
//!   Chrome Remote Desktop's input monitor relies on and the window server's own log says ("sender's PID isn't 0 or
//!   self"). Not probed: `CGEventPost` into the HID tap (it would reach the user's screen); it is a posted event like the
//!   one probed, so it is expected to carry its poster's pid. A card click that is not by hand is logged with its
//!   source (`island: a click on … is not the user's own pointer`), so a misjudged real click shows up.
//! - `kCGEventSourceStateID` is the HID system state (1): a synthetic event's source is the combined session state
//!   (0) or a private one, unless its poster asked for the HID state (which the pid then refuses).
//! - `kCGEventSourceUserData` is 0: tools that tag their own events set it.
//!
//! Not covered (a guard like the rest of the deny list, not a boundary): a virtual HID device (Karabiner's driver,
//! Universal Control from a paired iPad or Mac) looks like hardware; it needs a driver or the user's own devices. A process
//! that moves the pointer onto a granting option just as the user clicks elsewhere makes his real click land there. A
//! click through Screen Sharing is refused (its pid is the screen-sharing agent's), so a grant is clicked at the Mac.

/// What one mouse event says about where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventSource {
    /// A left mouse-down or mouse-up (not a right or other button, a drag, a key or a gesture).
    pub left_button: bool,
    /// `kCGEventSourceUnixProcessID`; `None` when the event has no `CGEvent`.
    pub source_pid: Option<i64>,
    /// `kCGEventSourceStateID`.
    pub source_state: i64,
    /// `kCGEventSourceUserData`.
    pub user_data: i64,
}

/// `kCGEventSourceStateHIDSystemState`.
pub const HID_SYSTEM_STATE: i64 = 1;

impl EventSource {
    /// An event Heimdall made itself, or one with no source at all: never a real click.
    pub const NONE: EventSource = EventSource { left_button: false, source_pid: None, source_state: 0, user_data: 0 };

    /// A left-button event the HID system delivered: no process posted it, its source is the HID state and nothing
    /// tagged it.
    pub fn is_hardware(&self) -> bool {
        self.left_button && self.source_pid == Some(0) && self.source_state == HID_SYSTEM_STATE && self.user_data == 0
    }

    /// For the log when a grant is withheld: where the click came from.
    pub fn describe(&self) -> String {
        match self.source_pid {
            None => "no event source".into(),
            Some(0) if !self.left_button => "not a left click".into(),
            Some(0) => format!("source state {}, user data {}", self.source_state, self.user_data),
            Some(pid) => format!("posted by process {pid}"),
        }
    }
}

/// A press and its release on the same target make a real click only when both are hardware.
pub fn real_click(down: &EventSource, up: &EventSource) -> bool {
    down.is_hardware() && up.is_hardware()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(pid: Option<i64>, state: i64, user: i64) -> EventSource {
        EventSource { left_button: true, source_pid: pid, source_state: state, user_data: user }
    }

    #[test]
    fn only_a_left_click_from_the_hid_system_is_real() {
        // A real trackpad click, down and up, as a passive monitor read seven of them on this Mac (4 Nov, macOS 27).
        let hand = ev(Some(0), HID_SYSTEM_STATE, 0);
        assert!(hand.is_hardware());
        assert!(real_click(&hand, &hand));
        // Values read on this Mac (4 Nov probe): an NSEvent or CGEvent made in-process carries this process's pid and
        // the combined session state; one made with a HID source still carries the pid; a forged pid 0 posted to a
        // pid arrives with the poster's pid.
        for (why, e) in [
            ("an NSEvent made in this process (the checks' sendEvent taps)", ev(Some(88081), 0, 0)),
            ("a CGEvent with a HID source, posted by another process", ev(Some(88200), HID_SYSTEM_STATE, 0)),
            ("cliclick / CGEventPost from the combined session state", ev(Some(4242), 0, 0)),
            ("a private source state claiming pid 0", ev(Some(0), 0, 0)),
            ("a private source state id", ev(Some(0), 7, 0)),
            ("tagged with user data", ev(Some(0), HID_SYSTEM_STATE, 99)),
            ("no CGEvent at all", ev(None, HID_SYSTEM_STATE, 0)),
            ("the empty source", EventSource::NONE),
        ] {
            assert!(!e.is_hardware(), "{why}");
            assert!(!real_click(&hand, &e) && !real_click(&e, &hand), "{why}: both halves must be real");
        }
        let right = EventSource { left_button: false, ..hand };
        assert!(!right.is_hardware(), "a right or other button");
        assert_eq!(ev(Some(4242), 0, 0).describe(), "posted by process 4242");
    }
}
