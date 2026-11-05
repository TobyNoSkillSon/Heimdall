// Adapted from flexaudio tap.rs/tap/buffer.rs, revision d3fe9bdb75fc883371757a780fb20b5efc39317a.
// Copyright (c) 2026 tubome / Studio Sadola. MIT: vendor/flexaudio/LICENSE.
// Heimdall negotiates ASBD before allocating any queues, delivers host timestamps,
// has a shared intake worker, and has no polling/self-probe or playback policy.
use super::hal::{self, Candidate, Target};
use crate::{
    echo::{EchoMux, EchoRing},
    pipeline::RawQueue,
    SourceInfo,
};
use anyhow::Result;
use block2::RcBlock;
use objc2::{rc::Retained, AnyThread};
use objc2_core_audio::*;
use objc2_core_audio_types::*;
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSString, NSUUID};
use std::{
    cell::RefCell,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
type IoBlock = RcBlock<
    dyn Fn(
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
    ),
>;
/// Native objects and blocks always stay on the shared control/owner thread.
pub struct NativeSource {
    device: u32,
    tap: u32,
    io: AudioDeviceIOProcID,
    block: Option<IoBlock>,
    desc: Option<Retained<CATapDescription>>,
    pub raw: Arc<RawQueue>,
    /// Microphones only: where a voice-processing session for this device puts its cleaned audio (`echo.rs`).
    pub echo: Option<Arc<EchoRing>>,
}
/// Asks macOS for System Audio Recording now: a private global tap in a private aggregate device, its IOProc started
/// and stopped at once, and everything destroyed. Starting audio through the tap is what shows the prompt (VM runs 5
/// Nov 08:09-08:41: making and destroying a tap showed nothing; the prompt came with the first capture's start), and
/// the start waits for the answer, so this runs on its own thread (`tap_gate`). Nothing is read or kept.
pub fn ask_system_audio() -> Result<()> {
    unsafe {
        let none = NSArray::<NSNumber>::new();
        let d = CATapDescription::initStereoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &none);
        d.setName(&NSString::from_str("Heimdall permission request"));
        d.setPrivate(true);
        d.setMuteBehavior(CATapMuteBehavior::Unmuted);
        let mut tap = 0;
        hal::check("create process tap", AudioHardwareCreateProcessTap(Some(&d), &mut tap))?;
        anyhow::ensure!(tap != 0, "tap returned no object");
        let result = (|| -> Result<()> {
            let aggregate = create_aggregate(&d.UUID().UUIDString())?;
            let block: IoBlock = RcBlock::new(|_: NonNull<AudioTimeStamp>, _: NonNull<AudioBufferList>, _: NonNull<AudioTimeStamp>, out: NonNull<AudioBufferList>, _: NonNull<AudioTimeStamp>| {
                zero_output(out.as_ptr());
            });
            let mut io: AudioDeviceIOProcID = None;
            let started = hal::check("create IOProc", AudioDeviceCreateIOProcIDWithBlock(NonNull::from(&mut io), aggregate, None, RcBlock::as_ptr(&block)))
                .and_then(|()| { anyhow::ensure!(io.is_some(), "callback registration returned no IOProc"); Ok(()) })
                .and_then(|()| hal::check("start the permission request", AudioDeviceStart(aggregate, io)));
            if io.is_some() {
                let _ = AudioDeviceStop(aggregate, io);
                let _ = AudioDeviceDestroyIOProcID(aggregate, io);
            }
            let _ = AudioHardwareDestroyAggregateDevice(aggregate);
            drop(block);
            started
        })();
        let _ = AudioHardwareDestroyProcessTap(tap);
        result
    }
}

