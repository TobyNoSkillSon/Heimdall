//! Apple voice processing (AUVoiceIO, `kAudioUnitSubType_VoiceProcessingIO`) for the echo guard, 23 Oct night.
//! The user: echo cancellation only when output goes to speakers, never on AirPods or headphones. The unit runs only
//! while the app asks (Heimdall speaking on speakers plus a short tail), so its ducking (about −8 dB at Min) and its
//! call-like state are limited to Heimdall's own replies, which already look call-like. Input-only: the echo
//! reference is the device's whole output mix (lab/aec/REPORT.md), so playback never goes through the unit.
//! Its cleaned mono audio goes into the primary mic's ring; the mic's own IOProc hands it to listen (`echo.rs`).
//!
//! Creation takes 1–3 s cold, so a dedicated worker thread creates, watches and tears down the unit; the HAL owner
//! thread only publishes which mic source is primary. Every property ID and struct below was checked against the
//! macOS 27 SDK headers (AUComponent.h, AudioComponent.h, AudioUnitProperties.h, AudioOutputUnit.h) on 23 Oct.
//!
//! Bug note 23 Oct 17:38 (build 748cb57): after the unit failed to start (−10875) several times, all of the Mac's
//! output, other apps included, went silent on AirPods until echo cancellation was switched off. So (night 24 Oct):
//! - No Bluetooth route at all: not the default output, input or alert output, not the mic, and no Bluetooth headset
//!   connected (macOS may make it the default input while the unit starts). The gates run before the unit exists,
//!   after it is created, after Initialize and after Start, and every tick while it runs (`refusal`).
//! - Every unit is torn down in reverse (Stop if started, Uninitialize if initialized, Dispose), and afterwards the
//!   system is compared with a snapshot taken before it existed: a default device moved to the unit's aggregate (or to
//!   nothing) is put back, and so is a changed nominal sample rate (`restore`, run twice as the HAL settles).
//! - Two failures in one run (a failed start, or a unit that started and stalled), or any teardown that had to put
//!   something back, switch echo cancellation off until Heimdall restarts: no unit is created again. A default device
//!   moved by the user or macOS to a real device is left alone and only logged. Known tradeoff: a nominal rate another
//!   app set during a reply is also put back.
//!
//! Everything that touches the system goes through `Platform`, so the gates and the failure-restore sequence are
//! tested with fakes. What only a live test can show: whether the unit changes anything the snapshot does not cover
//! (other apps' ducking, the AirPods' hands-free switch), and whether putting a device or rate back really brings the
//! sound back.
use super::hal;
use crate::{
    echo::{EchoRing, EchoTarget, Shared, State},
    pipeline::Command,
};
use objc2_core_audio::*;
use objc2_core_audio_types::*;
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// The built-in microphone stays raw until a live check (REPORT: while any unit runs, plain clients of the built-in
/// mic see 3 quiet channels instead of 1, which would restart Heimdall's own IOProc mid-reply and leave a gap).
/// Switch on only after `lab/aec/REPORT.md` live check 5 shows the IOProc keeps its format.
pub const CLEAN_BUILT_IN_MIC: bool = false;
/// The app forgot to release: the unit goes after this long anyway (longest replies run about a minute).
const MAX_SESSION: Duration = Duration::from_secs(120);
/// A started unit that delivers no processed input for this long is treated as failed (macOS 27 −10875 / starved
/// input reports, M1K3 PR #639).
#[cfg(not(test))]
const STALL: Duration = Duration::from_secs(1);
/// The first processed input after a cold Start may take longer (cold creation alone is 1–3 s).
#[cfg(not(test))]
const FIRST_INPUT: Duration = Duration::from_secs(3);
// Tests: the fake unit never calls back, so a stall shows within a tenth of a second.
#[cfg(test)]
const STALL: Duration = Duration::from_millis(100);
#[cfg(test)]
const FIRST_INPUT: Duration = Duration::from_millis(100);
/// On release the ring stays live this long first, so the mux fades out on real cleaned audio (fade is 10 ms).
const FADE_OUT_WAIT: Duration = Duration::from_millis(30);
/// After a unit is gone the system is compared with its snapshot twice: once the HAL has taken the unit's own
/// changes back, and again later (coreaudiod applies some of them asynchronously).
const SETTLE: [Duration; 2] = [Duration::from_millis(300), Duration::from_millis(900)];
/// Failures in one run of Heimdall (a failed start, or a unit that started and stalled) before echo cancellation stays
/// off until it restarts (23 Oct: four attempts in under a minute, each a new unit).
const MAX_FAILURES: u32 = 2;
/// How often a running unit's full gate (every device, the aggregate's members) is read; the defaults and the mic are
/// read every tick.
const FULL_GATE: Duration = Duration::from_secs(1);

const fn fourcc(c: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*c)
}
// AUComponent.h: kAudioUnitType_Output = 'auou', kAudioUnitSubType_VoiceProcessingIO = 'vpio',
// kAudioUnitManufacturer_Apple = 'appl'.
const TYPE_OUTPUT: u32 = fourcc(b"auou");
const SUBTYPE_VPIO: u32 = fourcc(b"vpio");
const MANUFACTURER_APPLE: u32 = fourcc(b"appl");
// AudioUnitProperties.h: kAudioUnitScope_Global = 0, _Input = 1, _Output = 2.
const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;
// AudioUnitProperties.h: kAudioUnitProperty_StreamFormat = 8, kAudioOutputUnitProperty_CurrentDevice = 2000,
// kAudioOutputUnitProperty_EnableIO = 2003, kAudioOutputUnitProperty_SetInputCallback = 2005,
// kAUVoiceIOProperty_VoiceProcessingEnableAGC = 2101, kAUVoiceIOProperty_OtherAudioDuckingConfiguration = 2108
// (macOS 14), kAUVoiceIOOtherAudioDuckingLevelMin = 10.
const PROP_STREAM_FORMAT: u32 = 8;
const PROP_CURRENT_DEVICE: u32 = 2000;
const PROP_ENABLE_IO: u32 = 2003;
const PROP_SET_INPUT_CALLBACK: u32 = 2005;
const PROP_VOICE_AGC: u32 = 2101;
const PROP_OTHER_AUDIO_DUCKING: u32 = 2108;
const DUCKING_LEVEL_MIN: u32 = 10;
/// Element 1 is the input (microphone) side of an I/O unit, element 0 the output side.
const INPUT_ELEMENT: u32 = 1;
const OUTPUT_ELEMENT: u32 = 0;

type AudioUnit = *mut c_void;
type AudioComponent = *mut c_void;
/// AudioComponent.h, inside `#pragma pack(push, 4)`: five UInt32s, so 20 bytes either way.
#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}
/// AUComponent.h `AURenderCallback`.
type RenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> i32;
/// AudioUnitProperties.h `AURenderCallbackStruct { AURenderCallback inputProc; void *inputProcRefCon; }`.
#[repr(C)]
struct RenderCallbackStruct {
    input_proc: Option<RenderCallback>,
    ref_con: *mut c_void,
}
/// AudioUnitProperties.h `AUVoiceIOOtherAudioDuckingConfiguration { Boolean mEnableAdvancedDucking;
/// AUVoiceIOOtherAudioDuckingLevel mDuckingLevel; }` (Boolean = unsigned char, the level a UInt32: 8 bytes).
#[repr(C)]
struct DuckingConfiguration {
    advanced: u8,
    level: u32,
}
#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(c: AudioComponent, desc: *const AudioComponentDescription) -> AudioComponent;
    fn AudioComponentInstanceNew(c: AudioComponent, out: *mut AudioUnit) -> i32;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> i32;
    fn AudioUnitInitialize(unit: AudioUnit) -> i32;
    fn AudioUnitUninitialize(unit: AudioUnit) -> i32;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> i32;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32)
        -> i32;
    fn AudioUnitRender(
        unit: AudioUnit,
        flags: *mut u32,
        time: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        data: *mut AudioBufferList,
    ) -> i32;
    fn AudioOutputUnitStart(unit: AudioUnit) -> i32;
    fn AudioOutputUnitStop(unit: AudioUnit) -> i32;
}

fn status(call: &str, s: i32) -> String {
    format!("{call}: OSStatus {s} (0x{:08x})", s as u32)
}

// ---- Safety gates: what the HAL says, then a pure decision ----

