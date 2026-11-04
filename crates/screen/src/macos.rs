use crate::{DisplayInfo, Frame, ScreenBuffer, ScreenConfig};
use anyhow::{anyhow, ensure, Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AnyThread, DefinedClass,
};
use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification, NSWorkspaceScreensDidWakeNotification, NSWorkspaceWillSleepNotification};
use objc2_core_graphics::*;
use objc2_core_media::{CMClock, CMSampleBuffer, CMSyncConvertTime, CMTime, CMTimeFlags};
use objc2_core_video::*;
use objc2_foundation::{NSArray, NSError, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSDistributedNotificationCenter, NSString};
use objc2_screen_capture_kit::*;
use std::{
    collections::BTreeMap,
    ffi::c_void,
    ptr::NonNull,
    sync::{mpsc, Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

static SESSION_LOCKED:AtomicBool=AtomicBool::new(false);

pub fn screen_permission_granted() -> bool {
    CGPreflightScreenCaptureAccess()
}
#[derive(Debug)]
pub enum ScreenEvent {
    Configured { error: Option<String> },
    Started { displays: usize },
    DisplaysChanged { displays: usize },
    Error(String),
    Recovering { attempt: u32, delay_seconds: u64 },
    Unavailable,
    Paused { reason: String },
}
/// Receive health separately from frames; no callback writes pixels to disk.
pub struct ScreenCapture {
    pub buffer: Arc<Mutex<ScreenBuffer>>,
    pub events: crossbeam_channel::Receiver<ScreenEvent>,
    control: mpsc::Sender<Control>,
    owner: Option<JoinHandle<()>>,
}
#[derive(Clone)]
enum Control {
    Configure(ScreenConfig),
    Refresh,
    Failed,
    Stop,
}
impl ScreenCapture {
    pub fn start(config: ScreenConfig) -> Result<Self> {
        let buffer = ScreenBuffer::lab(config.clone())?;
        Self::start_with(config, buffer)
    }
    pub fn start_recording(config: ScreenConfig, session: std::path::PathBuf, guard: Arc<dyn Fn(usize)->bool+Send+Sync>) -> Result<Self> {
        let mut buffer = ScreenBuffer::recording(config.clone(), session)?;
        buffer.set_write_guard(guard);
        Self::start_with(config, buffer)
    }
    fn start_with(mut config: ScreenConfig, buffer: ScreenBuffer) -> Result<Self> {
        config.validate()?;
        // This precedes every SCK content/stream call. No request/picker API exists here.
        ensure!(
            screen_permission_granted(),
            "Screen Recording is not granted; capture refused without prompting"
        );
        let buffer = Arc::new(Mutex::new(buffer));
        let (tx, rx) = mpsc::channel();
        let (event_tx, events) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let data = buffer.clone();
        let display_tx = tx.clone();
        let owner = thread::Builder::new()
            .name("heimdall-screen".into())
            .spawn(move || {
                objc2::rc::autoreleasepool(|_| {
                    let mut run = || -> Result<()> {
                        let _wake = WakeObserver::new(display_tx.clone(), data.clone());
                        let callback_tx = Box::new(display_tx.clone());
                        let context = &*callback_tx as *const mpsc::Sender<Control> as *mut c_void;
                        let result = unsafe {
                            CGDisplayRegisterReconfigurationCallback(Some(display_changed), context)
                        };
                        ensure!(
                            result == CGError::Success,
                            "display observer registration failed: {result:?}"
                        );
                        // RAII unregisters before callback context storage is freed.
                        let _observer = DisplayObserver {
                            context,
                            _sender: callback_tx,
                        };
                        let mut attempts = 0u32;
                        let mut retry: Option<Instant> = None;
                        let mut session = if capture_available() {
                            match objc2::rc::autoreleasepool(|_| Session::start(&config, data.clone(), event_tx.clone(), display_tx.clone())) {
                                Ok(s) => Some(s),
                                Err(e) => {
                                    attempts = 1;
                                    retry = Some(Instant::now() + Duration::from_secs(1));
                                    let _ = event_tx.send(ScreenEvent::Error(format!("screen initial stream start: {e:#}")));
                                    let _ = event_tx.send(ScreenEvent::Recovering { attempt: 1, delay_seconds: 1 });
                                    None
                                },
                            }
                        } else { None };
                        let mut healthy_since = Instant::now();
                        let mut paused = capture_pause_reason();
                        let _ = ready_tx.send(Ok(()));
                        if retry.is_none() {
                            let _ = event_tx.send(ScreenEvent::Started {
                                displays: session.as_ref().map_or(0, |s| s.streams.len()),
                            });
                        }
                        if let Some(reason) = paused {
                            let _ = event_tx.send(ScreenEvent::Paused { reason: reason.into() });
                        }
                        loop {
                            // Healthy capture expires history on frame delivery/selection.
                            // Only recovery has a timer; exhausted retries block on OS events.
                            let control = match retry {
                                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                                None => rx.recv_timeout(Duration::from_secs(30)),
                            };
                            match control {
                                Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Ok(Control::Configure(next)) => {
                                    let result = if let Some(s) = &session { s.configure(&next) } else { Ok(()) };
                                    match result {
                                        Ok(()) => {
                                            data.lock().unwrap().reconfigure(next.clone(), capture::host_now_ns())?;
                                            config = next;
                                            let _ = event_tx.send(ScreenEvent::Configured { error: None });
                                        },
                                        Err(e) => {
                                            // Restore any displays already changed before an error.
                                            if let Some(s) = &session { let _ = s.configure(&config); }
                                            let _ = event_tx.send(ScreenEvent::Configured { error: Some(format!("{e:#}")) });
                                        },
                                    }
                                    continue;
                                },
                                Ok(command) => {
                                    // Invalidate before recreation; never use a pre-wake frame.
                                    if matches!(command, Control::Refresh) || (session.is_some() && healthy_since.elapsed() >= Duration::from_secs(30)) { attempts = 0; }
                                    if matches!(command, Control::Refresh) || session.is_some() {
                                        data.lock().unwrap().set_displays(vec![]);
                                        objc2::rc::autoreleasepool(|_| drop(session.take()));
                                        if attempts < 5 {
                                            let delay = if attempts == 0 { Duration::from_millis(250) } else { Duration::from_secs(1u64 << (attempts - 1)) };
                                            let _ = event_tx.send(ScreenEvent::Recovering { attempt: attempts + 1, delay_seconds: delay.as_secs() });
                                            retry = Some(Instant::now() + delay);
                                        } else { retry = None; let _ = event_tx.send(ScreenEvent::Unavailable); }
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                            data.lock().unwrap().expire(capture::host_now_ns());
                            if let Some(reason) = capture_pause_reason() {
                                if paused != Some(reason) {
                                    let _ = event_tx.send(ScreenEvent::Paused { reason: reason.into() });
                                }
                                paused = Some(reason);
                                data.lock().unwrap().finish(reason)?;
                                data.lock().unwrap().set_displays(vec![]);
                                drop(session.take()); retry=None; attempts=0; continue;
                            }
                            paused = None;
                            // Exhausted retries wait for an OS refresh, not the 30-second maintenance tick.
                            if recovery_allowed(session.is_some(), retry.is_some(), attempts) { retry=Some(Instant::now()); }
                            if retry.is_some_and(|at| Instant::now() >= at) {
                                retry = None;
                                attempts += 1;
                                match objc2::rc::autoreleasepool(|_| {
                                    Session::start(&config, data.clone(), event_tx.clone(), display_tx.clone())
                                }) {
                                    Ok(next) => {
                                        let _ = event_tx.send(ScreenEvent::DisplaysChanged { displays: next.streams.len() });
                                        session = Some(next);
                                        healthy_since = Instant::now();
                                        // Consecutive immediate stop callbacks count toward the cap.
                                    }
                                    Err(e) => { data.lock().unwrap().set_displays(vec![]); let _ = event_tx.send(ScreenEvent::Error(e.to_string())); }
                                }
                                if session.is_none() {
                                    if attempts < 5 {
                                        let delay = 1u64 << (attempts - 1);
                                        let _ = event_tx.send(ScreenEvent::Recovering { attempt: attempts, delay_seconds: delay });
                                        retry = Some(Instant::now() + Duration::from_secs(delay));
                                    } else { let _ = event_tx.send(ScreenEvent::Unavailable); }
                                }
                            }
                        }
                        Ok(())
                    };
                    let result = run();
                    let result=result.and_then(|_|data.lock().unwrap().finish("screen capture stopped"));
                    data.lock().unwrap().set_displays(vec![]);
                    if let Err(e) = result {
                        let message = e.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        let _ = event_tx.send(ScreenEvent::Error(message));
                    }
                });
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(Self {
                buffer,
                events,
                control: tx,
                owner: Some(owner),
            }),
            result => {
                let _ = tx.send(Control::Stop);
                let _ = owner.join();
                Err(anyhow!("screen start failed: {result:?}"))
            }
        }
    }
    /// Call on wake; actual display changes are observed automatically.
    pub fn refresh(&self) {
        let _ = self.control.send(Control::Refresh);
    }
    pub fn reconfigure(&self, config: ScreenConfig) -> Result<()> {
        config.validate()?;
        self.control.send(Control::Configure(config)).context("screen owner stopped")
    }
}
impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}
struct WakeObserver {
    distributed: Retained<NSDistributedNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}
impl WakeObserver {
    fn new(tx: mpsc::Sender<Control>, buffer: Arc<Mutex<ScreenBuffer>>) -> Self {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let mut tokens=vec![];
        for name in unsafe {[NSWorkspaceScreensDidSleepNotification,NSWorkspaceScreensDidWakeNotification,NSWorkspaceWillSleepNotification]} {
            let (tx,buffer)=(tx.clone(),buffer.clone());
            let block=RcBlock::new(move |_:NonNull<NSNotification>| {buffer.lock().unwrap().set_displays(vec![]);let _=tx.send(Control::Refresh);});
            tokens.push(unsafe {center.addObserverForName_object_queue_usingBlock(Some(name),None,None,&block)});
        }
        let distributed=NSDistributedNotificationCenter::defaultCenter();
        for name in ["com.apple.screenIsLocked","com.apple.screenIsUnlocked","com.apple.sessionDidBecomeActive","com.apple.sessionDidResignActive"] {
            let (tx,buffer)=(tx.clone(),buffer.clone());
            let block=RcBlock::new(move |_:NonNull<NSNotification>| {
                if name=="com.apple.screenIsLocked" {SESSION_LOCKED.store(true,Ordering::Release);}
                if name=="com.apple.screenIsUnlocked" {SESSION_LOCKED.store(false,Ordering::Release);}
                buffer.lock().unwrap().set_displays(vec![]);let _=tx.send(Control::Refresh);});
            tokens.push(unsafe {distributed.addObserverForName_object_queue_usingBlock(Some(&NSString::from_str(name)),None,None,&block)});
        }
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            buffer.lock().unwrap().set_displays(vec![]);
            let _ = tx.send(Control::Refresh);
        });
        let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(NSWorkspaceDidWakeNotification), None, None, &block) };
        Self { center, token, distributed, tokens }
    }
}
impl Drop for WakeObserver {
    fn drop(&mut self) { unsafe { for token in &self.tokens {self.center.removeObserver(&*(Retained::as_ptr(token) as *const AnyObject));self.distributed.removeObserver(&*(Retained::as_ptr(token) as *const AnyObject));} self.center.removeObserver(&*(Retained::as_ptr(&self.token) as *const AnyObject)); } }
}
struct DisplayObserver {
    context: *mut c_void,
    _sender: Box<mpsc::Sender<Control>>,
}
impl Drop for DisplayObserver {
    fn drop(&mut self) {
        unsafe {
            CGDisplayRemoveReconfigurationCallback(Some(display_changed), self.context);
        }
    }
}
unsafe extern "C-unwind" fn display_changed(
    _: u32,
    flags: CGDisplayChangeSummaryFlags,
    context: *mut c_void,
) {
    if !flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        let tx = &*(context as *const mpsc::Sender<Control>);
        let _ = tx.send(Control::Refresh);
    }
}
// SCShareableContent/SCDisplay are immutable snapshots documented for async
// completion use. This wrapper moves sole ownership from the framework's
// completion queue to the screen owner; no mutable ObjC access is shared.
struct ContentTransfer(Retained<SCShareableContent>);
unsafe impl Send for ContentTransfer {}
fn content() -> Result<Retained<SCShareableContent>> {
    ensure!(
        screen_permission_granted(),
        // Not granted: never asked, denied, or revoked; the check cannot tell which (VM finding P-10, 28 Oct).
        "Screen Recording is not allowed for Heimdall; capture refused"
    );
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = unsafe {
                if let Some(e) = error.as_ref() {
                    Err(e.to_string())
                } else {
                    Retained::retain(content)
                        .map(ContentTransfer)
                        .ok_or_else(|| "empty shareable content".into())
                }
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false, false, &block);
    }
    Ok(rx
        .recv_timeout(Duration::from_secs(10))
        .context("display enumeration timeout")?
        .map_err(|s| anyhow!(s))?
        .0)
}
/// Heimdall's island, drawing overlay and pointer (see `EXCLUDED_WINDOW_TITLES`); each window exists from UI launch, ordered out,
/// and the content query includes off-screen windows, so it is found even while hidden.
fn excluded_windows(content: &SCShareableContent) -> Retained<NSArray<SCWindow>> {
    let own = std::process::id() as i32;
    let windows: Vec<Retained<SCWindow>> = unsafe { content.windows() }
        .iter()
        .filter(|w| unsafe {
            let owner = w.owningApplication().map(|a| a.processID());
            let title = w.title().map(|t| t.to_string());
            crate::is_excluded_window(owner, own, title.as_deref())
        })
        .collect();
    NSArray::from_retained_slice(&windows)
}
struct Session {
    active: Arc<AtomicBool>,
    generation: u64,
    buffer: Arc<Mutex<ScreenBuffer>>,
    streams: BTreeMap<u32, StreamOwner>,
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut buffer = self.buffer.lock().unwrap();
        self.active.store(false, Ordering::Release);
        buffer.retire_capture_generation(self.generation);
    }
}
struct StreamOwner {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
}
impl Drop for StreamOwner {
    fn drop(&mut self) {
        unsafe {
            let (tx, rx) = mpsc::sync_channel(1);
            let block = RcBlock::new(move |_: *mut NSError| {
                let _ = tx.send(());
            });
            self.stream.stopCaptureWithCompletionHandler(Some(&block));
            let _ = rx.recv_timeout(Duration::from_secs(5));
            let _ = self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output),
                SCStreamOutputType::Screen,
            );
        }
    }
}
impl Session {
    fn configure(&self, config: &ScreenConfig) -> Result<()> {
        for owner in self.streams.values() {
            let info = &owner.output.ivars().display;
            let (width,height)=config.size(info.width,info.height);
            unsafe {
                let c = SCStreamConfiguration::new();
                c.setWidth(width as usize);
                c.setHeight(height as usize);
                c.setMinimumFrameInterval(CMTime::new(1, config.fps as i32));
                c.setQueueDepth(3); c.setCapturesAudio(false); c.setShowsCursor(true);
                c.setPixelFormat(kCVPixelFormatType_32BGRA);
                let (tx, rx) = mpsc::sync_channel(1);
                let block = RcBlock::new(move |error: *mut NSError| {
                    let _ = tx.send(error.as_ref().map_or(Ok(()), |e| Err(e.to_string())));
                });
                owner.stream.updateConfiguration_completionHandler(&c, Some(&block));
                rx.recv_timeout(Duration::from_secs(5)).context("screen configuration timeout")?.map_err(|s| anyhow!(s))?;
            }
        }
        Ok(())
    }
    fn start(
        config: &ScreenConfig,
        buffer: Arc<Mutex<ScreenBuffer>>,
        events: crossbeam_channel::Sender<ScreenEvent>,
        control: mpsc::Sender<Control>,
    ) -> Result<Self> {
        ensure!(capture_available(), "screen recording paused: asleep, dark or locked");
        let content = content()?;
        let displays = unsafe { content.displays() };
        let mut infos = vec![];
        for d in displays.iter() {
            let rect = unsafe { d.frame() };
            infos.push(DisplayInfo {
                id: unsafe { d.displayID() },
                x: rect.origin.x,
                y: rect.origin.y,
                width: native_display_size(unsafe { d.displayID() })?.0,
                height: native_display_size(unsafe { d.displayID() })?.1,
            });
        }
        ensure!(!infos.is_empty(), "no displays available");
        let generation = {
            let mut buffer = buffer.lock().unwrap();
            buffer.set_displays(infos.clone());
            buffer.capture_generation()
        };
        let mut session = Self {
            active: Arc::new(AtomicBool::new(true)),
            generation,
            buffer: buffer.clone(),
            streams: BTreeMap::new(),
        };
        for (display, info) in displays.iter().zip(infos) {
            unsafe {
                let filter = SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &excluded_windows(&content),
                );
                let (width,height)=config.size(info.width,info.height);
                let configuration = SCStreamConfiguration::new();
                configuration.setWidth(width as usize);
                configuration.setHeight(height as usize);
                configuration.setMinimumFrameInterval(CMTime::new(1, config.fps as i32));
                configuration.setQueueDepth(3);
                configuration.setCapturesAudio(false);
                configuration.setShowsCursor(true);
                configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
                let output = Output::new(OutputState {
                    display: info.clone(),
                    buffer: buffer.clone(),
                    events: events.clone(),
                    control: control.clone(),
                    active: session.active.clone(),
                    generation,
                    gaps: Mutex::default(),
                });
                let stream = SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &configuration,
                    Some(ProtocolObject::from_ref(&*output)),
                );
                let queue = DispatchQueue::new(&format!("dev.heimdall.screen.{}", info.id), None);
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*output),
                        SCStreamOutputType::Screen,
                        Some(&queue),
                    )
                    .map_err(|e| anyhow!(e.to_string()))?;
                let owner = StreamOwner {
                    stream,
                    output,
                    _queue: queue,
                };
                let (tx, rx) = mpsc::sync_channel(1);
                let block = RcBlock::new(move |error: *mut NSError| {
                    let result = error.as_ref().map_or(Ok(()), |e| Err(e.to_string()));
                    let _ = tx.send(result);
                });
                owner.stream.startCaptureWithCompletionHandler(Some(&block));
                rx.recv_timeout(Duration::from_secs(10))
                    .context("stream start timeout")?
                    .map_err(|s| anyhow!(s))?;
                session.streams.insert(info.id, owner);
            }
        }
        Ok(session)
    }
}
struct OutputState {
    display: DisplayInfo,
    buffer: Arc<Mutex<ScreenBuffer>>,
    events: crossbeam_channel::Sender<ScreenEvent>,
    control: mpsc::Sender<Control>,
    active: Arc<AtomicBool>,
    generation: u64,
    /// Archive gaps over 1 s, logged with what macOS sent meanwhile (diagnosis only).
    gaps: Mutex<crate::gaps::GapLog>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HeimdallScreenOutput"]
    #[ivars = OutputState]
    struct Output;
    unsafe impl NSObjectProtocol for Output {}
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn sample(
            &self,
            stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen || !self.ivars().active.load(Ordering::Acquire) {
                return;
            }
            objc2::rc::autoreleasepool(|_| {
                if let Err(e) = self.accept(stream, sample) {
                    let mut buffer = self.ivars().buffer.lock().unwrap();
                    if !buffer.retire_capture_generation(self.ivars().generation) { return; }
                    let _ = self.ivars().events.send(ScreenEvent::Error(e.to_string()));
                    let _ = self.ivars().control.send(Control::Failed);
                }
            });
        }
    }
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stopped(&self, _: &SCStream, error: &NSError) {
            if !self.ivars().active.load(Ordering::Acquire) { return; }
            let mut buffer = self.ivars().buffer.lock().unwrap();
            if !buffer.retire_capture_generation(self.ivars().generation) { return; }
            let _ = self.ivars().control.send(Control::Failed);
            let _ = self.ivars().events.send(ScreenEvent::Error(format!(
                "display {} stopped: {error}",
                self.ivars().display.id
            )));
        }
    }
);
impl Output {
    fn new(state: OutputState) -> Retained<Self> {
        unsafe { msg_send![super(Self::alloc().set_ivars(state)), init] }
    }
    unsafe fn accept(&self, stream: &SCStream, sample: &CMSampleBuffer) -> Result<()> {
        let mut seen = crate::gaps::Seen::Stale;
        let out = self.take(stream, sample, &mut seen);
        let seen = if out.is_err() { crate::gaps::Seen::Failed } else { seen };
        let line = self.ivars().gaps.lock().unwrap().observe(self.ivars().display.id, capture::host_now_ns(), seen);
        if let Some(line) = line { eprintln!("{line}") }
        out
    }
    /// One callback; `seen` says how it was handled.
    unsafe fn take(&self, stream: &SCStream, sample: &CMSampleBuffer, seen: &mut crate::gaps::Seen) -> Result<()> {
        use crate::gaps::Seen;
        if !display_allowed(self.ivars().display.id) {
            *seen = Seen::Refused;
            let mut buffer=self.ivars().buffer.lock().unwrap();
            if buffer.is_capture_generation(self.ivars().generation) { buffer.invalidate_display(self.ivars().display.id); }
            return Ok(());
        }
        if !sample.is_valid() {
            *seen = Seen::Status(None);
            return Ok(());
        }
        let status = sample.sample_attachments_array(false).and_then(|a| {
            if a.count() == 0 {
                return None;
            }
            // Sample attachments are CFDictionary/NSDictionary (toll-free bridge).
            let dict = &*(a.value_at_index(0) as *const AnyObject);
            let number: Option<Retained<AnyObject>> =
                msg_send![dict, objectForKey: SCStreamFrameInfoStatus];
            number.map(|n| {
                let value: isize = msg_send![&*n, integerValue];
                value
            })
        });
        if !matches!(status, Some(0) | Some(1)) {
            *seen = Seen::Status(status);
            let mut buffer = self.ivars().buffer.lock().unwrap();
            if buffer.is_capture_generation(self.ivars().generation) {
                buffer.invalidate_display(self.ivars().display.id);
            }
            return Ok(());
        }
        let pts = sample.presentation_time_stamp();
        let clock = stream
            .synchronizationClock()
            .context("missing SCK synchronization clock")?;
        let host = CMClock::host_time_clock();
        let time = CMSyncConvertTime(pts, clock.as_ref(), host.as_ref());
        let ns = time_ns(time).context("invalid frame timestamp")?;
        if status == Some(1) {
            // Reuse only pixels still owned by the bounded history, without a second cache.
            let mut buffer = self.ivars().buffer.lock().unwrap();
            if !buffer.is_capture_generation(self.ivars().generation) { return Ok(()); }
            *seen = Seen::Kept;
            return buffer.repeat_idle(self.ivars().display.id, ns);
        }
        let frame = frame_from_sample(sample, self.ivars().display.clone(), ns)?;
        *seen = Seen::Kept;
        self.ivars().buffer.lock().unwrap().publish_capture_frame(self.ivars().generation, frame)
    }
}
/// Copy owned RGB and release the native pixel buffer before returning.
unsafe fn frame_from_sample(
    sample: &CMSampleBuffer,
    display: DisplayInfo,
    ns: u64,
) -> Result<Frame> {
    let pixels = sample
        .image_buffer()
        .context("complete sample without pixels")?;
    let pixel: &CVPixelBuffer = &pixels;
    ensure!(
        CVPixelBufferGetPixelFormatType(pixel) == kCVPixelFormatType_32BGRA,
        "unexpected screen pixel format"
    );
    ensure!(
        CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) == 0,
        "pixel lock failed"
    );
    struct Unlock<'a>(&'a CVPixelBuffer);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            unsafe {
                CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
    let _unlock = Unlock(pixel);
    let (w, h, stride) = (
        CVPixelBufferGetWidth(pixel),
        CVPixelBufferGetHeight(pixel),
        CVPixelBufferGetBytesPerRow(pixel),
    );
    ensure!(
        w > 0 && h > 0 && stride >= w * 4,
        "invalid screen buffer dimensions"
    );
    let base = CVPixelBufferGetBaseAddress(pixel);
    ensure!(!base.is_null(), "null screen pixels");
    let bytes = std::slice::from_raw_parts(base as *const u8, stride * h);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in bytes.chunks_exact(stride).take(h) {
        for pixel in row[..w * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    Frame::new(display, ns, w as u32, h as u32, rgb)
}

/// SCScreenshotManager is command-only: no stream, queue or retained surface at idle.
/// Timestamp is callback receipt in the host clock (not the historical stream PTS).
pub(crate) fn grab_full_resolution(id: u32) -> Result<Frame> {
    screenshot(id, Shot { cursor: true, display_p3: false }, |sample, info, (w, h)| {
        // SAFETY: a valid sample ScreenCaptureKit handed to the completion handler.
        let frame = unsafe { frame_from_sample(sample, info, capture::host_now_ns()) }?;
        ensure!(
            frame.width as usize == w && frame.height as usize == h,
            "screenshot is not native resolution"
        );
        Ok(frame)
    })
}

/// A still of one display for drawing mode to hold under its overlay while a menu is open (the menu
/// stays while he draws): native resolution, without the pointer (the real one moves over it), in Display P3 so the
/// still shows exactly what the panel showed. Heimdall's own windows are left out as in every grab; its menus are not.
pub fn grab_still(id: u32) -> Result<crate::Still> {
    screenshot(id, Shot { cursor: false, display_p3: true }, |sample, _, (w, h)| {
        // SAFETY: as above.
        let still = unsafe { still_from_sample(sample) }?;
        ensure!(
            still.width as usize == w && still.height as usize == h,
            "screenshot is not native resolution"
        );
        Ok(still)
    })
}

#[derive(Clone, Copy)]
struct Shot {
    cursor: bool,
    display_p3: bool,
}

/// One native-resolution screenshot of display `id` (Heimdall's excluded windows left out), read by `read` on
/// ScreenCaptureKit's queue with the expected pixel size.
fn screenshot<T: Send + 'static>(
    id: u32,
    shot: Shot,
    read: fn(&CMSampleBuffer, DisplayInfo, (usize, usize)) -> Result<T>,
) -> Result<T> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let content = content()?;
        let display = content
            .displays()
            .iter()
            .find(|d| d.displayID() == id)
            .context("display disappeared before full-resolution grab")?;
        let rect = display.frame();
        let info = DisplayInfo {
            id,
            x: rect.origin.x,
            y: rect.origin.y,
            width: display.width() as u32,
            height: display.height() as u32,
        };
        // Display mode pixel dimensions include Retina/scaled backing resolution.
        let mode = CGDisplayCopyDisplayMode(id).context("display mode unavailable")?;
        let (w, h) = (
            CGDisplayMode::pixel_width(Some(&mode)),
            CGDisplayMode::pixel_height(Some(&mode)),
        );
        ensure!(w > 0 && h > 0, "empty native display dimensions");
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &excluded_windows(&content),
        );
        let config = SCStreamConfiguration::new();
        config.setWidth(w);
        config.setHeight(h);
        config.setCapturesAudio(false);
        config.setShowsCursor(shot.cursor);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        if shot.display_p3 {
            config.setColorSpaceName(kCGColorSpaceDisplayP3);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let block = RcBlock::new(move |sample: *mut CMSampleBuffer, error: *mut NSError| {
            let result = objc2::rc::autoreleasepool(|_| {
                if let Some(e) = error.as_ref() {
                    return Err(e.to_string());
                }
                let result = (|| -> Result<T> {
                    let sample = sample.as_ref().context("empty screenshot sample")?;
                    ensure!(sample.is_valid(), "invalid screenshot sample");
                    read(sample, info.clone(), (w, h))
                })();
                result.map_err(|e| format!("{e:#}"))
            });
            // If the deadline elapsed, the owned pixels are dropped here with no file write.
            let _ = tx.send(result);
        });
        SCScreenshotManager::captureSampleBufferWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&block),
        );
        rx.recv_timeout(Duration::from_secs(5))
            .context("full-resolution screenshot timeout")?
            .map_err(|s| anyhow!(s))
    })
}

