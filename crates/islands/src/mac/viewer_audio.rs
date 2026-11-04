//! The viewer's audio player (Swift `ViewerAudio.swift`): play/pause, a waveform drawn once from the file
//! (AVAudioFile, off the main thread), click or drag on it to scrub, elapsed and total time. Only playback moves the
//! playhead; paused, nothing runs. With `FI_MUTE=1` in the environment the player is muted (lab runs).
#![allow(unused_unsafe)]

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSColor, NSEvent, NSTextAlignment, NSTextField, NSView};
use objc2_av_foundation::{AVPlayer, AVPlayerActionAtItemEnd};
use objc2_avf_audio::{AVAudioFile, AVAudioPCMBuffer};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_media::CMTime;
use objc2_foundation::NSURL;
use objc2_quartz_core::CALayer;

use super::draw::{Bitmap, Image};
use super::look::{self, cgr, ns, pt, Weight};
use super::sys;
use crate::model::geometry::{Point, Rect};

pub const HEIGHT: f64 = 72.0;
const BAR_W: f64 = 2.5;
const BAR_GAP: f64 = 2.0;

/// Lab runs set `FI_MUTE=1`: every player starts muted.
pub fn muted_by_env() -> bool {
    std::env::var("FI_MUTE").map(|v| v == "1").unwrap_or(false)
}

/// "0:07", "12:30", "1:02:03".
pub fn clock(s: f64) -> String {
    if !s.is_finite() || s < 0.0 {
        return "0:00".into();
    }
    let i = s.floor() as i64;
    if i >= 3600 {
        format!("{}:{:02}:{:02}", i / 3600, i / 60 % 60, i % 60)
    } else {
        format!("{}:{:02}", i / 60, i % 60)
    }
}

/// The level of each of `count` bars from the peaks: each bar takes the loudest bin it covers.
pub fn bar_levels(peaks: Option<&[f32]>, count: usize) -> Vec<f64> {
    (0..count)
        .map(|i| match peaks {
            Some(p) if !p.is_empty() => {
                let a = i * p.len() / count;
                let b = (a + 1).max((i + 1) * p.len() / count);
                p[a..b.min(p.len())].iter().cloned().fold(0.0f32, f32::max) as f64
            }
            _ => 0.0,
        })
        .collect()
}

/// How many bars fit a width.
pub fn bar_count(width: f64) -> usize {
    ((width + BAR_GAP) / (BAR_W + BAR_GAP)).max(0.0) as usize
}

/// RMS loudness per bin, 0…1, read from evenly spaced windows (bounded cost on long files), and the duration.
pub fn read_peaks(path: &Path, bins: usize) -> (Option<Vec<f32>>, f64) {
    objc2::rc::autoreleasepool(|_| unsafe {
        let url = NSURL::fileURLWithPath(&ns(&path.to_string_lossy()));
        let Ok(f) = AVAudioFile::initForReading_error(AVAudioFile::alloc(), &url) else { return (None, 0.0) };
        let frames = f.length();
        let fmt = f.processingFormat();
        let sr = fmt.sampleRate();
        let duration = if sr > 0.0 { frames as f64 / sr } else { 0.0 };
        if frames <= 0 {
            return (None, duration);
        }
        let Some(buf) = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), &fmt, 16_384) else {
            return (None, duration);
        };
        let bin_len = (frames / bins as i64).max(1);
        let mut out = vec![0f32; bins];
        for (b, slot) in out.iter_mut().enumerate() {
            let start = b as i64 * bin_len;
            if start >= frames {
                break;
            }
            f.setFramePosition(start);
            let want = 16_384i64.min(bin_len).min(frames - start) as u32;
            if f.readIntoBuffer_frameCount_error(&buf, want).is_err() {
                continue;
            }
            let ch = buf.floatChannelData();
            if ch.is_null() {
                continue;
            }
            let n = buf.frameLength() as usize;
            if n == 0 {
                continue;
            }
            let channels = fmt.channelCount() as usize;
            let mut sum = 0f32;
            for c in 0..channels {
                let p = (*ch.add(c)).as_ptr();
                for i in 0..n {
                    let v = *p.add(i);
                    sum += v * v;
                }
            }
            *slot = (sum / (n * channels) as f32).sqrt();
        }
        let top = out.iter().cloned().fold(0.0f32, f32::max).max(0.0001);
        (Some(out.iter().map(|v| (v / top).min(1.0)).collect()), duration)
    })
}