/// What the gates need to know about one device.
#[derive(Clone, Debug, Default)]
pub(crate) struct Route {
    pub uid: String,
    pub name: String,
    pub transport: u32,
    /// Output data source ('hdpn' when the built-in jack has headphones in, 'ispk' for the speakers).
    pub data_source: Option<u32>,
    pub has_input: bool,
}
impl Route {
    fn read(device: u32) -> Option<Self> {
        let global = kAudioObjectPropertyScopeGlobal;
        if device == 0 {
            return None;
        }
        Some(Self {
            uid: hal::string(device, kAudioDevicePropertyDeviceUID).unwrap_or_default(),
            name: hal::string(device, kAudioObjectPropertyName).unwrap_or_else(|| format!("device {device}")),
            transport: hal::property::<u32>(device, kAudioDevicePropertyTransportType, global).unwrap_or(0),
            data_source: hal::property::<u32>(device, kAudioDevicePropertyDataSource, kAudioObjectPropertyScopeOutput)
                .ok(),
            has_input: !hal::list(device, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput)
                .unwrap_or_default()
                .is_empty(),
        })
    }
    fn bluetooth(&self) -> bool {
        self.transport == fourcc(b"blue") || self.transport == fourcc(b"blea")
    }
}
fn transport_name(t: u32) -> String {
    let b = t.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        format!("0x{t:08x}")
    }
}
/// Only outputs that are loudspeakers: built-in (not the headphone jack), USB output-only, HDMI, DisplayPort,
/// Thunderbolt, PCI, FireWire. Bluetooth, AirPlay, virtual and aggregate outputs are never cleaned against.
fn speaker_output(out: &Route) -> Result<(), String> {
    let name = &out.name;
    if out.bluetooth() {
        return Err(format!("default output {name} is Bluetooth"));
    }
    if out.data_source == Some(fourcc(b"hdpn")) || out.uid.contains("Headphone") {
        return Err(format!("default output {name} is headphones"));
    }
    if out.transport == fourcc(b"usb ") && out.has_input {
        return Err(format!("default output {name} is a USB headset (it also has inputs)"));
    }
    const SPEAKERS: [&[u8; 4]; 7] = [b"bltn", b"usb ", b"hdmi", b"dprt", b"thun", b"pci ", b"1394"];
    if !SPEAKERS.iter().any(|t| fourcc(t) == out.transport) {
        return Err(format!(
            "default output {name} ({}) is not a speaker route",
            transport_name(out.transport)
        ));
    }
    Ok(())
}
/// Everything the gates read at one moment.
#[derive(Clone, Debug, Default)]
pub(crate) struct Routes {
    pub output: Option<Route>,
    pub input: Option<Route>,
    /// Where alert sounds go (macOS may keep it on AirPods while the main output moves).
    pub system_output: Option<Route>,
    pub mic: Route,
    pub mic_built_in: bool,
    /// Other connected Bluetooth devices that can record: macOS may make one the default input while the unit starts,
    /// and the unit then puts it into hands-free.
    pub bluetooth_headsets: Vec<Route>,
}
/// Every refusal rule, in order. `None` means the unit may exist.
pub(crate) fn refusal(r: &Routes) -> Option<String> {
    let Some(output) = &r.output else {
        return Some("no default output device".into());
    };
    if let Err(reason) = speaker_output(output) {
        return Some(reason);
    }
    if let Some(s) = r.system_output.as_ref().filter(|s| s.bluetooth()) {
        return Some(format!("alert sounds go to {}, which is Bluetooth", s.name));
    }
    // Creating the unit configures the DEFAULT devices before ours: a Bluetooth default input goes hands-free.
    if let Some(input) = r.input.as_ref().filter(|i| i.bluetooth()) {
        return Some(format!("default input {} is Bluetooth; the unit would force it into hands-free", input.name));
    }
    if r.mic.bluetooth() {
        return Some(format!("primary microphone {} is Bluetooth", r.mic.name));
    }
    if let Some(h) = r.bluetooth_headsets.first() {
        return Some(format!("{} is connected over Bluetooth with a microphone; the unit could put it into hands-free", h.name));
    }
    if r.mic_built_in && !CLEAN_BUILT_IN_MIC {
        return Some(format!(
            "primary microphone {} is the built-in one; the unit changes its format for other clients (off until a live check)",
            r.mic.name
        ));
    }
    None
}

// ---- Everything that touches the system, behind one seam ----

/// The system's default devices the unit configures or may move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Output,
    Input,
    SystemOutput,
}
impl Role {
    const ALL: [Role; 3] = [Role::Output, Role::Input, Role::SystemOutput];
    fn label(self) -> &'static str {
        match self {
            Role::Output => "output",
            Role::Input => "input",
            Role::SystemOutput => "alert output",
        }
    }
    fn selector(self) -> u32 {
        match self {
            Role::Output => kAudioHardwarePropertyDefaultOutputDevice,
            Role::Input => kAudioHardwarePropertyDefaultInputDevice,
            Role::SystemOutput => kAudioHardwarePropertyDefaultSystemOutputDevice,
        }
    }
}
/// Why a start ended without a running unit.
#[derive(Debug, PartialEq)]
enum End {
    Refused(String),
    Failed(String),
    Cancelled,
}
/// The HAL and the unit's calls. `CoreAudio` is the real one; tests use a fake that records the calls and changes
/// the system the way a misbehaving unit might.
trait Platform: Clone {
    type Unit;
    fn default_device(&self, role: Role) -> u32;
    fn route(&self, device: u32) -> Option<Route>;
    fn devices(&self) -> Vec<u32>;
    /// The unit's private aggregate ("VPAUAggregateAudioDevice").
    fn voice_processing(&self, device: u32) -> bool;
    fn members(&self, aggregate: u32) -> Vec<u32>;
    fn nominal_rate(&self, device: u32) -> Option<f64>;
    fn set_nominal_rate(&self, device: u32, rate: f64) -> i32;
    fn set_default_device(&self, role: Role, device: u32) -> i32;
    fn create(&self) -> Result<Self::Unit, String>;
    /// What the input callback renders from.
    fn render_handle(&self, unit: &Self::Unit) -> AudioUnit;
    /// Input only, the devices, Min ducking, AGC off, f32 mono at the mic's rate, the input callback.
    fn configure(&self, unit: &Self::Unit, target: &EchoTarget, ctx: *mut c_void) -> Result<(), End>;
    fn unit_devices(&self, unit: &Self::Unit) -> Vec<u32>;
    fn initialize(&self, unit: &Self::Unit) -> i32;
    fn start(&self, unit: &Self::Unit) -> i32;
    fn stop(&self, unit: &Self::Unit) -> i32;
    fn uninitialize(&self, unit: &Self::Unit) -> i32;
    fn dispose(&self, unit: Self::Unit) -> i32;
    fn settle(&self, wait: Duration);
}

fn routes<P: Platform>(p: &P, target: &EchoTarget) -> Routes {
    let bluetooth_headsets = p
        .devices()
        .into_iter()
        .filter(|&d| d != target.device && !p.voice_processing(d))
        .filter_map(|d| p.route(d))
        .filter(|r| r.bluetooth() && r.has_input)
        .collect();
    Routes { bluetooth_headsets, ..default_routes(p, target) }
}
/// The defaults and the mic only (a running unit's per-tick read).
fn default_routes<P: Platform>(p: &P, target: &EchoTarget) -> Routes {
    let read = |role| p.route(p.default_device(role));
    Routes {
        output: read(Role::Output),
        input: read(Role::Input),
        system_output: read(Role::SystemOutput),
        mic: p.route(target.device).unwrap_or_default(),
        mic_built_in: target.built_in,
        bluetooth_headsets: vec![],
    }
}
/// After Create, Initialize and Start: did a Bluetooth device end up in the unit, its private aggregate, or the
/// defaults it reconfigures?
fn bluetooth_in_unit<P: Platform>(p: &P, unit: &P::Unit) -> Option<String> {
    if p.unit_devices(unit).into_iter().any(|d| p.route(d).is_some_and(|r| r.bluetooth())) {
        return Some("the unit's device is Bluetooth".into());
    }
    for role in Role::ALL {
        if let Some(r) = p.route(p.default_device(role)).filter(|r| r.bluetooth()) {
            return Some(format!("the default {} became Bluetooth ({})", role.label(), r.name));
        }
    }
    for device in p.devices().into_iter().filter(|&d| p.voice_processing(d)) {
        if let Some(r) = p.members(device).into_iter().filter_map(|m| p.route(m)).find(|r| r.bluetooth()) {
            return Some(format!("the voice-processing aggregate contains Bluetooth {}", r.name));
        }
    }
    None
}