/// The sample's pixels as packed BGRA8 (rows without padding), released before returning.
unsafe fn still_from_sample(sample: &CMSampleBuffer) -> Result<crate::Still> {
    let pixels = sample.image_buffer().context("complete sample without pixels")?;
    let pixel: &CVPixelBuffer = &pixels;
    ensure!(
        CVPixelBufferGetPixelFormatType(pixel) == kCVPixelFormatType_32BGRA,
        "unexpected screen pixel format"
    );
    ensure!(
        CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) == 0,
        "pixel lock failed"
    );
    struct Unlock<'a>(&'a CVPixelBuffer);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            unsafe {
                CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
    let _unlock = Unlock(pixel);
    let (w, h, stride) = (
        CVPixelBufferGetWidth(pixel),
        CVPixelBufferGetHeight(pixel),
        CVPixelBufferGetBytesPerRow(pixel),
    );
    ensure!(w > 0 && h > 0 && stride >= w * 4, "invalid screen buffer dimensions");
    let base = CVPixelBufferGetBaseAddress(pixel);
    ensure!(!base.is_null(), "null screen pixels");
    let bytes = std::slice::from_raw_parts(base as *const u8, stride * h);
    let mut bgra = Vec::with_capacity(w * h * 4);
    for row in bytes.chunks_exact(stride).take(h) {
        bgra.extend_from_slice(&row[..w * 4]);
    }
    Ok(crate::Still { width: w as u32, height: h as u32, bgra })
}
fn time_ns(time: CMTime) -> Option<u64> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.epoch != 0
        || time.value < 0
        || time.timescale <= 0
    {
        return None;
    }
    u64::try_from(time.value as u128 * 1_000_000_000 / time.timescale as u128).ok()
}

