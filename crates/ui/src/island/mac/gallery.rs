//! The animation gallery and the visual judge. Every card in `object::gallery` (one per island state and transition)
//! plays through the real `Island` on virtual time (`object::sim::frames`) and each frame is painted by the same
//! `paint::Tree::apply` the live window calls, with drawing mode's light from `paint::LightLayers`, and rendered by
//! Core Animation's own renderer (`CARenderer` into a Metal texture, offscreen) at 2×: the system's symbol effects
//! (bounce, wiggle, rotate, breathe, the replace transition) are captured as the window server would show them. A
//! card with a symbol effect plays in real time (the effects run on the wall clock); the rest render as fast as they
//! can. Whole-screen cards (`Card::full`) show the display with the light round its edge, the capture's flash and the
//! pointing marks. Nothing appears on screen. `lab/animation-gallery/README.md` is the guide.
//!
//! Output: `<dir>/anims/<name>/{sN.png, meta.json}` and `<dir>/anims/manifest.js` for `gallery.html` (distinct frames
//! packed 32 to a sheet, the frame list run-length encoded), the desktop crops in `anims/_bg/`, and the judge's
//! findings and frame timings in `<dir>/anims/judge.json`.
//!
//! The judge (lab/testing/TOOLING.md §1.7) checks the pixels: symmetry of the shape's mask about the notch line,
//! content outside the shape, the notch showing below the shape, smoothness of the drawn width and height
//! (reversals, pops, stalls), and rest frames being identical (a symbol effect playing excepted); and the model: the
//! simulator's invariants over the card (I15 no empty box at full size, I16 the notch never below the shape, I17 no
//! text over text, I18 a reopened element keeps its kind, and the rest). It is a gate on mechanics, not a verdict on
//! taste.

use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::AnyThread as _;
use objc2::Message as _;
use objc2_app_kit::{NSAttributedStringNSStringDrawing as _, NSColor, NSCompositingOperation, NSFont, NSImage, NSScreen, NSWorkspace};
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, CGContext};
use objc2_foundation::{MainThreadMarker, NSDictionary, NSPoint, NSRect, NSString};
use objc2_metal::{MTLCommandBuffer as _, MTLCommandQueue as _, MTLDevice as _, MTLPixelFormat, MTLRegion, MTLStorageMode, MTLTexture as _, MTLTextureDescriptor, MTLTextureUsage};
use objc2_quartz_core::{CALayer, CARenderer, CATransaction};

use super::island::host::WINDOW_W;
use super::island::measure::AppKitMeasure;
use super::island::paint::{LightLayers, Tree};
use super::island::raster::{self, Canvas};
use super::util::{attributed, rect};
use crate::island::object::gallery::{self, Card};
use crate::island::object::sim::{self, Step};
use crate::island::object::{Display, Event, FrameModel};

/// Frames are drawn at 2× (Retina) and shown at their point size.
const SCALE: f64 = 2.0;
/// Wallpaper either side of the island's window in the crop (points).
const MARGIN: f64 = 40.0;
/// The desktop crop's height (points).
const BG_H: f64 = 560.0;
/// Distinct frames per PNG sheet.
const PER_SHEET: usize = 32;
/// Whole-screen cards are laid out at this fraction of the display's points (so they fit a card), still at 2×.
const MINI: f64 = 0.4;

/// The display the gallery plays on: its notch geometry, size and wallpaper.
pub(crate) struct GalleryDisplay {
    pub key: &'static str,
    pub display: Display,
    pub center_x: f64,
    pub name: String,
    pub wallpaper: Option<PathBuf>,
    pub corner: f64,
}

/// The user's built-in display when it has a notch (the island's home), else a stand-in with his geometry.
pub(crate) fn notch_display(mtm: MainThreadMarker) -> GalleryDisplay {
    let screens = NSScreen::screens(mtm);
    let notched = screens.iter().find(|s| s.safeAreaInsets().top > 0.0);
    let main = screens.iter().next();
    let wallpaper = notched.as_ref().or(main.as_ref()).and_then(|s| NSWorkspace::sharedWorkspace().desktopImageURLForScreen(s)).and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string()));
    let stand_in = |name: String, wallpaper: Option<PathBuf>| GalleryDisplay { key: "notch", display: Display::MBP16, center_x: 864.0, name, wallpaper, corner: 10.0 };
    let Some(screen) = notched else {
        return stand_in("16-inch MacBook Pro (stand-in; no notched display connected)".into(), wallpaper);
    };
    let frame = screen.frame();
    let inset = screen.safeAreaInsets().top;
    let (left, right) = (screen.auxiliaryTopLeftArea().size.width, screen.auxiliaryTopRightArea().size.width);
    let core = (frame.size.width - left - right).max(0.0);
    let display = Display { width: frame.size.width, height: frame.size.height, scale: SCALE, notch: Some((core, inset)) }.sanitized();
    GalleryDisplay { key: "notch", display, center_x: left + core / 2.0, name: screen.localizedName().to_string(), wallpaper, corner: super::draw::system::top_corner_points(&screen) }
}

/// A display without a notch: a 1920 × 1080 external display, on the same wallpaper.
pub(crate) fn plain_display(wallpaper: Option<PathBuf>) -> GalleryDisplay {
    GalleryDisplay {
        key: "plain",
        display: Display { width: 1920.0, height: 1080.0, scale: SCALE, notch: None },
        center_x: 960.0,
        name: "1920 × 1080 display without a notch".into(),
        wallpaper,
        corner: 0.0,
    }
}

/// The crop's left edge and width (points, screen coordinates from the display's left).
pub(crate) fn crop(d: &GalleryDisplay) -> (f64, f64) {
    let cw = WINDOW_W + 2.0 * MARGIN;
    (d.center_x - cw / 2.0, cw)
}

fn draw_text(text: &str, font: &NSFont, color: &NSColor, x: f64, center_y: f64) -> f64 {
    let s = attributed(text, font, color);
    let size = s.size();
    s.drawAtPoint(NSPoint::new(x.round(), (center_y - size.height / 2.0).round()));
    size.width
}

fn draw_symbol(name: &str, size: f64, color: &NSColor, cx: f64, cy: f64) {
    let Some(image) = super::util::symbol_sized(name, size, unsafe { objc2_app_kit::NSFontWeightRegular }) else { return };
    let config = objc2_app_kit::NSImageSymbolConfiguration::configurationWithPaletteColors(&objc2_foundation::NSArray::from_retained_slice(&[color.retain()]));
    let Some(image) = image.imageWithSymbolConfiguration(&config) else { return };
    let s = image.size();
    let r = rect((cx - s.width / 2.0).round(), (cy - s.height / 2.0).round(), s.width, s.height);
    unsafe { image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(r, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, true, None) };
}

/// The hardware notch: the camera housing.
fn draw_housing(d: &GalleryDisplay) {
    let Some((core, bar)) = d.display.notch else { return };
    let shape = crate::island::object::Shape { w: core, h: bar, flare: 5.0 };
    let path = objc2_app_kit::NSBezierPath::bezierPath();
    for seg in shape.path() {
        use crate::island::object::geom::Seg;
        match seg {
            Seg::Move(p) => path.moveToPoint(NSPoint::new(d.center_x + p.x, p.y)),
            Seg::Line(p) => path.lineToPoint(NSPoint::new(d.center_x + p.x, p.y)),
            Seg::Curve(a, b, c) => path.curveToPoint_controlPoint1_controlPoint2(NSPoint::new(d.center_x + c.x, c.y), NSPoint::new(d.center_x + a.x, a.y), NSPoint::new(d.center_x + b.x, b.y)),
            Seg::Close => path.closePath(),
        }
    }
    NSColor::blackColor().setFill();
    path.fill();
}