/// The system as it was before a unit existed: the default devices and the nominal sample rates of the devices the
/// unit can touch (the defaults and the mic; Bluetooth rates follow their profile and are never set).
#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    defaults: Vec<(Role, u32)>,
    rates: Vec<(u32, f64)>,
}
fn snapshot<P: Platform>(p: &P, target: &EchoTarget) -> Snapshot {
    let defaults: Vec<(Role, u32)> = Role::ALL.iter().map(|&r| (r, p.default_device(r))).collect();
    let mut devices: Vec<u32> = defaults.iter().map(|d| d.1).chain([target.device]).filter(|&d| d != 0).collect();
    devices.sort_unstable();
    devices.dedup();
    let rates = devices
        .into_iter()
        .filter(|&d| !p.route(d).is_some_and(|r| r.bluetooth()))
        .filter_map(|d| p.nominal_rate(d).map(|r| (d, r)))
        .collect();
    Snapshot { defaults, rates }
}
/// What a teardown found: what it had to put back (the unit's own changes: these switch echo cancellation off), and
/// differences it left alone (a device the user or macOS chose).
#[derive(Debug, Default, PartialEq)]
struct Notes {
    put_back: Vec<String>,
    left: Vec<String>,
}
impl Notes {
    fn all(&self) -> Vec<String> {
        self.put_back.iter().chain(&self.left).cloned().collect()
    }
}
/// After a unit is gone: puts back what it changed and says every difference from `before`. A default device moved
/// to the unit's aggregate, to nothing or to a device that no longer exists goes back to the one before (if it is
/// still there); one moved to another real device (AirPods connecting, the user's choice) is left and reported. A
/// changed nominal rate on a device still present goes back.
fn restore<P: Platform>(p: &P, before: &Snapshot) -> Notes {
    let present = p.devices();
    let name = |d: u32| match p.route(d) {
        Some(r) => r.name,
        None if d == 0 => "nothing".into(),
        None => format!("a device that is gone ({d})"),
    };
    let mut notes = Notes::default();
    for &(role, was) in &before.defaults {
        let now = p.default_device(role);
        if now == was {
            continue;
        }
        let unit_made = now == 0 || p.voice_processing(now) || !present.contains(&now);
        if unit_made && was != 0 && present.contains(&was) {
            let s = p.set_default_device(role, was);
            notes.put_back.push(if s == 0 {
                format!("default {} had moved to {}; put back to {}", role.label(), name(now), name(was))
            } else {
                format!("default {} had moved to {}; putting back {} failed ({})", role.label(), name(now), name(was), status("set default", s))
            });
        } else {
            notes.left.push(format!("default {} changed from {} to {}; left as it is", role.label(), name(was), name(now)));
        }
    }
    for &(device, rate) in &before.rates {
        if !present.contains(&device) {
            continue;
        }
        match p.nominal_rate(device) {
            Some(now) if now != rate => {
                let s = p.set_nominal_rate(device, rate);
                notes.put_back.push(if s == 0 {
                    format!("{} sample rate had changed to {now} Hz; put back to {rate} Hz", name(device))
                } else {
                    format!("{} sample rate had changed to {now} Hz; putting back {rate} Hz failed ({})", name(device), status("set rate", s))
                });
            }
            _ => {}
        }
    }
    notes
}
/// Waits for the HAL to settle after a teardown, then restores; twice, since some changes land late. The second pass
/// reports only what it still had to do.
fn settle_and_restore<P: Platform>(p: &P, before: &Snapshot) -> Notes {
    p.settle(SETTLE[0]);
    let mut notes = restore(p, before);
    p.settle(SETTLE[1]);
    let later = restore(p, before);
    notes.put_back.extend(later.put_back.into_iter().map(|n| format!("later: {n}")));
    // What was left alone the first time is still different; say it once.
    notes
}

// ---- The real Core Audio ----

#[derive(Clone, Copy)]
struct CoreAudio;
/// A unit from AudioComponentInstanceNew, disposed exactly once (`Owned::close`).
struct RawUnit(AudioUnit);
impl CoreAudio {
    fn set<T>(unit: &RawUnit, id: u32, scope: u32, element: u32, value: &T) -> i32 {
        // SAFETY: a live unit; `value` is the documented value type of `id` and its size is passed.
        unsafe { AudioUnitSetProperty(unit.0, id, scope, element, (value as *const T).cast(), std::mem::size_of::<T>() as u32) }
    }
    fn set_object<T>(object: u32, selector: u32, value: &T) -> i32 {
        let address = hal::address(selector, kAudioObjectPropertyScopeGlobal);
        // SAFETY: a plain property write of `value`'s documented type and size.
        unsafe {
            AudioObjectSetPropertyData(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                std::mem::size_of::<T>() as u32,
                NonNull::from(value).cast(),
            )
        }
    }
}
impl Platform for CoreAudio {
    type Unit = RawUnit;
    fn default_device(&self, role: Role) -> u32 {
        hal::property::<u32>(kAudioObjectSystemObject as u32, role.selector(), kAudioObjectPropertyScopeGlobal).unwrap_or(0)
    }
    fn route(&self, device: u32) -> Option<Route> {
        Route::read(device)
    }
    fn devices(&self) -> Vec<u32> {
        hal::list(kAudioObjectSystemObject as u32, kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal).unwrap_or_default()
    }
    fn voice_processing(&self, device: u32) -> bool {
        let uid = hal::string(device, kAudioDevicePropertyDeviceUID).unwrap_or_default();
        let name = hal::string(device, kAudioObjectPropertyName).unwrap_or_default();
        hal::voice_processing_device(&uid, &name)
    }
    fn members(&self, aggregate: u32) -> Vec<u32> {
        hal::list(aggregate, kAudioAggregateDevicePropertyActiveSubDeviceList, kAudioObjectPropertyScopeGlobal).unwrap_or_default()
    }
    fn nominal_rate(&self, device: u32) -> Option<f64> {
        hal::property::<f64>(device, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal).ok()
    }
    fn set_nominal_rate(&self, device: u32, rate: f64) -> i32 {
        Self::set_object(device, kAudioDevicePropertyNominalSampleRate, &rate)
    }
    fn set_default_device(&self, role: Role, device: u32) -> i32 {
        Self::set_object(kAudioObjectSystemObject as u32, role.selector(), &device)
    }
    fn create(&self) -> Result<RawUnit, String> {
        let desc = AudioComponentDescription {
            component_type: TYPE_OUTPUT,
            component_sub_type: SUBTYPE_VPIO,
            component_manufacturer: MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        // SAFETY: plain C calls with a valid description and out pointer.
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) };
        if component.is_null() {
            return Err("no voice-processing audio unit on this Mac".into());
        }
        let mut raw: AudioUnit = std::ptr::null_mut();
        let s = unsafe { AudioComponentInstanceNew(component, &mut raw) };
        if s != 0 || raw.is_null() {
            return Err(status("AudioComponentInstanceNew", s));
        }
        Ok(RawUnit(raw))
    }
    fn render_handle(&self, unit: &RawUnit) -> AudioUnit {
        unit.0
    }
    fn configure(&self, unit: &RawUnit, target: &EchoTarget, ctx: *mut c_void) -> Result<(), End> {
        let fail = |call: &str, s: i32| if s != 0 { Err(End::Failed(status(call, s))) } else { Ok(()) };
        // Input only: the echo reference is the device's whole output mix, and no output side means nothing to play.
        fail("EnableIO input", Self::set(unit, PROP_ENABLE_IO, SCOPE_INPUT, INPUT_ELEMENT, &1u32))?;
        fail("EnableIO output", Self::set(unit, PROP_ENABLE_IO, SCOPE_OUTPUT, OUTPUT_ELEMENT, &0u32))?;
        // Creation ducks every output until the output device is set (cubeb-rs 4733efda78): set it at once, and
        // ignore an error (output is disabled; some builds refuse the property then).
        let output = self.default_device(Role::Output);
        if output != 0 {
            let _ = Self::set(unit, PROP_CURRENT_DEVICE, SCOPE_GLOBAL, OUTPUT_ELEMENT, &output);
        }
        fail("CurrentDevice input", Self::set(unit, PROP_CURRENT_DEVICE, SCOPE_GLOBAL, INPUT_ELEMENT, &target.device))?;
        // Unset, the unit ducks other audio by about −30 dB; Min measured about −8 dB. No Min, no session.
        fail(
            "OtherAudioDuckingConfiguration",
            Self::set(unit, PROP_OTHER_AUDIO_DUCKING, SCOPE_GLOBAL, 0, &DuckingConfiguration { advanced: 0, level: DUCKING_LEVEL_MIN }),
        )?;
        // AGC off keeps the cleaned voice at the raw level, so the crossfade and the speech detector's thresholds hold.
        let _ = Self::set(unit, PROP_VOICE_AGC, SCOPE_GLOBAL, 0, &0u32);
        let rate = target.rate as f64;
        let wanted = AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: 1,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let s = Self::set(unit, PROP_STREAM_FORMAT, SCOPE_OUTPUT, INPUT_ELEMENT, &wanted);
        if s != 0 {
            return Err(End::Refused(format!("the unit refused f32 mono at {rate} Hz ({})", status("StreamFormat", s))));
        }
        let mut got = wanted;
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: a live unit; `got` is an ASBD of the size passed.
        let s = unsafe {
            AudioUnitGetProperty(unit.0, PROP_STREAM_FORMAT, SCOPE_OUTPUT, INPUT_ELEMENT, (&mut got as *mut AudioStreamBasicDescription).cast(), &mut size)
        };
        if s != 0 || got.mSampleRate != rate || got.mChannelsPerFrame != 1 || got.mBitsPerChannel != 32 || got.mFormatFlags & kAudioFormatFlagIsFloat == 0 {
            // No resampling in the mux: a mismatched format would drift against the IOProc's frames.
            return Err(End::Refused(format!(
                "the unit would not deliver f32 mono at {rate} Hz (got {} Hz, {} ch, {} bit)",
                got.mSampleRate, got.mChannelsPerFrame, got.mBitsPerChannel
            )));
        }
        let callback = RenderCallbackStruct { input_proc: Some(on_input), ref_con: ctx };
        fail("SetInputCallback", Self::set(unit, PROP_SET_INPUT_CALLBACK, SCOPE_GLOBAL, INPUT_ELEMENT, &callback))
    }
    fn unit_devices(&self, unit: &RawUnit) -> Vec<u32> {
        let mut out = vec![];
        for element in [INPUT_ELEMENT, OUTPUT_ELEMENT] {
            let mut device = 0u32;
            let mut size = 4u32;
            // SAFETY: a live unit; `device` is a UInt32 (AudioObjectID) of the size passed.
            let s = unsafe { AudioUnitGetProperty(unit.0, PROP_CURRENT_DEVICE, SCOPE_GLOBAL, element, (&mut device as *mut u32).cast(), &mut size) };
            if s == 0 && device != 0 {
                out.push(device);
            }
        }
        out
    }
    fn initialize(&self, unit: &RawUnit) -> i32 {
        unsafe { AudioUnitInitialize(unit.0) }
    }
    fn start(&self, unit: &RawUnit) -> i32 {
        unsafe { AudioOutputUnitStart(unit.0) }
    }
    fn stop(&self, unit: &RawUnit) -> i32 {
        unsafe { AudioOutputUnitStop(unit.0) }
    }
    fn uninitialize(&self, unit: &RawUnit) -> i32 {
        unsafe { AudioUnitUninitialize(unit.0) }
    }
    fn dispose(&self, unit: RawUnit) -> i32 {
        // SAFETY: `unit` came from AudioComponentInstanceNew and is consumed here, once.
        unsafe { AudioComponentInstanceDispose(unit.0) }
    }
    fn settle(&self, wait: Duration) {
        thread::sleep(wait);
    }
}

