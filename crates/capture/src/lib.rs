//! Independent native sources, a host-clock timeline, and a lossless raw archive.
//! No capture starts on import or discovery. Live capture requires explicit consent.
mod archive;
mod commit;
mod echo;
#[cfg(target_os = "macos")]
mod macos;
mod pipeline;
mod resample;
mod source;
pub mod tap_gate;
mod time;
use anyhow::Result;
pub use archive::{Archive, ArchivedBlock};
pub use echo::{EchoCancel, EchoControl};
pub use pipeline::{settle_stopped_archives, AudioChunk, CaptureEvent, CaptureReceiver};
pub use source::{app_identity, owning_app_path, AppIdentity, AppOwner, NativeFormat, SourceId, SourceInfo, SourceKind};
use std::{path::PathBuf, sync::Arc, thread::JoinHandle};
pub use time::{host_now_ns, ClockMapper};

/// An app-owned raw archive adapter. Called only on the intake thread, before
/// downmix/resampling/VAD. This permits format/retention/disk policy in app.
pub trait ArchiveSink: Send {
    fn add_source(&mut self, info: &SourceInfo, ns: u64) -> Result<()>;
    fn remove_source(&mut self, id: &SourceId, ns: u64) -> Result<()>;
    fn gap(&mut self, id: Option<&SourceId>, ns: u64, detail: &str) -> Result<()>;
    fn append(&mut self, id: &SourceId, ns: u64, format: NativeFormat, samples: &[f32], discontinuity: bool) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    fn commit_wait(&self) -> Option<std::time::Duration>;
    /// The capture stopped: commit everything accepted so far. Blocking; `Pipeline::close` bounds the wait.
    fn close(&mut self) -> Result<()> {
        self.flush()
    }
}
impl ArchiveSink for Archive {
    fn add_source(&mut self, info: &SourceInfo, ns: u64) -> Result<()> { self.add_source(info, ns) }
    fn remove_source(&mut self, id: &SourceId, ns: u64) -> Result<()> { self.remove_source(id, ns) }
    fn gap(&mut self, id: Option<&SourceId>, ns: u64, detail: &str) -> Result<()> { self.gap(id, ns, detail) }
    fn append(&mut self, id: &SourceId, ns: u64, format: NativeFormat, samples: &[f32], discontinuity: bool) -> Result<()> { self.append(id, ns, format, samples, discontinuity) }
    fn flush(&mut self) -> Result<()> { self.flush() }
    fn commit_wait(&self) -> Option<std::time::Duration> { self.commit_wait() }
}

/// An explicit acknowledgement of the setup action. Construct only after the
/// signed app's setup flow has obtained permission; this is not a TCC status bit.
#[derive(Clone, Copy, Debug)]
pub struct CaptureConsent {
    microphone: bool,
    system_audio: bool,
}
impl CaptureConsent {
    /// The caller guarantees the user granted capture consent. Starting a tap without
    /// it may trigger an OS prompt; there is no public AudioCapture preflight API.
    pub fn after_user_grant() -> Self {
        Self { microphone: true, system_audio: true }
    }
    /// Setup has been explicitly requested; each source type is enabled independently.
    pub fn after_setup(microphone: bool, system_audio: bool) -> Self { Self { microphone, system_audio } }
    fn bits(self) -> u32 { u32::from(self.microphone) | (u32::from(self.system_audio) << 1) }
}
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub archive_dir: PathBuf,
    /// UID of the user's chosen primary microphone. `None` means the built-in microphone.
    pub microphone: Option<String>,
    /// UIDs of extra microphones the user switched on. Off (empty) by default.
    pub extra_microphones: Vec<String>,
    /// Receives the input-device list and the selection whenever either changes.
    pub microphones: Option<crossbeam_channel::Sender<MicrophoneState>>,
    /// App Audio: `AppIdentity::key`s the user chose to record. Empty by default: no app's process gets a tap.
    /// Heimdall's own process is always tapped (its speech stays in the archive), independent of this list.
    pub recorded_apps: Vec<String>,
    /// Receives the apps whose processes are producing output now (Heimdall excluded), whenever that set
    /// changes. Learned from the process-output listeners alone; an unrecorded app is listed, never tapped.
    pub playing_apps: Option<crossbeam_channel::Sender<Vec<AppIdentity>>>,
    /// With no microphone chosen, use macOS's default input rather than the built-in one (the public edition,
    /// 5 Nov; the personal edition keeps the built-in fallback).
    pub system_default_input: bool,
}