/// The crop of the desktop the island sits on: the wallpaper as macOS fills it, the menu bar (its ink chosen for
/// the wallpaper), the menu bar items, the notch.
pub(crate) fn render_background(d: &GalleryDisplay, with_notch: bool) -> Result<(Canvas, String), String> {
    let (x0, cw) = crop(d);
    render_desktop(d, with_notch, x0, cw, BG_H, 1.0)
}

/// The whole display (whole-screen cards), at `MINI` of its points.
fn render_full_background(d: &GalleryDisplay) -> Result<(Canvas, String), String> {
    render_desktop(d, true, 0.0, d.display.width, d.display.height, MINI)
}

/// The desktop from `x0` (display points), `cw` × `ch` points large, drawn at `k` of its points.
fn render_desktop(d: &GalleryDisplay, with_notch: bool, x0: f64, cw: f64, ch: f64, k: f64) -> Result<(Canvas, String), String> {
    let canvas = Canvas::new(cw * k, ch * k, SCALE).ok_or("no bitmap")?;
    let image = d.wallpaper.as_ref().and_then(|p| NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&p.to_string_lossy())));
    let mut source = "a dark gradient (no wallpaper readable)".to_string();
    let (dw, dh) = (d.display.width, d.display.height);
    canvas.draw(|| {
        let t = objc2_foundation::NSAffineTransform::transform();
        t.scaleBy(k);
        t.translateXBy_yBy(-x0, 0.0);
        objc2_app_kit::NSAffineTransformNSAppKitAdditions::concat(&*t);
        match &image {
            Some(img) if img.size().width > 0.0 => {
                let s = img.size();
                let k = (dw / s.width).max(dh / s.height);
                let (w, h) = (s.width * k, s.height * k);
                unsafe { img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(rect((dw - w) / 2.0, (dh - h) / 2.0, w, h), NSRect::ZERO, NSCompositingOperation::Copy, 1.0, true, None) };
                source = d.wallpaper.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            }
            _ => {
                let top = NSColor::colorWithSRGBRed_green_blue_alpha(0.16, 0.20, 0.30, 1.0);
                let bottom = NSColor::colorWithSRGBRed_green_blue_alpha(0.30, 0.33, 0.42, 1.0);
                if let Some(g) = objc2_app_kit::NSGradient::initWithStartingColor_endingColor(objc2_app_kit::NSGradient::alloc(), &top, &bottom) {
                    g.drawInRect_angle(rect(0.0, 0.0, dw, dh), 90.0);
                }
            }
        }
    });
    let bar = d.display.notch.map_or(24.0, |(_, h)| h);
    let rows = ((bar * k * SCALE) as usize).min(canvas.ph);
    let px = canvas.pixels();
    let (mut sum, mut n) = (0.0, 0.0);
    for p in px[..rows * canvas.pw * 4].chunks_exact(4).step_by(7) {
        sum += 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
        n += 1.0;
    }
    let light = n > 0.0 && sum / n / 255.0 > 0.62;
    canvas.draw(|| {
        let t = objc2_foundation::NSAffineTransform::transform();
        t.scaleBy(k);
        t.translateXBy_yBy(-x0, 0.0);
        objc2_app_kit::NSAffineTransformNSAppKitAdditions::concat(&*t);
        let ink = if light { NSColor::colorWithWhite_alpha(0.0, 0.85) } else { NSColor::colorWithWhite_alpha(1.0, 0.95) };
        let font = NSFont::menuBarFontOfSize(0.0);
        let bold = NSFont::boldSystemFontOfSize(font.pointSize());
        let cy = bar / 2.0;
        draw_symbol("apple.logo", 14.0, &ink, 22.0, cy);
        let mut x = 44.0;
        for (i, title) in ["Finder", "File", "Edit", "View", "Go", "Window", "Help"].iter().enumerate() {
            x += draw_text(title, if i == 0 { &bold } else { &font }, &ink, x, cy) + 20.0;
        }
        let clock = "Sat 18 Oct  20:05";
        let w = attributed(clock, &font, &ink).size().width;
        let mut sx = dw - 14.0 - w;
        draw_text(clock, &font, &ink, sx, cy);
        sx -= 22.0;
        for name in ["switch.2", "magnifyingglass", "wifi", "battery.75percent"] {
            draw_symbol(name, 13.0, &ink, sx, cy);
            sx -= 32.0;
        }
        let g = super::helmet::GLYPH;
        super::helmet::draw_mark(rect(sx - g.width / 2.0, ((bar - g.height) / 2.0).round(), g.width, g.height), false, &ink);
        if with_notch {
            draw_housing(d);
        }
    });
    Ok((canvas, source))
}

// ------------------------------------------------------------------ playing a card

/// A card's steps with every note's body drawn and measured, as the live app does when a line arrives.
fn prepared(card: &Card, tree: &mut Tree, scale: f64) -> sim::Scenario {
    let mut s = card.scenario.clone();
    for (_, step) in &mut s.steps {
        if let Step::Ev(Event::Notice { notice, body }) = step {
            if let Some(detail) = &notice.detail {
                let log = matches!(detail, crate::agent_notice::Detail::Text(_)) && notice.kind == crate::agent_notice::NoticeKind::Error;
                if let Some(doc) = raster::note(detail, log, crate::island::object::note_max_w(&s.display), scale) {
                    *body = Some((doc.w, doc.h));
                    tree.rasters.notes.insert(notice.id.clone(), Rc::new(doc));
                }
            }
        }
    }
    s
}

/// One rendered frame and what the judge reads from it.
pub(crate) struct Played {
    pub frames: Vec<FrameModel>,
    /// Main-thread seconds per frame in the live path: `Island::frame` plus `Tree::apply`.
    pub cost: Vec<f64>,
    /// Offscreen rasterisation per frame (the render server's work live).
    pub render: Vec<f64>,
    /// Main-thread work when a state arrives (its text and symbols drawn ahead, `Tree::prewarm`).
    pub arrival: Vec<f64>,
}

/// This thread's CPU time, seconds.
fn thread_cpu() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// Whether a frame has a system symbol effect playing (its image views animate on their own clock): a held state's
/// repeating effect while it shows, an arrival's bounce or wiggle for a second and a half after it arrived, a
/// replace transition for most of a second.
fn effect_frames(frames: &[FrameModel]) -> Vec<bool> {
    use crate::island::object::{Content, Fx};
    let mut out = Vec::with_capacity(frames.len());
    let mut keys: HashMap<String, u64> = HashMap::new();
    let mut arrived: std::collections::HashSet<(String, u64)> = Default::default();
    let mut until = f64::NEG_INFINITY;
    for f in frames {
        for s in &f.slots {
            if !matches!(*s.content, Content::Symbol { .. }) || s.opacity <= 0.01 || s.id.contains('~') {
                continue;
            }
            let id = s.id.to_string();
            match s.fx {
                Fx::Rotate | Fx::Breathe | Fx::Variable => until = until.max(f.t + 0.05),
                Fx::Bounce | Fx::Wiggle if s.arrived && arrived.insert((id.clone(), s.key)) => until = until.max(f.t + 1.6),
                _ => {}
            }
            if let Some(k) = keys.insert(id, s.key) {
                if k != s.key {
                    until = until.max(f.t + 0.8);
                }
            }
        }
        // A symbol that left gets a fresh image view when it comes back (its arrival plays again).
        let here: std::collections::HashSet<&str> = f.slots.iter().map(|s| &*s.id).collect();
        keys.retain(|id, _| here.contains(id.as_str()));
        arrived.retain(|(id, _)| here.contains(id.as_str()));
        out.push(f.t <= until);
    }
    out
}