// ---- The unit and its input callback ----

/// Lives in a Box from before the callback is registered until after the unit is disposed.
struct InputCtx {
    unit: AudioUnit,
    ring: Arc<EchoRing>,
    scratch: UnsafeCell<Box<[f32]>>,
    entered: AtomicBool,
    callbacks: AtomicU64,
    render_errors: AtomicU64,
    last_error: AtomicI32,
}
/// The unit's input callback: render the processed mic into the preallocated scratch and hand it to the ring. No
/// allocation, lock, log or syscall.
unsafe extern "C" fn on_input(
    ref_con: *mut c_void,
    flags: *mut u32,
    time: *const AudioTimeStamp,
    bus: u32,
    frames: u32,
    _data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: ref_con is the session's boxed InputCtx, which outlives the unit (disposed first in `Owned::close`).
    let ctx = &*(ref_con as *const InputCtx);
    if ctx
        .entered
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return 0;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `entered` makes this the scratch's only user.
        let scratch = &mut *ctx.scratch.get();
        let n = frames as usize;
        if n == 0 || n > scratch.len() {
            ctx.render_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: (n * 4) as u32,
                mData: scratch.as_mut_ptr().cast(),
            }],
        };
        // SAFETY: the unit is live while callbacks run; `list` points at n f32 frames (the client format is f32 mono).
        let s = AudioUnitRender(ctx.unit, flags, time, bus, frames, &mut list);
        if s != 0 {
            ctx.render_errors.fetch_add(1, Ordering::Relaxed);
            ctx.last_error.store(s, Ordering::Relaxed);
            return;
        }
        let got = (list.mBuffers[0].mDataByteSize as usize / 4).min(n);
        ctx.ring.push_clean(&scratch[..got]);
        ctx.callbacks.fetch_add(1, Ordering::Release);
    }));
    ctx.entered.store(false, Ordering::Release);
    0
}

/// A created unit and how far it got; closed in reverse, each step only if reached.
struct Owned<P: Platform> {
    p: P,
    unit: Option<P::Unit>,
    initialized: bool,
    started: bool,
}
impl<P: Platform> Owned<P> {
    /// Stop, Uninitialize, Dispose; returns any errors. After this no callback runs.
    fn close(&mut self) -> Vec<String> {
        let Some(unit) = self.unit.take() else { return vec![] };
        let mut errors = vec![];
        if std::mem::take(&mut self.started) {
            let s = self.p.stop(&unit);
            if s != 0 {
                errors.push(status("AudioOutputUnitStop", s));
            }
        }
        if std::mem::take(&mut self.initialized) {
            let s = self.p.uninitialize(&unit);
            if s != 0 {
                errors.push(status("AudioUnitUninitialize", s));
            }
        }
        let s = self.p.dispose(unit);
        if s != 0 {
            errors.push(status("AudioComponentInstanceDispose", s));
        }
        errors
    }
}
impl<P: Platform> Drop for Owned<P> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
/// Field order matters: the unit is closed (no more callbacks) before the callback context is freed.
struct Session<P: Platform> {
    unit: Owned<P>,
    ctx: Box<InputCtx>,
    target: EchoTarget,
    /// The system before this unit existed (`restore`).
    before: Snapshot,
    began: Instant,
    active_at: Option<Instant>,
    last_callbacks: u64,
    last_progress: Instant,
    last_full_gate: Instant,
}
impl<P: Platform> Session<P> {
    fn close(&mut self) -> Vec<String> {
        let ring = &self.ctx.ring;
        let fade = ring.live.swap(false, Ordering::AcqRel) && self.unit.started;
        if fade {
            thread::sleep(FADE_OUT_WAIT);
        }
        self.unit.close()
    }
    fn counters(&self) -> String {
        let r = &self.ctx.ring;
        format!(
            "underruns {}, skips {}, dropped {}, render errors {}",
            r.underruns.load(Ordering::Relaxed),
            r.skips.load(Ordering::Relaxed),
            r.dropped.load(Ordering::Relaxed),
            self.ctx.render_errors.load(Ordering::Relaxed)
        )
    }
}