/// An input device Heimdall could use as the user's microphone. Listing one never opens it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    pub uid: String,
    pub name: String,
    /// Core Audio transport type is built-in (the MacBook Pro Microphone).
    pub built_in: bool,
    /// macOS's default input now (System Settings ▸ Sound ▸ Input).
    pub default: bool,
}

/// What the HAL owner sees: every connected input, the choices, and the mics it opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicrophoneState {
    pub devices: Vec<InputDevice>,
    /// The chosen primary UID; `None` means the built-in microphone.
    pub chosen: Option<String>,
    /// The primary microphone actually used: the chosen one, else built-in.
    pub in_use: Option<String>,
    /// Extra microphones the user switched on, connected or not.
    pub extras: Vec<String>,
    /// Every microphone Heimdall opens (with microphone consent): the primary first, then
    /// switched-on extras that are connected. Nothing else.
    pub open: Vec<String>,
}

/// Exactly one microphone: the chosen one when connected and usable, otherwise the built-in one.
/// No other input is ever a fallback, so an unchosen Bluetooth headset is never opened
/// (opening its mic forces it into low-quality headset mode). `usable` excludes a chosen device
/// whose recovery attempts are exhausted until a fresh HAL event.
pub fn select_microphone<'a>(
    devices: &'a [InputDevice],
    chosen: Option<&str>,
    usable: impl Fn(&InputDevice) -> bool,
) -> Option<&'a InputDevice> {
    select_microphone_from(devices, chosen, false, usable)
}

/// `select_microphone`, where `system_default` makes macOS's default input the one used while nothing is chosen
/// (`CaptureConfig::system_default_input`); a chosen device that is missing still falls back to the built-in one.
pub fn select_microphone_from<'a>(
    devices: &'a [InputDevice],
    chosen: Option<&str>,
    system_default: bool,
    usable: impl Fn(&InputDevice) -> bool,
) -> Option<&'a InputDevice> {
    match chosen {
        Some(uid) => devices.iter().find(|d| d.uid == uid && usable(d)).or_else(|| devices.iter().find(|d| d.built_in)),
        None if system_default => devices.iter().find(|d| d.default && usable(d)).or_else(|| devices.iter().find(|d| d.built_in)),
        None => devices.iter().find(|d| d.built_in),
    }
}

impl MicrophoneState {
    /// The published state for `devices`; `in_use` is `select_microphone`'s answer and `open`
    /// adds only extras the user switched on that are connected and usable.
    pub fn new(
        devices: Vec<InputDevice>,
        chosen: Option<String>,
        extras: Vec<String>,
        usable: impl Fn(&InputDevice) -> bool,
    ) -> Self {
        Self::with_default(devices, chosen, extras, false, usable)
    }

    /// `new`, with macOS's default input used while nothing is chosen when `system_default`.
    pub fn with_default(
        devices: Vec<InputDevice>,
        chosen: Option<String>,
        extras: Vec<String>,
        system_default: bool,
        usable: impl Fn(&InputDevice) -> bool,
    ) -> Self {
        let in_use = select_microphone_from(&devices, chosen.as_deref(), system_default, &usable).map(|d| d.uid.clone());
        let mut open: Vec<String> = in_use.iter().cloned().collect();
        for d in &devices {
            if extras.contains(&d.uid) && usable(d) && !open.contains(&d.uid) {
                open.push(d.uid.clone());
            }
        }
        Self { devices, chosen, in_use, extras, open }
    }
}
/// The archive session a standalone capture writes: `path` itself when it already names a session
/// (`.../session-*`), else a new `session-<ms>-capture` inside the store at `path`.
/// Asks for System Audio Recording now, without capturing: macOS's prompt shows (`tap_gate`). Returns at once; the
/// capture's taps start when the ask comes back, and the microphone never waits for it. Asked again after a failed
/// ask; once answered in this launch, it answers at once (macOS shows its prompt once per app anyway).
pub fn ask_system_audio() -> tap_gate::TapAccess {
    let gate = tap_gate::global();
    gate.ask_again();
    gate.access()
}