/// Plays `card` at `fps` and paints every frame into a bitmap (window-sized and transparent, or the whole display for
/// a whole-screen card), handing each to `each` with whether a symbol effect is playing in it.
pub(crate) fn play(card: &Card, d: &GalleryDisplay, fps: f64, mut each: impl FnMut(usize, &FrameModel, &Canvas, bool)) -> Result<Played, String> {
    let h = (d.display.height * 0.5 + 80.0).min(d.display.height).max(260.0).min(BG_H);
    let mut tree = Tree::new((WINDOW_W, h), SCALE);
    let scenario = prepared(card, &mut tree, SCALE).on(d.display);
    let mut light = LightLayers::new((d.display.width, d.display.height), d.corner);
    let (cw, ch, k) = if card.full { (d.display.width * MINI, d.display.height * MINI, MINI) } else { (WINDOW_W, h, 1.0) };
    let mut canvas = Canvas::new(cw, ch, SCALE).ok_or("no bitmap")?;
    let (frames, model_cost) = sim::frames_timed(&scenario, fps, card.end, Box::new(AppKitMeasure::default()));
    let effects = effect_frames(&frames);
    let realtime = effects.iter().any(|e| *e);
    // Where the island's window sits on the display (points from its left edge).
    let window_x = d.center_x - WINDOW_W / 2.0;
    let mut stage = Stage::new(canvas.pw, canvas.ph).ok_or("no Metal device for Core Animation's renderer")?;
    let px = SCALE * k;
    let (light_x, tree_x) = if card.full { (0.0, window_x * px) } else { (-window_x * px, 0.0) };
    stage.add(&light.root, px, light_x);
    stage.add(&tree.root, px, tree_x);
    // The agent's pointing marks (whole-screen cards): the live overlay's layers, built for this display.
    let marks = if card.full && frames.iter().any(|f| f.pointing > 0.0) { pointing_marks(d) } else { None };
    let mut cost = Vec::with_capacity(frames.len());
    let mut render = Vec::with_capacity(frames.len());
    let started_at = Instant::now();
    let mut arrival = vec![];
    for (i, f) in frames.iter().enumerate() {
        // A state arriving (a motion's first frame): its text and symbols are drawn now, as the live host does on
        // the event, from the look it settles into.
        if i == 0 || frames[i - 1].motion.0 != f.motion.0 {
            let last = frames[i..].iter().take_while(|g| g.motion.0 == f.motion.0).last().unwrap_or(f);
            let warm = thread_cpu();
            tree.prewarm(last);
            arrival.push(thread_cpu() - warm);
        }
        // The main thread's own work (its CPU time: a real-time card sleeps between frames, and the wall clock
        // would count the wake-up).
        let started = thread_cpu();
        tree.apply(f);
        let applied = thread_cpu() - started;
        light.apply(&f.light, &f.shape);
        if realtime {
            // The image views' effects run on the wall clock: this frame renders at its moment of real time.
            if let Some(wait) = std::time::Duration::from_secs_f64(f.t).checked_sub(started_at.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let drawn = Instant::now();
        canvas.clear();
        if let Some(m) = &marks {
            if f.pointing > 0.0 {
                m.setOpacity(f.pointing.clamp(0.0, 1.0) as f32);
                // The container runs y up (an overlay window's layer): flip it into the y-down canvas.
                let ctx = &canvas.ctx;
                CGContext::save_g_state(Some(ctx));
                CGContext::scale_ctm(Some(ctx), k, k);
                CGContext::translate_ctm(Some(ctx), 0.0, d.display.height);
                CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
                m.renderInContext(ctx);
                CGContext::restore_g_state(Some(ctx));
            }
        }
        stage.render_over(&mut canvas);
        render.push(drawn.elapsed().as_secs_f64());
        // The live path's main-thread work per frame: the model's frame plus the layer tree's update.
        cost.push(applied + model_cost.get(i).copied().unwrap_or(0.0));
        if std::env::var_os("ISLAND_SLOW_FRAMES").is_some() && applied > 0.004 {
            eprintln!("slow frame: {} frame {i} ({:.3} s) Tree::apply {:.1} ms", card.name, f.t, applied * 1000.0);
        }
        each(i, f, &canvas, effects[i]);
    }
    stage.clear();
    Ok(Played { frames, cost, render, arrival })
}

/// The pointing marks the whole-screen cards show: a circle round the menu bar's clock and an arrow at Heimdall's
/// helmet, built by the pointing overlay's own code for this display.
fn pointing_marks(d: &GalleryDisplay) -> Option<Retained<CALayer>> {
    use crate::drawing::spec::{Mark, Rect as R, Shape as S};
    let mtm = MainThreadMarker::new()?;
    let screen = NSScreen::screens(mtm).iter().next()?;
    let (w, h) = (d.display.width, d.display.height);
    let display = super::draw::point::Display { index: 1, screen, width: w, height: h, scale: SCALE };
    let marks = vec![
        Mark { shape: S::Circle(R { x: w - 150.0, y: 2.0, w: 140.0, h: 20.0 }), label: Some("The time".into()), side: None },
        Mark { shape: S::Arrow { from: None, to: R { x: w - 268.0, y: 3.0, w: 18.0, h: 18.0 } }, label: Some("Heimdall".into()), side: None },
    ];
    let built = super::draw::point::build(&super::draw::point::Resolved { display, marks }, None);
    Some(built.container)
}

/// Core Animation's renderer over a bitmap: the layers added (the island's tree, the light) rendered by `CARenderer`
/// into a Metal texture at the current moment of real time, over the bitmap's pixels (the symbol effects included,
/// as the window server would composite them).
struct Stage {
    renderer: Retained<CARenderer>,
    texture: Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLTexture>>,
    queue: Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandQueue>>,
    root: Retained<CALayer>,
    pw: usize,
    ph: usize,
    rows: Vec<u8>,
}

impl Stage {
    fn new(pw: usize, ph: usize) -> Option<Stage> {
        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA8Unorm, pw, ph, false) };
        desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        let texture = device.newTextureWithDescriptor(&desc)?;
        let queue = device.newCommandQueue()?;
        let space = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }))?;
        let space_obj: &objc2::runtime::AnyObject = unsafe { &*(CFRetained::as_ptr(&space).as_ptr() as *const objc2::runtime::AnyObject) };
        let queue_obj: &objc2::runtime::AnyObject = unsafe { &*(Retained::as_ptr(&queue) as *const objc2::runtime::AnyObject) };
        let options = unsafe {
            NSDictionary::<NSString, objc2::runtime::AnyObject>::from_slices(
                &[objc2_quartz_core::kCARendererColorSpace, objc2_quartz_core::kCARendererMetalCommandQueue],
                &[space_obj, queue_obj],
            )
        };
        let renderer = unsafe { CARenderer::rendererWithMTLTexture_options(&texture, Some(options.cast_unchecked())) };
        let full = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pw as f64, ph as f64));
        renderer.setBounds(full);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let root = CALayer::new();
        root.setBounds(full);
        root.setAnchorPoint(CGPoint::new(0.0, 0.0));
        root.setPosition(CGPoint::new(0.0, 0.0));
        // Not flipped: the trees added flip themselves (y down); flipping here too would turn them upside down.
        CATransaction::commit();
        renderer.setLayer(Some(&root));
        Some(Stage { renderer, texture, queue, root, pw, ph, rows: vec![0u8; pw * ph * 4] })
    }

    /// Adds `layer` (a y-down tree, points) scaled to `scale` pixels a point, its top-left `x` pixels from the
    /// bitmap's left at its top.
    fn add(&self, layer: &CALayer, scale: f64, x: f64) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let holder = CALayer::new();
        holder.setAnchorPoint(CGPoint::new(0.0, 0.0));
        let lb = layer.bounds();
        holder.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lb.size.width, lb.size.height)));
        // The stage runs y up: the holder's bottom sits the layer's scaled height below the top.
        holder.setPosition(CGPoint::new(x, self.ph as f64 - lb.size.height * scale));
        holder.setAffineTransform(CGAffineTransform { a: scale, b: 0.0, c: 0.0, d: scale, tx: 0.0, ty: 0.0 });
        layer.removeFromSuperlayer();
        let b = layer.bounds();
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setPosition(CGPoint::new(0.0, 0.0));
        let _ = b;
        holder.addSublayer(layer);
        self.root.addSublayer(&holder);
        CATransaction::commit();
    }

    /// Lets go of the layers (they belong to the card's tree).
    fn clear(&self) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if let Some(subs) = unsafe { self.root.sublayers() } {
            let subs: Vec<Retained<CALayer>> = subs.iter().collect();
            for sub in subs {
                sub.removeFromSuperlayer();
            }
        }
        CATransaction::commit();
        self.renderer.setLayer(None);
    }

    /// Renders the layers now over `canvas`'s pixels (the same size).
    fn render_over(&mut self, canvas: &mut Canvas) {
        let (pw, ph) = (self.pw, self.ph);
        if canvas.pw != pw || canvas.ph != ph {
            return;
        }
        CATransaction::flush();
        // The texture starts as the bitmap (rows bottom up, the renderer's origin); the layers render over it.
        {
            let px = canvas.pixels();
            for y in 0..ph {
                self.rows[(ph - 1 - y) * pw * 4..(ph - y) * pw * 4].copy_from_slice(&px[y * pw * 4..(y + 1) * pw * 4]);
            }
        }
        let region = MTLRegion { origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 }, size: objc2_metal::MTLSize { width: pw, height: ph, depth: 1 } };
        unsafe { self.texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, std::ptr::NonNull::new(self.rows.as_mut_ptr().cast()).unwrap(), pw * 4) };
        let full = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pw as f64, ph as f64));
        unsafe { self.renderer.beginFrameAtTime_timeStamp(objc2_quartz_core::CACurrentMediaTime(), std::ptr::null_mut()) };
        self.renderer.addUpdateRect(full);
        self.renderer.render();
        self.renderer.endFrame();
        if let Some(buffer) = self.queue.commandBuffer() {
            buffer.commit();
            buffer.waitUntilCompleted();
        }
        unsafe { self.texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(std::ptr::NonNull::new(self.rows.as_mut_ptr().cast()).unwrap(), pw * 4, region, 0) };
        let px = canvas.pixels_mut();
        for y in 0..ph {
            px[y * pw * 4..(y + 1) * pw * 4].copy_from_slice(&self.rows[(ph - 1 - y) * pw * 4..(ph - y) * pw * 4]);
        }
    }
}