fn native_display_size(id:u32)->Result<(u32,u32)> {
    let mode=CGDisplayCopyDisplayMode(id).context("native display mode")?;
    Ok((CGDisplayMode::pixel_width(Some(&mode)) as u32,CGDisplayMode::pixel_height(Some(&mode)) as u32))
}
fn session_unlocked()->bool {
    if SESSION_LOCKED.load(Ordering::Acquire){return false}
    let Some(session)=CGSessionCopyCurrentDictionary() else {return false};
    unsafe {
        let dict=&*(&*session as *const _ as *const AnyObject);
        let locked:Option<Retained<AnyObject>>=msg_send![dict,objectForKey:&*NSString::from_str("CGSSessionScreenIsLocked")];
        let console:Option<Retained<AnyObject>>=msg_send![dict,objectForKey:&*NSString::from_str("kCGSSessionOnConsoleKey")];
        let locked=locked.is_some_and(|v|msg_send![&*v,boolValue]);
        let console=console.is_some_and(|v|msg_send![&*v,boolValue]);
        let login:Option<Retained<AnyObject>>=msg_send![dict,objectForKey:&*NSString::from_str("kCGSessionLoginDoneKey")];
        let login=login.is_some_and(|v|msg_send![&*v,boolValue]);
        console && login && !locked
    }
}
fn display_allowed(id:u32)->bool {
    crate::CaptureGate{seeing:true,awake:!CGDisplayIsAsleep(id),unlocked:session_unlocked(),brightness:display_brightness(id)}.allows()
}
// macOS has no public per-CG-display brightness API. This optional read-only
// DisplayServices query handles the built-in panel; unsupported external panels
// use the public asleep state. Never load code outside the system framework.
fn display_brightness(id:u32)->Option<f32> {
    type ReadBrightness=unsafe extern "C" fn(u32,*mut f32)->i32;
    static QUERY:std::sync::OnceLock<Option<ReadBrightness>>=std::sync::OnceLock::new();
    let query=QUERY.get_or_init(||unsafe {
        let handle=libc::dlopen(c"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices".as_ptr(),libc::RTLD_LAZY|libc::RTLD_LOCAL);
        if handle.is_null(){return None}
        let symbol=libc::dlsym(handle,c"DisplayServicesGetBrightness".as_ptr());
        if symbol.is_null(){libc::dlclose(handle);None}else{Some(std::mem::transmute::<*mut c_void,ReadBrightness>(symbol))}
    });
    let mut brightness=0.;match query {Some(read) if unsafe {read(id,&mut brightness)}==0=>Some(brightness),_=>None}
}