impl NativeSource {
    pub fn create(
        candidate: &Candidate,
        generation: u64,
        wake: std::thread::Thread,
        control_wake: std::thread::Thread,
    ) -> Result<Self> {
        let mut tap = 0;
        let mut aggregate = 0;
        let mut desc = None;
        let setup = (|| unsafe {
            let (device, format, encoding) = match &candidate.target {
                Target::Mic(device) => (
                    *device,
                    candidate
                        .format
                        .ok_or_else(|| anyhow::anyhow!("missing input format"))?,
                    candidate
                        .encoding
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("missing PCM encoding"))?,
                ),
                Target::Tap {
                    object,
                    device_uid,
                    stream,
                } => {
                    let objects =
                        NSArray::from_retained_slice(&[NSNumber::numberWithUnsignedInt(*object)]);
                    let d = if let Some(uid) = device_uid {
                        CATapDescription::initWithProcesses_andDeviceUID_withStream(
                            CATapDescription::alloc(),
                            &objects,
                            &NSString::from_str(uid),
                            *stream as isize,
                        )
                    } else {
                        CATapDescription::initStereoMixdownOfProcesses(
                            CATapDescription::alloc(),
                            &objects,
                        )
                    };
                    d.setName(&NSString::from_str("Heimdall per-process archive"));
                    d.setPrivate(true);
                    d.setMuteBehavior(CATapMuteBehavior::Unmuted);
                    hal::check(
                        "create process tap",
                        AudioHardwareCreateProcessTap(Some(&d), &mut tap),
                    )?;
                    anyhow::ensure!(tap != 0, "tap returned no object");
                    // Format handshake FIRST: no cached 48k stereo assumption.
                    let asbd = hal::property(
                        tap,
                        kAudioTapPropertyFormat,
                        kAudioObjectPropertyScopeGlobal,
                    )?;
                    let format = hal::format(asbd)?;
                    let encoding = hal::PcmEncoding::from_asbd(asbd)?;
                    aggregate = create_aggregate(&d.UUID().UUIDString())?;
                    desc = Some(d);
                    (aggregate, format, encoding)
                }
            };
            let mut info = SourceInfo::new(candidate.kind.clone(), format, generation);
            info.native_encoding = format!("{encoding:?}");
            let microphone = matches!(candidate.target, Target::Mic(_));
            // A microphone keeps its raw samples for the archive and a cleaned copy for listen (pipeline::Packet).
            let raw = if microphone { RawQueue::with_clean_overlay(info, wake) } else { RawQueue::new(info, wake) };
            let raw_for_block = raw.clone();
            let entered = AtomicBool::new(false);
            // echo guard (23 Oct): every mic carries an idle mux (192 KiB at 48 kHz); only the primary's ever goes
            // live. The IOProc keeps its own timestamps and source, so a session's start and end never restart it.
            let echo = microphone.then(|| EchoRing::for_rate(format.sample_rate));
            let mux = echo
                .clone()
                .map(|ring| RefCell::new(EchoMux::new(ring, format.sample_rate)));
            let block = RcBlock::new(
                move |_now: NonNull<AudioTimeStamp>,
                      input: NonNull<AudioBufferList>,
                      input_time: NonNull<AudioTimeStamp>,
                      out: NonNull<AudioBufferList>,
                      _out_time: NonNull<AudioTimeStamp>| {
                    if microphone {
                        zero_output(out.as_ptr());
                    }
                    if entered
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_err()
                    {
                        raw_for_block.reject_frames(1);
                        return;
                    }
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        let time = *input_time.as_ptr();
                        let ns = if time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
                            crate::time::ticks_to_ns(time.mHostTime)
                        } else {
                            raw_for_block.reject_frames(1);
                            return;
                        };
                        let first = raw_for_block.last_callback.load(Ordering::Acquire) == 0;
                        // `entered` already excludes reentry, so the borrow cannot fail; if it did, raw passes.
                        let mut mux = mux.as_ref().and_then(|m| m.try_borrow_mut().ok());
                        push_buffers(&raw_for_block, &encoding, input.as_ptr(), ns, mux.as_deref_mut());
                        if first && raw_for_block.last_callback.load(Ordering::Acquire) != 0 {
                            // Activate the shared health check once audio arrives.
                            // Dormant, auto-start-gated taps need no timer.
                            control_wake.unpark();
                        }
                    }));
                    if result.is_err() {
                        raw_for_block.reject_frames(1);
                    }
                    entered.store(false, Ordering::Release);
                },
            );
            let mut source = Self {
                device,
                tap,
                io: None,
                block: Some(block),
                desc,
                raw,
                echo,
            };
            // Source takes ownership now. On subsequent errors its Drop rolls back.
            tap = 0;
            aggregate = 0;
            hal::check(
                "create IOProc",
                AudioDeviceCreateIOProcIDWithBlock(
                    NonNull::from(&mut source.io),
                    device,
                    None,
                    RcBlock::as_ptr(source.block.as_ref().unwrap()),
                ),
            )?;
            anyhow::ensure!(
                source.io.is_some(),
                "callback registration returned no IOProc"
            );
            // voice (night 1022): a microphone is read, never played to; see `mic_outputs_off`.
            if microphone {
                if let Err(e) = mic_outputs_off(device, source.io) {
                    eprintln!("capture: could not switch off the microphone's output side: {e:#}");
                }
            }
            Ok(source)
        })();
        if setup.is_err() {
            unsafe {
                if aggregate != 0 {
                    let _ = AudioHardwareDestroyAggregateDevice(aggregate);
                }
                if tap != 0 {
                    let _ = AudioHardwareDestroyProcessTap(tap);
                }
            }
        }
        setup
    }
    pub fn start(&self) -> Result<()> {
        unsafe {
            hal::check(
                "start native source",
                AudioDeviceStart(self.device, self.io),
            )
        }
    }
}
impl NativeSource {
    pub fn stop_checked(&mut self) -> Vec<String> {
        self.raw.stopped.store(true, Ordering::Release);
        let mut errors = vec![];
        unsafe {
            if self.io.is_some() {
                for (call, status) in [
                    ("AudioDeviceStop", AudioDeviceStop(self.device, self.io)),
                    (
                        "AudioDeviceDestroyIOProcID",
                        AudioDeviceDestroyIOProcID(self.device, self.io),
                    ),
                ] {
                    if status != 0 {
                        errors.push(format!("{call}: OSStatus {status}"));
                    }
                }
                self.io = None;
            }
            if self.tap != 0 {
                for (call, status) in [
                    (
                        "AudioHardwareDestroyAggregateDevice",
                        AudioHardwareDestroyAggregateDevice(self.device),
                    ),
                    (
                        "AudioHardwareDestroyProcessTap",
                        AudioHardwareDestroyProcessTap(self.tap),
                    ),
                ] {
                    if status != 0 {
                        errors.push(format!("{call}: OSStatus {status}"));
                    }
                }
                self.tap = 0;
            }
        }
        self.block.take();
        self.desc.take();
        errors
    }
}
impl Drop for NativeSource {
    fn drop(&mut self) {
        let _ = self.stop_checked();
    }
}
unsafe fn zero_output(list: *mut AudioBufferList) {
    let n = (*list).mNumberBuffers as usize;
    if n > 32 {
        return;
    }
    for b in std::slice::from_raw_parts_mut((*list).mBuffers.as_mut_ptr(), n) {
        if !b.mData.is_null() {
            std::ptr::write_bytes(b.mData.cast::<u8>(), 0, b.mDataByteSize as usize);
        }
    }
}