// ------------------------------------------------------------------ the judge

/// A judge finding: the card, the frame, the metric and what it measured.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct Finding {
    pub card: String,
    pub frame: usize,
    pub t: f64,
    pub metric: &'static str,
    pub detail: String,
}

/// What the judge reads from one frame's pixels.
#[derive(Clone, Copy, Debug, Default)]
struct Measures {
    /// Mismatched shape-mask pixels when mirrored about the notch line, ÷ mask pixels.
    asymmetry: f64,
    /// Content pixels outside the (1 px dilated) shape mask.
    clipped: usize,
    /// The drawn shape's width and height (points), from the mask.
    width: f64,
    height: f64,
    /// A 1/4-size grey copy's mean difference from the previous frame.
    diff: f64,
}

fn measure_frame(c: &Canvas, prev: Option<&[u8]>, small: &mut Vec<u8>, glow: bool, shape: &crate::island::object::Shape) -> Measures {
    let reach = shape.h.max(0.0);
    // Only the rows the shape (and its glow) can reach.
    let (w, h) = (c.pw, (((reach + 24.0) * c.scale).ceil() as usize).min(c.ph).max(4));
    let px = &c.pixels()[..w * h * 4];
    let cx = w / 2; // the notch line is the window's middle
    let mut mask_n = 0usize;
    let mut mismatch = 0usize;
    let mut clipped = 0usize;
    let (mut x_min, mut x_max, mut y_max) = (usize::MAX, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let a = px[i + 3];
            if a >= 250 {
                mask_n += 1;
                let mx = 2 * cx as isize - x as isize - 1;
                if mx < 0 || mx as usize >= w || px[(y * w + mx as usize) * 4 + 3] < 250 {
                    mismatch += 1;
                }
                x_min = x_min.min(x);
                x_max = x_max.max(x);
                y_max = y_max.max(y);
            }
            // Content outside the shape: a coloured (not black) pixel outside the outline dilated by 1.5 px. The
            // shape's own anti-aliased edge is black; drawing mode's glow is excluded.
            if !glow && a > 40 {
                let bright = px[i].max(px[i + 1]).max(px[i + 2]) as f64 / a as f64;
                if bright > 0.25 {
                    let p = crate::island::object::Point::new(x as f64 / c.scale - w as f64 / c.scale / 2.0, y as f64 / c.scale);
                    if !shape.contains(p, 1.5 / c.scale + 0.5) {
                        clipped += 1;
                    }
                }
            }
        }
    }
    // Downsampled grey for the difference series: the whole canvas every 4th pixel each way (a fixed size, so every
    // frame compares with the last).
    let full = c.pixels();
    let (sw, sh) = (c.pw / 4, c.ph / 4);
    small.clear();
    small.reserve(sw * sh);
    for y in 0..sh {
        for x in 0..sw {
            let i = ((y * 4) * c.pw + x * 4) * 4;
            let v = (full[i] as u32 + full[i + 1] as u32 + full[i + 2] as u32 + full[i + 3] as u32) / 4;
            small.push(v as u8);
        }
    }
    let diff = prev.filter(|p| p.len() == small.len()).map_or(0.0, |p| p.iter().zip(small.iter()).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as f64).sum::<f64>() / small.len().max(1) as f64);
    let s = c.scale;
    // The drawn shape's width on a row through its middle and its depth on the centre column (the screen's light
    // along the top edge stays out of both).
    let mid = ((shape.h * 0.5).min(12.0) * s) as usize;
    let mid = mid.min(h.saturating_sub(1));
    let row = &px[mid * w * 4..(mid + 1) * w * 4];
    let first = row.chunks_exact(4).position(|p| p[3] >= 250);
    let last = row.chunks_exact(4).rposition(|p| p[3] >= 250);
    let width = match (first, last) {
        (Some(a), Some(b)) => (b + 1 - a) as f64 / s,
        _ => 0.0,
    };
    let col = w / 2;
    let depth = (0..h).rev().find(|y| px[(y * w + col) * 4 + 3] >= 250).map_or(0.0, |y| (y + 1) as f64 / s);
    let _ = (x_min, x_max, y_max);
    let (width, height) = (width, depth);
    Measures { asymmetry: if mask_n > 0 { mismatch as f64 / mask_n as f64 } else { 0.0 }, clipped, width, height, diff }
}