/// Rounded bars (2.5 pt wide, 2 pt apart), centred, at least a dot tall; a flat line while the peaks load.
pub fn wave_image(peaks: Option<&[f32]>, size: CGSize, color: &NSColor, scale: f64) -> Option<Image> {
    let (w, h) = ((size.width * scale) as usize, (size.height * scale) as usize);
    let b = Bitmap::new(w, h)?;
    b.scale(scale, scale);
    let count = bar_count(size.width);
    for (i, v) in bar_levels(peaks, count).into_iter().enumerate() {
        let bh = BAR_W.max(size.height * (0.06 + 0.94 * v));
        let r = CGRect::new(CGPoint::new(i as f64 * (BAR_W + BAR_GAP), (size.height - bh) / 2.0), CGSize::new(BAR_W, bh));
        b.rounded_path(r, BAR_W / 2.0);
    }
    b.fill_path(&color.CGColor());
    b.image()
}

pub struct AudioIvars {
    path: PathBuf,
    player: Retained<AVPlayer>,
    button: Retained<CALayer>,
    glyph: Retained<CALayer>,
    dim: Retained<CALayer>,
    bright: Retained<CALayer>,
    bright_mask: Retained<CALayer>,
    elapsed: RefCell<Option<Retained<NSTextField>>>,
    total: RefCell<Option<Retained<NSTextField>>>,
    peaks: RefCell<Option<Vec<f32>>>,
    duration: Cell<f64>,
    scrubbing: Cell<bool>,
    time_observer: RefCell<Option<Retained<AnyObject>>>,
    end_observer: RefCell<Option<Retained<AnyObject>>>,
}

define_class!(
    /// The player: a white round button, the waveform (dim, with a bright copy masked up to the playhead), times.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerAudioPlayer"]
    #[ivars = AudioIvars]
    pub struct AudioPlayerView;

    impl AudioPlayerView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(layout))]
        fn layout_(&self) {
            let _: () = unsafe { msg_send![super(self), layout] };
            self.relayout();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let p = self.local(e);
            let iv = self.ivars();
            if look::rect_of(iv.button.frame()).inset(-6.0, -6.0).contains(p) {
                self.toggle();
                return;
            }
            let w = self.wave_rect();
            if w.inset(0.0, -10.0).contains(p) {
                iv.scrubbing.set(true);
                self.seek((p.x - w.x) / w.w);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            if !self.ivars().scrubbing.get() {
                return;
            }
            let p = self.local(e);
            let w = self.wave_rect();
            self.seek((p.x - w.x) / w.w);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _e: &NSEvent) {
            self.ivars().scrubbing.set(false);
        }
    }
);

