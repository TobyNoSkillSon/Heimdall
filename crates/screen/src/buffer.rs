use anyhow::{ensure, Context, Result};
use heimdall_archive::{store, video, timeline::recording::{Context as ScreenContext, Detector, Observation}};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
pub type HostTimeNs = u64;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    /// Desktop coordinates, including negative origins on secondary displays.
    pub x: f64,
    pub y: f64,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug)]
pub struct Frame {
    pub display: DisplayInfo,
    pub host_time_ns: HostTimeNs,
    pub width: u32,
    pub height: u32,
    /// Packed RGB8. The history uses downscaled frames; one-shot frames stay outside it.
    pub rgb: Arc<[u8]>,
}
impl Frame {
    pub fn new(
        display: DisplayInfo,
        host_time_ns: u64,
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    ) -> Result<Self> {
        ensure!(width > 0 && height > 0, "empty frame");
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(3));
        ensure!(expected == Some(rgb.len()), "invalid RGB length");
        Ok(Self {
            display,
            host_time_ns,
            width,
            height,
            rgb: rgb.into(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ScreenConfig {
    pub fps: u32,
    /// Half the display's native pixel dimensions; other choices use an absolute cap.
    pub half_native: bool,
    pub max_width: u32,
    pub max_height: u32,
}
impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            fps: 5,
            half_native: true,
            max_width: 16384,
            max_height: 16384,
        }
    }
}
impl ScreenConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=30).contains(&self.fps) && self.max_width > 0 && self.max_height > 0,
            "invalid recording configuration"
        );
        Ok(())
    }
    pub fn size(&self, native_width: u32, native_height: u32) -> (u32, u32) {
        let scale = if self.half_native {
            0.5
        } else {
            (self.max_width as f64 / native_width as f64)
                .min(self.max_height as f64 / native_height as f64)
                .min(1.)
        };
        // The hardware encoder reports an even coded size (a requested
        // 1728x1117 was measured as 1728x1116). Ask SCK for that size too,
        // rather than silently cropping a row in the codec.
        let width = ((native_width as f64 * scale).round() as u32).max(2) & !1;
        let height = ((native_height as f64 * scale).round() as u32).max(2) & !1;
        (width, height)
    }
}
#[derive(Debug)]
pub struct FrameSet {
    pub trigger_ns: u64,
    pub frames: Vec<Frame>,
    pub missing_displays: Vec<u32>,
    pub errors: Vec<String>,
}
/// A display waiting for a busy archive tries to begin its epoch again after this long even if the writer has not
/// reached its last begin yet (a writer that dropped it would otherwise hold the display off for good).
pub const ARCHIVE_RETRY: std::time::Duration = std::time::Duration::from_secs(60);
struct Epoch {
    id: i64,
    start: u64,
    last: u64,
    end: u64,
    width: u32,
    height: u32,
    fps: u32,
    #[cfg(target_os = "macos")]
    encoder: Option<video::native::Encoder>,
}
/// Compatibility name for the command facade. It retains NO RGB history: live
/// frames are hardware HEVC in the one store; replay fixtures are lossless PNGs
/// in an isolated temporary store and never invoke native capture/permissions.
pub struct ScreenBuffer {
    config: ScreenConfig,
    guard: Option<Arc<dyn Fn(usize) -> bool + Send + Sync>>,
    session: PathBuf,
    temporary: Option<Arc<tempfile::TempDir>>,
    native: bool,
    newest: BTreeMap<u32, u64>,
    displays: BTreeMap<u32, DisplayInfo>,
    epochs: BTreeMap<u32, Epoch>,
    detectors: BTreeMap<u32, Detector>,
    capture_generation: u64,
    invalidation_generation: u64,
    /// Displays whose epoch could not begin because the archive was busy, with the begin the writer still has to
    /// reach: their frames wait (dropped, the stream kept) until it has (bug note 2026-11-02T15-58-15).
    archive_wait: BTreeMap<u32, (heimdall_archive::live::Busy, std::time::Instant)>,
    /// Keeps the store's writer running while the buffer lives (frames open a writer each; without a handle that
    /// outlives them the writer thread would start and stop with every frame).
    _recorder: Option<video::Writer>,
}
#[derive(Clone)]
pub(crate) struct FrameRef {
    pub host_time_ns: u64,
    end_ns: u64,
    session: PathBuf,
    display: DisplayInfo,
    pin_lock: Arc<heimdall_archive::compact::ReaderLock>,
    temporary: Option<Arc<tempfile::TempDir>>,
}
impl FrameRef {
    pub fn decode(&self, trigger: u64) -> Result<Frame> {
        let _keep_alive = (&self.temporary, &self.pin_lock);
        let (segment, samples) = video::at(&self.session, self.display.id, trigger)?
            .context("recorded frame unavailable")?;
        let mut rgb = Vec::new();
        if segment.format.parameters.is_empty() {
            let image = image_decode(&samples.last().context("empty recording")?.data)?;
            rgb = image;
        } else {
            #[cfg(target_os = "macos")]
            {
                let mut decoder = video::native::Decoder::new(&segment.format)?;
                for (i, sample) in samples.iter().enumerate() {
                    if i + 1 == samples.len() {
                        rgb = decoder.decode(sample)?;
                    } else {
                        decoder.advance(sample)?;
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("HEVC frame decoding requires macOS");
        }
        let sample = samples.last().context("empty recording")?;
        let period = 1_000_000_000 / segment.fps as u64;
        let timestamp = if segment.format.parameters.is_empty() {
            sample.host_ns
        } else {
            sample.host_ns
                + (trigger
                    .min(sample.end_ns.saturating_sub(1))
                    .saturating_sub(sample.host_ns)
                    / period)
                    * period
        };
        let frame = Frame::new(
            self.display.clone(),
            timestamp,
            segment.format.width,
            segment.format.height,
            rgb,
        )?;
        Ok(frame)
    }
    /// Batch history extraction decodes each GOP once. Reference frames advance
    /// the decoder without copying RGB; only requested frames own pixels.
    pub fn history(&self, from: u64, to: u64, max: usize) -> Result<Vec<Frame>> {
        let (root, name) = store::locate(&self.session)?;
        let db = store::read_index(&root)?;
        let session = store::find_session(&db, &name)?.context("recording session")?;
        let mut out = vec![];
        for seg in video::range(&root, session, from, to)? {
            if seg.geometry.display != self.display.id {
                continue;
            }
            let samples = video::samples(&root, seg.id, to)?;
            let mut requested: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
            let mut seen = std::collections::HashSet::new();
            let mut at = to;
            while at > from && seen.len() < max {
                if let Some((i, s)) = samples
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, s)| s.host_ns <= at && at < s.end_ns)
                {
                    let period = 1_000_000_000 / seg.fps as u64;
                    let stamp = if seg.format.parameters.is_empty() {
                        s.host_ns
                    } else {
                        s.host_ns + (at - s.host_ns) / period * period
                    };
                    if seen.insert(stamp) {
                        requested.entry(i).or_default().push(stamp);
                    }
                }
                at = at.saturating_sub(250_000_000);
            }
            if requested.is_empty() {
                continue;
            }
            #[cfg(target_os = "macos")]
            let mut decoder = if seg.format.parameters.is_empty() {
                None
            } else {
                Some(video::native::Decoder::new(&seg.format)?)
            };
            let last = *requested.keys().next_back().unwrap();
            for (i, s) in samples.iter().enumerate().take(last + 1) {
                let wanted = requested.get(&i);
                let rgb = if seg.format.parameters.is_empty() {
                    if wanted.is_some() {
                        image_decode(&s.data)?
                    } else {
                        continue;
                    }
                } else {
                    #[cfg(target_os = "macos")]
                    {
                        let d = decoder.as_mut().unwrap();
                        if wanted.is_some() {
                            d.decode(s)?
                        } else {
                            d.advance(s)?;
                            continue;
                        }
                    }
                    #[cfg(not(target_os = "macos"))]
                    anyhow::bail!("HEVC history requires macOS");
                };
                let mut frame = Frame::new(
                    self.display.clone(),
                    s.host_ns,
                    seg.format.width,
                    seg.format.height,
                    rgb,
                )?;
                if let Some(stamps) = wanted {
                    for &stamp in stamps {
                        frame.host_time_ns = stamp;
                        out.push(frame.clone());
                    }
                }
            }
        }
        out.sort_by_key(|f| std::cmp::Reverse(f.host_time_ns));
        out.truncate(max);
        Ok(out)
    }
}
fn image_encode(frame: &Frame) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&frame.rgb)?;
    }
    Ok(bytes)
}
fn image_decode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
    let len = reader.output_buffer_size().context("PNG output size")?;
    ensure!(len <= 128 << 20, "PNG decode bound");
    let mut out = vec![0; len];
    let info = reader.next_frame(&mut out)?;
    out.truncate(info.buffer_size());
    Ok(out)
}
impl ScreenBuffer {
    pub fn new(config: ScreenConfig) -> Result<Self> {
        let temporary = Arc::new(tempfile::tempdir()?);
        let session = temporary.path().join("session-screen-replay");
        Self::create(config, session, Some(temporary), false)
    }
    /// The placeholder while Seeing is off: no frames, no displays and no store. Built without touching the disk,
    /// so the live loop can swap it in at once (26 Oct 23:01: building `new`'s store there took 2 s of fsync).
    pub fn idle() -> Self {
        Self {
            config: ScreenConfig::default(),
            guard: None,
            session: PathBuf::new(),
            temporary: None,
            native: false,
            newest: BTreeMap::new(),
            displays: BTreeMap::new(),
            epochs: BTreeMap::new(),
            detectors: BTreeMap::new(),
            capture_generation: 0,
            invalidation_generation: 0,
            archive_wait: BTreeMap::new(),
            _recorder: None,
        }
    }
    /// True for `idle`'s placeholder, which has no store.
    pub fn is_idle(&self) -> bool {
        self.session.as_os_str().is_empty()
    }
    pub fn lab(config: ScreenConfig) -> Result<Self> {
        let temporary = Arc::new(tempfile::tempdir()?);
        let session = temporary.path().join("session-screen-lab");
        Self::create(config, session, Some(temporary), true)
    }
    pub fn recording(config: ScreenConfig, session: PathBuf) -> Result<Self> {
        Self::create(config, session, None, true)
    }
    fn create(
        config: ScreenConfig,
        session: PathBuf,
        temporary: Option<Arc<tempfile::TempDir>>,
        native: bool,
    ) -> Result<Self> {
        config.validate()?;
        let writer = video::Writer::open(&session)?;
        if native {
            writer.recover_previous()?;
        }
        Ok(Self {
            config,
            guard: None,
            session,
            temporary,
            native,
            newest: BTreeMap::new(),
            displays: BTreeMap::new(),
            epochs: BTreeMap::new(),
            detectors: BTreeMap::new(),
            capture_generation: 0,
            invalidation_generation: 0,
            archive_wait: BTreeMap::new(),
            _recorder: Some(writer),
        })
    }
    pub fn set_write_guard(&mut self, guard: Arc<dyn Fn(usize) -> bool + Send + Sync>) {
        self.guard = Some(guard);
    }
    fn close_epochs(&mut self) -> Result<()> {
        let writer = video::Writer::open(&self.session)?;
        for (_, epoch) in std::mem::take(&mut self.epochs) {
            writer.close(epoch.id)?;
        }
        self.detectors.clear();
        Ok(())
    }
    pub fn set_displays(&mut self, displays: Vec<DisplayInfo>) {
        if !self.displays.is_empty() {
            if let Ok(writer) = video::Writer::open(&self.session) {
                for id in self.displays.keys() {
                    if let Err(e) =
                        writer.gap(*id, capture::host_now_ns(), "capture generation ended")
                    {
                        eprintln!("screen gap: {e:#}");
                    }
                }
            }
        }
        // Native callers use finish() when failures must be propagated.
        if let Err(e) = self.close_epochs() {
            eprintln!("screen segment close: {e:#}");
        }
        self.capture_generation += 1;
        self.displays = displays.into_iter().map(|d| (d.id, d)).collect();
    }
    pub fn finish(&mut self, reason: &str) -> Result<()> {
        let writer = video::Writer::open(&self.session)?;
        for id in self.displays.keys() {
            writer.gap(*id, capture::host_now_ns(), reason)?;
        }
        self.close_epochs()
    }
    pub fn reconfigure(&mut self, config: ScreenConfig, _now: u64) -> Result<()> {
        config.validate()?;
        self.close_epochs()?;
        self.config = config;
        Ok(())
    }
    pub(crate) fn capture_generation(&self) -> u64 {
        self.capture_generation
    }
    pub(crate) fn is_capture_generation(&self, g: u64) -> bool {
        self.capture_generation == g
    }
    pub(crate) fn retire_capture_generation(&mut self, g: u64) -> bool {
        if !self.is_capture_generation(g) {
            return false;
        }
        self.set_displays(vec![]);
        true
    }
    pub(crate) fn publish_capture_frame(&mut self, g: u64, frame: Frame) -> Result<()> {
        if self.is_capture_generation(g) {
            self.push(frame)
        } else {
            Ok(())
        }
    }
    pub fn push(&mut self, frame: Frame) -> Result<()> {
        ensure!(
            frame.rgb.len() == frame.width as usize * frame.height as usize * 3
                && frame.rgb.len() <= 128 << 20,
            "invalid RGB frame"
        );
        if self.guard.as_ref().is_some_and(|g| !g(frame.rgb.len())) {
            self.close_epochs()?;
            anyhow::bail!("screen recording below disk floor");
        }
        let id = frame.display.id;
        let ns = frame.host_time_ns;
        let period = 1_000_000_000 / self.config.fps as u64;
        // SCK callback timestamps vary around cadence. Cover at most half a period
        // of jitter, equally for live tails, in-segment samples and rotation.
        let coverage = period + period / 2;
        if self.newest.get(&id).is_some_and(|latest| ns <= *latest) {
            return Ok(());
        }
        if !self.archive_ready(id) {
            return Ok(());
        }
        let writer = video::Writer::open(&self.session)?;
        let new = self.epochs.get(&id).is_none_or(|e| {
            ns - e.start >= video::SEGMENT_NS
                || e.width != frame.width
                || e.height != frame.height
                || e.fps != self.config.fps
        });
        if new {
            if let Some(e) = self.epochs.remove(&id) {
                writer.close_at(e.id,ns)?;
            }
        }
        #[cfg(target_os = "macos")]
        let (format, sample) = if self.native {
            if new {
                let mut encoder =
                    video::native::Encoder::new(frame.width, frame.height, self.config.fps, 0.8)?;
                let (f, s) = encoder.encode(&frame.rgb, ns, ns + coverage)?;
                let sid = match writer.begin(
                    &geometry(&frame.display),
                    &f,
                    ns,
                    heimdall_archive::wall_ns(),
                    self.config.fps,
                ) {
                    Ok(sid) => sid,
                    Err(e) => return self.archive_busy(id, e),
                };
                self.epochs.insert(
                    id,
                    Epoch {
                        id: sid,
                        start: ns,
                        last: ns,
                        end: ns,
                        width: frame.width,
                        height: frame.height,
                        fps: self.config.fps,
                        encoder: Some(encoder),
                    },
                );
                (f, s)
            } else {
                self.epochs
                    .get_mut(&id)
                    .unwrap()
                    .encoder
                    .as_mut()
                    .unwrap()
                    .encode(&frame.rgb, ns, ns + coverage)?
            }
        } else {
            synthetic(&frame, coverage)?
        };
        #[cfg(not(target_os = "macos"))]
        let (format, sample) = synthetic(&frame, coverage)?;
        if !self.epochs.contains_key(&id) {
            let sid = match writer.begin(
                &geometry(&frame.display),
                &format,
                ns,
                heimdall_archive::wall_ns(),
                self.config.fps,
            ) {
                Ok(sid) => sid,
                Err(e) => return self.archive_busy(id, e),
            };
            self.epochs.insert(
                id,
                Epoch {
                    id: sid,
                    start: ns,
                    last: ns,
                    end: ns,
                    width: frame.width,
                    height: frame.height,
                    fps: self.config.fps,
                    #[cfg(target_os = "macos")]
                    encoder: None,
                },
            );
        }
        let context = self.screen_context(id, ns);
        let mut detector = self.detectors.get(&id).cloned().unwrap_or_default();
        let mut observation = detector.observe(ns, frame.width, frame.height, &frame.rgb, context.clone())?;
        if new && observation.is_none() { observation = Some(Observation {context, change: heimdall_archive::timeline::ScreenChange::Unchanged}); }
        let epoch = self.epochs.get_mut(&id).unwrap();
        writer.append_screen(epoch.id, &sample, observation.as_ref())?;
        self.detectors.insert(id, detector);
        epoch.end = ns + coverage;
        epoch.last = ns;
        self.newest.insert(id, ns);
        self.displays.insert(id, frame.display);
        Ok(())
    }
    /// False while `id`'s epoch waits for a busy archive: its frame is dropped and the stream kept. The wait ends when
    /// the writer has reached the begin it gave up on, or after `ARCHIVE_RETRY` (one more try).
    fn archive_ready(&mut self, id: u32) -> bool {
        let Some((busy, since)) = self.archive_wait.get(&id) else { return true };
        if !busy.settled() && since.elapsed() < ARCHIVE_RETRY {
            return false;
        }
        let waited = since.elapsed();
        self.archive_wait.remove(&id);
        eprintln!("Screen: the archive is back after {:.1} s; display {id} records again", waited.as_secs_f64());
        true
    }
    /// An epoch's begin that found the archive busy is no stream failure (2 Nov 15:51: five such answers spent every
    /// restart and capture stopped for good): its display waits for the writer, the stream stays. Other errors return.
    fn archive_busy(&mut self, id: u32, e: anyhow::Error) -> Result<()> {
        let Some(busy) = heimdall_archive::live::busy(&e) else { return Err(e) };
        if self.archive_wait.is_empty() {
            eprintln!("Screen: the archive is busy; display {id} waits for it with the stream kept");
        }
        self.archive_wait.insert(id, (busy.clone(), std::time::Instant::now()));
        Ok(())
    }
    pub(crate) fn repeat_idle(&mut self, id: u32, ns: u64) -> Result<()> {
        if let Some(e) = self.epochs.get(&id) {
            if ns.saturating_sub(e.start) >= video::SEGMENT_NS {
                let prior = e.last;
                if let Some(mut frame) = self
                    .select(prior, ns)
                    .frames
                    .into_iter()
                    .find(|f| f.display.id == id)
                {
                    frame.host_time_ns = ns;
                    return self.push(frame);
                }
            }
        }
        let period = 1_000_000_000 / self.config.fps as u64;
        // SCK callback timestamps vary around cadence. Cover at most half a period
        // of jitter, equally for live tails, in-segment samples and rotation.
        let coverage = period + period / 2;
        // An SCK idle callback explicitly confirms unchanged pixels. It may extend
        // coverage; lock/sleep/error paths retire epochs before accepting callbacks.
        let context = self.screen_context(id, ns);
        let mut detector = self.detectors.get(&id).cloned();
        let observation = detector.as_mut().and_then(|d|d.idle(ns, context));
        if let Some(e) = self.epochs.get_mut(&id) {
            if ns >= e.last {
                video::Writer::open(&self.session)?.extend_screen_at(e.id, ns + coverage, observation.as_ref(),Some(ns))?;
                e.end = ns + coverage;
                e.last = ns;
                if let Some(detector) = detector { self.detectors.insert(id, detector); }
                self.newest.insert(id, ns);
            }
        }
        Ok(())
    }
    fn screen_context(&self, id:u32, ns:u64)->ScreenContext {
        #[cfg(target_os="macos")]
        if self.native && self.detectors.get(&id).is_none_or(|d|d.due(ns)) { return crate::macos::frontmost_context(); }
        self.detectors.get(&id).map(|d|d.context().clone()).unwrap_or_default()
    }
    /// Recording has permanent history; selection does not evict it.
    pub fn expire(&mut self, _now: u64) {}
    pub fn select(&mut self, trigger: u64, now: u64) -> FrameSet {
        let (_, refs) = self.pin(trigger, trigger, now);
        let mut frames = vec![];
        let mut missing_displays = vec![];
        let mut errors = vec![];
        for id in self.displays.keys() {
            match refs.get(id).and_then(|r| causal_ref(r.iter(), trigger)) {
                Some(reference) => match reference.decode(trigger) {
                    Ok(frame) => frames.push(frame),
                    Err(error) => {
                        missing_displays.push(*id);
                        errors.push(format!("display {id}: {error:#}"));
                    },
                },
                None => missing_displays.push(*id),
            }
        }
        FrameSet { trigger_ns:trigger, frames, missing_displays, errors }
    }
    pub(crate) fn pin(
        &mut self,
        from: u64,
        to: u64,
        now: u64,
    ) -> (Vec<u32>, BTreeMap<u32, Vec<FrameRef>>) {
        let mut refs = BTreeMap::new();
        let ids: Vec<_> = self.displays.keys().copied().collect();
        if from > now {
            return (ids, refs);
        }
        // Pin the archive lifetime and a time range, not decoded frames or a
        // database scan on the detecting thread. Resolve the precise phrase
        // timestamp only when it is known/approved.
        if let Ok((root, _)) = store::locate(&self.session) {
            match heimdall_archive::compact::reader_lock(&root) {
                Ok(lock) => {
                    let guard = Arc::new(lock);
                    for (&id, display) in &self.displays {
                        if self.epochs.contains_key(&id) {
                            refs.insert(
                                id,
                                vec![FrameRef {
                                    host_time_ns: from,
                                    end_ns: to.min(now).saturating_add(1),
                                    session: self.session.clone(),
                                    display: display.clone(),
                                    pin_lock: guard.clone(),
                                    temporary: self.temporary.clone(),
                                }],
                            );
                        }
                    }
                }
                Err(e) => eprintln!("screen recording pin: {e:#}"),
            }
        }
        (ids, refs)
    }
    pub fn invalidate_display(&mut self, id: u32) {
        self.invalidation_generation += 1;
        self.detectors.remove(&id);
        if let Some(e) = self.epochs.remove(&id) {
            let writer = video::Writer::open(&self.session);
            if let Ok(w) = writer {
                let _ = w.close(e.id);
                let _ = w.gap(id, capture::host_now_ns(), "display unavailable");
            }
        }
    }
    pub(crate) fn attachment_epoch(&self) -> (u64, u64) {
        (self.capture_generation, self.invalidation_generation)
    }
    pub fn history(&mut self, trigger: u64, lookback: u64, now: u64, max: usize) -> Vec<Frame> {
        let from = trigger.saturating_sub(lookback);
        let (_, refs) = self.pin(from, trigger, now);
        let mut frames: Vec<_> = refs
            .values()
            .flatten()
            .flat_map(|r| r.history(from, trigger, max).unwrap_or_default())
            .collect();
        frames.sort_by_key(|f| std::cmp::Reverse(f.host_time_ns));
        frames.truncate(max);
        frames
    }
    /// RGB retained by the recording: always zero. Commands own decoded pixels briefly.
    pub fn bytes(&self) -> usize {
        0
    }
    pub fn frame_count(&self) -> usize {
        let Ok((root, name)) = store::locate(&self.session) else {
            return 0;
        };
        let Ok(db) = store::read_index(&root) else {
            return 0;
        };
        db.query_row("SELECT count(*) FROM video_samples v JOIN video_segments s ON s.id=v.segment JOIN sessions a ON a.id=s.session WHERE a.name=?1",[name],|r|r.get::<_,i64>(0)).unwrap_or(0) as usize
    }
    pub fn display_count(&self) -> usize {
        self.displays.len()
    }
}
impl Drop for ScreenBuffer {
    fn drop(&mut self) {
        // Nothing open: no store to open just to close nothing.
        if !self.epochs.is_empty() {
            let _ = self.close_epochs();
        }
    }
}
fn geometry(d: &DisplayInfo) -> video::Geometry {
    video::Geometry {
        display: d.id,
        x: d.x,
        y: d.y,
        width: d.width,
        height: d.height,
    }
}
fn synthetic(frame: &Frame, coverage: u64) -> Result<(video::Format, video::Sample)> {
    Ok((
        video::Format {
            width: frame.width,
            height: frame.height,
            hvcc: vec![],
            parameters: vec![],
        },
        video::Sample {
            host_ns: frame.host_time_ns,
            end_ns: frame.host_time_ns + coverage,
            key: true,
            data: image_encode(frame)?,
        },
    ))
}
pub(crate) fn causal_ref<'a>(
    frames: impl DoubleEndedIterator<Item = &'a FrameRef>,
    trigger: u64,
) -> Option<&'a FrameRef> {
    frames
        .rev()
        .find(|f| f.host_time_ns <= trigger)
        .filter(|f| trigger < f.end_ns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    fn frame(id: u32, ns: u64, value: u8) -> Frame {
        Frame::new(
            DisplayInfo {
                id,
                x: 0.,
                y: 0.,
                width: 2,
                height: 1,
            },
            ns,
            2,
            1,
            vec![value; 6],
        )
        .unwrap()
    }
    /// Seeing off swaps this in on the live loop: it must not touch the disk, now or when dropped.
    #[test]
    fn the_idle_placeholder_has_no_store_and_no_frames() {
        let cwd = std::env::current_dir().unwrap();
        let before: Vec<_> = std::fs::read_dir(&cwd).unwrap().map(|e| e.unwrap().path()).collect();
        let mut idle = ScreenBuffer::idle();
        assert!(idle.is_idle() && !ScreenBuffer::new(ScreenConfig::default()).unwrap().is_idle());
        let set = idle.select(1_000, 2_000);
        assert!(set.frames.is_empty() && set.missing_displays.is_empty());
        assert!(idle.history(1_000, 1_000, 2_000, 4).is_empty());
        assert_eq!((idle.frame_count(), idle.display_count(), idle.bytes()), (0, 0, 0));
        drop(idle);
        let after: Vec<_> = std::fs::read_dir(&cwd).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(before.len(), after.len(), "the placeholder wrote beside the crate");
    }
    #[test]
    fn failed_video_write_retires_then_restarts_recording_and_screenshots() {
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        let root = store::locate(&b.session).unwrap().0;
        let db = store::open(&root).unwrap();
        let first = capture::host_now_ns();
        let display = frame(1, first, 7).display;
        b.set_displays(vec![display.clone()]);
        b.push(frame(1, first, 7)).unwrap();
        let generation = b.capture_generation();
        db.execute_batch("CREATE TRIGGER fail_video_publication BEFORE INSERT ON video_samples BEGIN SELECT RAISE(ABORT,'database is locked'); END;").unwrap();
        let error = b.publish_capture_frame(generation, frame(1, first+200_000_000, 8)).unwrap_err();
        assert!(format!("{error:#}").contains("database is locked"));
        // The native callback runs this exact retirement before asking its owner to retry.
        assert!(b.retire_capture_generation(generation));
        assert!(b.select(first+200_000_000, first+200_000_000).frames.is_empty());
        db.execute_batch("DROP TRIGGER fail_video_publication").unwrap();
        b.set_displays(vec![display]);
        let recovered = first+400_000_000;
        b.publish_capture_frame(b.capture_generation(), frame(1, recovered, 9)).unwrap();
        let selected = b.select(recovered, recovered);
        assert_eq!(&*selected.frames[0].rgb, &[9;6]);
        assert!(selected.errors.is_empty() && selected.missing_displays.is_empty());
        let shared = Arc::new(Mutex::new(b));
        let matched = crate::PhraseMatch { source:"mic".into(), phrase:"screenshot this".into(), start_ns:recovered, trigger_ns:recovered };
        let hold = crate::ScreenHold::pin(&shared, "mic", recovered, Some(recovered), recovered, None);
        let shots = tempfile::tempdir().unwrap();
        let action = crate::attach_held(hold, &matched, recovered, shots.path()).unwrap();
        assert_eq!(action.shots.len(),1);
        assert!(action.image_path.is_file());
    }

    /// Bug note 2026-11-02T15-58-15: the archive held for 110 s answered an epoch's begin "the archive is busy", the
    /// stream counted it as a failure five times and capture stopped for good. Now the stream stays, the display's
    /// frames wait without asking the archive again, and recording resumes as soon as the writer is back.
    #[test]
    fn a_busy_archive_keeps_the_stream_and_records_again_once_the_writer_is_back() {
        use std::time::{Duration, Instant};
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        let root = store::locate(&b.session).unwrap().0;
        let holder = store::open(&root).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE; INSERT INTO sessions(name,started_wall_ns) VALUES('session-holder',0);").unwrap();
        b.push(frame(1, 0, 1)).expect("a busy archive is no stream failure");
        let at = Instant::now();
        for i in 1..10 {
            b.push(frame(1, i * 200_000_000, 2)).expect("frames meanwhile are dropped, not failed");
        }
        assert!(at.elapsed() < Duration::from_millis(500), "frames meanwhile never wait for the archive: {:?}", at.elapsed());
        holder.execute_batch("COMMIT").unwrap();
        assert!(heimdall_archive::live::Queue::shared(&root).unwrap().settle(Duration::from_secs(30)));
        b.push(frame(1, 2_000_000_000, 3)).unwrap();
        assert_eq!(&*b.select(2_000_000_000, 2_000_000_000).frames[0].rgb, &[3; 6], "recording again");
        let db = store::read_index(&root).unwrap();
        let epochs: Vec<(i64, i64)> = db
            .prepare("SELECT closed, (SELECT count(*) FROM video_samples s WHERE s.segment=v.id) FROM video_segments v WHERE display=1 ORDER BY id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .map(|r| r.unwrap()).collect();
        assert_eq!(epochs, vec![(1, 0), (0, 1)], "one abandoned begin (closed, empty), not one per frame, then the recording");
    }

    #[test]
    fn direct_and_held_retrieval_keep_errors_and_recover_on_the_same_pinned_range() {
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        b.push(frame(1, 1_000_000_000, 17)).unwrap();
        let root = store::locate(&b.session).unwrap().0;
        let db = store::open(&root).unwrap();
        let sha: String = db.query_row("SELECT sha256 FROM video_samples", [], |row| row.get(0)).unwrap();
        let shared = Arc::new(Mutex::new(b));
        let hold = crate::ScreenHold::pin(&shared, "mic", 1_000_000_000, Some(1_000_000_000), 1_000_000_000, None);
        db.execute("UPDATE video_samples SET sha256='missing-for-retrieval-test'", []).unwrap();
        let selected = hold.select(1_000_000_000);
        assert!(selected.errors[0].contains("archive blob hot/v/missing-for-retrieval-test missing"));
        let matched = crate::PhraseMatch { source:"mic".into(), phrase:"screenshot this".into(), start_ns:1_000_000_000, trigger_ns:1_000_000_000 };
        let shots = tempfile::tempdir().unwrap();
        let error = crate::attach_screenshot(&mut shared.lock().unwrap(), &matched, 1_000_000_000, shots.path()).unwrap_err();
        assert!(format!("{error:#}").contains("screen frame retrieval failed: display 1: archive blob"));
        db.execute("UPDATE video_samples SET sha256=?1", [&sha]).unwrap();
        assert!(hold.select(1_000_000_000).errors.is_empty());
        let action = crate::attach_held(hold, &matched, 1_000_000_000, shots.path()).unwrap();
        assert!(action.image_path.is_file(), "the same pinned range recovers after the archive read failure");
    }

    #[test]
    fn no_rgb_history_and_five_fps_phrase_time_survives_hours() {
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        for i in 0..20 {
            let f = frame(1, i * 200_000_000, i as u8);
            let weak = Arc::downgrade(&f.rgb);
            b.push(f).unwrap();
            assert_eq!(weak.strong_count(), 0);
        }
        assert_eq!(b.bytes(), 0);
        assert_eq!(b.frame_count(), 20);
        let picked = b.select(2_550_000_000, 24 * 3600 * 1_000_000_000);
        assert_eq!(picked.frames[0].host_time_ns, 2_400_000_000);
        assert_eq!(&*picked.frames[0].rgb, &[12; 6]);
        assert!(b
            .select(4_800_000_000, 24 * 3600 * 1_000_000_000)
            .frames
            .is_empty());
    }
    #[test]
    fn generation_rejects_late_callbacks_and_off_ends_active_capture() {
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        b.set_displays(vec![frame(1, 0, 1).display]);
        let old = b.capture_generation();
        b.publish_capture_frame(old, frame(1, 0, 1)).unwrap();
        b.set_displays(vec![]);
        b.publish_capture_frame(old, frame(1, 200_000_000, 2))
            .unwrap();
        assert!(b.select(0, 1_000_000_000).frames.is_empty());
        assert_eq!(b.bytes(), 0);
        b.set_displays(vec![frame(1, 0, 1).display]);
        let new = b.capture_generation();
        assert!(!b.retire_capture_generation(old));
        b.publish_capture_frame(new, frame(1, 400_000_000, 3))
            .unwrap();
        assert_eq!(
            &*b.select(400_000_000, 1_000_000_000).frames[0].rgb,
            &[3; 6]
        );
    }
    #[test]
    fn idle_coverage_is_durable_without_duplicate_blobs_and_segments_rotate() {
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        b.push(frame(1, 0, 8)).unwrap();
        for i in 1..400 {
            b.repeat_idle(1, i * 200_000_000).unwrap();
        }
        assert_eq!(b.frame_count(), 2);
        assert_eq!(b.bytes(), 0);
        assert_eq!(
            &*b.select(79_800_000_000, 80_000_000_000).frames[0].rgb,
            &[8; 6]
        );
        let (root, name) = store::locate(&b.session).unwrap();
        let db = store::read_index(&root).unwrap();
        let session = store::find_session(&db, &name).unwrap().unwrap();
        assert_eq!(video::segments(&root, session).unwrap().len(), 2);
        b.finish("fixture stop").unwrap();
        let mut chunks = heimdall_archive::timeline::search(&db, &heimdall_archive::timeline::Search {
            kinds: vec!["screen".into()], ..Default::default()
        }).unwrap();
        chunks.sort_by_key(|c| c.start_ns);
        assert_eq!(chunks.iter().map(|c| (c.start_ns, c.end_ns)).collect::<Vec<_>>(),
            vec![(0, 45_000_000_000), (45_000_000_000, 80_100_000_000)]);
        assert!(chunks.iter().all(|c| c.links.iter().any(|l| l.relation == "video_frame")));

    }
    #[test]
    fn cadence_jitter_covers_rotation_and_live_tail_but_never_real_gaps() {
        let mut b=ScreenBuffer::new(Default::default()).unwrap();
        b.push(frame(1,0,1)).unwrap();
        b.push(frame(1,59_800_000_000,2)).unwrap();
        // The phrase precedes the next slightly late callback, even before it arrives.
        assert_eq!(&*b.select(60_005_000_000,60_005_000_000).frames[0].rgb,&[2;6]);
        b.push(frame(1,60_010_000_000,3)).unwrap();
        assert_eq!(&*b.select(60_005_000_000,60_020_000_000).frames[0].rgb,&[2;6]);
        assert_eq!(&*b.select(60_015_000_000,60_020_000_000).frames[0].rgb,&[3;6]);
        b.push(frame(1,60_220_000_000,4)).unwrap();
        assert_eq!(&*b.select(60_215_000_000,60_230_000_000).frames[0].rgb,&[3;6]);
        b.push(frame(1,61_000_000_000,5)).unwrap();
        assert!(b.select(60_800_000_000,61_000_000_000).frames.is_empty());
        video::Writer::open(&b.session).unwrap().gap(1,61_050_000_000,"display locked").unwrap();
        b.close_epochs().unwrap();
        b.push(frame(1,62_200_000_000,6)).unwrap();
        assert!(b.select(61_060_000_000,62_200_000_000).frames.is_empty());
        assert!(b.select(61_250_000_000,62_200_000_000).frames.is_empty());
    }
    #[test]
    fn disk_guard_failures_stop_publication_and_displays_have_separate_streams() {
        let mut b = ScreenBuffer::new(Default::default()).unwrap();
        b.push(frame(1, 0, 1)).unwrap();
        b.push(frame(2, 0, 2)).unwrap();
        assert_eq!(b.select(0, 200_000_000).frames.len(), 2);
        b.set_write_guard(Arc::new(|_| false));
        assert!(b.push(frame(1, 200_000_000, 3)).is_err());
        assert_eq!(b.frame_count(), 2);
    }
    #[test]
    fn reconfigure_preserves_causal_geometry_and_cadence_and_rejects_regression() {
        let mut b = ScreenBuffer::new(ScreenConfig {
            fps: 1,
            ..Default::default()
        })
        .unwrap();
        b.push(frame(1, 0, 1)).unwrap();
        b.reconfigure(
            ScreenConfig {
                fps: 5,
                half_native: false,
                max_width: 1920,
                max_height: 1080,
            },
            0,
        )
        .unwrap();
        b.push(frame(1, 1_000_000_000, 2)).unwrap();
        b.push(frame(1, 500_000_000, 3)).unwrap();
        // A regressing timestamp must not create overlapping epochs.
        assert_eq!(
            b.select(1_000_000_000, 2_000_000_000).frames[0].host_time_ns,
            1_000_000_000
        );
        assert_eq!(
            b.select(800_000_000, 2_000_000_000).frames[0].host_time_ns,
            0
        );
        assert!(b
            .reconfigure(
                ScreenConfig {
                    fps: 0,
                    ..Default::default()
                },
                0
            )
            .is_err());
    }
}