/// Judges one card's frames as they are painted.
pub(crate) struct Judge {
    card: String,
    fps: f64,
    /// Per frame: time, measures, moving, motion id, pixel hash, the model's shape (w, h).
    series: Vec<(f64, Measures, bool, u64, u64, (f64, f64))>,
    /// Per frame: the nudge's landing jiggle plays (`FrameModel::jiggle`).
    jiggle: Vec<bool>,
    prev_small: Option<Vec<u8>>,
    small: Vec<u8>,
    prev_rest: Option<(u64, u64)>,
    last_hash: Option<u64>,
    /// The display's notch (width, height): the shape never shows it below itself.
    notch: Option<(f64, f64)>,
    pub findings: Vec<Finding>,
}

fn pixel_hash(px: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    px.hash(&mut h);
    h.finish()
}

impl Judge {
    pub(crate) fn new(card: &str, fps: f64, notch: Option<(f64, f64)>) -> Judge {
        Judge { card: card.into(), fps, series: vec![], jiggle: vec![], prev_small: None, small: vec![], prev_rest: None, last_hash: None, notch, findings: vec![] }
    }

    fn find(&mut self, frame: usize, t: f64, metric: &'static str, detail: String) {
        if self.findings.len() < 40 {
            self.findings.push(Finding { card: self.card.clone(), frame, t, metric, detail });
        }
    }

    pub(crate) fn frame(&mut self, k: usize, f: &FrameModel, c: &Canvas, effect: bool) {
        self.jiggle.push(f.jiggle);
        if !f.visible && f.glow <= 0.0 {
            self.series.push((f.t, Measures::default(), false, 0, 0, (0.0, 0.0)));
            self.prev_small = None;
            return;
        }
        // An unchanged frame measures the same (and differs by nothing).
        let rows = (((f.shape.h.max(0.0) + 24.0) * c.scale).ceil() as usize).min(c.ph);
        let hash = pixel_hash(&c.pixels()[..c.pw * rows * 4]);
        let model = (f.shape.w, f.shape.h);
        if let (Some(last), Some((_, m, _, _, _, _))) = (self.last_hash, self.series.last().cloned()) {
            if last == hash {
                self.series.push((f.t, Measures { diff: 0.0, ..m }, f.moving, f.motion.0, hash, model));
                return;
            }
        }
        self.last_hash = Some(hash);
        let m = measure_frame(c, self.prev_small.as_deref(), &mut self.small, f.glow > 0.001, &f.shape);
        self.prev_small = Some(std::mem::take(&mut self.small));
        // Symmetry: ≤ 0.5 % of the mask (TOOLING.md §1.7's gate), the glow and light excluded by the solid test.
        if m.asymmetry > 0.005 {
            self.find(k, f.t, "symmetry", format!("{:.2} % of the shape's mask has no mirror", m.asymmetry * 100.0));
        }
        if m.clipped > 0 {
            self.find(k, f.t, "clip", format!("{} content pixels outside the shape", m.clipped));
        }
        // The notch: while the drawn shape is at least as wide as the camera housing, it is at least as deep.
        if let Some((nw, nh)) = self.notch {
            if m.width >= nw + 1.0 && m.height + 0.5 < nh {
                self.find(k, f.t, "notch", format!("the shape is {:.1} pt deep at {:.1} pt wide: the {nh:.0} pt notch shows below it", m.height, m.width));
            }
        }
        // Rest: two equal models paint equal pixels (a symbol effect plays on its own clock: not compared).
        if !f.moving && !effect {
            let model = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                format!("{:?}", FrameModel { t: 0.0, ..f.clone() }).hash(&mut h);
                h.finish()
            };
            let px = hash;
            if let Some((pm, pp)) = self.prev_rest {
                if pm == model && pp != px {
                    self.find(k, f.t, "rest", "two equal frames at rest painted different pixels".into());
                }
            }
            self.prev_rest = Some((model, px));
        } else {
            self.prev_rest = None;
        }
        self.series.push((f.t, m, f.moving, f.motion.0, hash, model));
    }

    /// Smoothness over the whole card: the drawn width and height per motion (direction reversals: a spring may
    /// overshoot once), pops (a frame's difference far above its neighbours') and stalls (no pixel change while the
    /// shape moved).
    pub(crate) fn finish(&mut self) {
        let s = self.series.clone();
        if std::env::var("ISLAND_JUDGE_TRACE").ok().as_deref() == Some(self.card.as_str()) {
            for (i, e) in s.iter().enumerate() {
                eprintln!("{i:4} t {:.3} diff {:6.2} moving {} hash {:x} shape {:.1}×{:.1} mask {:.1}×{:.1}", e.0, e.1.diff, e.2, e.4 & 0xffff, e.5 .0, e.5 .1, e.1.width, e.1.height);
            }
        }
        let mut by_motion: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, e) in s.iter().enumerate() {
            // The nudge's landing jiggle swells and settles three times by design (the user kept it, 27 Oct): its frames
            // are not counted; the spring's own overshoot around it still is.
            if e.2 && !self.jiggle.get(i).copied().unwrap_or(false) {
                by_motion.entry(e.3).or_default().push(i);
            }
        }
        for (_, idx) in by_motion {
            for dim in 0..2 {
                let v: Vec<f64> = idx.iter().map(|i| if dim == 0 { s[*i].1.width } else { s[*i].1.height }).collect();
                let mut reversals = 0;
                let mut last_sign = 0i8;
                for w in v.windows(2) {
                    let d = w[1] - w[0];
                    if d.abs() < 0.75 {
                        continue;
                    }
                    let sign = if d > 0.0 { 1 } else { -1 };
                    if last_sign != 0 && sign != last_sign {
                        reversals += 1;
                    }
                    last_sign = sign;
                }
                if reversals > 2 {
                    let k = idx[0];
                    self.find(k, s[k].0, "smoothness", format!("the drawn {} reverses {reversals} times in one motion", if dim == 0 { "width" } else { "height" }));
                }
            }
        }
        // Pops: while the shape itself moves, a frame whose difference exceeds 4× the median of its ±3 neighbours.
        // (Reader units and pointer feedback are deliberate cuts on a still shape.)
        for i in 3..s.len().saturating_sub(3) {
            let moved = (s[i].5 .0 - s[i - 1].5 .0).abs() > 0.1 || (s[i].5 .1 - s[i - 1].5 .1).abs() > 0.1;
            if !s[i].2 || !moved {
                continue;
            }
            let mut around: Vec<f64> = (i - 3..=i + 3).filter(|j| *j != i).map(|j| s[j].1.diff).collect();
            around.sort_by(|a, b| a.total_cmp(b));
            let median = around[around.len() / 2];
            if s[i].1.diff > 4.0 * median.max(0.4) && s[i].1.diff > 3.0 {
                self.find(i, s[i].0, "smoothness", format!("a pop: frame difference {:.1} against a median of {median:.1} (shape {:.1}×{:.1} → {:.1}×{:.1})", s[i].1.diff, s[i - 1].5 .0, s[i - 1].5 .1, s[i].5 .0, s[i].5 .1));
            }
        }
        // Stalls: the model's shape moved by more than half a point but not one pixel changed.
        for i in 1..s.len() {
            let (a, b) = (&s[i - 1], &s[i]);
            let moved = (b.5 .0 - a.5 .0).abs() > 0.5 || (b.5 .1 - a.5 .1).abs() > 0.5;
            if b.2 && a.2 && moved && a.4 == b.4 && b.4 != 0 {
                self.find(i, b.0, "smoothness", "a stalled frame mid-motion".into());
            }
        }
        let _ = self.fps;
    }
}