/// A start that ended without a running unit, and what its teardown found (close errors, `restore` notes); empty when
/// no unit was ever created.
struct Failure {
    end: End,
    errors: Vec<String>,
    notes: Notes,
}
fn cancelled(shared: &Shared) -> bool {
    !shared.wanted.load(Ordering::Acquire) || shared.closed.load(Ordering::Acquire)
}
fn start<P: Platform>(p: &P, shared: &Shared) -> Result<Session<P>, Failure> {
    let none = |end| Failure { end, errors: vec![], notes: Notes::default() };
    let target = shared
        .target
        .lock()
        .ok()
        .and_then(|t| t.clone())
        .ok_or_else(|| none(End::Refused("no primary microphone is recording".into())))?;
    if let Some(reason) = refusal(&routes(p, &target)) {
        return Err(none(End::Refused(reason)));
    }
    let before = snapshot(p, &target);
    let unit = p.create().map_err(|f| none(End::Failed(f)))?;
    let owned = Owned { p: p.clone(), unit: Some(unit), initialized: false, started: false };
    match open(p, shared, owned, target, before.clone()) {
        Ok(session) => Ok(session),
        Err((end, errors)) => Err(Failure { end, errors, notes: settle_and_restore(p, &before) }),
    }
}
/// From a created unit to a running session; any failure closes the unit (in reverse) and returns its errors.
fn open<P: Platform>(p: &P, shared: &Shared, owned: Owned<P>, target: EchoTarget, before: Snapshot) -> Result<Session<P>, (End, Vec<String>)> {
    let ctx = Box::new(InputCtx {
        unit: p.render_handle(owned.unit.as_ref().unwrap()),
        ring: target.ring.clone(),
        scratch: UnsafeCell::new(vec![0.; 16_384].into_boxed_slice()),
        entered: AtomicBool::new(false),
        callbacks: AtomicU64::new(0),
        render_errors: AtomicU64::new(0),
        last_error: AtomicI32::new(0),
    });
    // From here the Session owns both, so every early return closes the unit before freeing the context.
    let mut s = Session { unit: owned, ctx, target, before, began: Instant::now(), active_at: None, last_callbacks: 0, last_progress: Instant::now(), last_full_gate: Instant::now() };
    macro_rules! bail {
        ($end:expr) => {{
            let end = $end;
            return Err((end, s.close()));
        }};
    }
    // Creating the unit already configured the defaults: the routes again before anything is set.
    if let Some(reason) = refusal(&routes(p, &s.target)).or_else(|| bluetooth_in_unit(p, s.unit.unit.as_ref().unwrap())) {
        bail!(End::Refused(format!("{reason}; torn down after creation")));
    }
    if cancelled(shared) {
        bail!(End::Cancelled);
    }
    let ctx_ptr = (&*s.ctx as *const InputCtx).cast_mut().cast();
    if let Err(end) = p.configure(s.unit.unit.as_ref().unwrap(), &s.target, ctx_ptr) {
        bail!(end);
    }
    if cancelled(shared) {
        bail!(End::Cancelled);
    }
    let st = p.initialize(s.unit.unit.as_ref().unwrap());
    if st != 0 {
        bail!(End::Failed(status("AudioUnitInitialize", st)));
    }
    s.unit.initialized = true;
    if cancelled(shared) {
        bail!(End::Cancelled);
    }
    if let Some(reason) = bluetooth_in_unit(p, s.unit.unit.as_ref().unwrap()).or_else(|| refusal(&routes(p, &s.target))) {
        bail!(End::Refused(format!("{reason}; torn down before start")));
    }
    let st = p.start(s.unit.unit.as_ref().unwrap());
    if st != 0 {
        bail!(End::Failed(status("AudioOutputUnitStart", st)));
    }
    s.unit.started = true;
    s.ctx.ring.live.store(true, Ordering::Release);
    s.last_progress = Instant::now();
    if let Some(reason) = bluetooth_in_unit(p, s.unit.unit.as_ref().unwrap()).or_else(|| refusal(&routes(p, &s.target))) {
        bail!(End::Refused(format!("{reason}; torn down after start")));
    }
    if cancelled(shared) {
        bail!(End::Cancelled);
    }
    Ok(s)
}

/// Why a running session should end now, if it should, and whether one automatic retry may follow.
fn check<P: Platform>(p: &P, shared: &Shared, s: &mut Session<P>) -> Option<(State, String, bool)> {
    if !shared.wanted.load(Ordering::Acquire) || shared.closed.load(Ordering::Acquire) {
        return Some((State::Off, "released".into(), false));
    }
    let same = shared
        .target
        .lock()
        .ok()
        .is_some_and(|t| t.as_ref().is_some_and(|t| t.same(&s.target)));
    if !same {
        let f = "the primary microphone source changed under the unit".to_string();
        return Some((State::Failed(f.clone()), f, true));
    }
    if s.began.elapsed() >= MAX_SESSION {
        let f = format!("hard limit of {} s reached; the app did not release it", MAX_SESSION.as_secs());
        return Some((State::Failed(f.clone()), f, false));
    }
    let full = s.last_full_gate.elapsed() >= FULL_GATE;
    let reason = if full {
        s.last_full_gate = Instant::now();
        refusal(&routes(p, &s.target)).or_else(|| s.unit.unit.as_ref().and_then(|u| bluetooth_in_unit(p, u)))
    } else {
        refusal(&default_routes(p, &s.target))
    };
    if let Some(reason) = reason {
        let r = format!("route changed: {reason}");
        return Some((State::Refused(r.clone()), r, false));
    }
    let callbacks = s.ctx.callbacks.load(Ordering::Acquire);
    if callbacks != s.last_callbacks {
        s.last_callbacks = callbacks;
        s.last_progress = Instant::now();
    } else if s.last_progress.elapsed() >= if callbacks == 0 { FIRST_INPUT } else { STALL } {
        let limit = if callbacks == 0 { FIRST_INPUT } else { STALL };
        let last = s.ctx.last_error.load(Ordering::Relaxed);
        let f = if last != 0 {
            format!("no processed input for {} s ({})", limit.as_secs(), status("AudioUnitRender", last))
        } else {
            format!("no processed input for {} s", limit.as_secs())
        };
        return Some((State::Failed(f.clone()), f, true));
    }
    None
}