fn recovery_allowed(has_session: bool, has_retry: bool, attempts: u32) -> bool {
    !has_session && !has_retry && attempts < 5
}

fn capture_available()->bool { capture_pause_reason().is_none() }
fn capture_pause_reason()->Option<&'static str> {
    if !session_unlocked(){return Some("session locked or inactive")}
    let mut count=0;
    if unsafe {CGGetOnlineDisplayList(0,std::ptr::null_mut(),&mut count)}!=CGError::Success {return Some("online display enumeration failed")}
    if count==0 {return Some("no online display")}
    let mut ids=vec![0;count as usize];
    if unsafe {CGGetOnlineDisplayList(count,ids.as_mut_ptr(),&mut count)}!=CGError::Success {return Some("online display enumeration failed")}
    if ids.into_iter().take(count as usize).any(display_allowed) {None} else {Some("all displays asleep or dark")}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhausted_recovery_waits_for_an_os_event() {
        assert!(recovery_allowed(false, false, 4));
        assert!(!recovery_allowed(false, false, 5));
        assert!(!recovery_allowed(false, false, 6));
        assert!(!recovery_allowed(true, false, 0));
        assert!(!recovery_allowed(false, true, 0));
        assert!(recovery_allowed(false, false, 0), "OS refresh resets the attempts");
    }
    #[test]
    fn exact_rational_host_conversion() {
        assert_eq!(time_ns(unsafe { CMTime::new(3, 2) }), Some(1_500_000_000));
        assert_eq!(time_ns(unsafe { CMTime::new(-1, 1) }), None);
    }
    #[test]
    fn invalid_times_are_not_origin_zero() {
        assert_eq!(
            time_ns(CMTime {
                value: 0,
                timescale: 1,
                flags: CMTimeFlags::empty(),
                epoch: 0
            }),
            None
        );
    }
}

