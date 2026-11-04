//! When Apple's echo cancellation runs (night 23 Oct; "only when output is the built-in speakers,
//! off on AirPods and headphones"). Capture owns the voice-processing unit and refuses it where it would break the
//! AirPods fix (a Bluetooth default device or mic; capture/src/macos/vpio.rs); this decides when it is wanted:
//!
//! - Heimdall's speech is queued or synthesizing (the unit's 1-3 s start overlaps the synthesis), an ack waits out
//!   The user's quiet before it plays (the same head start: an ack lasts about a second, so a unit asked for only once it
//!   plays came up after it ended; review 23 Oct, finding 3), a bank line or ack plays, or a sentence played through
//!   speakers less than `ECHO_CANCEL_TAIL` ago (`SpeechPolicy::echo_cancel_wanted`). Not on a wake alone: a text-only reply never starts it (review 23 Oct), and
//! - the default output is speakers (`output_route`, read only while it is wanted).
//!
//! Only then: the unit ducks other apps' audio and makes Heimdall look like a call, so it never runs while Heimdall is
//! quiet, and never on headphones, where there is no echo. Everything else (the first words before the unit is up,
//! an output it refuses) is the text guard's (pipeline/echo_guard.rs).
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};
use std::time::Duration;

/// How often the wanted state is looked at: atomics only, and one route read while it is wanted.
const TICK: Duration = Duration::from_millis(100);

/// Heimdall is about to make a sound or is making one, and echo cancellation is not switched off (`wanted`'s first
/// half; atomics only).
pub(crate) fn heimdall_sounding_soon(t: &super::Tools) -> bool {
    t.speech_policy.echo_cancel_on()
        && (t.speech_policy.echo_cancel_wanted(capture::host_now_ns()) || t.agent_speech_busy() || t.voice_bank.ack_waiting()
            || t.voice_bank.line_playing())
}

/// The pure decision.
pub(crate) fn wanted(heimdall_speaking_soon: bool, speakers: impl FnOnce() -> bool) -> bool {
    heimdall_speaking_soon && speakers()
}

/// Runs until `stop` is set or the tools are gone, telling capture each change and logging capture's answer.
pub(crate) fn spawn(control: capture::EchoControl, tools: Weak<super::Tools>, stop: Arc<AtomicBool>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("heimdall-echo-cancel".into()).spawn(move || {
        let mut asked = false;
        let mut reported = capture::EchoCancel::Off;
        while !stop.load(Ordering::Acquire) {
            let Some(t) = tools.upgrade() else { break };
            let soon = heimdall_sounding_soon(&t);
            let want = wanted(soon, || {
                crate::pipeline::output_route::current() == crate::pipeline::output_route::Route::Speakers
            });
            // Never the last owner here (shutdown drops Tools elsewhere).
            if Arc::strong_count(&t) == 1 { std::thread::spawn(move || drop(t)); } else { drop(t); }
            if want != asked {
                control.set_echo_cancellation(want);
                asked = want;
            }
            let state = control.echo_cancellation();
            if state != reported {
                eprintln!("voice: echo cancellation {state:?}");
                reported = state;
            }
            std::thread::sleep(TICK);
        }
        control.set_echo_cancellation(false);
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_while_speaking_and_on_speakers() {
        assert!(super::wanted(true, || true));
        assert!(!super::wanted(true, || false), "headphones and AirPods: never");
        let mut read = false;
        assert!(!super::wanted(false, || { read = true; true }));
        assert!(!read, "the route is not read while Heimdall is quiet");
    }
    /// Review 23 Oct, finding 3: an ack waiting out the user's 2.5 s already wants the unit, so it is up when the ack
    /// plays, not after it ended. Echo cancellation switched off (settings `echo_cancel_off`, the shipped default
    /// tonight): never.
    #[test]
    fn a_waiting_ack_warms_the_unit_unless_echo_cancellation_is_off() {
        use crate::voice_bank::tests::{add_clip, probe, tools, wait_idle};
        let (_t, tools) = tools();
        let clip = add_clip(&tools, "on it", 1600);
        let _log = probe(&tools, std::time::Duration::from_millis(1));
        assert!(!super::heimdall_sounding_soon(&tools), "quiet");
        crate::voice_bank::play_ack_after(&tools, clip, "u1", true, std::time::Duration::from_millis(400), |_| {}).unwrap();
        assert!(tools.voice_bank.ack_waiting() && !tools.voice_bank.line_playing(), "still waiting");
        assert!(super::heimdall_sounding_soon(&tools), "the waiting ack did not ask for the unit");
        tools.speech_policy.set_echo_cancel_off(true);
        assert!(!super::heimdall_sounding_soon(&tools), "echo cancellation off");
        wait_idle(&tools);
    }
}
