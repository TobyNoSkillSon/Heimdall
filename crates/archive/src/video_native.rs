//! Native hardware HEVC, no process/runtime codec dependency and no permissions.
use super::{Format, Sample};
use anyhow::{ensure, Context, Result};
use block2::RcBlock;
use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::*;
use objc2_core_video::*;
use objc2_video_toolbox::*;
use std::{
    ptr::{self, NonNull},
    sync::mpsc,
    time::Duration,
};

fn ok(status: i32, what: &str) -> Result<()> {
    ensure!(status == 0, "{what}: OSStatus {status}");
    Ok(())
}
fn time(ns: u64) -> CMTime {
    unsafe { CMTime::new(ns as i64, 1_000_000_000) }
}
fn dict(key: &CFString, value: &CFType) -> CFRetained<CFDictionary<CFString, CFType>> {
    CFDictionary::from_slices(&[key], &[value])
}
fn pixel_attributes() -> CFRetained<CFDictionary<CFString, CFType>> {
    unsafe {
        dict(
            kCVPixelBufferPixelFormatTypeKey,
            CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32).as_ref(),
        )
    }
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    width: u32,
    height: u32,
}
impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, quality: f64) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width <= 16384 && height <= 16384,
            "invalid encoder dimensions"
        );
        unsafe {
            let spec = dict(
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                CFBoolean::new(true).as_ref(),
            );
            let attrs = pixel_attributes();
            let mut raw = ptr::null_mut();
            ok(
                VTCompressionSession::create(
                    None,
                    width as i32,
                    height as i32,
                    kCMVideoCodecType_HEVC,
                    Some(spec.as_ref()),
                    Some(attrs.as_ref()),
                    None,
                    None,
                    ptr::null_mut(),
                    NonNull::from(&mut raw),
                ),
                "create hardware HEVC encoder",
            )?;
            let session = CFRetained::from_raw(NonNull::new(raw).context("null encoder")?);
            let set = |key, value: &CFType| {
                ok(
                    VTSessionSetProperty(session.as_ref(), key, Some(value)),
                    "configure HEVC",
                )
            };
            set(
                kVTCompressionPropertyKey_RealTime,
                CFBoolean::new(true).as_ref(),
            )?;
            set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false).as_ref(),
            )?;
            set(
                kVTCompressionPropertyKey_Quality,
                CFNumber::new_f64(quality).as_ref(),
            )?;
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                CFNumber::new_i32(fps as i32).as_ref(),
            )?;
            set(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                CFNumber::new_f64(60.).as_ref(),
            )?;
            set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                CFNumber::new_i32(300).as_ref(),
            )?;
            ok(session.prepare_to_encode_frames(), "prepare encoder")?;
            Ok(Self {
                session,
                width,
                height,
            })
        }
    }
    pub fn encode(&mut self, rgb: &[u8], host_ns: u64, end_ns: u64) -> Result<(Format, Sample)> {
        ensure!(
            rgb.len() == self.width as usize * self.height as usize * 3 && end_ns > host_ns,
            "invalid encoder frame"
        );
        unsafe {
            let attrs = pixel_attributes();
            let mut raw = ptr::null_mut();
            ok(
                CVPixelBufferCreate(
                    None,
                    self.width as usize,
                    self.height as usize,
                    kCVPixelFormatType_32BGRA,
                    Some(attrs.as_ref()),
                    NonNull::from(&mut raw),
                ),
                "create encoder pixels",
            )?;
            let pixel = CFRetained::from_raw(NonNull::new(raw).context("null pixels")?);
            ok(
                CVPixelBufferLockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()),
                "lock encoder pixels",
            )?;
            let base = CVPixelBufferGetBaseAddress(&pixel) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(&pixel);
            for y in 0..self.height as usize {
                for x in 0..self.width as usize {
                    let i = (y * self.width as usize + x) * 3;
                    let p = base.add(y * stride + x * 4);
                    ptr::copy_nonoverlapping([rgb[i + 2], rgb[i + 1], rgb[i], 255].as_ptr(), p, 4);
                }
            }
            ok(
                CVPixelBufferUnlockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()),
                "unlock encoder pixels",
            )?;
            let (tx, rx) = mpsc::sync_channel(1);
            let block = RcBlock::new(
                move |status: i32, _: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
                    let result = (|| -> Result<_> {
                        ok(status, "HEVC encode callback")?;
                        let s = sample.as_ref().context("encoder dropped frame")?;
                        let format = s.format_description().context("missing HEVC format")?;
                        let extensions = format.extensions().context("missing HEVC extensions")?;
                        let extensions = &*(&*extensions as *const CFDictionary
                            as *const CFDictionary<CFString, CFType>);
                        let atoms = extensions
                            .get(CFString::from_str("SampleDescriptionExtensionAtoms").as_ref())
                            .context("missing codec atoms")?;
                        let atoms = atoms
                            .downcast_ref::<CFDictionary>()
                            .context("invalid codec atoms")?;
                        let atoms = &*(atoms as *const CFDictionary
                            as *const CFDictionary<CFString, CFType>);
                        let data = atoms
                            .get(CFString::from_str("hvcC").as_ref())
                            .context("missing hvcC")?;
                        let hvcc = data
                            .downcast_ref::<CFData>()
                            .context("invalid hvcC")?
                            .to_vec();
                        let mut parameters = vec![];
                        let mut count = 0;
                        ok(
                            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                                &format,
                                0,
                                ptr::null_mut(),
                                ptr::null_mut(),
                                &mut count,
                                ptr::null_mut(),
                            ),
                            "HEVC parameter count",
                        )?;
                        for i in 0..count {
                            let mut p = ptr::null();
                            let mut len = 0;
                            ok(
                                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                                    &format,
                                    i,
                                    &mut p,
                                    &mut len,
                                    ptr::null_mut(),
                                    ptr::null_mut(),
                                ),
                                "HEVC parameter",
                            )?;
                            parameters.push(std::slice::from_raw_parts(p, len).to_vec());
                        }
                        let buffer = s.data_buffer().context("missing HEVC data")?;
                        let mut bytes = vec![0; buffer.data_length()];
                        ensure!(
                            bytes.len() <= super::MAX_SAMPLE_BYTES,
                            "encoded sample too large"
                        );
                        ok(
                            buffer.copy_data_bytes(
                                0,
                                bytes.len(),
                                NonNull::new(bytes.as_mut_ptr().cast()).unwrap(),
                            ),
                            "copy HEVC sample",
                        )?;
                        let sync = s
                            .sample_attachments_array(false)
                            .and_then(|a| {
                                if a.count() == 0 {
                                    return None;
                                }
                                let d = &*(a.value_at_index(0)
                                    as *const CFDictionary<CFString, CFType>);
                                d.get(kCMSampleAttachmentKey_NotSync.as_ref())
                                    .and_then(|v| {
                                        v.downcast_ref::<CFBoolean>().map(|b| b.as_bool())
                                    })
                            })
                            .unwrap_or(false);
                        let dims = CMVideoFormatDescriptionGetDimensions(&format);
                        Ok((
                            Format {
                                width: dims.width as u32,
                                height: dims.height as u32,
                                hvcc,
                                parameters,
                            },
                            Sample {
                                host_ns,
                                end_ns,
                                key: !sync,
                                data: bytes,
                            },
                        ))
                    })()
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(result);
                },
            );
            ok(
                self.session.encode_frame_with_output_handler(
                    &pixel,
                    time(host_ns),
                    time(end_ns - host_ns),
                    None,
                    ptr::null_mut(),
                    RcBlock::as_ptr(&block) as *mut _,
                ),
                "submit HEVC frame",
            )?;
            // No reorder/lookahead queue: bounded outstanding native/RGB surfaces.
            ok(
                self.session.complete_frames(time(host_ns)),
                "complete HEVC frame",
            )?;
            rx.recv_timeout(Duration::from_secs(5))
                .context("HEVC encoder timeout")?
                .map_err(anyhow::Error::msg)
        }
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Pixels {
    None,
    Rgb,
    Bgra,
}
pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
}
impl Decoder {
    pub fn new(format: &Format) -> Result<Self> {
        unsafe {
            ensure!(
                !format.parameters.is_empty() && format.parameters.len() <= 16,
                "invalid HEVC parameter sets"
            );
            let mut pointers: Vec<_> = format
                .parameters
                .iter()
                .map(|p| NonNull::new(p.as_ptr() as *mut u8).unwrap())
                .collect();
            let mut sizes: Vec<_> = format.parameters.iter().map(Vec::len).collect();
            let mut raw = ptr::null();
            ok(
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    pointers.len(),
                    NonNull::new(pointers.as_mut_ptr()).unwrap(),
                    NonNull::new(sizes.as_mut_ptr()).unwrap(),
                    4,
                    None,
                    NonNull::from(&mut raw),
                ),
                "create HEVC decoder format",
            )?;
            let description = CFRetained::from_raw(
                NonNull::new(raw as *mut CMFormatDescription).context("null decoder format")?,
            );
            let attrs = pixel_attributes();
            let mut raw = ptr::null_mut();
            ok(
                VTDecompressionSession::create(
                    None,
                    &description,
                    None,
                    Some(attrs.as_ref()),
                    ptr::null(),
                    NonNull::from(&mut raw),
                ),
                "create HEVC decoder",
            )?;
            Ok(Self {
                session: CFRetained::from_raw(NonNull::new(raw).context("null decoder")?),
                format: description,
            })
        }
    }
    pub fn decode(&mut self, sample: &Sample) -> Result<Vec<u8>> {
        self.decode_inner(sample, Pixels::Rgb)
    }
    /// Decoded BGRA rows, tightly packed (`width * 4` bytes each), for a native encoder input.
    pub fn decode_bgra(&mut self, sample: &Sample) -> Result<Vec<u8>> {
        self.decode_inner(sample, Pixels::Bgra)
    }
    pub fn advance(&mut self, sample: &Sample) -> Result<()> {
        self.decode_inner(sample, Pixels::None)?;
        Ok(())
    }
    fn decode_inner(&mut self, sample: &Sample, pixels: Pixels) -> Result<Vec<u8>> {
        unsafe {
            ensure!(
                !sample.data.is_empty() && sample.data.len() <= super::MAX_SAMPLE_BYTES,
                "invalid compressed frame"
            );
            let mut raw = ptr::null_mut();
            ok(
                CMBlockBuffer::create_with_memory_block(
                    None,
                    ptr::null_mut(),
                    sample.data.len(),
                    None,
                    ptr::null(),
                    0,
                    sample.data.len(),
                    0,
                    NonNull::from(&mut raw),
                ),
                "create HEVC block",
            )?;
            let data = CFRetained::from_raw(NonNull::new(raw).context("null data block")?);
            ok(
                CMBlockBuffer::replace_data_bytes(
                    NonNull::new(sample.data.as_ptr() as *mut _).unwrap(),
                    &data,
                    0,
                    sample.data.len(),
                ),
                "fill HEVC block",
            )?;
            let timing = CMSampleTimingInfo {
                duration: time(sample.end_ns - sample.host_ns),
                presentationTimeStamp: time(sample.host_ns),
                decodeTimeStamp: time(sample.host_ns),
            };
            let mut raw = ptr::null_mut();
            ok(
                CMSampleBuffer::create_ready(
                    None,
                    Some(&data),
                    Some(&self.format),
                    1,
                    1,
                    &timing,
                    1,
                    &sample.data.len(),
                    NonNull::from(&mut raw),
                ),
                "create HEVC sample",
            )?;
            let compressed = CFRetained::from_raw(NonNull::new(raw).context("null sample")?);
            let (tx, rx) = mpsc::sync_channel(1);
            let block = RcBlock::new(
                move |status: i32,
                      _: VTDecodeInfoFlags,
                      p: *mut CVImageBuffer,
                      _: CMTime,
                      _: CMTime| {
                    let result = (|| -> Result<_> {
                        ok(status, "HEVC decode callback")?;
                        if pixels == Pixels::None {
                            return Ok(Vec::new());
                        }
                        let pixel = p.as_ref().context("decoder dropped frame")?;
                        ok(
                            CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly),
                            "lock decoded pixels",
                        )?;
                        let w = CVPixelBufferGetWidth(pixel);
                        let h = CVPixelBufferGetHeight(pixel);
                        let stride = CVPixelBufferGetBytesPerRow(pixel);
                        let base = CVPixelBufferGetBaseAddress(pixel) as *const u8;
                        let valid = w > 0
                            && h > 0
                            && w * h <= 16384 * 16384
                            && stride >= w * 4
                            && !base.is_null();
                        let mut rgb = vec![];
                        if valid && pixels == Pixels::Bgra {
                            rgb.reserve(w * h * 4);
                            for y in 0..h {
                                rgb.extend_from_slice(std::slice::from_raw_parts(base.add(y * stride), w * 4));
                            }
                        } else if valid {
                            rgb.reserve(w * h * 3);
                            for y in 0..h {
                                for x in 0..w {
                                    let p = base.add(y * stride + x * 4);
                                    rgb.extend_from_slice(&[*p.add(2), *p.add(1), *p]);
                                }
                            }
                        }
                        CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly);
                        ensure!(valid, "invalid decoded pixels");
                        Ok(rgb)
                    })()
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(result);
                },
            );
            ok(
                self.session.decode_frame_with_output_handler(
                    &compressed,
                    VTDecodeFrameFlags::empty(),
                    ptr::null_mut(),
                    RcBlock::as_ptr(&block) as *mut _,
                ),
                "submit HEVC decode",
            )?;
            ok(
                self.session.wait_for_asynchronous_frames(),
                "wait HEVC decode",
            )?;
            rx.recv_timeout(Duration::from_secs(5))
                .context("HEVC decoder timeout")?
                .map_err(anyhow::Error::msg)
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() }
    }
}

// The owning ScreenBuffer mutex serializes all access; VideoToolbox sessions may be used on different threads.
unsafe impl Send for Encoder {}

/// Archival video is allowed only on AC after five minutes without input.
/// Unknown power/idle state closes the gate; audio keeps its existing policy.
pub fn maintenance_allowed() -> bool {
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> *mut CFType;
        fn IOPSGetProvidingPowerSourceType(info: *const CFType) -> *const CFString;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event: u32) -> f64;
    }
    unsafe {
        let Some(info) = NonNull::new(IOPSCopyPowerSourcesInfo()) else {
            return false;
        };
        let info = CFRetained::from_raw(info);
        let source = IOPSGetProvidingPowerSourceType(&*info);
        let Some(source) = source.as_ref() else {
            return false;
        };
        let idle = CGEventSourceSecondsSinceLastEventType(0, u32::MAX);
        source.to_string() == "AC Power" && idle.is_finite() && idle >= 300.
    }
}