/// Read-only window catalogue under the recorder's already granted permission.
/// No AX/TCC requests. Missing names remain empty rather than guessed.
pub(crate) fn frontmost_context() -> heimdall_archive::timeline::recording::Context {
    let mut context = heimdall_archive::timeline::recording::Context::default();
    objc2::rc::autoreleasepool(|_| unsafe {
        let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication() else {
            return;
        };
        context.app = app.bundleIdentifier().map(|s| s.to_string());
        let pid = app.processIdentifier();
        let Some(windows) = CGWindowListCopyWindowInfo(
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
            0,
        ) else {
            return;
        };
        for i in 0..windows.count() {
            let dict = &*(windows.value_at_index(i) as *const AnyObject);
            let number: Option<Retained<AnyObject>> =
                msg_send![dict, objectForKey: &*NSString::from_str("kCGWindowOwnerPID")];
            let owner: i32 = number.map(|n| msg_send![&*n, intValue]).unwrap_or(-1);
            let layer: Option<Retained<AnyObject>> =
                msg_send![dict,objectForKey:&*NSString::from_str("kCGWindowLayer")];
            let layer: i32 = layer.map(|n| msg_send![&*n, intValue]).unwrap_or(-1);
            if owner == pid && layer == 0 {
                let name: Option<Retained<NSString>> =
                    msg_send![dict,objectForKey:&*NSString::from_str("kCGWindowName")];
                context.window_title = name.map(|s| s.to_string()).unwrap_or_default();
                let number: Option<Retained<AnyObject>> =
                    msg_send![dict,objectForKey:&*NSString::from_str("kCGWindowNumber")];
                context.window_id = number.map(|n| msg_send![&*n, unsignedIntValue]);
                break;
            }
        }
    });
    context
}