// ------------------------------------------------------------------ encoding

struct Encoder {
    out: PathBuf,
    bounds: Option<(usize, usize, usize, usize)>,
    frames: Vec<Option<Vec<u8>>>,
}

fn bounds_of(px: &[u8], pw: usize, ph: usize) -> Option<(usize, usize, usize, usize)> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..ph {
        let row = &px[y * pw * 4..(y + 1) * pw * 4];
        let Some(first) = row.chunks_exact(4).position(|p| p[3] != 0) else { continue };
        let last = pw - 1 - row.chunks_exact(4).rev().position(|p| p[3] != 0).unwrap_or(0);
        x0 = x0.min(first);
        x1 = x1.max(last);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    (x0 != usize::MAX).then_some((x0, y0, x1, y1))
}

fn union(a: Option<(usize, usize, usize, usize)>, b: Option<(usize, usize, usize, usize)>) -> Option<(usize, usize, usize, usize)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, None) => a,
        (None, b) => b,
    }
}

impl Encoder {
    /// Keeps the frame's pixels (drawn) or nothing.
    fn push(&mut self, c: &Canvas, drawn: bool) {
        if !drawn {
            self.frames.push(None);
            return;
        }
        self.bounds = union(self.bounds, bounds_of(c.pixels(), c.pw, c.ph));
        self.frames.push(Some(c.pixels().to_vec()));
    }

    /// Writes the sheets: the box of everything drawn, distinct frames packed `PER_SHEET` to a PNG.
    fn finish(self, pw: usize, ph: usize) -> Result<(Vec<(i64, usize)>, (usize, usize, usize, usize), usize, usize, u64, usize), String> {
        let (x0, y0, x1, y1) = self.bounds.unwrap_or((0, 0, 1, 1));
        // Even, so the point-sized placement lands on whole pixels at 2×.
        let (bx, by) = (x0.saturating_sub(2) & !1, y0.saturating_sub(2) & !1);
        let (bw, bh) = (((x1 + 4).min(pw) - bx + 1) & !1, ((y1 + 4).min(ph) - by + 1) & !1);
        let (bw, bh) = (bw.max(2).min(pw - bx), bh.max(2).min(ph - by));
        let mut files: HashMap<u64, usize> = HashMap::new();
        let mut runs: Vec<(i64, usize)> = vec![];
        // As many frames to a sheet as fit a bitmap (tall boxes, such as drawing mode's light, pack fewer).
        let per = (16000 / bh.max(1)).clamp(1, PER_SHEET);
        let mut sheet = Canvas::new(bw as f64 / SCALE, (bh * per) as f64 / SCALE, SCALE).ok_or("no bitmap")?;
        let mut sheets = 0usize;
        let mut bytes = 0u64;
        let write = |sheet: &mut Canvas, n: usize| -> Result<u64, String> {
            let png = sheet.png().ok_or("PNG encoding failed")?;
            std::fs::write(self.out.join(format!("s{n}.png")), &png).map_err(|e| e.to_string())?;
            for v in sheet.pixels_mut().iter_mut() {
                *v = 0;
            }
            Ok(png.len() as u64)
        };
        for f in &self.frames {
            let index = match f {
                Some(px) => {
                    let mut crop = Vec::with_capacity(bw * bh * 4);
                    for y in 0..bh {
                        let from = ((by + y) * pw + bx) * 4;
                        crop.extend_from_slice(&px[from..from + bw * 4]);
                    }
                    let h = pixel_hash(&crop);
                    let next = files.len();
                    let i = *files.entry(h).or_insert(next);
                    if i == next {
                        let slot = i % per;
                        let spw = sheet.pw;
                        let dst = sheet.pixels_mut();
                        for y in 0..bh {
                            let to = ((slot * bh + y) * spw) * 4;
                            dst[to..to + bw * 4].copy_from_slice(&crop[y * bw * 4..(y + 1) * bw * 4]);
                        }
                        if slot == per - 1 {
                            bytes += write(&mut sheet, i / per)?;
                            sheets += 1;
                        }
                    }
                    i as i64
                }
                None => -1,
            };
            match runs.last_mut() {
                Some((last, n)) if *last == index => *n += 1,
                _ => runs.push((index, 1)),
            }
        }
        if !files.len().is_multiple_of(per) {
            bytes += write(&mut sheet, files.len() / per)?;
            sheets += 1;
        }
        Ok((runs, (bx, by, bw, bh), files.len(), sheets, bytes, per))
    }
}

/// Renders `card` into `dir/<name>/` and judges it. Returns a summary line, the judge's findings and the costs.
fn render_one(card: &Card, d: &GalleryDisplay, dir: &Path, fps: f64, write: bool) -> Result<(String, Vec<Finding>, Played), String> {
    let started = Instant::now();
    let out = dir.join(card.name);
    if write {
        if out.exists() {
            for e in std::fs::read_dir(&out).map_err(|e| e.to_string())?.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "png" || x == "json") {
                    std::fs::remove_file(p).ok();
                }
            }
        }
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    }
    let mut judge = Judge::new(card.name, fps, d.display.notch);
    let mut enc = Encoder { out: out.clone(), bounds: None, frames: vec![] };
    let (mut pw, mut ph) = (0, 0);
    let played = play(card, d, fps, |k, f, c, effect| {
        // A whole-screen card is the same states as its window-sized twin: its pixels are not measured again.
        if !card.full {
            judge.frame(k, f, c, effect);
        }
        if write {
            enc.push(c, f.visible || f.glow > 0.0 || f.light.run > 0.0 || f.light.flash > 0.0 || f.pointing > 0.0);
        }
        pw = c.pw;
        ph = c.ph;
    })?;
    judge.finish();
    // The model's checks over the card: every simulator invariant (I1–I18) on the card's own sequence.
    let model = sim::run(&card.scenario, &sim::Config { fps, ..sim::Config::default() });
    for f in model.findings.iter().take(10) {
        judge.find(((f.at * fps).round()) as usize, f.at, "model", format!("{}: {}", f.invariant, f.detail));
    }
    let count = played.frames.len();
    if !write {
        return Ok((format!("{:<24} {count:>4} frames judged", card.name), judge.findings, played));
    }
    let (runs, (bx, by, bw, bh), files, sheets, bytes, per) = enc.finish(pw, ph)?;
    let (x0, _) = crop(d);
    let window_x = d.center_x - WINDOW_W / 2.0 - x0;
    let version = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_millis() as u64);
    let meta = serde_json::json!({
        "name": card.name,
        "title": card.title,
        "group": card.group,
        "what": card.what,
        "source": "crates/ui/src/island/object/gallery.rs",
        "engine": "object::Island + paint::Tree (the live window's draw call)",
        "fps": fps,
        "scale": SCALE,
        "count": count,
        "files": files,
        "sheets": sheets,
        "per": per,
        "frames": runs.iter().map(|(i, n)| [*i, *n as i64]).collect::<Vec<_>>(),
        "bg": if card.full { format!("{}-full", d.key) } else { d.key.to_string() },
        "x": if card.full { 0.0 } else { window_x } + bx as f64 / SCALE,
        "y": by as f64 / SCALE,
        "w": bw as f64 / SCALE,
        "h": bh as f64 / SCALE,
        "view_h": if card.full { d.display.height * MINI } else { ((by + bh) as f64 / SCALE + 30.0).max(160.0).min(BG_H) },
        "marks": card.marks.iter().map(|(t, l)| serde_json::json!({ "t": t, "label": l })).collect::<Vec<_>>(),
        "version": version,
    });
    std::fs::write(out.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).map_err(|e| e.to_string())?;
    let line = format!(
        "{:<24} {count:>4} frames at {fps:.0} fps ({:.2} s), {files:>3} distinct, {:>6.0} KB, {:.2} s to render",
        card.name,
        count as f64 / fps,
        bytes as f64 / 1024.0,
        started.elapsed().as_secs_f64()
    );
    Ok((line, judge.findings, played))
}

