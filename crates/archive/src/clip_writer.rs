//! The clip's native muxer: AVAssetWriter with the hardware H.264/HEVC encoder, one AAC track per archived audio
//! source (QuickTime track names), and a 3GPP timed-text subtitle track. No ffmpeg, no external process.
//!
//! Media is queued per input and handed over only while that input is ready. The subtitle track is complete before
//! the first frame (every cue, then a blank to the end) and is marked finished as soon as the writer took it: with
//! audio present, AVAssetWriter otherwise holds the other tracks back waiting for more text. Every track keeps a
//! timescale whose 32-bit sample durations cannot overflow: subtitles in milliseconds (a 5-second cue in
//! nanoseconds failed the writer with OSStatus -17771), video at 90 kHz, audio at its sample rate. The movie is written to a hidden partial file
//! beside the destination and renamed into place only after the writer completed.
use super::{Layout, Sink, VideoCodec};
use anyhow::{bail, ensure, Context, Result};
use objc2::{rc::Retained, runtime::AnyObject, AnyThread};
use objc2_av_foundation::*;
use objc2_core_audio_types::{AudioChannelLayout, AudioStreamBasicDescription};
use objc2_core_foundation::{CFRetained, CGSize};
use objc2_core_media::*;
use objc2_core_video::*;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    time::{Duration, Instant},
};

const STALL: Duration = Duration::from_secs(if cfg!(test) { 10 } else { 120 });