impl AudioPlayerView {
    pub fn new(path: &Path, mtm: MainThreadMarker) -> Retained<AudioPlayerView> {
        let url = NSURL::fileURLWithPath(&ns(&path.to_string_lossy()));
        let player = unsafe { AVPlayer::playerWithURL(&url, mtm) };
        unsafe {
            player.setActionAtItemEnd(AVPlayerActionAtItemEnd::Pause);
            if muted_by_env() {
                player.setMuted(true);
            }
        }
        let this = Self::alloc(mtm).set_ivars(AudioIvars {
            path: path.to_owned(),
            player,
            button: look::layer(),
            glyph: look::layer(),
            dim: look::layer(),
            bright: look::layer(),
            bright_mask: look::layer(),
            elapsed: RefCell::new(None),
            total: RefCell::new(None),
            peaks: RefCell::new(None),
            duration: Cell::new(0.0),
            scrubbing: Cell::new(false),
            time_observer: RefCell::new(None),
            end_observer: RefCell::new(None),
        });
        let v: Retained<AudioPlayerView> = unsafe { msg_send![super(this), initWithFrame: cgr(Rect::new(0.0, 0.0, 400.0, HEIGHT))] };
        v.setWantsLayer(true);
        let iv = v.ivars();
        if let Some(l) = v.layer() {
            look::no_implicit(&l);
            look::set_bg(&iv.button, &look::white(1.0));
            look::continuous(&iv.button);
            l.addSublayer(&iv.button);
            iv.button.addSublayer(&iv.glyph);
            unsafe {
                iv.dim.setContentsGravity(objc2_quartz_core::kCAGravityResize);
                iv.bright.setContentsGravity(objc2_quartz_core::kCAGravityResize);
            }
            look::set_bg(&iv.bright_mask, &look::black());
            iv.bright_mask.setAnchorPoint(pt(0.0, 0.0));
            unsafe { iv.bright.setMask(Some(&iv.bright_mask)) };
            l.addSublayer(&iv.dim);
            l.addSublayer(&iv.bright);
        }
        for (s, slot, right) in [("0:00", &iv.elapsed, false), ("–:––", &iv.total, true)] {
            let f = NSTextField::labelWithString(&ns(s), mtm);
            f.setFont(Some(&look::small_mono()));
            f.setTextColor(Some(&look::ink2()));
            if right {
                f.setAlignment(NSTextAlignment::Right);
            }
            v.addSubview(&f);
            *slot.borrow_mut() = Some(f);
        }
        v.set_glyph();
        // Playback ticks at 30 Hz only while it plays (AVPlayer's periodic observer); the end puts it back to zero.
        let weak = objc2::rc::Weak::from_retained(&v);
        let w2 = weak.clone();
        let block = block2::RcBlock::new(move |_t: CMTime| {
            if let Some(v) = w2.load() {
                v.tick();
            }
        });
        let obs: Retained<AnyObject> = unsafe {
            msg_send![&*iv.player, addPeriodicTimeObserverForInterval: CMTime::new(1, 30), queue: std::ptr::null::<AnyObject>(), usingBlock: &*block]
        };
        *iv.time_observer.borrow_mut() = Some(obs);
        let w3 = weak.clone();
        let end = block2::RcBlock::new(move |_n: std::ptr::NonNull<objc2_foundation::NSNotification>| {
            if let Some(v) = w3.load() {
                let _: () = unsafe { msg_send![&*v.ivars().player, seekToTime: objc2_core_media::kCMTimeZero] };
                v.set_glyph();
                v.tick();
            }
        });
        let item = unsafe { iv.player.currentItem() };
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        let main_q = objc2_foundation::NSOperationQueue::mainQueue();
        let eo: Retained<AnyObject> = unsafe {
            msg_send![&*center, addObserverForName: &*ns("AVPlayerItemDidPlayToEndTimeNotification"), object: item.as_deref().map(|i| look::any(i)), queue: &*main_q, usingBlock: &*end]
        };
        *iv.end_observer.borrow_mut() = Some(eo);
        // The waveform, off the main thread.
        let p = path.to_owned();
        let back = unsafe { super::viewer::MainOnly::new(weak) };
        std::thread::spawn(move || {
            let (peaks, d) = read_peaks(&p, 160);
            sys::soon(move || {
                let back = back.into_inner(MainThreadMarker::new().expect("main thread"));
                if let Some(v) = back.load() {
                    *v.ivars().peaks.borrow_mut() = peaks;
                    v.ivars().duration.set(d);
                    if let Some(t) = v.ivars().total.borrow().as_ref() {
                        t.setStringValue(&ns(&clock(d)));
                    }
                    v.draw_wave();
                }
            });
        });
        v
    }

    fn local(&self, e: &NSEvent) -> Point {
        let p = self.convertPoint_fromView(e.locationInWindow(), None);
        Point::new(p.x, p.y)
    }

    fn scale(&self) -> f64 {
        self.window().map(|w| w.backingScaleFactor()).unwrap_or(2.0)
    }

    fn wave_rect(&self) -> Rect {
        Rect::new(58.0, 4.0, (self.bounds().size.width - 58.0).max(10.0), 44.0)
    }