/// Where system audio's ask is now (`tap_gate::TapAccess`), without sending it.
pub fn system_audio() -> Option<tap_gate::TapAccess> {
    tap_gate::global().peek()
}

pub fn session_address(path: &std::path::Path) -> PathBuf {
    if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("session-")) {
        return path.to_path_buf();
    }
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    path.join(format!("session-{ms}-capture"))
}
impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            archive_dir: session_address(&heimdall_home::data_dir().join("archive")),
            microphone: None,
            extra_microphones: vec![],
            microphones: None,
            recorded_apps: vec![],
            playing_apps: None,
            system_default_input: false,
        }
    }
}
/// RAII capture owner. Drop stops all native sources, drains raw queues, and
/// commits the session. Keep this alongside the receiver in the app process.
pub struct Capture {
    stop: Arc<std::sync::atomic::AtomicBool>,
    owner: Option<JoinHandle<()>>,
    wake: std::thread::Thread,
    access: Arc<std::sync::atomic::AtomicU32>,
    microphone: Arc<std::sync::Mutex<(Option<String>, Vec<String>)>>,
    recorded_apps: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    dirty: Arc<std::sync::atomic::AtomicU32>,
    echo: EchoControl,
}
impl Capture {
    /// Echo guard (23 Oct): ask for Apple voice-processing echo cancellation on the primary microphone, or release
    /// it. Non-blocking and idempotent; call it on every change (the app: while Heimdall speaks on speakers, plus a
    /// short tail). It runs only when every safety gate passes (speaker output, no Bluetooth default input or mic,
    /// not the built-in mic), and for at most 120 s per request.
    pub fn set_echo_cancellation(&self, wanted: bool) {
        self.echo.set_echo_cancellation(wanted)
    }
    /// What echo cancellation is doing now; `Active` means the mic source carries cleaned audio.
    pub fn echo_cancellation(&self) -> EchoCancel {
        self.echo.echo_cancellation()
    }
    /// A `Clone + Send + Sync` handle with the same two calls, for another thread. Inert once `Capture` is dropped.
    pub fn echo_control(&self) -> EchoControl {
        self.echo.clone()
    }
    /// Choose the primary microphone by UID (`None`: built-in) and the extra microphones
    /// the user switched on. Takes effect on the HAL owner thread: a mic that is no longer wanted
    /// ends (an archived removal) and a newly wanted one starts as a new source.
    pub fn set_microphones(&self, primary: Option<String>, extras: Vec<String>) {
        if let Ok(mut m) = self.microphone.lock() {
            *m = (primary, extras);
        }
        #[cfg(target_os = "macos")]
        self.dirty.fetch_or(macos::MICROPHONE, std::sync::atomic::Ordering::Release);
        self.wake.unpark();
    }
    /// App Audio: the app keys to record from now on. Applied on the HAL owner thread: a newly chosen
    /// app's processes get taps (playing or dormant), an unchosen app's taps end as archived removals.
    pub fn set_recorded_apps(&self, keys: Vec<String>) {
        if let Ok(mut r) = self.recorded_apps.lock() {
            *r = keys.into_iter().collect();
        }
        #[cfg(target_os = "macos")]
        self.dirty.fetch_or(macos::APPS, std::sync::atomic::Ordering::Release);
        self.wake.unpark();
    }
    /// Event-driven grant/revocation update. Bit 4 asks the owner to retry denied taps.
    pub fn refresh_permissions(&self, consent: CaptureConsent) {
        self.access.store(consent.bits() | 4, std::sync::atomic::Ordering::Release);
        self.wake.unpark();
    }
    #[cfg(target_os = "macos")]
    pub fn start(
        config: CaptureConfig,
        consent: CaptureConsent,
    ) -> Result<(Self, CaptureReceiver)> {
        macos::start(config, consent)
    }
    #[cfg(not(target_os = "macos"))]
    pub fn start(_: CaptureConfig, _: CaptureConsent) -> Result<(Self, CaptureReceiver)> {
        anyhow::bail!("Heimdall capture requires macOS 14.4+")
    }
    /// Same consent gate as start; the app supplies the archive on its owning thread.
    #[cfg(target_os = "macos")]
    pub fn start_with_archive(config: CaptureConfig, consent: CaptureConsent, archive: Box<dyn ArchiveSink>) -> Result<(Self, CaptureReceiver)> {
        macos::start_with_archive(config, consent, archive)
    }
    /// Read-only discovery with HAL listeners, no taps/devices started, no archive.
    #[cfg(target_os = "macos")]
    pub fn discover_only() -> Result<Self> {
        macos::discover_only(&CaptureConfig::default())
    }
    /// Discovery that also reports microphones to `config.microphones`; nothing is opened.
    #[cfg(target_os = "macos")]
    pub fn discover(config: &CaptureConfig) -> Result<Self> {
        macos::discover_only(config)
    }
    /// Permission-free idle measurement of the archive intake and discovery.
    #[cfg(target_os = "macos")]
    pub fn idle(config: CaptureConfig) -> Result<(Self, CaptureReceiver)> {
        macos::idle(config)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.echo.0.closed.store(true, std::sync::atomic::Ordering::Release);
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.wake.unpark();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

#[cfg(test)]
mod microphone_tests {
    use super::*;
    fn dev(uid: &str, name: &str, built_in: bool) -> InputDevice {
        InputDevice { uid: uid.into(), name: name.into(), built_in, default: false }
    }

    /// 5 Nov: the public edition starts on macOS's default input; a choice still wins, and the personal
    /// edition keeps the built-in fallback.
    #[test]
    fn with_nothing_chosen_the_public_edition_uses_the_system_default_input() {
        let shure = InputDevice { default: true, ..dev("Shure", "Shure MV7i", false) };
        let devices = [built_in(), shure.clone(), airpods()];
        assert_eq!(select_microphone_from(&devices, None, true, |_| true).map(|d| d.uid.as_str()), Some("Shure"));
        assert_eq!(select_microphone_from(&devices, None, false, |_| true).map(|d| d.uid.as_str()), Some("BuiltInMicrophoneDevice"));
        assert_eq!(select_microphone_from(&devices, Some("AC-3E-B1:input"), true, |_| true).map(|d| d.uid.as_str()), Some("AC-3E-B1:input"));
        assert_eq!(select_microphone_from(&devices, None, true, |d| d.uid != "Shure").map(|d| d.uid.as_str()), Some("BuiltInMicrophoneDevice"), "a failing default falls back");
        assert_eq!(select_microphone_from(&[], None, true, |_| true), None, "no input at all: none");
        assert_eq!(MicrophoneState::with_default(devices.to_vec(), None, vec![], true, |_| true).open, ["Shure"]);
    }
    fn built_in() -> InputDevice {
        dev("BuiltInMicrophoneDevice", "MacBook Pro Microphone", true)
    }
    fn airpods() -> InputDevice {
        dev("AC-3E-B1:input", "AirPods Pro", false)
    }
    fn zoom() -> InputDevice {
        dev("zoom.us.ZoomAudioDevice", "ZoomAudioDevice", false)
    }
    fn pick(devices: &[InputDevice], chosen: Option<&str>) -> Option<String> {
        select_microphone(devices, chosen, |_| true).map(|d| d.uid.clone())
    }

    /// voice (night 1022): the user's setup, the Shure chosen and AirPods connecting while he watches his iPhone. The
    /// AirPods mic is opened only when chosen in the mic picker: never when they connect, never as the Shure's
    /// fallback, never as an extra by default.
    #[test]
    fn capture_never_auto_picks_a_bluetooth_input() {
        let shure = dev("AppleUSBAudioEngine:Shure Inc:Shure MV7i:2,3", "Shure MV7i", false);
        let devices = [airpods(), shure.clone(), built_in()];
        let state = MicrophoneState::new(devices.to_vec(), Some(shure.uid.clone()), vec![], |_| true);
        assert_eq!(state.open, [shure.uid.clone()]);
        // The Shure failing or unplugged falls back to the built-in mic, not the AirPods.
        let failing = MicrophoneState::new(devices.to_vec(), Some(shure.uid.clone()), vec![], |d| d.uid != shure.uid);
        assert_eq!(failing.open, ["BuiltInMicrophoneDevice"]);
        let unplugged = MicrophoneState::new(vec![airpods(), built_in()], Some(shure.uid.clone()), vec![], |_| true);
        assert_eq!(unplugged.open, ["BuiltInMicrophoneDevice"]);
        // Only an explicit choice opens them.
        assert_eq!(pick(&devices, Some("AC-3E-B1:input")).as_deref(), Some("AC-3E-B1:input"));
    }
    /// Bug note 2026-10-31T01-32-22 ("the chosen microphone returns after a restart"): every launch from 30 Oct
    /// 18:42 announced "Shure MV7i → MacBook Pro Microphone". macOS's own log shows the Shure left USB at 30 Oct
    /// 17:27:30 and never enumerated again, and settings.json still holds it: the fallback was right. The HAL owner
    /// recomputes this state on every device-list event, so the choice must survive any launch order.
    #[test]
    fn the_chosen_mic_stays_chosen_while_missing_and_returns_when_it_appears() {
        let shure = dev("AppleUSBAudioEngine:Shure Inc:Shure MV7i:MV7i#7-8bc6:2,3", "Shure MV7i", false);
        let chosen = Some(shure.uid.clone());
        let at = |devices: Vec<InputDevice>| MicrophoneState::new(devices, chosen.clone(), vec![], |_| true);
        // Missing at launch (unplugged, or enumerating late): built-in records, the Shure stays the choice.
        let launch = at(vec![built_in(), airpods()]);
        assert_eq!((launch.chosen.as_deref(), launch.in_use.as_deref()), (Some(shure.uid.as_str()), Some("BuiltInMicrophoneDevice")));
        assert_eq!(launch.open, ["BuiltInMicrophoneDevice"]);
        // It appears later (the device-list listener's next snapshot): back to the Shure, the built-in closes.
        let appeared = at(vec![built_in(), airpods(), shure.clone()]);
        assert_eq!((appeared.in_use.as_deref(), appeared.open.clone()), (Some(shure.uid.as_str()), vec![shure.uid.clone()]));
        // Present at launch: the Shure at once.
        assert_eq!(at(vec![shure.clone(), built_in()]).in_use, chosen);
        // Never appears: built-in for good, still never the AirPods, and the choice is kept.
        let never = at(vec![airpods(), built_in()]);
        assert_eq!((never.chosen, never.in_use.as_deref()), (chosen.clone(), Some("BuiltInMicrophoneDevice")));
    }
    #[test]
    fn first_run_uses_built_in_and_never_opens_airpods() {
        // Default choice (None) with AirPods connected: only the built-in mic.
        let devices = [airpods(), zoom(), built_in()];
        assert_eq!(pick(&devices, None).as_deref(), Some("BuiltInMicrophoneDevice"));
    }

    #[test]
    fn chosen_mic_wins_when_connected() {
        let devices = [built_in(), airpods(), zoom()];
        assert_eq!(pick(&devices, Some("AC-3E-B1:input")).as_deref(), Some("AC-3E-B1:input"));
        assert_eq!(pick(&devices, Some("zoom.us.ZoomAudioDevice")).as_deref(), Some("zoom.us.ZoomAudioDevice"));
    }

    #[test]
    fn disconnect_falls_back_to_built_in_and_reconnect_switches_back() {
        let chosen = Some("AC-3E-B1:input");
        let timeline = [
            (vec![built_in(), airpods()], "AC-3E-B1:input"),
            (vec![built_in()], "BuiltInMicrophoneDevice"),
            (vec![built_in(), zoom()], "BuiltInMicrophoneDevice"),
            (vec![built_in(), zoom(), airpods()], "AC-3E-B1:input"),
        ];
        for (devices, expected) in timeline {
            assert_eq!(pick(&devices, chosen).as_deref(), Some(expected), "{devices:?}");
        }
    }

    #[test]
    fn exhausted_chosen_device_falls_back_without_other_inputs() {
        let devices = [airpods(), zoom(), built_in()];
        let usable = |d: &InputDevice| d.uid != "AC-3E-B1:input";
        let state = MicrophoneState::new(devices.to_vec(), Some("AC-3E-B1:input".into()), vec![], usable);
        assert_eq!(state.in_use.as_deref(), Some("BuiltInMicrophoneDevice"));
        assert_eq!(state.chosen.as_deref(), Some("AC-3E-B1:input"));
        assert_eq!(state.open, ["BuiltInMicrophoneDevice"]);
    }

    #[test]
    fn only_the_primary_is_open_by_default() {
        let state = MicrophoneState::new(vec![airpods(), zoom(), built_in()], None, vec![], |_| true);
        assert_eq!(state.open, ["BuiltInMicrophoneDevice"]);
        let state =
            MicrophoneState::new(vec![airpods(), zoom(), built_in()], Some("AC-3E-B1:input".into()), vec![], |_| true);
        assert_eq!(state.open, ["AC-3E-B1:input"]);
    }

    #[test]
    fn extras_open_only_when_switched_on_and_connected() {
        let extras = vec!["zoom.us.ZoomAudioDevice".to_string(), "AC-3E-B1:input".to_string()];
        let all = MicrophoneState::new(vec![built_in(), airpods(), zoom()], None, extras.clone(), |_| true);
        // Primary first, then switched-on extras in device order; nothing unswitched.
        assert_eq!(all.open, ["BuiltInMicrophoneDevice", "AC-3E-B1:input", "zoom.us.ZoomAudioDevice"]);
        // An extra disconnects: it closes, the rest stay; it reopens on reconnect.
        let gone = MicrophoneState::new(vec![built_in(), zoom()], None, extras.clone(), |_| true);
        assert_eq!(gone.open, ["BuiltInMicrophoneDevice", "zoom.us.ZoomAudioDevice"]);
        assert_eq!(gone.extras, extras);
        let back = MicrophoneState::new(vec![built_in(), zoom(), airpods()], None, extras.clone(), |_| true);
        assert_eq!(back.open, ["BuiltInMicrophoneDevice", "zoom.us.ZoomAudioDevice", "AC-3E-B1:input"]);
        // An extra that is also the primary opens once.
        let same = MicrophoneState::new(vec![built_in(), airpods()], Some("AC-3E-B1:input".into()), extras, |_| true);
        assert_eq!(same.open, ["AC-3E-B1:input"]);
    }

    #[test]
    fn extra_with_exhausted_recovery_is_not_reopened() {
        let usable = |d: &InputDevice| d.uid != "zoom.us.ZoomAudioDevice";
        let state = MicrophoneState::new(vec![built_in(), zoom()], None, vec!["zoom.us.ZoomAudioDevice".into()], usable);
        assert_eq!(state.open, ["BuiltInMicrophoneDevice"]);
    }

    #[test]
    fn no_built_in_and_no_chosen_opens_nothing() {
        // Never substitute another input (an AirPods mic would switch them to headset mode).
        assert_eq!(pick(&[airpods(), zoom()], None), None);
        assert_eq!(pick(&[airpods(), zoom()], Some("missing")), None);
        assert_eq!(pick(&[], None), None);
    }

    #[test]
    fn built_in_is_found_by_transport_not_name_or_order() {
        let renamed = dev("BuiltInMicrophoneDevice", "Mikrofon (MacBook Pro)", true);
        let devices = [zoom(), airpods(), renamed];
        assert_eq!(pick(&devices, None).as_deref(), Some("BuiltInMicrophoneDevice"));
    }
}