#[cfg(test)]
mod chunk_metadata_cost {
    #[test]
    #[ignore = "agent-only short metadata CPU probe; no capture, no permission request"]
    fn frontmost_metadata_cpu() {
        fn cpu() -> f64 {
            unsafe {
                let mut r = std::mem::zeroed::<libc::rusage>();
                assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut r), 0);
                r.ru_utime.tv_sec as f64
                    + r.ru_utime.tv_usec as f64 / 1e6
                    + r.ru_stime.tv_sec as f64
                    + r.ru_stime.tv_usec as f64 / 1e6
            }
        }
        let _ = super::frontmost_context();
        let start = cpu();
        for _ in 0..100 {
            std::hint::black_box(super::frontmost_context());
        }
        println!(
            "SCREEN_METADATA cpu_s_per_hour_at_1hz={:.6}",
            (cpu() - start) * 36.
        );
    }
}

#[cfg(test)]
mod still_probe {
    /// Drawing mode's still of the main display through ScreenCaptureKit: native size, packed BGRA, opaque, and how
    /// long it takes. Needs Screen Recording already granted to the running process (never asks); nothing is saved.
    #[test]
    #[ignore = "agent-only: one real capture of the main display, in memory only"]
    fn still_of_the_main_display() {
        if !super::screen_permission_granted() {
            println!("STILL skipped: no Screen Recording permission");
            return;
        }
        let id = objc2_core_graphics::CGMainDisplayID();
        let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(id).unwrap();
        let (w, h) = (objc2_core_graphics::CGDisplayMode::pixel_width(Some(&mode)), objc2_core_graphics::CGDisplayMode::pixel_height(Some(&mode)));
        let _ = super::grab_still(id).unwrap();
        let mut times = vec![];
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let still = super::grab_still(id).unwrap();
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!((still.width as usize, still.height as usize), (w, h));
            assert_eq!(still.bgra.len(), w * h * 4);
            assert!(still.bgra.chunks_exact(4).step_by(997).all(|p| p[3] == 255), "opaque");
        }
        println!("STILL {w}x{h} ms {times:.0?}");
    }
}