    fn relayout(&self) {
        let iv = self.ivars();
        let d = 44.0;
        look::quiet(|| {
            iv.button.setFrame(cgr(Rect::new(2.0, 4.0, d, d)));
            iv.button.setCornerRadius(d / 2.0);
            // As Swift's layout: the glyph fills the button (the optical offset comes back on the next toggle).
            iv.glyph.setFrame(iv.button.bounds());
            let w = cgr(self.wave_rect());
            iv.dim.setFrame(w);
            iv.bright.setFrame(w);
        });
        let w = self.wave_rect();
        let bw = self.bounds().size.width;
        if let Some(f) = iv.elapsed.borrow().as_ref() {
            f.setFrame(cgr(Rect::new(w.x, 54.0, 100.0, 18.0)));
        }
        if let Some(f) = iv.total.borrow().as_ref() {
            f.setFrame(cgr(Rect::new(bw - 100.0, 54.0, 100.0, 18.0)));
        }
        self.draw_wave();
        self.tick();
    }

    fn playing(&self) -> bool {
        unsafe { self.ivars().player.rate() != 0.0 }
    }

    fn set_glyph(&self) {
        let iv = self.ivars();
        let scale = self.scale();
        let name = if self.playing() { "pause.fill" } else { "play.fill" };
        if let Some(img) = look::symbol_image(name, 17.0, &[look::black(), look::black()], Weight::Bold) {
            let c = img.layerContentsForContentsScale(scale);
            unsafe { iv.glyph.setContents(Some(&c)) };
        }
        iv.glyph.setContentsScale(scale);
        unsafe { iv.glyph.setContentsGravity(objc2_quartz_core::kCAGravityCenter) };
        self.place_glyph();
    }

    /// The play triangle sits optically centred a point and a half to the right.
    fn place_glyph(&self) {
        let iv = self.ivars();
        let b = iv.button.bounds();
        let dx = if self.playing() { 0.0 } else { 1.5 };
        look::quiet(|| iv.glyph.setFrame(CGRect::new(CGPoint::new(b.origin.x + dx, b.origin.y), b.size)));
    }

    fn draw_wave(&self) {
        let iv = self.ivars();
        let r = self.wave_rect();
        if r.w <= 10.0 {
            return;
        }
        let scale = self.scale();
        let peaks = iv.peaks.borrow();
        let size = CGSize::new(r.w, r.h);
        let dim = wave_image(peaks.as_deref(), size, &look::white(0.28), scale);
        let bright = wave_image(peaks.as_deref(), size, &look::white(1.0), scale);
        look::quiet(|| unsafe {
            iv.dim.setContents(dim.as_ref().map(|i| i.object()));
            iv.bright.setContents(bright.as_ref().map(|i| i.object()));
        });
    }

    fn duration(&self) -> f64 {
        let iv = self.ivars();
        if iv.duration.get() > 0.0 {
            return iv.duration.get();
        }
        unsafe { iv.player.currentItem().map(|i| { let d: CMTime = msg_send![&*i, duration]; d.seconds() }).unwrap_or(0.0) }
    }

    fn tick(&self) {
        let iv = self.ivars();
        let t = self.seconds();
        let d = self.duration();
        let f = if d > 0.0 && d.is_finite() && t.is_finite() { (t / d).clamp(0.0, 1.0) } else { 0.0 };
        let w = self.wave_rect();
        look::quiet(|| iv.bright_mask.setFrame(cgr(Rect::new(0.0, 0.0, w.w * f, w.h))));
        if let Some(e) = iv.elapsed.borrow().as_ref() {
            e.setStringValue(&ns(&clock(if t.is_finite() { t } else { 0.0 })));
        }
    }

    pub fn toggle(&self) {
        let p = &self.ivars().player;
        unsafe {
            if self.playing() {
                p.pause()
            } else {
                p.play()
            }
        }
        self.set_glyph();
    }