/// Supports interleaved, planar and several interleaved channel groups. Reject
/// mismatched byte counts/layouts before dereferencing any sample pointer.
unsafe fn push_buffers(
    raw: &RawQueue,
    encoding: &hal::PcmEncoding,
    list: *const AudioBufferList,
    ns: u64,
    mux: Option<&mut EchoMux>,
) {
    let n = (*list).mNumberBuffers as usize;
    if n == 0 || n > 32 {
        raw.reject_frames(1);
        return;
    }
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n);
    let mut channels = 0;
    let mut frames = None;
    let width = encoding.bytes();
    for b in buffers {
        let ch = b.mNumberChannels as usize;
        if ch == 0 || ch > 32 || b.mData.is_null() || !(b.mDataByteSize as usize).is_multiple_of(width * ch) {
            raw.reject_frames(1);
            return;
        }
        let count = b.mDataByteSize as usize / (width * ch);
        if frames.is_some_and(|f| f != count) {
            raw.reject_frames(count as u64);
            return;
        }
        frames = Some(count);
        channels += ch;
    }
    if channels != raw.info.format.channels as usize {
        raw.reject_frames(frames.unwrap_or(0) as u64);
        return;
    }
    push_mixed(raw, mux, frames.unwrap_or(0), ns, |frame, channel| {
        let mut c = channel;
        for b in buffers {
            if c < b.mNumberChannels as usize {
                return encoding.sample(
                    (b.mData as *const u8).add((frame * b.mNumberChannels as usize + c) * width),
                );
            }
            c -= b.mNumberChannels as usize;
        }
        0.
    });
}
/// The echo mux's only touch point: while a session is live, listen gets cleaned audio frame for frame and the archive
/// still gets the raw samples (the user's decision, 23 Oct); the frame count and the IOProc's own host time `ns` pass through
/// unchanged.
fn push_mixed(
    raw: &RawQueue,
    mux: Option<&mut EchoMux>,
    frames: usize,
    ns: u64,
    read: impl Fn(usize, usize) -> f32,
) {
    let mux = mux.and_then(|m| if m.plan(frames) { Some(&*m) } else { None });
    match mux {
        Some(m) => {
            let channels = raw.info.format.channels.max(1) as usize;
            let clean = |f: usize| (0..channels).map(|c| m.blend(f, read(f, c))).sum::<f32>() / channels as f32;
            raw.push_frames_cleaned(frames, ns, &read, clean)
        }
        None => raw.push_frames(frames, ns, read),
    }
}
#[cfg(test)]
mod echo_mux_tests {
    use crate::{echo::{EchoMux, EchoRing}, pipeline::RawQueue, NativeFormat, SourceInfo, SourceKind};
    use std::sync::atomic::Ordering;
    #[test]
    fn substitution_keeps_the_ioprocs_timestamps_and_frame_count() {
        let format = NativeFormat { sample_rate: 48_000, channels: 2 };
        let info = SourceInfo::new(SourceKind::InputDevice { name: "Shure MV7i".into(), uid: "Shure".into() }, format, 1);
        let raw = RawQueue::with_clean_overlay(info, std::thread::current());
        let ring = EchoRing::for_rate(48_000);
        let mut mux = EchoMux::new(ring.clone(), 48_000);
        ring.live.store(true, Ordering::Release);
        ring.push_clean(&[0.5; 4096]);
        let ns = [1_000_000_000u64, 1_010_666_666, 1_021_333_333];
        for &t in &ns {
            super::push_mixed(&raw, Some(&mut mux), 512, t, |_, _| -0.5);
        }
        let packets = raw.take_packets_with_clean();
        assert_eq!(packets.iter().map(|p| p.0).collect::<Vec<_>>(), ns, "host times are the IOProc's own");
        assert!(packets.iter().all(|p| p.1.len() == 1024), "frame for frame, every channel");
        // The user's decision (23 Oct): the archive holds the raw microphone, even while echo cancellation runs.
        assert!(packets.iter().all(|p| p.1.iter().all(|&v| v == -0.5)), "cleaned audio reached the archive's samples");
        let clean: Vec<&Vec<f32>> = packets.iter().map(|p| p.2.as_ref().expect("listen's cleaned copy")).collect();
        assert!(clean.iter().all(|c| c.len() == 512), "one mono sample per frame");
        let first = 0.5 / 480. - 0.5 * (1. - 1. / 480.);
        assert!((clean[0][0] - first).abs() < 1e-6, "crossfade starts at the first frame");
        assert!(clean[2].iter().all(|&v| v == 0.5), "cleaned mono after the fade");
        // No mux, or an idle one: the raw samples, untouched.
        ring.live.store(false, Ordering::Release);
        let mut idle = EchoMux::new(EchoRing::for_rate(48_000), 48_000);
        super::push_mixed(&raw, Some(&mut idle), 512, 2_000_000_000, |_, _| -0.5);
        super::push_mixed(&raw, None, 512, 2_010_666_666, |_, _| -0.5);
        let packets = raw.take_packets_with_clean();
        assert!(packets.iter().all(|p| p.1.iter().all(|&v| v == -0.5) && p.2.is_none()));
        assert_eq!(packets[1].0, 2_010_666_666);
    }
}
/// A microphone's IOProc reads input only. On a device that also has outputs (the Shure MV7i has a headphone jack)
/// the HAL otherwise runs this process's output on it too, and macOS counts Heimdall as recording and playing at
/// once: a call (Smart Routing audio score 501, measured 22 Oct while Heimdall was silent) that outranks a video on
/// the user's iPhone, so his AirPods are pulled back to the Mac whenever Heimdall makes a sound. Returns how many output
/// streams were switched off.
fn mic_outputs_off(device: u32, io: AudioDeviceIOProcID) -> Result<usize> {
    let Some(proc_) = io else { return Ok(0) };
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyIOProcStreamUsage,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(device, NonNull::from(&address), 0, std::ptr::null(), NonNull::from(&mut size))
    };
    if status != 0 || (size as usize) < STREAM_USAGE_HEADER {
        return Ok(0); // no output side
    }
    // u64 words keep the record's pointer aligned.
    let mut record = vec![0u64; (size as usize).div_ceil(8)];
    record[0] = proc_ as usize as u64;
    unsafe {
        hal::check(
            "get IOProc stream usage",
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(record.as_mut_ptr()).unwrap().cast(),
            ),
        )?;
    }
    let bytes = unsafe { std::slice::from_raw_parts_mut(record.as_mut_ptr().cast::<u8>(), size as usize) };
    let off = outputs_off(bytes)?;
    if off > 0 {
        unsafe {
            hal::check(
                "set IOProc stream usage",
                AudioObjectSetPropertyData(
                    device,
                    NonNull::from(&address),
                    0,
                    std::ptr::null(),
                    size,
                    NonNull::new(record.as_mut_ptr()).unwrap().cast(),
                ),
            )?;
        }
    }
    Ok(off)
}
/// `AudioHardwareIOProcStreamUsage`: the IOProc pointer, the stream count, then one on/off word per stream.
const STREAM_USAGE_HEADER: usize = std::mem::size_of::<usize>() + 4;
/// Switches every stream in a stream-usage record off; returns how many were on.
fn outputs_off(record: &mut [u8]) -> Result<usize> {
    anyhow::ensure!(record.len() >= STREAM_USAGE_HEADER, "stream usage record too short");
    let count = u32::from_ne_bytes(record[8..12].try_into().unwrap()) as usize;
    anyhow::ensure!(record.len() >= STREAM_USAGE_HEADER + 4 * count, "stream usage record shorter than its {count} streams");
    let mut on = 0;
    for stream in 0..count {
        let at = STREAM_USAGE_HEADER + 4 * stream;
        if record[at..at + 4] != [0; 4] {
            on += 1;
        }
        record[at..at + 4].copy_from_slice(&0u32.to_ne_bytes());
    }
    Ok(on)
}
#[cfg(test)]
mod stream_usage_tests {
    #[test]
    fn every_output_stream_of_a_microphone_is_switched_off() {
        // A pointer, two streams (both on), as the HAL returns it for a duplex USB microphone.
        let mut record = vec![0u8; 8 + 4 + 8];
        record[..8].copy_from_slice(&0x1234_5678_u64.to_ne_bytes());
        record[8..12].copy_from_slice(&2u32.to_ne_bytes());
        record[12..16].copy_from_slice(&1u32.to_ne_bytes());
        record[16..20].copy_from_slice(&1u32.to_ne_bytes());
        assert_eq!(super::outputs_off(&mut record).unwrap(), 2);
        assert_eq!(&record[12..], &[0; 8]);
        assert_eq!(&record[..8], &0x1234_5678_u64.to_ne_bytes(), "the IOProc is kept");
        // Already off, or no output streams: nothing to set.
        assert_eq!(super::outputs_off(&mut record).unwrap(), 0);
        let mut none = vec![0u8; 12];
        assert_eq!(super::outputs_off(&mut none).unwrap(), 0);
        // A record shorter than its count is refused, not overrun.
        let mut short = vec![0u8; 12];
        short[8..12].copy_from_slice(&3u32.to_ne_bytes());
        assert!(super::outputs_off(&mut short).is_err());
    }
}
fn key(c: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(c.to_str().unwrap())
}
fn create_aggregate(uid: &NSString) -> Result<u32> {
    let yes = NSNumber::numberWithBool(true);
    let no = NSNumber::numberWithBool(false);
    let subtap: Retained<NSDictionary<NSString, NSObject>> = NSDictionary::from_slices::<NSString>(
        &[
            &key(kAudioSubTapUIDKey),
            &key(kAudioSubTapDriftCompensationKey),
        ],
        &[uid.as_ref(), yes.as_ref()],
    );
    let taps: Retained<NSArray<NSObject>> =
        NSArray::from_retained_slice(&[Retained::into_super(subtap)]);
    let aggregate_uid = NSString::from_str(&format!(
        "dev.heimdall.aggregate.{}",
        NSUUID::new().UUIDString()
    ));
    let name = NSString::from_str("Heimdall private tap");
    let dict: Retained<NSDictionary<NSString, NSObject>> = NSDictionary::from_slices::<NSString>(
        &[
            &key(kAudioAggregateDeviceNameKey),
            &key(kAudioAggregateDeviceUIDKey),
            &key(kAudioAggregateDeviceIsPrivateKey),
            &key(kAudioAggregateDeviceIsStackedKey),
            &key(kAudioAggregateDeviceTapAutoStartKey),
            &key(kAudioAggregateDeviceTapListKey),
        ],
        &[
            name.as_ref(),
            aggregate_uid.as_ref(),
            yes.as_ref(),
            no.as_ref(),
            yes.as_ref(),
            taps.as_ref(),
        ],
    );
    let cf = unsafe { &*(Retained::as_ptr(&dict) as *const CFDictionary) };
    let mut id = 0;
    unsafe {
        hal::check(
            "create private aggregate",
            AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut id)),
        )?;
    }
    anyhow::ensure!(id != 0, "aggregate returned no object");
    Ok(id)
}