/// The session worker. Parked while nothing is asked; ticks only while a unit exists.
pub(crate) fn spawn(
    shared: Arc<Shared>,
    tx: crossbeam_channel::Sender<Command>,
    intake: thread::Thread,
) -> std::io::Result<thread::JoinHandle<()>> {
    spawn_with(CoreAudio, shared, tx, intake)
}
fn spawn_with<P: Platform + Send + 'static>(
    p: P,
    shared: Arc<Shared>,
    tx: crossbeam_channel::Sender<Command>,
    intake: thread::Thread,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new().name("heimdall-echo".into()).spawn(move || {
        if let Ok(mut w) = shared.worker.lock() {
            *w = Some(thread::current());
        }
        let diag = |detail: String| {
            let _ = tx.send(Command::Diagnostic(format!("echo cancellation: {detail}")));
            intake.unpark();
        };
        let mut handled = 0u64;
        // One automatic retry per request after a failure (a slow cold start, a starved unit, a mic restart).
        let mut retried = 0u64;
        let mut retry = false;
        let mut failures = 0u32;
        // Echo cancellation is off until Heimdall restarts, and why (23 Oct 17:38).
        let mut disabled: Option<String> = None;
        let mut session: Option<Session<P>> = None;
        // A teardown's notes are logged; anything it had to put back switches echo cancellation off for this run.
        let after_teardown = |notes: Notes, disabled: &mut Option<String>| {
            let all = notes.all();
            if !all.is_empty() {
                diag(format!("after the unit was gone: {}", all.join("; ")));
            }
            if !notes.put_back.is_empty() {
                disabled.get_or_insert_with(|| "the unit left the system's audio changed".into());
            }
        };
        loop {
            let closed = shared.closed.load(Ordering::Acquire);
            let wanted = shared.wanted.load(Ordering::Acquire);
            let request = shared.requests.load(Ordering::Acquire);
            if let Some(s) = session.as_mut() {
                handled = request; // a quick off → on while running is the same session
                if s.active_at.is_none() && s.ctx.ring.substituting.load(Ordering::Acquire) {
                    s.active_at = Some(Instant::now());
                    diag(format!("active ({} ms after start)", s.began.elapsed().as_millis()));
                }
                if let Some((state, why, retryable)) = check(&p, &shared, s) {
                    let mut s = session.take().unwrap();
                    let errors = s.close();
                    let ran = s.began.elapsed();
                    let mut line = format!("stopped after {:.1} s ({why}); {}", ran.as_secs_f64(), s.counters());
                    for e in errors {
                        line.push_str("; ");
                        line.push_str(&e);
                    }
                    diag(line);
                    let before = s.before.clone();
                    drop(s);
                    after_teardown(settle_and_restore(&p, &before), &mut disabled);
                    // A unit that started but stalled (the starved −10875 mode) or ran out its limit counts like a
                    // failed start; a mic that changed under it does not.
                    if matches!(state, State::Failed(_)) && !why.starts_with("the primary microphone source changed") {
                        failures += 1;
                        if failures >= MAX_FAILURES {
                            disabled.get_or_insert_with(|| format!("{failures} failures"));
                        }
                    }
                    match &disabled {
                        Some(why) => shared.set_state(State::Failed(format!("off until Heimdall restarts: {why}"))),
                        None => shared.set_state(state),
                    }
                    if disabled.is_none() && retryable && retried != handled && shared.wanted.load(Ordering::Acquire) {
                        retried = handled;
                        retry = true;
                    }
                    continue;
                }
                let tick = if s.active_at.is_some() { 250 } else { 20 };
                thread::park_timeout(Duration::from_millis(tick));
                continue;
            }
            if closed {
                shared.set_state(State::Off);
                break;
            }
            if wanted && (request != handled || retry) {
                if retry && request == handled {
                    diag("retrying once".into());
                }
                retry = false;
                handled = request;
                if let Some(why) = &disabled {
                    // No unit, no device read: what failed before stays failed until a restart.
                    shared.set_state(State::Failed(format!("off until Heimdall restarts: {why}")));
                    continue;
                }
                shared.set_state(State::Starting);
                let t0 = Instant::now();
                match start(&p, &shared) {
                    Ok(mut s) => {
                        let ms = t0.elapsed().as_millis();
                        diag(format!("started on {} in {ms} ms (Min ducking, input only)", s.target.name));
                        s.began = Instant::now();
                        shared.set_state(State::Running(s.ctx.ring.clone()));
                        session = Some(s);
                    }
                    Err(Failure { end, errors, notes }) => {
                        let failed = matches!(end, End::Failed(_));
                        if !errors.is_empty() {
                            diag(format!("closing the unit: {}", errors.join("; ")));
                        }
                        after_teardown(notes, &mut disabled);
                        if failed {
                            failures += 1;
                            if failures >= MAX_FAILURES {
                                disabled.get_or_insert_with(|| format!("{failures} failures"));
                            }
                        }
                        // The state first: whoever reads the line sees it (check of 23 Oct 03:12: Starting).
                        match end {
                            End::Refused(r) => {
                                shared.set_state(State::Refused(r.clone()));
                                diag(format!("refused: {r}"));
                            }
                            End::Failed(f) => {
                                shared.set_state(State::Failed(f.clone()));
                                diag(format!("failed: {f}"));
                                if disabled.is_none() && retried != handled {
                                    retried = handled;
                                    retry = true;
                                }
                            }
                            End::Cancelled => {
                                diag(format!("released while starting ({} ms); no unit kept", t0.elapsed().as_millis()));
                                shared.set_state(State::Off);
                            }
                        }
                        if let Some(why) = &disabled {
                            diag(format!("off until Heimdall restarts ({why})"));
                            shared.set_state(State::Failed(format!("off until Heimdall restarts: {why}")));
                        }
                    }
                }
                continue;
            }
            if !wanted && disabled.is_none() {
                shared.set_state(State::Off);
            }
            thread::park();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    fn route(name: &str, uid: &str, transport: &[u8; 4], source: Option<&[u8; 4]>, has_input: bool) -> Route {
        Route {
            uid: uid.into(),
            name: name.into(),
            transport: fourcc(transport),
            data_source: source.map(fourcc),
            has_input,
        }
    }
    fn speakers() -> Route {
        route("MacBook Pro Speakers", "BuiltInSpeakerDevice", b"bltn", Some(b"ispk"), false)
    }
    fn shure() -> Route {
        route("Shure MV7i", "AppleUSBAudioEngine:Shure Inc:Shure MV7i:2,3", b"usb ", None, true)
    }
    fn airpods() -> Route {
        route("AirPods Pro", "AC-3E-B1:output", b"blue", None, true)
    }
    fn built_in_mic() -> Route {
        route("MacBook Pro Microphone", "BuiltInMicrophoneDevice", b"bltn", None, true)
    }
    fn gates(output: Option<Route>, input: Option<Route>, mic: Route, built_in: bool) -> Routes {
        Routes { system_output: output.clone(), output, input, mic, mic_built_in: built_in, bluetooth_headsets: vec![] }
    }

    #[test]
    fn only_speakers_with_a_wired_mic_get_the_unit() {
        assert_eq!(refusal(&gates(Some(speakers()), Some(shure()), shure(), false)), None);
        assert_eq!(refusal(&gates(Some(speakers()), Some(built_in_mic()), shure(), false)), None);
        let hdmi = route("LG UltraFine", "hdmi-1", b"hdmi", None, false);
        assert_eq!(refusal(&gates(Some(hdmi), None, shure(), false)), None);
        let refused = |r: Routes, needle: &str| {
            let reason = refusal(&r).expect("refused");
            assert!(reason.contains(needle), "{reason}");
        };
        // AirPods (Bluetooth or LE), the headphone jack, a USB headset, AirPlay or a virtual device as output.
        refused(gates(Some(airpods()), None, shure(), false), "Bluetooth");
        refused(gates(Some(route("Beats", "b", b"blea", None, false)), None, shure(), false), "Bluetooth");
        let jack = route("External Headphones", "BuiltInHeadphoneOutputDevice", b"bltn", Some(b"hdpn"), false);
        refused(gates(Some(jack), None, shure(), false), "headphones");
        let jack_by_source = route("MacBook Pro Speakers", "BuiltInSpeakerDevice", b"bltn", Some(b"hdpn"), false);
        refused(gates(Some(jack_by_source), None, shure(), false), "headphones");
        refused(gates(Some(shure()), Some(shure()), shure(), false), "USB headset");
        refused(gates(Some(route("Living Room", "airplay", b"airp", None, false)), None, shure(), false), "not a speaker");
        refused(gates(Some(route("BlackHole", "bh", b"virt", None, true)), None, shure(), false), "not a speaker");
        refused(gates(None, None, shure(), false), "no default output");
        // AirPods as default input (hands-free) or as the primary mic.
        refused(gates(Some(speakers()), Some(airpods()), shure(), false), "hands-free");
        refused(gates(Some(speakers()), None, airpods(), false), "primary microphone AirPods Pro is Bluetooth");
        // The built-in mic stays raw until the live format check switches CLEAN_BUILT_IN_MIC on.
        if !CLEAN_BUILT_IN_MIC {
            refused(gates(Some(speakers()), Some(built_in_mic()), built_in_mic(), true), "built-in");
        }
    }
    /// Bug note 23 Oct 17:38: never with Bluetooth anywhere near the unit. Alert sounds on AirPods, or AirPods merely
    /// connected (macOS may make them the default input while the unit starts): refused.
    #[test]
    fn no_unit_while_airpods_are_connected_or_carry_the_alerts() {
        let mut r = gates(Some(speakers()), Some(shure()), shure(), false);
        r.system_output = Some(airpods());
        assert!(refusal(&r).is_some_and(|r| r.contains("alert sounds go to AirPods Pro")));
        let mut r = gates(Some(speakers()), Some(shure()), shure(), false);
        r.bluetooth_headsets = vec![airpods()];
        assert!(refusal(&r).is_some_and(|r| r.contains("AirPods Pro is connected over Bluetooth")), "{:?}", refusal(&r));
    }

    // ---- A fake system: devices, defaults and rates, and a unit that can fail or misbehave at each step ----

    const SPEAKERS: u32 = 10;
    const SHURE: u32 = 20;
    const AIRPODS: u32 = 30;
    const AGGREGATE: u32 = 99;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        Create,
        Configure,
        Initialize,
        Start,
    }
    #[derive(Default)]
    struct System {
        devices: HashMap<u32, Route>,
        defaults: HashMap<&'static str, u32>,
        rates: HashMap<u32, f64>,
        /// Unit calls in order.
        calls: Vec<String>,
        /// The step that fails, and with what OSStatus.
        fail: Option<(Step, i32)>,
        /// What the unit does to the system as it initializes (where it builds its aggregate): the default output
        /// moved to its aggregate and the speakers' rate changed (the kind of change the snapshot must undo).
        misbehave: bool,
        /// AirPods connect while the unit initializes.
        airpods_at_initialize: bool,
        units: u32,
        disposed: u32,
    }
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<System>>);
    fn key(role: Role) -> &'static str {
        role.label()
    }
    impl Fake {
        fn new() -> Self {
            let fake = Fake::default();
            {
                let mut s = fake.0.lock().unwrap();
                s.devices.insert(SPEAKERS, speakers());
                s.devices.insert(SHURE, shure());
                for role in Role::ALL {
                    s.defaults.insert(key(role), if role == Role::Input { SHURE } else { SPEAKERS });
                }
                s.rates.insert(SPEAKERS, 48_000.);
                s.rates.insert(SHURE, 48_000.);
            }
            fake
        }
        fn state(&self) -> (HashMap<&'static str, u32>, HashMap<u32, f64>, Vec<u32>) {
            let s = self.0.lock().unwrap();
            let mut devices: Vec<u32> = s.devices.keys().copied().collect();
            devices.sort_unstable();
            (s.defaults.clone(), s.rates.clone(), devices)
        }
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().calls.clone()
        }
        fn step(&self, step: Step) -> i32 {
            let mut s = self.0.lock().unwrap();
            s.calls.push(format!("{step:?}"));
            match s.fail {
                Some((at, code)) if at == step => code,
                _ => 0,
            }
        }
    }
    fn target(device: u32) -> EchoTarget {
        EchoTarget { device, name: "Shure MV7i".into(), built_in: false, rate: 48_000, ring: EchoRing::for_rate(48_000) }
    }
    impl Platform for Fake {
        type Unit = u32;
        fn default_device(&self, role: Role) -> u32 {
            self.0.lock().unwrap().defaults[key(role)]
        }
        fn route(&self, device: u32) -> Option<Route> {
            self.0.lock().unwrap().devices.get(&device).cloned()
        }
        fn devices(&self) -> Vec<u32> {
            let mut d: Vec<u32> = self.0.lock().unwrap().devices.keys().copied().collect();
            d.sort_unstable();
            d
        }
        fn voice_processing(&self, device: u32) -> bool {
            device == AGGREGATE
        }
        fn members(&self, aggregate: u32) -> Vec<u32> {
            if aggregate == AGGREGATE { vec![SHURE, SPEAKERS] } else { vec![] }
        }
        fn nominal_rate(&self, device: u32) -> Option<f64> {
            self.0.lock().unwrap().rates.get(&device).copied()
        }
        fn set_nominal_rate(&self, device: u32, rate: f64) -> i32 {
            let mut s = self.0.lock().unwrap();
            s.calls.push(format!("SetRate {device} {rate}"));
            s.rates.insert(device, rate);
            0
        }
        fn set_default_device(&self, role: Role, device: u32) -> i32 {
            let mut s = self.0.lock().unwrap();
            s.calls.push(format!("SetDefault {} {device}", role.label()));
            s.defaults.insert(key(role), device);
            0
        }
        fn create(&self) -> Result<u32, String> {
            let code = self.step(Step::Create);
            if code != 0 {
                return Err(status("AudioComponentInstanceNew", code));
            }
            let mut s = self.0.lock().unwrap();
            s.units += 1;
            Ok(s.units)
        }
        fn render_handle(&self, _: &u32) -> AudioUnit {
            std::ptr::null_mut()
        }
        fn configure(&self, _: &u32, _: &EchoTarget, _: *mut c_void) -> Result<(), End> {
            match self.step(Step::Configure) {
                0 => Ok(()),
                code => Err(End::Failed(status("EnableIO input", code))),
            }
        }
        fn unit_devices(&self, _: &u32) -> Vec<u32> {
            vec![SHURE]
        }
        fn initialize(&self, _: &u32) -> i32 {
            let code = self.step(Step::Initialize);
            let mut s = self.0.lock().unwrap();
            if s.airpods_at_initialize {
                s.devices.insert(AIRPODS, airpods());
            }
            if s.misbehave {
                s.devices.insert(AGGREGATE, route("VPAUAggregateAudioDevice-0x1", "VPAUAggregateAudioDevice-0x1", b"grup", None, true));
                s.defaults.insert(key(Role::Output), AGGREGATE);
                s.rates.insert(SPEAKERS, 16_000.);
            }
            code
        }
        fn start(&self, _: &u32) -> i32 {
            self.step(Step::Start)
        }
        fn stop(&self, _: &u32) -> i32 {
            self.0.lock().unwrap().calls.push("Stop".into());
            0
        }
        fn uninitialize(&self, _: &u32) -> i32 {
            self.0.lock().unwrap().calls.push("Uninitialize".into());
            0
        }
        fn dispose(&self, _: u32) -> i32 {
            let mut s = self.0.lock().unwrap();
            s.calls.push("Dispose".into());
            s.disposed += 1;
            // The unit's aggregate goes with it; what it changed on real devices stays (the misbehaviour).
            s.devices.remove(&AGGREGATE);
            0
        }
        fn settle(&self, _: Duration) {}
    }
    fn shared_with(device: u32) -> Arc<Shared> {
        let shared = Shared::new();
        *shared.target.lock().unwrap() = Some(target(device));
        shared.wanted.store(true, Ordering::Release);
        shared
    }

    /// The 23 Oct failure, −10875 at Initialize: the unit is disposed (never stopped or uninitialized, it got neither
    /// far), and what it changed on the way (the default output on its aggregate, the speakers at 16 kHz) is put
    /// back. The system ends exactly as it was.
    #[test]
    fn a_failed_initialize_disposes_the_unit_and_puts_the_system_back() {
        let fake = Fake::new();
        let before = fake.state();
        {
            let mut s = fake.0.lock().unwrap();
            s.fail = Some((Step::Initialize, -10875));
            s.misbehave = true;
        }
        let shared = shared_with(SHURE);
        let Err(f) = start(&fake, &shared) else { panic!("started") };
        assert_eq!(f.end, End::Failed(status("AudioUnitInitialize", -10875)));
        assert_eq!(fake.calls(), ["Create", "Configure", "Initialize", "Dispose", "SetDefault output 10", "SetRate 10 48000"]);
        assert_eq!(fake.state(), before, "the system was left changed");
        // The aggregate went with the unit, so the default pointed at nothing real.
        assert!(f.notes.put_back.iter().any(|n| n == "default output had moved to a device that is gone (99); put back to MacBook Pro Speakers"), "{:?}", f.notes);
        assert!(f.notes.put_back.iter().any(|n| n.contains("sample rate had changed to 16000 Hz; put back to 48000 Hz")), "{:?}", f.notes);
        assert_eq!(fake.0.lock().unwrap().disposed, 1);
    }
    /// Each step's failure closes exactly what was reached, in reverse; a unit that changed nothing leaves no notes.
    #[test]
    fn every_failure_closes_in_reverse_only_what_was_reached() {
        for (step, calls) in [
            (Step::Create, &["Create"][..]),
            (Step::Configure, &["Create", "Configure", "Dispose"][..]),
            (Step::Initialize, &["Create", "Configure", "Initialize", "Dispose"][..]),
            (Step::Start, &["Create", "Configure", "Initialize", "Start", "Uninitialize", "Dispose"][..]),
        ] {
            let fake = Fake::new();
            let before = fake.state();
            fake.0.lock().unwrap().fail = Some((step, -10875));
            let Err(f) = start(&fake, &shared_with(SHURE)) else { panic!("started at {step:?}") };
            assert!(matches!(f.end, End::Failed(_)), "{step:?}: {:?}", f.end);
            assert_eq!(fake.calls(), calls, "{step:?}");
            assert_eq!(f.notes, Notes::default(), "{step:?}");
            assert_eq!(fake.state(), before);
        }
        // Started, then released: Stop, Uninitialize, Dispose.
        let fake = Fake::new();
        let mut s = start(&fake, &shared_with(SHURE)).ok().expect("runs");
        assert!(s.ctx.ring.live.load(Ordering::Acquire));
        assert!(s.close().is_empty());
        assert!(!s.ctx.ring.live.load(Ordering::Acquire));
        assert_eq!(fake.calls(), ["Create", "Configure", "Initialize", "Start", "Stop", "Uninitialize", "Dispose"]);
        assert_eq!(settle_and_restore(&fake, &s.before), Notes::default());
    }
    /// AirPods connecting while the unit initializes: torn down before it starts, and nothing it touched stays.
    /// AirPods already connected: no unit is ever created.
    #[test]
    fn airpods_turning_up_mid_start_tear_the_unit_down_and_connected_airpods_get_none() {
        let fake = Fake::new();
        fake.0.lock().unwrap().airpods_at_initialize = true;
        let Err(f) = start(&fake, &shared_with(SHURE)) else { panic!("started") };
        assert!(matches!(&f.end, End::Refused(r) if r.contains("AirPods Pro is connected") && r.contains("before start")), "{:?}", f.end);
        assert_eq!(fake.calls(), ["Create", "Configure", "Initialize", "Uninitialize", "Dispose"]);
        let calls = fake.calls().len();
        let Err(f) = start(&fake, &shared_with(SHURE)) else { panic!("started") };
        assert!(matches!(&f.end, End::Refused(r) if r.contains("AirPods Pro")), "{:?}", f.end);
        assert_eq!(fake.calls().len(), calls, "a unit was created with AirPods connected");
        // AirPods as the default output: no unit either.
        let fake = Fake::new();
        {
            let mut s = fake.0.lock().unwrap();
            s.devices.insert(AIRPODS, airpods());
            s.defaults.insert(key(Role::Output), AIRPODS);
        }
        assert!(start(&fake, &shared_with(SHURE)).is_err());
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // A Bluetooth speaker that cannot record, connected but not in use, is no headset.
        let fake = Fake::new();
        fake.0.lock().unwrap().devices.insert(AIRPODS, route("JBL Flip", "jbl", b"blue", None, false));
        assert!(start(&fake, &shared_with(SHURE)).is_ok());
    }
    /// Released while starting: the unit goes and the system is checked like any other teardown.
    #[test]
    fn released_while_starting_keeps_no_unit() {
        let fake = Fake::new();
        let shared = shared_with(SHURE);
        shared.wanted.store(false, Ordering::Release);
        let Err(f) = start(&fake, &shared) else { panic!("started") };
        assert_eq!(f.end, End::Cancelled);
        assert_eq!(fake.calls(), ["Create", "Dispose"]);
    }
    /// A default device moved by macOS or the user to a real device while the unit ran (AirPods connecting) is left
    /// where it is, and said; a device that vanished is not touched.
    #[test]
    fn restore_moves_back_only_what_the_unit_made() {
        let fake = Fake::new();
        let before = snapshot(&fake, &target(SHURE));
        {
            let mut s = fake.0.lock().unwrap();
            s.devices.insert(AIRPODS, airpods());
            s.defaults.insert(key(Role::Output), AIRPODS);
            s.devices.remove(&SHURE);
        }
        let notes = restore(&fake, &before);
        assert_eq!(notes.left, ["default output changed from MacBook Pro Speakers to AirPods Pro; left as it is"]);
        assert!(notes.put_back.is_empty(), "left-alone changes never switch echo cancellation off");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert_eq!(fake.default_device(Role::Output), AIRPODS);
    }

    fn worker(fake: &Fake) -> (Arc<Shared>, crate::EchoControl, crossbeam_channel::Receiver<Command>, thread::JoinHandle<()>) {
        let shared = Shared::new();
        *shared.target.lock().unwrap() = Some(target(SHURE));
        let control = crate::EchoControl(shared.clone());
        let (tx, rx) = crossbeam_channel::unbounded();
        let join = spawn_with(fake.clone(), shared.clone(), tx, thread::current()).unwrap();
        while shared.worker.lock().unwrap().is_none() {
            thread::yield_now();
        }
        (shared, control, rx, join)
    }
    fn settled(control: &crate::EchoControl, done: impl Fn(&crate::EchoCancel) -> bool) -> crate::EchoCancel {
        let t0 = Instant::now();
        loop {
            let state = control.echo_cancellation();
            if done(&state) {
                return state;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "stuck at {state:?}");
            thread::sleep(Duration::from_millis(2));
        }
    }
    /// 23 Oct 17:35–17:36: four units in under a minute, each failing −10875. Now a failed start is retried once; the
    /// second failure switches echo cancellation off for the run, and later replies create no unit at all.
    #[test]
    fn two_failed_starts_switch_echo_cancellation_off_until_a_restart() {
        let fake = Fake::new();
        fake.0.lock().unwrap().fail = Some((Step::Initialize, -10875));
        let (shared, control, rx, join) = worker(&fake);
        control.set_echo_cancellation(true);
        let state = settled(&control, |s| matches!(s, crate::EchoCancel::Failed(f) if f.starts_with("off until")));
        assert!(matches!(&state, crate::EchoCancel::Failed(f) if f.contains("2 failures")), "{state:?}");
        assert_eq!(fake.0.lock().unwrap().units, 2, "the first request and its one retry");
        for _ in 0..3 {
            control.set_echo_cancellation(false);
            control.set_echo_cancellation(true);
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.0.lock().unwrap().units, 2, "a unit was created after the switch-off");
        assert!(matches!(control.echo_cancellation(), crate::EchoCancel::Failed(f) if f.starts_with("off until")));
        let lines: Vec<String> = rx.try_iter().filter_map(|c| match c { Command::Diagnostic(d) => Some(d), _ => None }).collect();
        assert!(lines.iter().any(|l| l.contains("off until Heimdall restarts")), "{lines:?}");
        shared.closed.store(true, Ordering::Release);
        shared.wake();
        join.join().unwrap();
    }
    /// A teardown that found the system changed switches it off at once, with no retry.
    #[test]
    fn a_unit_that_changed_the_system_is_never_tried_again() {
        let fake = Fake::new();
        // It moves the default output onto its aggregate as it initializes: refused before Start, torn down.
        fake.0.lock().unwrap().misbehave = true;
        let before = fake.state();
        let (shared, control, rx, join) = worker(&fake);
        control.set_echo_cancellation(true);
        let state = settled(&control, |s| matches!(s, crate::EchoCancel::Failed(f) if f.starts_with("off until")));
        assert!(matches!(&state, crate::EchoCancel::Failed(f) if f.contains("left the system's audio changed")), "{state:?}");
        assert_eq!(fake.0.lock().unwrap().units, 1, "retried after changing the system");
        // Its changes were put back before the line was written.
        let (defaults, rates, _) = fake.state();
        assert_eq!((defaults, rates), (before.0, before.1));
        let lines: Vec<String> = rx.try_iter().filter_map(|c| match c { Command::Diagnostic(d) => Some(d), _ => None }).collect();
        assert!(lines.iter().any(|l| l.starts_with("echo cancellation: after the unit was gone: default output had moved")), "{lines:?}");
        shared.closed.store(true, Ordering::Release);
        shared.wake();
        join.join().unwrap();
    }

    /// Review 24 Oct: a unit that starts but never delivers audio (the starved −10875 mode) counts as a failure too:
    /// it stalls, is retried once, stalls again, and echo cancellation goes off for the run.
    #[test]
    fn a_unit_that_starts_but_stalls_twice_switches_echo_cancellation_off() {
        let fake = Fake::new();
        let (shared, control, rx, join) = worker(&fake);
        control.set_echo_cancellation(true);
        let state = settled(&control, |s| matches!(s, crate::EchoCancel::Failed(f) if f.starts_with("off until")));
        assert!(matches!(&state, crate::EchoCancel::Failed(f) if f.contains("2 failures")), "{state:?}");
        assert_eq!(fake.0.lock().unwrap().units, 2);
        assert_eq!(fake.0.lock().unwrap().disposed, 2);
        let lines: Vec<String> = rx.try_iter().filter_map(|c| match c { Command::Diagnostic(d) => Some(d), _ => None }).collect();
        assert_eq!(lines.iter().filter(|l| l.contains("no processed input")).count(), 2, "{lines:?}");
        control.set_echo_cancellation(false);
        control.set_echo_cancellation(true);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.0.lock().unwrap().units, 2, "a unit was created after the switch-off");
        shared.closed.store(true, Ordering::Release);
        shared.wake();
        join.join().unwrap();
    }
    /// The worker without a primary mic source: refuses before any HAL read or unit (no device is touched), once per
    /// request, reads Off on release, and exits on close.
    #[test]
    fn worker_refuses_without_a_mic_and_exits_on_close() {
        let shared = Shared::new();
        let control = crate::EchoControl(shared.clone());
        let (tx, rx) = crossbeam_channel::unbounded();
        let join = spawn(shared.clone(), tx, thread::current()).unwrap();
        while shared.worker.lock().unwrap().is_none() {
            thread::yield_now();
        }
        let line = |rx: &crossbeam_channel::Receiver<Command>| match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Command::Diagnostic(d)) => d,
            _ => panic!("no diagnostic"),
        };
        control.set_echo_cancellation(true);
        assert_eq!(line(&rx), "echo cancellation: refused: no primary microphone is recording");
        assert_eq!(control.echo_cancellation(), crate::EchoCancel::Refused("no primary microphone is recording".into()));
        control.set_echo_cancellation(true); // repeated wish: no second attempt
        control.set_echo_cancellation(false);
        let t0 = Instant::now();
        while control.echo_cancellation() != crate::EchoCancel::Off {
            assert!(t0.elapsed() < Duration::from_secs(2));
            thread::yield_now();
        }
        assert!(rx.try_recv().is_err(), "one line per request");
        shared.closed.store(true, Ordering::Release);
        shared.wake();
        join.join().unwrap();
        control.set_echo_cancellation(true);
        assert_eq!(control.echo_cancellation(), crate::EchoCancel::Off);
    }

    #[test]
    fn ffi_layouts_match_the_sdk_headers() {
        assert_eq!(std::mem::size_of::<AudioComponentDescription>(), 20);
        assert_eq!(std::mem::size_of::<RenderCallbackStruct>(), 16);
        assert_eq!(std::mem::size_of::<DuckingConfiguration>(), 8);
        assert_eq!(std::mem::offset_of!(DuckingConfiguration, level), 4);
        assert_eq!(TYPE_OUTPUT, 0x6175_6f75);
        assert_eq!(SUBTYPE_VPIO, 0x7670_696f);
        assert_eq!(MANUFACTURER_APPLE, 0x6170_706c);
        assert_eq!(fourcc(b"blue"), kAudioDeviceTransportTypeBluetooth);
        assert_eq!(fourcc(b"bltn"), kAudioDeviceTransportTypeBuiltIn);
        assert_eq!(fourcc(b"usb "), kAudioDeviceTransportTypeUSB);
        assert_eq!(kAudioHardwarePropertyDefaultSystemOutputDevice, fourcc(b"sOut"));
        assert_eq!(kAudioDevicePropertyNominalSampleRate, fourcc(b"nsrt"));
    }
}