    /// Seeks to a fraction (0…1) of the file; the playhead follows at once.
    pub fn seek(&self, fraction: f64) {
        let d = self.duration();
        if !d.is_finite() || d <= 0.0 {
            return;
        }
        let f = fraction.clamp(0.0, 1.0);
        let t = unsafe { CMTime::with_seconds(f * d, 600) };
        let weak = objc2::rc::Weak::from_retained(&self.retain());
        let done = block2::RcBlock::new(move |_ok: objc2::runtime::Bool| {
            if let Some(v) = weak.load() {
                v.tick();
            }
        });
        unsafe {
            let zero = objc2_core_media::kCMTimeZero;
            let _: () = msg_send![&*self.ivars().player, seekToTime: t, toleranceBefore: zero, toleranceAfter: zero, completionHandler: &*done];
        }
        let w = self.wave_rect();
        look::quiet(|| self.ivars().bright_mask.setFrame(cgr(Rect::new(0.0, 0.0, w.w * f, w.h))));
        if let Some(e) = self.ivars().elapsed.borrow().as_ref() {
            e.setStringValue(&ns(&clock(f * d)));
        }
    }

    fn retain(&self) -> Retained<AudioPlayerView> {
        unsafe { Retained::retain(self as *const Self as *mut Self) }.expect("self")
    }

    // Lab hooks (the self-test).
    pub fn is_playing(&self) -> bool {
        self.playing()
    }
    pub fn seconds(&self) -> f64 {
        unsafe {
            let t: CMTime = msg_send![&*self.ivars().player, currentTime];
            t.seconds()
        }
    }
    pub fn mute(&self) {
        unsafe { self.ivars().player.setMuted(true) }
    }
    pub fn is_muted(&self) -> bool {
        unsafe { self.ivars().player.isMuted() }
    }
    pub fn has_peaks(&self) -> bool {
        self.ivars().peaks.borrow().is_some()
    }
    /// The play button's centre, in this view's coordinates (y down).
    pub fn button_centre(&self) -> Point {
        let f = self.ivars().button.frame();
        Point::new(f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0)
    }
    /// A point on the waveform at `fraction` of its width, in this view's coordinates.
    pub fn wave_point(&self, fraction: f64) -> Point {
        let w = self.wave_rect();
        Point::new(w.x + w.w * fraction, w.mid_y())
    }
    /// A point of this view in its window's coordinates (AppKit, y up): where to send a synthetic click.
    pub fn to_window(&self, p: Point) -> Point {
        let q = self.convertPoint_toView(CGPoint::new(p.x, p.y), None);
        Point::new(q.x, q.y)
    }
    pub fn button_centre_in_window(&self) -> Point {
        self.to_window(self.button_centre())
    }
    pub fn wave_point_in_window(&self, fraction: f64) -> Point {
        self.to_window(self.wave_point(fraction))
    }
    pub fn path(&self) -> &Path {
        &self.ivars().path
    }

    pub fn pause(&self) {
        unsafe { self.ivars().player.pause() };
        self.set_glyph();
    }

    pub fn teardown(&self) {
        let iv = self.ivars();
        unsafe {
            iv.player.pause();
            if let Some(o) = iv.time_observer.borrow_mut().take() {
                iv.player.removeTimeObserver(&o);
            }
            if let Some(o) = iv.end_observer.borrow_mut().take() {
                objc2_foundation::NSNotificationCenter::defaultCenter().removeObserver(&o);
            }
            iv.player.replaceCurrentItemWithPlayerItem(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(6.9), "0:06");
        assert_eq!(clock(754.0), "12:34");
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(clock(f64::NAN), "0:00");
        assert_eq!(clock(-1.0), "0:00");
    }

    #[test]
    fn wave_bins() {
        // 402 pt of waveform: 89 bars.
        assert_eq!(bar_count(402.0), 89);
        assert_eq!(bar_count(0.0), 0);
        // Each bar takes the loudest bin it covers; no peaks: flat.
        let p = [0.1f32, 0.9, 0.2, 0.4];
        let two = bar_levels(Some(&p), 2);
        assert!((two[0] - 0.9).abs() < 1e-6 && (two[1] - 0.4).abs() < 1e-6);
        // More bars than bins: each bar covers one bin.
        let eight = bar_levels(Some(&p), 8);
        assert_eq!(eight.len(), 8);
        assert!((eight[1] - 0.1).abs() < 1e-6 && (eight[2] - 0.9).abs() < 1e-6);
        assert_eq!(bar_levels(None, 3), vec![0.0, 0.0, 0.0]);
    }
}
