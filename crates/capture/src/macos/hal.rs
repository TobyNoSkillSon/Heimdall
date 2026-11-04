// Property/string/list reads adapted from flexaudio common.rs/processes.rs.
// Copyright (c) 2026 tubome / Studio Sadola, MIT; see vendor/flexaudio/LICENSE.
use crate::{owning_app_path, AppOwner, NativeFormat, SourceKind};
use anyhow::{Context, Result};
use block2::RcBlock;
use objc2_core_audio::*;
use objc2_core_audio_types::*;
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::{NSBundle, NSString};
use std::{
    ptr::NonNull,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    thread::Thread,
};
pub const DISCOVERY: u32 = 1;
pub const ROUTE: u32 = 2;
pub const SLEEP: u32 = 4;
pub const WAKE: u32 = 8;
pub const MICROPHONE: u32 = 16;
pub const APPS: u32 = 32;
pub fn address(sel: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: sel,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}
pub fn check(call: &str, status: i32) -> Result<()> {
    anyhow::ensure!(
        status == 0,
        "{call}: Core Audio OSStatus {status} (0x{:08x})",
        status as u32
    );
    Ok(())
}
pub fn property<T: Copy>(object: u32, sel: u32, scope: u32) -> Result<T> {
    let a = address(sel, scope);
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    unsafe {
        check(
            "get property",
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&a),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(value.as_mut_ptr()).unwrap().cast(),
            ),
        )?;
    }
    anyhow::ensure!(
        size as usize == std::mem::size_of::<T>(),
        "property size changed"
    );
    Ok(unsafe { value.assume_init() })
}
pub fn list(object: u32, sel: u32, scope: u32) -> Result<Vec<u32>> {
    let a = address(sel, scope);
    for _ in 0..4 {
        let mut size = 0;
        unsafe {
            check(
                "property size",
                AudioObjectGetPropertyDataSize(
                    object,
                    NonNull::from(&a),
                    0,
                    std::ptr::null(),
                    NonNull::from(&mut size),
                ),
            )?;
        }
        if size == 0 {
            return Ok(vec![]);
        }
        anyhow::ensure!(
            size % 4 == 0 && size < 1024 * 1024,
            "invalid object list size"
        );
        let mut ids = vec![0; size as usize / 4];
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&a),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(ids.as_mut_ptr()).unwrap().cast(),
            )
        };
        if status == kAudioHardwareBadPropertySizeError {
            continue;
        }
        check("object list", status)?;
        ids.truncate(size as usize / 4);
        return Ok(ids);
    }
    anyhow::bail!("HAL list changed during all four snapshot attempts")
}
pub fn string(object: u32, sel: u32) -> Option<String> {
    let p = property::<*mut CFString>(object, sel, kAudioObjectPropertyScopeGlobal).ok()?;
    let cf = unsafe { CFRetained::from_raw(NonNull::new(p)?) };
    Some(cf.to_string())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PcmEncoding {
    Float {
        bytes: usize,
        big_endian: bool,
    },
    Signed {
        bytes: usize,
        bits: u32,
        aligned_high: bool,
        big_endian: bool,
    },
}
impl PcmEncoding {
    pub fn from_asbd(a: AudioStreamBasicDescription) -> Result<Self> {
        anyhow::ensure!(
            a.mFormatID == kAudioFormatLinearPCM && a.mChannelsPerFrame > 0,
            "unsupported non-PCM ASBD"
        );
        let lanes = if a.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
            1
        } else {
            a.mChannelsPerFrame
        };
        anyhow::ensure!(
            a.mBytesPerFrame.is_multiple_of(lanes),
            "PCM frame padding is unsupported"
        );
        let bytes = (a.mBytesPerFrame / lanes) as usize;
        let big_endian = a.mFormatFlags & kAudioFormatFlagIsBigEndian != 0;
        if a.mFormatFlags & kAudioFormatFlagIsFloat != 0 {
            anyhow::ensure!(
                (a.mBitsPerChannel == 32 && bytes == 4) || (a.mBitsPerChannel == 64 && bytes == 8),
                "unsupported float PCM"
            );
            Ok(Self::Float { bytes, big_endian })
        } else {
            anyhow::ensure!(
                a.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0
                    && (1..=4).contains(&bytes)
                    && (1..=32).contains(&a.mBitsPerChannel)
                    && a.mBitsPerChannel as usize <= bytes * 8,
                "unsupported integer PCM"
            );
            Ok(Self::Signed {
                bytes,
                bits: a.mBitsPerChannel,
                aligned_high: a.mFormatFlags & kAudioFormatFlagIsAlignedHigh != 0,
                big_endian,
            })
        }
    }
    pub fn bytes(&self) -> usize {
        match self {
            Self::Float { bytes, .. } | Self::Signed { bytes, .. } => *bytes,
        }
    }
    pub unsafe fn sample(&self, p: *const u8) -> f32 {
        match *self {
            Self::Float {
                bytes: 4,
                big_endian,
            } => {
                let bytes = std::ptr::read_unaligned(p.cast::<[u8; 4]>());
                f32::from_bits(if big_endian {
                    u32::from_be_bytes(bytes)
                } else {
                    u32::from_le_bytes(bytes)
                })
            }
            Self::Float {
                bytes: 8,
                big_endian,
            } => {
                let bytes = std::ptr::read_unaligned(p.cast::<[u8; 8]>());
                f64::from_bits(if big_endian {
                    u64::from_be_bytes(bytes)
                } else {
                    u64::from_le_bytes(bytes)
                }) as f32
            }
            Self::Signed {
                bytes,
                bits,
                aligned_high,
                big_endian,
            } => {
                let mut value = 0u64;
                for i in 0..bytes {
                    let shift = if big_endian {
                        (bytes - 1 - i) * 8
                    } else {
                        i * 8
                    };
                    value |= (*p.add(i) as u64) << shift;
                }
                if aligned_high {
                    value >>= bytes * 8 - bits as usize;
                }
                let signed = ((value << (64 - bits)) as i64) >> (64 - bits);
                (signed as f64 / (1u64 << (bits - 1)) as f64) as f32
            }
            _ => unreachable!("validated encoding"),
        }
    }
}
pub fn format(asbd: AudioStreamBasicDescription) -> Result<NativeFormat> {
    PcmEncoding::from_asbd(asbd)?;
    anyhow::ensure!(
        asbd.mSampleRate.is_finite() && asbd.mSampleRate.fract() == 0.,
        "nonintegral native sample rate"
    );
    NativeFormat {
        sample_rate: asbd.mSampleRate as u32,
        channels: asbd.mChannelsPerFrame.try_into()?,
    }
    .validate()
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Mic(u32),
    Tap {
        object: u32,
        device_uid: Option<String>,
        stream: u32,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub key: String,
    pub kind: SourceKind,
    pub target: Target,
    pub format: Option<NativeFormat>,
    pub encoding: Option<PcmEncoding>,
    /// Input device on the built-in transport (the user's fallback microphone).
    pub built_in: bool,
    /// The app an emitting process belongs to (App Audio's key); `None` for input devices.
    pub app: Option<crate::AppIdentity>,
}
#[derive(Default)]
pub struct Snapshot {
    pub candidates: Vec<Candidate>,
    pub watches: Vec<(u32, u32, u32, u32)>,
    pub errors: Vec<String>,
    /// Heimdall's own process object and its device UIDs (sorted, voice-processing plumbing left out), so the owner
    /// can tell a device-list change caused only by the echo unit from a real route change.
    pub own_devices: Option<(u32, Vec<String>)>,
}
/// A process's devices that can be routes: the voice-processing aggregate never is (a tap on it, or a ROUTE
/// restart because it joined Heimdall's own list, would restart every source mid-reply).
pub fn route_devices(object: u32) -> Vec<(u32, String)> {
    list(object, kAudioProcessPropertyDevices, kAudioObjectPropertyScopeGlobal)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|device| {
            let uid = string(device, kAudioDevicePropertyDeviceUID)?;
            let name = string(device, kAudioObjectPropertyName).unwrap_or_default();
            (!voice_processing_device(&uid, &name)).then_some((device, uid))
        })
        .collect()
}
pub fn snapshot() -> Result<Snapshot> {
    let mut s = Snapshot::default();
    let global = kAudioObjectPropertyScopeGlobal;
    let devices = list(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDevices,
        global,
    )?;
    for device in devices {
        let Some(uid) = string(device, kAudioDevicePropertyDeviceUID) else {
            continue;
        };
        let name = string(device, kAudioObjectPropertyName);
        // No listeners either: the voice-processing aggregate comes and goes with each echo session, and a ROUTE
        // watch on it would restart every source.
        if uid.starts_with("dev.heimdall.") || voice_processing_device(&uid, name.as_deref().unwrap_or("")) {
            continue;
        }
        for (sel, scope, bits) in [
            (kAudioDevicePropertyDeviceIsAlive, global, DISCOVERY),
            (kAudioDevicePropertyNominalSampleRate, global, ROUTE),
            (
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeInput,
                DISCOVERY,
            ),
            (
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
                ROUTE,
            ),
        ] {
            s.watches.push((device, sel, scope, bits));
        }
        let streams = list(
            device,
            kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeInput,
        )
        .unwrap_or_default();
        if streams.is_empty()
            || property::<u32>(device, kAudioDevicePropertyDeviceIsAlive, global).unwrap_or(0) == 0
        {
            continue;
        }
        let mut native = None;
        let mut channels: u16 = 0;
        let mut encoding = None;
        for stream in streams {
            s.watches
                .push((stream, kAudioStreamPropertyVirtualFormat, global, ROUTE));
            let result = property::<AudioStreamBasicDescription>(
                stream,
                kAudioStreamPropertyVirtualFormat,
                global,
            )
            .and_then(|a| Ok((format(a)?, PcmEncoding::from_asbd(a)?)));
            match result {
                Ok((f, e)) => {
                    if encoding.as_ref().is_some_and(|previous| previous != &e) {
                        s.errors.push(format!(
                            "{uid}: differing PCM encodings among input streams"
                        ));
                        channels = 0;
                        break;
                    }
                    encoding = Some(e);
                    if native.is_some_and(|rate| rate != f.sample_rate) {
                        s.errors.push(format!("{uid}: input stream rates differ"));
                        native = None;
                        channels = 0;
                        break;
                    }
                    native = Some(f.sample_rate);
                    channels = channels
                        .checked_add(f.channels)
                        .context("input channel count overflow")?;
                }
                Err(e) => {
                    s.errors.push(format!("{uid}: {e}"));
                    channels = 0;
                    break;
                }
            }
        }
        if channels > 0 {
            let format = match (NativeFormat {
                sample_rate: native.unwrap(),
                channels,
            })
            .validate()
            {
                Ok(format) => format,
                Err(error) => {
                    s.errors.push(format!("{uid}: {error}"));
                    continue;
                }
            };
            let built_in = property::<u32>(device, kAudioDevicePropertyTransportType, global)
                .is_ok_and(|t| t == kAudioDeviceTransportTypeBuiltIn);
            if speakers_as_input(&uid, built_in) {
                continue;
            }
            s.candidates.push(Candidate {
                key: format!("mic:{uid}"),
                kind: SourceKind::InputDevice {
                    name: name.unwrap_or_else(|| "Input device".into()),
                    uid,
                },
                target: Target::Mic(device),
                format: Some(format),
                encoding,
                built_in,
                app: None,
            });
        }
    }
    let processes = list(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyProcessObjectList,
        global,
    )?;
    for object in processes {
        s.watches.push((
            object,
            kAudioProcessPropertyIsRunningOutput,
            global,
            DISCOVERY,
        ));
        // HAL does not notify IsRunningOutput changes (12 Oct receipt: an afplay process went 0 -> 1 with
        // no callback); IsRunning fires as its IO starts. App Audio lists playing apps from this event.
        s.watches
            .push((object, kAudioProcessPropertyIsRunning, global, DISCOVERY));
        s.watches
            .push((object, kAudioProcessPropertyDevices, global, ROUTE));
        let Ok(pid) = property::<i32>(object, kAudioProcessPropertyPID, global) else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        if pid as u32 == std::process::id() {
            let mut uids: Vec<String> = route_devices(object).into_iter().map(|(_, uid)| uid).collect();
            uids.sort();
            s.own_devices = Some((object, uids));
        }
        if property::<u32>(object, kAudioProcessPropertyIsRunningOutput, global).unwrap_or(0) == 0 {
            continue;
        }
        let Some((birth, path)) = process_identity(pid) else {
            s.errors
                .push(format!("PID {pid}: identity vanished during discovery"));
            continue;
        };
        let name = std::path::Path::new(&path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let bundle_id = string(object, kAudioProcessPropertyBundleID);
        let owner = owner(&path);
        let app = crate::app_identity(&path, bundle_id.as_deref(), owner.as_ref());
        let mut routes = vec![];
        for (device, uid) in route_devices(object) {
            let streams = list(
                device,
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
            )
            .unwrap_or_default();
            for (index, stream) in streams.iter().enumerate() {
                s.watches
                    .push((*stream, kAudioStreamPropertyVirtualFormat, global, ROUTE));
                routes.push((Some(uid.clone()), index as u32));
            }
        }
        if routes.is_empty() {
            routes.push((None, 0));
        }
        for (uid, stream) in routes {
            let kind = SourceKind::Process {
                name: name.clone(),
                bundle_id: bundle_id.clone(),
                pid: pid as u32,
                birth_us: birth,
                owner: owner.clone(),
                output_devices: uid
                    .iter()
                    .map(|u| format!("{u} / stream {stream}"))
                    .collect(),
            };
            s.candidates.push(Candidate {
                key: format!(
                    "pid:{pid}:{birth}:{}:{stream}",
                    uid.as_deref().unwrap_or("all")
                ),
                kind,
                target: Target::Tap {
                    object,
                    device_uid: uid,
                    stream,
                },
                format: None,
                encoding: None,
                built_in: false,
                app: Some(app.clone()),
            });
        }
    }
    Ok(s)
}
/// echo guard (23 Oct): while a voice-processing unit exists the HAL lists this process's private
/// "VPAUAggregateAudioDevice" (lab/aec/REPORT.md). It is the unit's plumbing, never a microphone or a route: it gets
/// no candidate, no picker row and no listener, so the unit appearing or going away never adds or restarts a source.
pub fn voice_processing_device(uid: &str, name: &str) -> bool {
    uid.contains("VPAUAggregateAudioDevice") || name.contains("VPAUAggregateAudioDevice")
}
/// The same unit may make the built-in speakers show input streams (its echo reference). Speakers are never the user's
/// microphone; the device keeps its output listeners, it only gets no input candidate.
pub fn speakers_as_input(uid: &str, built_in: bool) -> bool {
    built_in && uid.contains("Speaker")
}
pub fn process_identity(pid: i32) -> Option<(u64, String)> {
    let mut b = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    if unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            b.as_mut_ptr().cast(),
            size as i32,
        )
    } != size as i32
    {
        return None;
    }
    let b = unsafe { b.assume_init() };
    let mut path = [0u8; 4096];
    let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if len <= 0 {
        return None;
    }
    let path = String::from_utf8_lossy(&path[..len as usize])
        .trim_end_matches('\0')
        .to_string();
    Some((b.pbi_start_tvsec * 1_000_000 + b.pbi_start_tvusec, path))
}
pub fn owner(path: &str) -> Option<AppOwner> {
    let path = owning_app_path(path)?;
    let bundle = NSBundle::bundleWithPath(&NSString::from_str(&path));
    let bundle_id = bundle
        .as_ref()
        .and_then(|b| b.bundleIdentifier())
        .map(|s| s.to_string());
    let name = bundle
        .as_ref()
        .and_then(|b| b.objectForInfoDictionaryKey(&NSString::from_str("CFBundleDisplayName")))
        .and_then(|v| v.downcast::<NSString>().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            std::path::Path::new(&path)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    Some(AppOwner {
        name,
        bundle_id,
        path,
    })
}
type ListenerBlock = RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)>;
pub struct Listener {
    object: u32,
    address: AudioObjectPropertyAddress,
    block: ListenerBlock,
}
impl Listener {
    pub fn new(
        object: u32,
        sel: u32,
        scope: u32,
        bits: u32,
        dirty: Arc<AtomicU32>,
        wake: Thread,
        changes: Arc<crossbeam_queue::ArrayQueue<(u32, u32)>>,
    ) -> Result<Self> {
        let a = address(sel, scope);
        let block = RcBlock::new(move |_: u32, _: NonNull<AudioObjectPropertyAddress>| {
            let _ = changes.push((object, sel));
            dirty.fetch_or(bits, Ordering::Release);
            wake.unpark();
        });
        unsafe {
            check(
                "HAL add listener",
                AudioObjectAddPropertyListenerBlock(
                    object,
                    NonNull::from(&a),
                    None,
                    RcBlock::as_ptr(&block),
                ),
            )?;
        }
        Ok(Self {
            object,
            address: a,
            block,
        })
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            let _ = AudioObjectRemovePropertyListenerBlock(
                self.object,
                NonNull::from(&self.address),
                None,
                RcBlock::as_ptr(&self.block),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn voice_processing_plumbing_is_never_a_microphone() {
        assert!(voice_processing_device("VPAUAggregateAudioDevice-0x600003a1c000", "VPAUAggregateAudioDevice"));
        assert!(voice_processing_device("~:AMS2_Aggregate:0", "VPAUAggregateAudioDevice"));
        assert!(speakers_as_input("BuiltInSpeakerDevice", true));
        // Real inputs stay: the built-in mic, the Shure, a USB speakerphone (not built-in), the AirPods.
        assert!(!speakers_as_input("BuiltInMicrophoneDevice", true));
        assert!(!speakers_as_input("AppleUSBAudioEngine:Jabra:Jabra Speaker 510:1", false));
        for (uid, name) in [
            ("BuiltInMicrophoneDevice", "MacBook Pro Microphone"),
            ("AppleUSBAudioEngine:Shure Inc:Shure MV7i:2,3", "Shure MV7i"),
            ("AC-3E-B1:input", "AirPods Pro"),
        ] {
            assert!(!voice_processing_device(uid, name));
        }
    }
    #[test]
    fn native_integer_formats_and_endianness() {
        let pcm = PcmEncoding::Signed {
            bytes: 3,
            bits: 24,
            aligned_high: false,
            big_endian: false,
        };
        assert_eq!(unsafe { pcm.sample([0, 0, 128].as_ptr()) }, -1.);
        assert_eq!(unsafe { pcm.sample([0, 0, 64].as_ptr()) }, 0.5);
        let pcm = PcmEncoding::Signed {
            bytes: 4,
            bits: 24,
            aligned_high: true,
            big_endian: true,
        };
        assert_eq!(unsafe { pcm.sample([0x40, 0, 0, 0].as_ptr()) }, 0.5);
        assert_eq!(unsafe { pcm.sample([0x80, 0, 0, 0].as_ptr()) }, -1.);
        let pcm = PcmEncoding::Float {
            bytes: 4,
            big_endian: true,
        };
        assert_eq!(unsafe { pcm.sample(0.25f32.to_be_bytes().as_ptr()) }, 0.25);
    }
}