/// Rebuilds `anims/manifest.js` from every `anims/*/meta.json` of a current card and `anims/_bg/*.json`.
fn write_manifest(dir: &Path) -> Result<(), String> {
    let order: Vec<&str> = gallery::all().iter().map(|c| c.name).collect();
    let mut anims = vec![];
    let mut backgrounds = serde_json::Map::new();
    for e in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let p = e.path();
        if p.file_name().is_some_and(|n| n == "_bg") {
            for b in std::fs::read_dir(&p).map_err(|e| e.to_string())?.flatten() {
                let bp = b.path();
                if bp.extension().is_some_and(|x| x == "json") {
                    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&bp).unwrap_or_default()).map_err(|e| e.to_string())?;
                    backgrounds.insert(v["key"].as_str().unwrap_or_default().to_string(), v);
                }
            }
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(p.join("meta.json")) {
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if order.contains(&v["name"].as_str().unwrap_or_default()) {
                anims.push(v);
            }
        }
    }
    anims.sort_by_key(|v| order.iter().position(|n| *n == v["name"].as_str().unwrap_or_default()));
    let manifest = serde_json::json!({ "backgrounds": backgrounds, "anims": anims });
    let tmp = dir.join(format!("manifest.js.{}", std::process::id()));
    std::fs::write(&tmp, format!("// Written by `heimdall-ui-demo --gallery`; do not edit.\nwindow.HEIMDALL_GALLERY = {};\n", serde_json::to_string_pretty(&manifest).unwrap())).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("manifest.js")).map_err(|e| e.to_string())
}

/// Percentiles of a sample, in milliseconds.
pub(crate) fn ms_stats(v: &[f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let mean = s.iter().sum::<f64>() / s.len() as f64;
    let p95 = s[(s.len() as f64 * 0.95) as usize % s.len()];
    (mean * 1000.0, p95 * 1000.0, s[s.len() - 1] * 1000.0)
}

/// `heimdall-ui-demo --gallery <dir> [--only a,b] [--fps n]`: renders every card (or `only`) into `<dir>/anims/`,
/// judges every frame, and rewrites the manifest and `judge.json`. Returns what failed (judge findings included).
pub(crate) fn run(dir: &Path, only: &[String], fps: f64) -> Vec<String> {
    render_all(dir, only, fps, true)
}

/// The judge alone (`--motion`): every card played and judged, nothing written but `judge.json`.
pub(crate) fn judge_all(dir: &Path, fps: f64) -> Vec<String> {
    render_all(dir, &[], fps, false)
}

fn render_all(dir: &Path, only: &[String], fps: f64, write: bool) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("main thread");
    let mut failures = vec![];
    let all = gallery::all();
    for name in only {
        if !all.iter().any(|c| c.name == name) {
            failures.push(format!("no card named {name:?} (crates/ui/src/island/object/gallery.rs)"));
        }
    }
    let anims_dir = dir.join("anims");
    let bg_dir = anims_dir.join("_bg");
    std::fs::create_dir_all(&bg_dir).ok();
    let notch = notch_display(mtm);
    let plain = plain_display(notch.wallpaper.clone());
    if write {
        for d in [&notch, &plain] {
            match render_background(d, true) {
                Ok((canvas, wallpaper)) => {
                    if let Some(png) = canvas.png() {
                        std::fs::write(bg_dir.join(format!("{}.png", d.key)), png).ok();
                    }
                    let (_, cw) = crop(d);
                    let meta = serde_json::json!({
                        "key": d.key,
                        "w": cw,
                        "h": BG_H,
                        "scale": SCALE,
                        "display": d.name,
                        "notch": d.display.notch.map_or("none".to_string(), |(w, h)| format!("{w:.0} × {h:.0} pt")),
                        "screen": format!("{:.0} × {:.0} pt", d.display.width, d.display.height),
                        "wallpaper": wallpaper,
                        "version": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_millis() as u64),
                    });
                    std::fs::write(bg_dir.join(format!("{}.json", d.key)), serde_json::to_string_pretty(&meta).unwrap()).ok();
                    if d.key == "notch" {
                        println!("display: {} — notch {}, wallpaper {wallpaper}", d.name, meta["notch"].as_str().unwrap_or(""));
                    }
                }
                Err(e) => failures.push(format!("background {}: {e}", d.key)),
            }
        }
        // The whole display, for whole-screen cards (at `MINI` of its points).
        match render_full_background(&notch) {
            Ok((canvas, wallpaper)) => {
                let key = format!("{}-full", notch.key);
                if let Some(png) = canvas.png() {
                    std::fs::write(bg_dir.join(format!("{key}.png")), png).ok();
                }
                let meta = serde_json::json!({
                    "key": key,
                    "w": notch.display.width * MINI,
                    "h": notch.display.height * MINI,
                    "scale": SCALE,
                    "display": format!("{} (whole screen, shown at {:.0} %)", notch.name, MINI * 100.0),
                    "notch": notch.display.notch.map_or("none".to_string(), |(w, h)| format!("{w:.0} × {h:.0} pt")),
                    "screen": format!("{:.0} × {:.0} pt", notch.display.width, notch.display.height),
                    "wallpaper": wallpaper,
                    "version": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_millis() as u64),
                });
                std::fs::write(bg_dir.join(format!("{key}.json")), serde_json::to_string_pretty(&meta).unwrap()).ok();
            }
            Err(e) => failures.push(format!("background {}-full: {e}", notch.key)),
        }
    }
    let mut findings: Vec<Finding> = vec![];
    let (mut cost, mut render, mut arrival) = (vec![], vec![], vec![]);
    for card in all.iter().filter(|c| only.is_empty() || only.iter().any(|o| o == c.name)) {
        let d = if card.scenario.display.has_notch() { &notch } else { &plain };
        match render_one(card, d, &anims_dir, fps, write) {
            Ok((line, f, played)) => {
                println!("{line}{}", if f.is_empty() { String::new() } else { format!("  — {} judge finding(s)", f.len()) });
                findings.extend(f);
                cost.extend(played.cost);
                render.extend(played.render);
                arrival.extend(played.arrival);
            }
            Err(e) => failures.push(format!("{}: {e}", card.name)),
        }
    }
    if write {
        if let Err(e) = write_manifest(&anims_dir) {
            failures.push(format!("manifest: {e}"));
        }
    }
    let (cm, cp, cx) = ms_stats(&cost);
    let (rm, rp, rx) = ms_stats(&render);
    println!("main thread per frame (Island::frame + Tree::apply): mean {cm:.3} ms, p95 {cp:.3} ms, max {cx:.3} ms over {} frames", cost.len());
    println!("offscreen raster per frame (render server's work live): mean {rm:.3} ms, p95 {rp:.3} ms, max {rx:.3} ms");
    let (am, ap, ax) = ms_stats(&arrival);
    println!("state arrival (text and symbols drawn ahead, once per state): mean {am:.3} ms, p95 {ap:.3} ms, max {ax:.3} ms over {} arrivals", arrival.len());
    let report = serde_json::json!({
        "findings": findings,
        "apply_ms": { "mean": cm, "p95": cp, "max": cx, "frames": cost.len() },
        "raster_ms": { "mean": rm, "p95": rp, "max": rx },
        "arrival_ms": { "mean": am, "p95": ap, "max": ax, "arrivals": arrival.len() },
        "fps": fps,
    });
    std::fs::write(anims_dir.join("judge.json"), serde_json::to_string_pretty(&report).unwrap()).ok();
    for f in &findings {
        failures.push(format!("judge: {} frame {} ({:.3} s) {}: {}", f.card, f.frame, f.t, f.metric, f.detail));
    }
    failures
}