fn ok(status: i32, what: &str) -> Result<()> {
    ensure!(status == 0, "{what}: OSStatus {status}");
    Ok(())
}
fn ns_time(ns: u64) -> CMTime {
    unsafe { CMTime::new(ns as i64, 1_000_000_000) }
}
fn key(k: Option<&'static NSString>) -> &'static NSString {
    k.expect("AVFoundation constant")
}
fn number(n: f64) -> Retained<NSNumber> {
    NSNumber::new_f64(n)
}
fn int(n: i64) -> Retained<NSNumber> {
    NSNumber::new_i64(n)
}
fn dictionary(pairs: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|p| p.1).collect();
    NSDictionary::from_slices(&keys, &values)
}
fn file_type(path: &Path) -> Result<&'static AVFileType> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    unsafe {
        Ok(match ext.as_str() {
            "mp4" => AVFileTypeMPEG4,
            "mov" => AVFileTypeQuickTimeMovie,
            "m4v" => AVFileTypeAppleM4V,
            _ => bail!("the clip must end in .mp4, .mov or .m4v"),
        }
        .context("AVFoundation file type")?)
    }
}
/// A 3GPP `tx3g` sample entry (TS 26.245): white text, centred at the bottom on a translucent box.
fn tx3g_entry(width: u32, height: u32) -> Vec<u8> {
    let font = b"Helvetica";
    let mut e = vec![];
    e.extend([0u8; 6]); // reserved
    e.extend(1u16.to_be_bytes()); // data reference index
    e.extend(0u32.to_be_bytes()); // display flags
    e.push(1); // horizontal: centre
    e.push(0xff); // vertical: bottom
    e.extend([0, 0, 0, 0x99]); // background RGBA
    e.extend([0i16, 0, height.min(i16::MAX as u32) as i16, width.min(i16::MAX as u32) as i16].iter().flat_map(|v| v.to_be_bytes()));
    e.extend([0u16, 0, 1].iter().flat_map(|v| v.to_be_bytes())); // style: chars 0..0, font 1
    e.push(0); // face
    e.push((height / 22).clamp(12, 255) as u8); // size
    e.extend([255, 255, 255, 255]); // text RGBA
    let ftab = 8 + 2 + 2 + 1 + font.len();
    e.extend((ftab as u32).to_be_bytes());
    e.extend(b"ftab");
    e.extend(1u16.to_be_bytes());
    e.extend(1u16.to_be_bytes());
    e.push(font.len() as u8);
    e.extend(font);
    let mut out = ((e.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend(b"tx3g");
    out.extend(e);
    out
}
/// The track's name: ISO user data in MP4 (`uiso/titl`), QuickTime user data in a movie (`udta/tnam`).
unsafe fn track_name(input: &AVAssetWriterInput, title: &str) {
    let item = AVMutableMetadataItem::new();
    item.setIdentifier(AVMetadataIdentifierQuickTimeUserDataTrackName);
    item.setValue(Some(&NSString::from_str(title)));
    input.setMetadata(&NSArray::from_retained_slice(&[Retained::into_super(item)]));
}
fn block(bytes: &[u8]) -> Result<CFRetained<CMBlockBuffer>> {
    unsafe {
        let mut raw = ptr::null_mut();
        ok(CMBlockBuffer::create_with_memory_block(None, ptr::null_mut(), bytes.len().max(1), None, ptr::null(), 0, bytes.len().max(1), 0, NonNull::from(&mut raw)), "create media block")?;
        let data = CFRetained::from_raw(NonNull::new(raw).context("null media block")?);
        if !bytes.is_empty() {
            ok(CMBlockBuffer::replace_data_bytes(NonNull::new(bytes.as_ptr() as *mut _).unwrap(), &data, 0, bytes.len()), "fill media block")?;
        }
        Ok(data)
    }
}
fn sample(data: &CFRetained<CMBlockBuffer>, format: &CMFormatDescription, count: isize, timing: CMSampleTimingInfo, size: usize) -> Result<CFRetained<CMSampleBuffer>> {
    unsafe {
        let mut raw = ptr::null_mut();
        ok(CMSampleBuffer::create_ready(None, Some(data), Some(format), count, 1, &timing, 1, &size, NonNull::from(&mut raw)), "create media sample")?;
        Ok(CFRetained::from_raw(NonNull::new(raw).context("null media sample")?))
    }
}

enum Pending {
    Pixels(CFRetained<CVPixelBuffer>, CMTime),
    Sample(CFRetained<CMSampleBuffer>),
}
struct Input {
    input: Retained<AVAssetWriterInput>,
    queue: VecDeque<Pending>,
    /// No more media will be queued; the input is marked finished once its queue drains.
    closed: bool,
    finished: bool,
}
pub struct Writer {
    writer: Retained<AVAssetWriter>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    /// 0: video, then one per audio track, then the subtitles if the container took them.
    inputs: Vec<Input>,
    audio_formats: Vec<(CFRetained<CMFormatDescription>, u32, u16)>,
    /// The subtitle format and the end of the last cue so far; `None` without a subtitle track.
    text: Option<(CFRetained<CMFormatDescription>, u64)>,
    /// Cues are accepted until the first frame; then the track is closed.
    text_open: bool,
    width: u32,
    height: u32,
    duration_ns: u64,
    last_video: Option<u64>,
    partial: PathBuf,
    out: PathBuf,
    done: bool,
}
impl Writer {
    /// Creates the writer for `layout`; the destination appears only when `finish` succeeds.
    pub fn create(out: &Path, layout: &Layout) -> Result<Self> {
        let kind = file_type(out)?;
        let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        ensure!(dir.is_dir(), "the clip's folder {} does not exist", dir.display());
        let name = out.file_name().context("clip file name")?.to_string_lossy();
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let partial = dir.join(format!(".{name}.partial-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_file(&partial);
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&partial.to_string_lossy()));
            let writer = AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, kind)
                .map_err(|e| anyhow::anyhow!("AVAssetWriter: {}", e.localizedDescription()))?;
            writer.setMovieTimeScale(90_000);
            // Video: hardware H.264/HEVC, a keyframe at least every two seconds so the clip seeks well.
            let pixels = (layout.width as u64 * layout.height as u64).max(1);
            let bitrate = (pixels * layout.fps_hint.max(1) as u64 / 4).clamp(1_000_000, 20_000_000);
            let (bitrate, two, fps, no) = (int(bitrate as i64), number(2.), int(layout.fps_hint.max(1) as i64), NSNumber::new_bool(false));
            let mut props: Vec<(&NSString, &AnyObject)> = vec![
                (key(AVVideoAverageBitRateKey), &bitrate),
                (key(AVVideoMaxKeyFrameIntervalDurationKey), &two),
                (key(AVVideoExpectedSourceFrameRateKey), &fps),
                (key(AVVideoAllowFrameReorderingKey), &no),
            ];
            let profile = NSString::from_str("H264_High_AutoLevel");
            if layout.codec == VideoCodec::H264 {
                props.push((key(AVVideoProfileLevelKey), &profile));
            }
            let props = dictionary(&props);
            let codec = if layout.codec == VideoCodec::H264 { AVVideoCodecTypeH264 } else { AVVideoCodecTypeHEVC }.context("codec")?;
            let (w, h) = (int(layout.width as i64), int(layout.height as i64));
            let settings = dictionary(&[(key(AVVideoCodecKey), codec), (key(AVVideoWidthKey), &w), (key(AVVideoHeightKey), &h), (key(AVVideoCompressionPropertiesKey), &props)]);
            let video = AVAssetWriterInput::initWithMediaType_outputSettings(AVAssetWriterInput::alloc(), AVMediaTypeVideo.context("video")?, Some(&settings));
            video.setExpectsMediaDataInRealTime(false);
            video.setMediaTimeScale(90_000);
            track_name(&video, &layout.video_title);
            ensure!(writer.canAddInput(&video), "the writer refused the video track");
            writer.addInput(&video);
            // CoreVideo's kCVPixelBufferPixelFormatTypeKey / WidthKey / HeightKey.
            let (bgra, pw, ph) = (int(kCVPixelFormatType_32BGRA as i64), int(layout.width as i64), int(layout.height as i64));
            let attributes = dictionary(&[(&NSString::from_str("PixelFormatType"), &bgra), (&NSString::from_str("Width"), &pw), (&NSString::from_str("Height"), &ph)]);
            let adaptor = AVAssetWriterInputPixelBufferAdaptor::initWithAssetWriterInput_sourcePixelBufferAttributes(AVAssetWriterInputPixelBufferAdaptor::alloc(), &video, Some(&attributes));
            let mut inputs = vec![Input { input: video, queue: VecDeque::new(), closed: false, finished: false }];
            let mut audio_formats = vec![];
            for (rate, channels, title) in &layout.audio {
                let format = lpcm_format(*rate, *channels)?;
                // AVFAudio's AVFormatIDKey / AVSampleRateKey / AVNumberOfChannelsKey; the encoder picks the AAC bitrate
                // that suits the rate (a fixed 192 kb/s is refused at 16 kHz).
                let (aac, r, c) = (int(0x6161_6320), number(*rate as f64), int(*channels as i64));
                let settings = dictionary(&[
                    (&NSString::from_str("AVFormatIDKey"), &aac),
                    (&NSString::from_str("AVSampleRateKey"), &r),
                    (&NSString::from_str("AVNumberOfChannelsKey"), &c),
                ]);
                let input = AVAssetWriterInput::initWithMediaType_outputSettings_sourceFormatHint(AVAssetWriterInput::alloc(), AVMediaTypeAudio.context("audio")?, Some(&settings), Some(&format));
                input.setExpectsMediaDataInRealTime(false);
                track_name(&input, title);
                ensure!(writer.canAddInput(&input), "the writer refused audio track {title}");
                writer.addInput(&input);
                inputs.push(Input { input, queue: VecDeque::new(), closed: false, finished: false });
                audio_formats.push((format, *rate, *channels));
            }
            let mut text = None;
            if layout.subtitles {
                let entry = tx3g_entry(layout.width, layout.height);
                let mut raw = ptr::null();
                ok(
                    CMTextFormatDescriptionCreateFromBigEndianTextDescriptionData(None, NonNull::new(entry.as_ptr() as *mut u8).unwrap(), entry.len(), None, kCMMediaType_Subtitle, NonNull::from(&mut raw)),
                    "subtitle format",
                )?;
                let format: CFRetained<CMFormatDescription> = CFRetained::from_raw(NonNull::new(raw as *mut CMFormatDescription).context("null subtitle format")?);
                let input = AVAssetWriterInput::initWithMediaType_outputSettings_sourceFormatHint(AVAssetWriterInput::alloc(), AVMediaTypeSubtitle.context("subtitle")?, None, Some(&format));
                input.setExpectsMediaDataInRealTime(false);
                input.setNaturalSize(CGSize { width: layout.width as f64, height: layout.height as f64 });
                track_name(&input, "Transcript");
                if writer.canAddInput(&input) {
                    writer.addInput(&input);
                    inputs.push(Input { input, queue: VecDeque::new(), closed: false, finished: false });
                    text = Some((format, 0));
                }
            }
            ensure!(writer.startWriting(), "AVAssetWriter did not start: {}", writer.error().map(|e| e.localizedDescription().to_string()).unwrap_or_default());
            writer.startSessionAtSourceTime(ns_time(0));
            Ok(Self { writer, adaptor, inputs, audio_formats, text, text_open: true, width: layout.width, height: layout.height, duration_ns: layout.duration_ns, last_video: None, partial, out: out.to_path_buf(), done: false })
        }
    }
    /// Each input's queue length and readiness, for a stall report.
    fn queues(&self) -> String {
        let state: Vec<String> = self.inputs.iter().map(|i| format!("{}{}", i.queue.len(), if unsafe { i.input.isReadyForMoreMediaData() } { "r" } else { "w" })).collect();
        format!("queues {}", state.join(" "))
    }
    fn failure(&self) -> String {
        unsafe {
            self.writer
                .error()
                .map(|e| {
                    let under = e.userInfo().objectForKey(&NSString::from_str("NSUnderlyingError")).and_then(|u| u.downcast::<objc2_foundation::NSError>().ok());
                    let under = under.map(|u| format!("; underlying {} {}", u.domain(), u.code())).unwrap_or_default();
                    format!("{} ({} {}{under})", e.localizedDescription(), e.domain(), e.code())
                })
                .unwrap_or_else(|| "unknown writer error".into())
        }
    }
    /// Hands queued media to every ready input. Returns whether anything moved.
    fn pump(&mut self) -> Result<bool> {
        let mut moved = false;
        for i in 0..self.inputs.len() {
            loop {
                let input = &mut self.inputs[i];
                if input.queue.is_empty() || !unsafe { input.input.isReadyForMoreMediaData() } {
                    break;
                }
                let appended = match input.queue.pop_front().unwrap() {
                    Pending::Pixels(p, t) => unsafe { self.adaptor.appendPixelBuffer_withPresentationTime(&p, t) },
                    Pending::Sample(s) => unsafe { input.input.appendSampleBuffer(&s) },
                };
                ensure!(appended, "the movie writer failed: {}", self.failure());
                moved = true;
            }
            let input = &mut self.inputs[i];
            if input.closed && input.queue.is_empty() && !input.finished {
                unsafe { input.input.markAsFinished() };
                input.finished = true;
            }
        }
        Ok(moved)
    }
    /// Waits until at most `video` frames and `audio` buffers per track are still queued. When no input with queued
    /// media is ready while another open input has nothing queued, the writer is waiting for that input's next
    /// window: return after a short grace so the caller can supply it (never a deadlock on the bounds).
    fn drain(&mut self, video: usize, audio: usize) -> Result<()> {
        let mut since = Instant::now();
        loop {
            if self.pump()? {
                since = Instant::now();
            }
            let text = self.text.is_some() as usize;
            let audio_end = self.inputs.len() - text;
            let waiting = self.inputs[0].queue.len() > video || self.inputs[1..audio_end].iter().any(|i| i.queue.len() > audio);
            if !waiting {
                return Ok(());
            }
            let starved = self.inputs.iter().any(|i| !i.closed && i.queue.is_empty());
            if !self.closing() && starved && since.elapsed() > Duration::from_millis(250) {
                return Ok(());
            }
            ensure!(unsafe { self.writer.status() } == AVAssetWriterStatus::Writing, "the movie writer stopped: {}", self.failure());
            ensure!(since.elapsed() < STALL, "the movie writer stalled: {}", self.queues());
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn closing(&self) -> bool {
        self.inputs.iter().all(|i| i.closed)
    }
    /// Completes the movie and moves it into place. Returns whether the subtitle track was written.
    pub fn finish(mut self) -> Result<bool> {
        self.close_text()?;
        for input in &mut self.inputs {
            input.closed = true;
        }
        self.drain(0, 0)?;
        let mut since = Instant::now();
        while self.inputs.iter().any(|i| !i.finished) {
            if self.pump()? {
                since = Instant::now();
            }
            ensure!(since.elapsed() < STALL, "the movie writer stalled finishing: {}", self.queues());
            std::thread::sleep(Duration::from_millis(1));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        unsafe {
            self.writer.endSessionAtSourceTime(ns_time(self.duration_ns));
            let done = block2::RcBlock::new(move || {
                let _ = tx.send(());
            });
            self.writer.finishWritingWithCompletionHandler(&done);
        }
        rx.recv_timeout(STALL).context("the movie writer did not finish")?;
        ensure!(unsafe { self.writer.status() } == AVAssetWriterStatus::Completed, "the movie writer failed: {}", self.failure());
        std::fs::rename(&self.partial, &self.out)?;
        self.done = true;
        Ok(self.text.is_some())
    }
    /// Abandons the movie and removes the partial file.
    pub fn cancel(mut self) {
        unsafe { self.writer.cancelWriting() };
        let _ = std::fs::remove_file(&self.partial);
        self.done = true;
    }
    fn text_sample(&mut self, start: u64, end: u64, text: &str) -> Result<()> {
        let (format, at) = self.text.as_mut().context("no subtitle track")?;
        ensure!(start >= *at && end > start, "subtitle cues must be in order");
        let mut bytes = (text.len().min(u16::MAX as usize) as u16).to_be_bytes().to_vec();
        bytes.extend(&text.as_bytes()[..text.len().min(u16::MAX as usize)]);
        // Whole milliseconds on both edges, so consecutive samples abut exactly.
        let (a, b) = ((start / 1_000_000) as i64, (end / 1_000_000) as i64);
        *at = end;
        if b <= a {
            return Ok(());
        }
        let timing = unsafe { CMSampleTimingInfo { duration: CMTime::new(b - a, 1000), presentationTimeStamp: CMTime::new(a, 1000), decodeTimeStamp: kCMTimeInvalid } };
        let s = sample(&block(&bytes)?, format, 1, timing, bytes.len())?;
        let i = self.inputs.len() - 1;
        self.inputs[i].queue.push_back(Pending::Sample(s));
        Ok(())
    }
    /// Ends the subtitle track with a blank to the clip's end; it is marked finished once the writer took it.
    fn close_text(&mut self) -> Result<()> {
        if !std::mem::replace(&mut self.text_open, false) {
            return Ok(());
        }
        let Some((_, at)) = self.text else { return Ok(()) };
        if at < self.duration_ns {
            self.text_sample(at, self.duration_ns, "")?;
        }
        let i = self.inputs.len() - 1;
        self.inputs[i].closed = true;
        Ok(())
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if !self.done {
            unsafe { self.writer.cancelWriting() };
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}
fn lpcm_format(rate: u32, channels: u16) -> Result<CFRetained<CMFormatDescription>> {
    let bytes = 4 * channels as u32;
    let asbd = AudioStreamBasicDescription {
        mSampleRate: rate as f64,
        mFormatID: 0x6c70_636d, // 'lpcm'
        mFormatFlags: 1 | 8,    // float, packed
        mBytesPerPacket: bytes,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes,
        mChannelsPerFrame: channels as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut layout: AudioChannelLayout = unsafe { std::mem::zeroed() };
    layout.mChannelLayoutTag = if channels == 1 { (100 << 16) | 1 } else { (101 << 16) | 2 }; // Mono / Stereo
    unsafe {
        let mut raw = ptr::null();
        ok(
            CMAudioFormatDescriptionCreate(None, NonNull::from(&asbd), std::mem::size_of::<AudioChannelLayout>(), &layout, 0, ptr::null(), None, NonNull::from(&mut raw)),
            "audio format",
        )?;
        Ok(CFRetained::from_raw(NonNull::new(raw as *mut CMFormatDescription).context("null audio format")?))
    }
}
impl Sink for Writer {
    fn video(&mut self, t_ns: u64, bgra: Option<&[u8]>) -> Result<()> {
        self.close_text()?;
        ensure!(self.last_video.is_none_or(|l| t_ns > l), "video frames must move forward");
        self.last_video = Some(t_ns);
        let (w, h) = (self.width as usize, self.height as usize);
        unsafe {
            let pool = match self.adaptor.pixelBufferPool() { Some(p) => p, None => bail!("the writer has no pixel buffer pool (status {:?}: {})", self.writer.status(), self.failure()) };
            let mut raw = ptr::null_mut();
            ok(CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)), "frame buffer")?;
            let pixels: CFRetained<CVPixelBuffer> = CFRetained::from_raw(NonNull::new(raw).context("null frame buffer")?);
            ok(CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::empty()), "lock frame")?;
            let base = CVPixelBufferGetBaseAddress(&pixels) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(&pixels);
            let fits = !base.is_null() && stride >= w * 4 && CVPixelBufferGetWidth(&pixels) == w && CVPixelBufferGetHeight(&pixels) == h;
            if fits {
                for y in 0..h {
                    let row = base.add(y * stride);
                    match bgra {
                        Some(src) if src.len() >= w * h * 4 => ptr::copy_nonoverlapping(src.as_ptr().add(y * w * 4), row, w * 4),
                        _ => {
                            for x in 0..w {
                                ptr::copy_nonoverlapping([0u8, 0, 0, 255].as_ptr(), row.add(x * 4), 4);
                            }
                        }
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::empty());
            ensure!(fits, "frame buffer geometry");
            self.inputs[0].queue.push_back(Pending::Pixels(pixels, ns_time(t_ns)));
        }
        // Frames go to the writer as they are made (a 60 fps window at 4096 px would otherwise hold gigabytes).
        self.pump()?;
        if self.inputs[0].queue.len() > 12 {
            self.drain(12, usize::MAX)?;
        }
        Ok(())
    }
    fn audio(&mut self, track: usize, frame: u64, pcm: &[f32]) -> Result<()> {
        let (format, rate, channels) = self.audio_formats.get(track).context("audio track")?;
        let frames = pcm.len() / *channels as usize;
        if frames == 0 {
            return Ok(());
        }
        let bytes = crate::audio::pcm_bytes(pcm);
        let timing = CMSampleTimingInfo { duration: unsafe { CMTime::new(1, *rate as i32) }, presentationTimeStamp: unsafe { CMTime::new(frame as i64, *rate as i32) }, decodeTimeStamp: unsafe { kCMTimeInvalid } };
        let s = sample(&block(&bytes)?, format, frames as isize, timing, 4 * *channels as usize)?;
        self.inputs[1 + track].queue.push_back(Pending::Sample(s));
        Ok(())
    }
    fn text(&mut self, start_ns: u64, end_ns: u64, text: &str) -> Result<()> {
        ensure!(self.text_open, "subtitle cues come before the first frame");
        let Some((_, at)) = self.text else { return Ok(()) };
        if start_ns > at {
            self.text_sample(at, start_ns, "")?;
        }
        let (start, end) = (start_ns.max(at), end_ns.min(self.duration_ns));
        if end > start {
            self.text_sample(start, end, text)?;
        }
        Ok(())
    }
    fn window(&mut self, _end_ns: u64) -> Result<()> {
        // About two windows of frames: at 1728 x 1116 each queued BGRA frame holds 7.7 MB.
        self.drain(12, 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_video_audio_and_subtitles_into_one_movie() {
        let dir = tempfile::tempdir().unwrap();
        for ext in ["mp4", "mov"] {
            let out = dir.path().join(format!("clip.{ext}"));
            let layout = Layout { width: 1728, height: 1116, codec: VideoCodec::H264, fps_hint: 5, video_title: "Screen".into(), audio: vec![(48_000, 1, "Mic: test".into()), (16_000, 2, "App: test".into())], subtitles: true, duration_ns: 2_000_000_000 };
            let mut w = Writer::create(&out, &layout).unwrap();
            w.text(100_000_000, 900_000_000, "User: hello").unwrap();
            w.text(1_200_000_000, 3_000_000_000, "Vim: hi").unwrap();
            let frame = [40u8, 80, 120, 255].repeat(1728 * 1116);
            std::thread::sleep(Duration::from_millis(500));
            for (i, t) in [0u64, 200_000_000, 1_000_000_000].into_iter().enumerate() {
                w.video(t, (i != 1).then_some(&frame[..])).unwrap();
            }
            w.audio(0, 0, &vec![0.1; 48_000]).unwrap();
            w.audio(1, 0, &vec![0.1; 32_000]).unwrap();
            w.window(1_000_000_000).unwrap();
            w.audio(0, 48_000, &vec![0.1; 48_000]).unwrap();
            w.audio(1, 16_000, &vec![0.1; 32_000]).unwrap();
            w.window(2_000_000_000).unwrap();
            assert!(w.finish().unwrap(), "{ext}: the subtitle track is written");
            assert!(std::fs::metadata(&out).unwrap().len() > 1000);
            assert!(std::fs::read_dir(dir.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("partial")));
        }
    }
}
#[cfg(test)]
mod long_subtitles {
    use super::*;
    fn run(cues: &[(u64, u64, &str)], name: &str) -> Result<()> { run_with(cues, name, true) }
    fn run_with(cues: &[(u64, u64, &str)], name: &str, with_audio: bool) -> Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(name);
        let layout = Layout { width: 320, height: 208, codec: VideoCodec::H264, fps_hint: 5, video_title: "Screen".into(), audio: if with_audio { vec![(48_000, 2, "Mic".into())] } else { vec![] }, subtitles: true, duration_ns: 20_000_000_000 };
        let mut w = Writer::create(&out, &layout)?;
        for (a, b, t) in cues {
            w.text(*a, *b, t)?;
        }
        let frame = [40u8, 80, 120, 255].repeat(320 * 208);
        for s in 0..20u64 {
            for k in 0..5 {
                w.video(s * 1_000_000_000 + k * 200_000_000, Some(&frame))?;
            }
            if with_audio {
                w.audio(0, s * 48_000, &vec![0.01; 96_000])?;
            }
            w.window((s + 1) * 1_000_000_000)?;
        }
        w.finish()?;
        Ok(())
    }
    #[test]
    fn twenty_seconds_of_subtitles_beside_audio_and_video_never_stall() {
        let s = 1_000_000_000u64;
        let live = [(0, 1_697_359_876, "ego lite: Remember.\nOwner: like very well executed music"), (1_697_359_876, 5_100_549_209, "User: like very well executed music"), (5_100_549_209, 10_593_749_209, "User: videos or like just in"), (10_593_749_209, 16_086_949_209, "User: general videos. You know it's"), (16_086_949_209, 20 * s, "User: just that this one is")];
        run_with(&live, "n.mp4", false).unwrap();
        run(&live, "a.mp4").unwrap();
        run(&[(s, 3 * s, "User: a"), (4 * s, 20 * s, "User: b")], "d.mov").unwrap();
    }
}