// ------------------------------------------------------------------ contact sheets

/// `--sheet <dir> (--seed N | --grid a,b,phase | --card name)`: a contact sheet of a simulated sequence's key frames
/// (every motion's start, 25 / 50 / 75 % and settle, and every 0.5 s at rest), each labelled with its time and size
/// class, on the user's desktop crop. For looking at a failing random or grid sequence. Returns the sheet's path.
pub(crate) fn sheet(dir: &Path, what: &str, arg: &str) -> Result<PathBuf, String> {
    let mtm = MainThreadMarker::new().ok_or("main thread")?;
    let scenario = match what {
        "--seed" => sim::random(arg.parse().map_err(|_| "--seed N")?),
        "--grid" => {
            let v: Vec<usize> = arg.split(',').filter_map(|x| x.parse().ok()).collect();
            if v.len() != 3 {
                return Err("--grid a,b,phase".into());
            }
            sim::pair(v[0], v[1], v[2], (v[0] * 31 + v[1] * 7 + v[2]) as u64 * 0x9E37_79B9 + 1)
        }
        "--card" => gallery::all().into_iter().find(|c| c.name == arg).map(|c| c.scenario).ok_or("no such card")?,
        _ => return Err("--seed N | --grid a,b,phase | --card name".into()),
    };
    let end = scenario.steps.iter().map(|(t, _)| *t).fold(0.0, f64::max) + 3.0;
    let card = Card { name: "sheet", title: "sheet", group: "sheet", what: "sheet", scenario: scenario.clone(), end: end.min(120.0), marks: vec![], full: false };
    let d = if scenario.display.has_notch() { notch_display(mtm) } else { plain_display(None) };
    let (bg, _) = render_background(&d, true)?;
    let (x0, cw) = crop(&d);
    let window_x = d.center_x - WINDOW_W / 2.0 - x0;
    let fps = 60.0;
    let mut picks: Vec<(usize, String, Vec<u8>)> = vec![];
    let mut last_motion = u64::MAX;
    let mut motion_start = 0usize;
    let mut last_rest = -1.0f64;
    // Each cell is as tall as the sequence's tallest shape (a pull-down is never cut by the sheet), at least 200.
    let mut tallest = 0.0f64;
    play(&card, &d, fps, |k, f, c, _| {
        let t = f.t;
        tallest = tallest.max(f.shape.h);
        let mut why = None;
        if f.motion.0 != last_motion {
            last_motion = f.motion.0;
            motion_start = k;
            why = Some("start");
        } else if f.moving {
            let age = k - motion_start;
            if [3usize, 7, 12, 30].contains(&age) {
                why = Some(match age { 3 => "25 %", 7 => "50 %", 12 => "75 %", _ => "settle" });
            }
        } else if t - last_rest >= 0.5 && f.visible {
            last_rest = t;
            why = Some("rest");
        }
        if let Some(w) = why {
            if picks.len() < 60 {
                picks.push((k, format!("{t:.3} s · {w} · {:.0}×{:.0}", f.shape.w, f.shape.h), c.pixels().to_vec()));
            }
        }
    })?;
    let (cell_w, cell_h) = (cw, (tallest + 24.0).clamp(200.0, BG_H));
    let cols = 4usize;
    let rows = picks.len().div_ceil(cols).max(1);
    let out = Canvas::new(cell_w * cols as f64, (cell_h + 18.0) * rows as f64, SCALE).ok_or("no bitmap")?;
    let frame_canvas = Canvas::new(WINDOW_W, (d.display.height * 0.5 + 80.0).min(d.display.height).max(260.0).min(BG_H), SCALE).ok_or("no bitmap")?;
    let bg_image = bg.image().ok_or("background")?;
    out.draw(|| {
        NSColor::whiteColor().setFill();
        objc2_app_kit::NSBezierPath::fillRect(rect(0.0, 0.0, cell_w * cols as f64, (cell_h + 18.0) * rows as f64));
    });
    for (i, (_, label, px)) in picks.iter().enumerate() {
        let (cx, cy) = ((i % cols) as f64 * cell_w, (i / cols) as f64 * (cell_h + 18.0));
        let mut fc = Canvas::new(frame_canvas.pw as f64 / SCALE, frame_canvas.ph as f64 / SCALE, SCALE).ok_or("no bitmap")?;
        fc.pixels_mut().copy_from_slice(px);
        let img = fc.image().ok_or("frame")?;
        let ctx = &out.ctx;
        objc2_core_graphics::CGContext::save_g_state(Some(ctx));
        objc2_core_graphics::CGContext::clip_to_rect(Some(ctx), objc2_core_foundation::CGRect::new(objc2_core_foundation::CGPoint::new(cx, cy), objc2_core_foundation::CGSize::new(cell_w, cell_h)));
        objc2_core_graphics::CGContext::translate_ctm(Some(ctx), cx, cy + BG_H);
        objc2_core_graphics::CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
        objc2_core_graphics::CGContext::draw_image(Some(ctx), objc2_core_foundation::CGRect::new(objc2_core_foundation::CGPoint::new(0.0, 0.0), objc2_core_foundation::CGSize::new(cw, BG_H)), Some(&bg_image));
        let (fw, fh) = (fc.pw as f64 / SCALE, fc.ph as f64 / SCALE);
        objc2_core_graphics::CGContext::draw_image(Some(ctx), objc2_core_foundation::CGRect::new(objc2_core_foundation::CGPoint::new(window_x, BG_H - fh), objc2_core_foundation::CGSize::new(fw, fh)), Some(&img));
        objc2_core_graphics::CGContext::restore_g_state(Some(ctx));
        out.draw(|| {
            draw_text(label, &NSFont::monospacedDigitSystemFontOfSize_weight(10.0, 0.0), &NSColor::blackColor(), cx + 6.0, cy + cell_h + 9.0);
        });
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("sheet{}.png", what.replace('-', "_") + "_" + &arg.replace(',', "-")));
    std::fs::write(&path, out.png().ok_or("PNG")?).map_err(|e| e.to_string())?;
    Ok(path)
}
