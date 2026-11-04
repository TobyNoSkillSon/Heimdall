//! The island's one draw function: a `FrameModel` applied to a Core Animation layer tree. The live window and every
//! offscreen render (stills, the gallery, the judge) call `Tree::apply`; nothing here decides what to show.
//!
//! The tree: the black shape and one content container masked by the same outline holding a layer per slot. A
//! slot's layer shows its cached raster (`raster.rs`) and the blurred twin at the slot's blur; an SF Symbol is a real
//! `NSImageView` (its layer in the tree) so its effects are Apple's own (`NSSymbolEffect`: bounce, wiggle, rotate,
//! breathe, the replace transition), played by the render server; dots, the waveform and the spinner are a few live
//! sublayers. Per frame only geometry and opacity change: no drawing on the main thread, no implicit animations.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_app_kit::{NSImageScaling, NSImageSymbolConfiguration, NSImageView};
use objc2_foundation::{MainThreadMarker, NSArray, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGImage, CGMutablePath, CGPath};
use objc2_quartz_core::{
    kCAAnimationDiscrete, kCALineCapRound, kCALineJoinRound, CACurrentMediaTime, CAFrameRateRange, CAKeyframeAnimation, CALayer, CAMediaTiming, CAShapeLayer, CATransaction,
};

use super::raster::{self, NoteDoc, Raster, BLUR, PAD};
use crate::island::object::geom::Seg;
use crate::island::object::{Content, FrameModel, Fx, Light, Shape, Slot};

fn as_object<T>(r: &T) -> &AnyObject {
    unsafe { &*(r as *const T as *const AnyObject) }
}

fn set_image(layer: &CALayer, image: Option<&CGImage>) {
    unsafe { layer.setContents(image.map(as_object)) };
}

fn cg_rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w.max(0.0), h.max(0.0)))
}

/// The island's outline as a path, the notch's centre line at `cx`.
pub(crate) fn outline(shape: &Shape, cx: f64) -> CFRetained<CGMutablePath> {
    let p = CGMutablePath::new();
    let m = Some(&*p);
    let t = std::ptr::null();
    for seg in shape.path() {
        unsafe {
            match seg {
                Seg::Move(a) => CGMutablePath::move_to_point(m, t, cx + a.x, a.y),
                Seg::Line(a) => CGMutablePath::add_line_to_point(m, t, cx + a.x, a.y),
                Seg::Curve(a, b, c) => CGMutablePath::add_curve_to_point(m, t, cx + a.x, a.y, cx + b.x, b.y, cx + c.x, c.y),
                Seg::Close => CGMutablePath::close_subpath(m),
            }
        }
    }
    p
}

enum Body {
    /// A raster and its blurred twin.
    Image { sharp: Retained<CALayer>, blurred: Retained<CALayer>, has_blur: bool },
    /// Live sublayers (dots, bars, spinner spokes).
    Live(Vec<Retained<CALayer>>),
    /// An SF Symbol in a real image view, and its blurred raster twin. `played`: the arrival effect (or the replace
    /// transition that was its arrival) has run for the content shown.
    /// `name`: the symbol the view shows (a slashed pair turns with the magic replace).
    Symbol { view: Retained<NSImageView>, blurred: Retained<CALayer>, has_blur: bool, played: bool, changed_at: f64, ready: bool, name: String },
}

struct SlotLayer {
    layer: Retained<CALayer>,
    body: Body,
    key: u64,
    /// Whether the body was drawn with the pad (rasters) or fills the slot (live layers).
    padded: bool,
    /// The shown raster's size in pixels (none for live layers or a raster that could not be drawn).
    px: Option<(f64, f64)>,
    /// A veil's mask: its fade, in island coordinates from the shape's top.
    mask: Option<Retained<CALayer>>,
    /// Live layers looping on their own (`loop_layers`): the slot's `since` and the parts' count the loop was made for.
    looped: Option<(f64, usize)>,
}

/// Rasters by content key and scale, kept while in use and a little longer.
#[derive(Default)]
pub(crate) struct Rasters {
    map: HashMap<u64, Rc<Raster>>,
    /// Each note's drawing by notice id (the turn's notes are placed in the pulled-down body), and their ids oldest
    /// first (the oldest drawing goes when there are too many).
    pub notes: HashMap<String, Rc<NoteDoc>>,
    pub note_order: std::collections::VecDeque<String>,
    /// The bodies' bitmaps (the pulled-down turn and the conversation's chunks, megabytes each), newest last: only
    /// `MAX_BODIES` stay, so scrolling a day or a conversation that keeps changing never piles them up.
    bodies: std::collections::VecDeque<u64>,
}

/// Body bitmaps kept: the few in view and the ones just scrolled past.
const MAX_BODIES: usize = 10;

impl Rasters {
    /// Keeps a note's drawing under its id (a new drawing of the same id replaces it), at most `most` of them.
    pub(crate) fn keep_note(&mut self, id: &str, doc: NoteDoc, most: usize) {
        self.note_order.retain(|k| k != id);
        while self.note_order.len() >= most.max(1) {
            if let Some(old) = self.note_order.pop_front() {
                self.notes.remove(&old);
            }
        }
        self.note_order.push_back(id.to_string());
        self.notes.insert(id.to_string(), Rc::new(doc));
    }

    fn get(&mut self, slot: &Slot, scale: f64) -> Option<Rc<Raster>> {
        let key = raster_key(&slot.content, slot.key, slot.size);
        if let Some(r) = self.map.get(&key) {
            return Some(r.clone());
        }
        let r = Rc::new(raster::draw(&slot.content, slot.size, scale, &self.notes)?);
        if self.map.len() > 512 {
            self.map.clear();
            self.bodies.clear();
        }
        if matches!(&*slot.content, Content::Body { .. }) {
            self.bodies.push_back(key);
            while self.bodies.len() > MAX_BODIES {
                if let Some(old) = self.bodies.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        self.map.insert(key, r.clone());
        Some(r)
    }
}

/// The visible part of a body's raster as a unit contents rect (y, height). The tree's geometry is flipped, so the
/// rect runs down from the image's top: scrolled to 0 it shows the top (the 40-row table render opened on row 22
/// with a bottom-up rect).
pub(crate) fn body_window(scroll: f64, visible: f64, full: f64, scale: f64) -> (f64, f64) {
    let full = full.max(1e-6);
    let vh = (visible / full).clamp(0.0, 1.0);
    // A scroll between two pixel rows would resample every line of the body (soft text): it starts on a whole row.
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let scroll = (scroll * scale).round() / scale;
    let y = (scroll / full).clamp(0.0, 1.0 - vh);
    if y.is_finite() && vh.is_finite() { (y, vh) } else { (0.0, 1.0) }
}

/// The veil's opacity at `y` points from the shape's top, the band ending at `bottom` (`Content::Body::veil`):
/// clear at the screen's edge, rising under the ears, held, then giving way to the sharp lines over the last
/// `VEIL_INTO` points (where they fade in from black).
pub(crate) fn veil_alpha(y: f64, bottom: f64) -> f64 {
    let smooth = |a: f64, b: f64, x: f64| {
        let t = ((x - a) / (b - a).max(1e-6)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let into = crate::island::object::VEIL_INTO.min(bottom * 0.5);
    let top = smooth(2.0, 2.0 + VEIL_EDGE.min((bottom - into) * 0.6), y);
    let low = 1.0 - smooth(bottom - into, bottom, y);
    VEIL_MAX * top * low
}

/// The veil's strongest opacity (a dark material: the blurred colours show, never as bright as the lines), and how
/// far below the shape's top it reaches it.
pub(crate) const VEIL_MAX: f64 = 0.8;
const VEIL_EDGE: f64 = 22.0;

/// The veil's mask for a band `bottom` points tall: one pixel column, `veil_alpha` per row.
fn veil_ramp(bottom: f64, scale: f64) -> Option<CFRetained<CGImage>> {
    let c = raster::Canvas::new(1.0, bottom.max(1.0), scale)?;
    let rows = (bottom * scale).ceil().max(1.0) as usize;
    c.draw(|| {
        for k in 0..rows {
            let y = (k as f64 + 0.5) / scale;
            objc2_app_kit::NSColor::colorWithWhite_alpha(1.0, veil_alpha(y, bottom)).setFill();
            objc2_app_kit::NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, k as f64 / scale), NSSize::new(1.0, 1.0 / scale)));
        }
    });
    c.image()
}

/// A slot layer's frame in the island window (points, y down): its origin and size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayerFrame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where a raster slot's layer goes. Its size is the raster's own pixels (a canvas is drawn whole pixels large, so a
/// slot 250.8 pt wide has a 502 px raster: shown in 501.6 px it was resampled), scaled with the slot during a motion;
/// at its own size (at rest, gliding) its origin sits on a whole pixel, so the bitmap lands 1:1 on the screen's
/// pixels (bug note 2026-10-24T14-01-17: a table at a fractional origin read soft, "pixelated"). A body keeps its
/// visible height (it shows a window onto its raster). `pad`: the margin the raster carries round the slot.
pub(crate) fn pixel_frame(cx: f64, rect: crate::island::object::Rect, size: (f64, f64), pad: f64, px: Option<(f64, f64)>, scale: f64, body: bool) -> LayerFrame {
    let k = if size.0 > 1e-6 { rect.w / size.0 } else { 1.0 };
    let pad = pad * k;
    let (mut w, mut h) = (rect.w + 2.0 * pad, rect.h + 2.0 * pad);
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    if let Some((pw, ph)) = px.filter(|(pw, ph)| *pw > 0.0 && *ph > 0.0) {
        w = pw / scale * k;
        if !body {
            h = ph / scale * k;
        }
    }
    let (mut x, mut y) = (cx + rect.x - pad, rect.y - pad);
    if (k - 1.0).abs() < 1e-9 {
        x = (x * scale).round() / scale;
        y = (y * scale).round() / scale;
    }
    LayerFrame { x, y, w, h }
}

/// The raster cache's key for a slot's `content` (its `content.key()` is `key`) drawn at its natural `size`.
fn raster_key(content: &Content, key: u64, size: (f64, f64)) -> u64 {
    match content {
        // A body is drawn once whole: its scroll and visible height only move the window onto it.
        Content::Body { lines, font, color, line_h, notes, marks, veil, .. } => body_key(lines, *font, *color, *line_h, notes, marks, size.0) ^ u64::from(*veil > 0.0),
        // The configured mark (Personalise) is part of the helmet's drawing.
        Content::Helmet { .. } => key ^ super::super::helmet::mark_key().rotate_left(1),
        // A panel is drawn to its slot's size, which its content does not hold: two step chips of one look but
        // different widths shared one raster, and the open tab's white chip ran past the card's edge (found while
        // rendering bug note 2026-10-28T19-45-55's card).
        Content::Panel { .. } => key ^ size.0.to_bits().rotate_left(17) ^ size.1.to_bits().rotate_left(43),
        _ => key,
    }
}

#[cfg(test)]
mod window_tests {
    /// Two panels of one look at different sizes are two rasters (a step chip drawn at another chip's width ran past
    /// the card's edge); the same panel at the same size is one.
    #[test]
    fn a_panel_raster_is_keyed_by_its_size() {
        use crate::island::object::{Content, Rgba};
        let chip = Content::Panel { fill: Rgba::WHITE, ring: None, radius: 13.0 };
        let k = chip.key();
        assert_ne!(super::raster_key(&chip, k, (79.0, 26.0)), super::raster_key(&chip, k, (57.0, 26.0)));
        assert_eq!(super::raster_key(&chip, k, (57.0, 26.0)), super::raster_key(&chip, k, (57.0, 26.0)));
    }
    #[test]
    fn a_body_scrolled_to_zero_shows_its_top_and_scrolls_down() {
        assert_eq!(super::body_window(0.0, 500.0, 1000.0, 2.0), (0.0, 0.5));
        assert_eq!(super::body_window(250.0, 500.0, 1000.0, 2.0), (0.25, 0.5));
        assert_eq!(super::body_window(9e9, 500.0, 1000.0, 2.0), (0.5, 0.5), "never past its end");
        assert_eq!(super::body_window(0.0, 80.0, 80.0, 2.0), (0.0, 1.0), "a body that fits shows whole");
        assert_eq!(super::body_window(f64::NAN, 80.0, 80.0, 2.0), (0.0, 1.0));
    }

    /// Bug note 2026-10-26T19-14-21: the veil is clear at the screen's edge, a dark material under the ears, and gives
    /// way to the sharp lines over the window's first `VEIL_INTO` points.
    #[test]
    fn the_veil_fades_in_under_the_bar_and_out_into_the_window() {
        let bottom = 62.0;
        let a = |y: f64| super::veil_alpha(y, bottom);
        assert_eq!(a(0.0), 0.0, "clear at the screen's edge");
        assert!((a(30.0) - super::VEIL_MAX).abs() < 1e-9, "full under the ears: {}", a(30.0));
        assert_eq!(a(bottom), 0.0, "gone where the sharp lines are whole");
        let mid = bottom - crate::island::object::VEIL_INTO / 2.0;
        assert!(a(mid) > 0.2 && a(mid) < super::VEIL_MAX - 0.2, "crossing into the window: {}", a(mid));
        let mut last = 0.0;
        for k in 0..=40 {
            let v = a(k as f64);
            assert!(v >= last - 1e-12, "rising to the band: {k}");
            last = v;
        }
        assert!((0..=620).all(|k| (0.0..=super::VEIL_MAX).contains(&a(k as f64 / 10.0))));
    }

    /// Bug note 2026-10-24T14-01-17: a scrolled body starts on a whole pixel row (a scroll of 100.3 pt at 2x showed
    /// every line between two rows).
    #[test]
    fn a_scrolled_body_starts_on_a_whole_pixel_row() {
        let (y, _) = super::body_window(100.3, 400.0, 1000.0, 2.0);
        let row = y * 1000.0 * 2.0;
        assert!((row - row.round()).abs() < 1e-9, "row {row}");
        assert_eq!(row, 201.0);
    }

    /// Bug note 2026-10-24T14-01-17: a raster slot at rest is shown 1:1 on whole pixels: its layer is the raster's
    /// pixel size, its origin on a pixel, whatever fraction the layout's rect has.
    #[test]
    fn a_raster_at_rest_lands_on_whole_pixels_at_its_pixel_size() {
        use crate::island::object::Rect;
        // A body 250.8 pt wide at x -125.4 (the 'note_open' reply): a 502 x 80 px raster at 2x.
        let g = super::pixel_frame(390.0, Rect::new(-125.4, 40.25, 250.8, 40.0), (250.8, 40.0), 0.0, Some((502.0, 80.0)), 2.0, true);
        assert_eq!((g.w * 2.0, g.h * 2.0), (502.0, 80.0), "shown at its own pixels");
        for v in [g.x, g.y] {
            assert!((v * 2.0 - (v * 2.0).round()).abs() < 1e-9, "origin {v} on a pixel");
        }
        assert!((g.x - (390.0 - 125.4)).abs() <= 0.25 && (g.y - 40.25).abs() <= 0.25, "moved under half a pixel");
        // A padded text slot: the raster is (w + 2 PAD) pt rounded up to whole pixels.
        let pad = super::PAD;
        let px = (((114.953 + 2.0 * pad) * 2.0).ceil(), ((22.0 + 2.0 * pad) * 2.0).ceil());
        let g = super::pixel_frame(390.0, Rect::new(-140.0, 239.0, 114.953, 22.0), (114.953, 22.0), pad, Some(px), 2.0, false);
        assert_eq!((g.w * 2.0, g.h * 2.0), px);
        assert_eq!(g.x, 390.0 - 140.0 - pad);
        // Mid-motion, scaled to 0.5: half its pixels, unsnapped (it moves continuously).
        let g = super::pixel_frame(390.0, Rect::new(-30.1, 10.3, 57.4765, 11.0), (114.953, 22.0), pad, Some(px), 2.0, false);
        assert!((g.w - px.0 / 2.0 * 0.5).abs() < 1e-9);
        assert!((g.x - (390.0 - 30.1 - pad * 0.5)).abs() < 1e-9);
        // At 1x the origin snaps to whole points.
        let g = super::pixel_frame(390.0, Rect::new(-125.4, 40.25, 250.8, 40.0), (250.8, 40.0), 0.0, Some((251.0, 40.0)), 1.0, true);
        assert_eq!((g.x, g.y, g.w), (265.0, 40.0, 251.0));
    }
}

/// A body's raster key: its lines, its notes (each drawing's revision) and look at its width, not its scroll.
fn body_key(lines: &[crate::island::object::prose::TextRow], font: crate::island::object::Font, color: crate::island::object::Rgba, line_h: f64, notes: &[crate::island::object::Placed], marks: &[crate::island::object::Mark], w: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("body {lines:?} {font:?} {color:?} {line_h} {notes:?} {marks:?} {w}").hash(&mut h);
    h.finish()
}

/// The island's layer tree.
pub(crate) struct Tree {
    pub root: Retained<CALayer>,
    shape: Retained<CAShapeLayer>,
    /// The drop's glow round the island (`island::drop::Swell`), under the shape.
    drop_glow: Retained<CAShapeLayer>,
    /// A drag over the island: its shape swollen and lit (set by the drop target; content and its mask stay put).
    pub drop: Option<crate::island::drop::Swell>,
    content: Retained<CALayer>,
    mask: Retained<CAShapeLayer>,
    slots: HashMap<Arc<str>, SlotLayer>,
    /// Image views need the main thread (always the case: the window and the gallery run there).
    mtm: Option<MainThreadMarker>,
    /// Image views made ahead (`prewarm`), by content key: a state's symbols are set up when it arrives.
    spares: HashMap<u64, Retained<NSImageView>>,
    pub rasters: Rasters,
    /// The veil's fade masks, by the band's height (quarter points).
    ramps: HashMap<u64, CFRetained<CGImage>>,
    pub scale: f64,
    pub size: (f64, f64),
    /// The notch's centre line in the tree (the window's middle).
    pub cx: f64,
    /// The last `apply`'s `CATransaction::commit` in milliseconds (lab timing).
    pub last_commit_ms: f64,
    /// The dots and the spinner loop as their layers' own animation (`loop_layers`): the island's host, whose clock
    /// is then not ticked for them. The gallery steps virtual time frame by frame and keeps drawing each phase.
    pub loop_layers: bool,
}

impl Tree {
    /// A tree `size` points large (the island window), drawn at `scale`.
    pub(crate) fn new(size: (f64, f64), scale: f64) -> Tree {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // Flipped for `render_into` (offscreen: no hosting view). A hosted root takes its flip from the view
        // (`HostView::isFlipped`); AppKit resets this one while the layer is hosted.
        let root = CALayer::new();
        root.setGeometryFlipped(true);
        root.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        let white = CGColor::new_srgb(1.0, 1.0, 1.0, 1.0);
        let shape = CAShapeLayer::new();
        shape.setFillColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
        let content = CALayer::new();
        content.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        let mask = CAShapeLayer::new();
        mask.setFillColor(Some(&white));
        mask.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        unsafe { content.setMask(Some(&mask)) };
        shape.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        shape.setLineWidth(1.0);
        // The drop target's halo (mac/drop: a cool white light, soft and wide), on the island's own shape.
        let drop_glow = CAShapeLayer::new();
        drop_glow.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        drop_glow.setFillColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
        drop_glow.setShadowColor(Some(&CGColor::new_srgb(0.80, 0.88, 1.0, 1.0)));
        drop_glow.setShadowOffset(CGSize::new(0.0, 0.0));
        drop_glow.setShadowRadius(14.0);
        drop_glow.setShadowOpacity(0.0);
        drop_glow.setHidden(true);
        root.addSublayer(&drop_glow);
        root.addSublayer(&shape);
        root.addSublayer(&content);
        CATransaction::commit();
        Tree { root, shape, drop_glow, drop: None, content, mask, slots: HashMap::new(), mtm: MainThreadMarker::new(), spares: HashMap::new(), rasters: Rasters::default(), ramps: HashMap::new(), scale, size, cx: size.0 / 2.0, last_commit_ms: 0.0, loop_layers: false }
    }

    /// The island's black shape as drawn (its path's bounds, points): checks.
    pub(crate) fn shape_size(&self) -> (f64, f64) {
        let b = self.shape.path().map(|p| CGPath::bounding_box(Some(&p))).unwrap_or(CGRect::ZERO);
        (b.size.width, b.size.height)
    }

    /// Paints `f`. The only thing the live window and the renders call per frame.
    pub(crate) fn apply(&mut self, f: &FrameModel) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let visible = f.visible;
        self.root.setHidden(!visible);
        if visible {
            let path = outline(&f.shape, self.cx);
            let p: &CGPath = &path;
            self.mask.setPath(Some(p));
            match self.drop {
                Some(d) => {
                    let swollen = outline(&d.on(f.shape), self.cx);
                    let sp: &CGPath = &swollen;
                    self.shape.setPath(Some(sp));
                    self.drop_glow.setPath(Some(sp));
                    self.drop_glow.setShadowOpacity((0.55 * d.glow) as f32);
                    self.shape.setStrokeColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.16 * d.glow)));
                }
                None => {
                    self.shape.setPath(Some(p));
                    self.shape.setStrokeColor(None);
                }
            }
            self.drop_glow.setHidden(self.drop.is_none_or(|d| d.glow <= 0.002));
        }
        let mut seen: Vec<&Arc<str>> = Vec::with_capacity(f.slots.len());
        for slot in &f.slots {
            seen.push(&slot.id);
            self.slot(slot, f.t);
        }
        let stale: Vec<Arc<str>> = self.slots.keys().filter(|k| !seen.contains(k)).cloned().collect();
        for k in stale {
            if let Some(s) = self.slots.remove(&k) {
                s.layer.removeFromSuperlayer();
            }
        }
        let commit = std::time::Instant::now();
        CATransaction::commit();
        self.last_commit_ms = commit.elapsed().as_secs_f64() * 1000.0;
    }

    fn slot(&mut self, s: &Slot, t: f64) {
        let scale = self.scale;
        let live = matches!(*s.content, Content::Dots | Content::Spinner | Content::Waveform { .. });
        let symbol = matches!(*s.content, Content::Symbol { .. }) && self.mtm.is_some();
        let kind = |b: &Body| match b {
            Body::Live(_) => 1,
            Body::Symbol { .. } => 2,
            Body::Image { .. } => 0,
        };
        let want = if live { 1 } else if symbol { 2 } else { 0 };
        // A slot that changes kind (a spinner becoming a symbol under the same id) gets a fresh layer.
        if self.slots.get(&s.id).is_some_and(|l| kind(&l.body) != want) {
            if let Some(old) = self.slots.remove(&s.id) {
                old.layer.removeFromSuperlayer();
            }
        }
        let mtm = self.mtm;
        let spare = if symbol && !self.slots.contains_key(&s.id) { self.spares.remove(&s.key) } else { None };
        let sl = self.slots.entry(s.id.clone()).or_insert_with(|| {
            let layer = CALayer::new();
            let body = if live {
                Body::Live(vec![])
            } else if let (true, Some(mtm)) = (symbol, mtm) {
                let ready = spare.is_some();
                let view = spare.unwrap_or_else(|| new_view(mtm));
                let blurred = CALayer::new();
                blurred.setContentsScale(scale);
                layer.addSublayer(&blurred);
                Body::Symbol { view, blurred, has_blur: false, played: false, changed_at: t, ready, name: String::new() }
            } else {
                let sharp = CALayer::new();
                let blurred = CALayer::new();
                for l in [&*sharp, &*blurred] {
                    l.setContentsScale(scale);
                    layer.addSublayer(l);
                }
                Body::Image { sharp, blurred, has_blur: false }
            };
            // The pulled-down turn's body is drawn without the pad (its image is exactly the body).
            let note = matches!(*s.content, Content::Body { .. });
            SlotLayer { layer, body, key: u64::MAX, padded: !live && !note, px: None, mask: None, looped: None }
        });
        // Bodies under everything; the veil over them (and over the sharp lines' fade, which it takes over) but under
        // the ears, the status row and the controls.
        let veil = match &*s.content {
            Content::Body { veil, .. } => *veil,
            _ => 0.0,
        };
        let z = match &*s.content {
            _ if veil > 0.0 => 0.5,
            Content::Body { .. } => 0.0,
            // A body's fades over its lines (they were under a body that arrived after them: no fade showed).
            Content::Fade { .. } | Content::FadeTop { .. } => 0.25,
            _ => 1.0,
        };
        if sl.layer.zPosition() != z {
            sl.layer.setZPosition(z);
        }
        if let Body::Symbol { .. } = sl.body {
            symbol_slot(sl, s, t, &mut self.rasters, scale);
            if sl.layer.superlayer().is_none() {
                self.content.addSublayer(&sl.layer);
            }
            sl.layer.setPosition(CGPoint::new(self.cx + s.rect.mid_x(), s.rect.mid_y()));
            return;
        }
        if sl.layer.superlayer().is_none() {
            self.content.addSublayer(&sl.layer);
        }
        // The raster first: its pixel size sets the layer's (bug note 2026-10-24T14-01-17, the table looked pixelated).
        if let Body::Image { sharp, blurred, has_blur } = &mut sl.body {
            // A panel is drawn to its slot's size, which its key does not hold: one that grows under the same id (the
            // composer making room for a drop, 30 Oct) needs the raster of its new size, or it keeps its old height.
            let key = if matches!(&*s.content, Content::Panel { .. }) { raster_key(&s.content, s.key, s.size) } else { s.key };
            if sl.key != key {
                let r = self.rasters.get(s, scale);
                set_image(sharp, r.as_ref().map(|r| &*r.image));
                set_image(blurred, r.as_ref().and_then(|r| r.blurred.as_deref()));
                *has_blur = r.as_ref().is_some_and(|r| r.blurred.is_some());
                sl.px = r.as_ref().map(|r| (CGImage::width(Some(&r.image)) as f64, CGImage::height(Some(&r.image)) as f64));
                sl.key = key;
            }
        }
        // Geometry: the slot's rect (the raster carries `PAD` round it), on whole pixels at rest.
        let body = matches!(&*s.content, Content::Body { .. });
        // A veil is a blurred picture at its own low scale: shown at the slot's size, not its pixels.
        let px = if matches!(sl.body, Body::Image { .. }) && veil <= 0.0 { sl.px } else { None };
        let mut g = pixel_frame(self.cx, s.rect, s.size, if sl.padded { PAD } else { 0.0 }, px, scale, body);
        if veil > 0.0 {
            // The veil's raster carries its blur's margin either side.
            let k = if s.size.0 > 1e-6 { s.rect.w / s.size.0 } else { 1.0 };
            g.x -= raster::VEIL_PAD * k;
            g.w += 2.0 * raster::VEIL_PAD * k;
        }
        let (w, h) = (g.w, g.h);
        sl.layer.setBounds(cg_rect(0.0, 0.0, w, h));
        sl.layer.setPosition(CGPoint::new(g.x + w / 2.0, g.y + h / 2.0));
        sl.layer.setOpacity(s.opacity as f32);
        match &mut sl.body {
            Body::Image { sharp, blurred, has_blur } => {
                let b = if *has_blur { (s.blur / BLUR).clamp(0.0, 1.0) } else { 0.0 };
                for l in [&**sharp, &**blurred] {
                    l.setFrame(cg_rect(0.0, 0.0, w, h));
                }
                if let c @ Content::Body { h: visible, scroll, .. } = &*s.content {
                    // The turn's body: the visible part of its whole raster, starting on a whole pixel row.
                    let full = px.map_or(c.body_full(), |(_, ph)| ph / scale);
                    let (y, vh) = body_window(*scroll, *visible, full, scale);
                    sharp.setContentsRect(cg_rect(0.0, y, 1.0, vh));
                }
                if veil > 0.0 {
                    let mask = sl.mask.get_or_insert_with(CALayer::new);
                    let key = (veil * 4.0).round() as u64;
                    if !self.ramps.contains_key(&key) {
                        if let Some(img) = veil_ramp(veil, scale) {
                            self.ramps.insert(key, img);
                        }
                    }
                    set_image(mask, self.ramps.get(&key).map(|i| &**i));
                    mask.setContentsScale(scale);
                    // The ramp is in island coordinates: its top at the shape's top, whatever part this body covers.
                    mask.setFrame(cg_rect(0.0, -g.y, w, veil));
                    if sl.layer.mask().is_none() {
                        unsafe { sl.layer.setMask(Some(mask)) };
                    }
                }
                sharp.setOpacity((1.0 - b) as f32);
                blurred.setOpacity(b as f32);
            }
            Body::Live(parts) => {
                sl.key = s.key;
                live_parts(&sl.layer, parts, s, w, h);
                if self.loop_layers {
                    loop_layers(&sl.layer, parts, &mut sl.looped, s);
                }
            }
            Body::Symbol { .. } => {}
        }
    }

    /// Wakes the symbol renderer. The image views' symbol layers are drawn by RenderBox, which sets itself up (Metal
    /// device, render thread, surfaces) inside the first commit that holds a symbol layer and drops part of that again
    /// after a second or so without one: measured 17–25 ms on the main thread in the commit that brought the first
    /// symbol of a process into the island, 10–13 ms for the first symbol after the island rested a few seconds,
    /// under 1.5 ms otherwise. Committing one fresh symbol view in the tree while the window is ordered out pays it
    /// before a motion starts; the view is taken out in the same turn. Returns whether it ran.
    pub(crate) fn wake_renderer(&mut self) -> bool {
        let Some(mtm) = self.mtm else { return false };
        let view = new_view(mtm);
        view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(24.0, 24.0)));
        view.setSymbolConfiguration(Some(&symbol_config(12.0, false, &[crate::island::object::Rgba::WHITE])));
        let image = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(&objc2_foundation::NSString::from_str("circle.fill"), None);
        view.setImage(image.as_deref());
        prime(&view);
        view.display();
        let Some(layer) = view.layer() else { return false };
        let (hidden, path) = (self.root.isHidden(), self.mask.path());
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setPosition(CGPoint::new(self.cx, 16.0));
        self.content.addSublayer(&layer);
        let open = unsafe { CGPath::with_rect(cg_rect(0.0, 0.0, self.size.0, 32.0), std::ptr::null()) };
        self.mask.setPath(Some(&open));
        self.root.setHidden(false);
        CATransaction::commit();
        CATransaction::flush();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.removeFromSuperlayer();
        self.mask.setPath(path.as_deref());
        self.root.setHidden(hidden);
        CATransaction::commit();
        true
    }

    /// The content layer's sublayer count and the mask's path (checks).
    pub(crate) fn content_layers(&self) -> usize {
        unsafe { self.content.sublayers() }.map_or(0, |l| l.count())
    }

    pub(crate) fn mask_path(&self) -> Option<Retained<CGPath>> {
        self.mask.path()
    }

    /// Draws every raster `f` will need now, so a state's first frame does no drawing (text and symbols are drawn
    /// when the state arrives, not when their slot first becomes visible mid-motion).
    pub(crate) fn prewarm(&mut self, f: &FrameModel) {
        for s in &f.slots {
            if !matches!(*s.content, Content::Dots | Content::Spinner | Content::Waveform { .. }) {
                let _ = self.rasters.get(s, self.scale);
            }
            // A symbol that will need a new image view gets one now, set up and drawn.
            if let (Content::Symbol { .. }, Some(mtm)) = (&*s.content, self.mtm) {
                let showing = self.slots.get(&s.id).is_some_and(|l| matches!(l.body, Body::Symbol { .. }));
                if !showing && !self.spares.contains_key(&s.key) {
                    if self.spares.len() >= 16 {
                        self.spares.clear();
                    }
                    let view = new_view(mtm);
                    setup_view(&view, s);
                    self.spares.insert(s.key, view);
                }
            }
        }
    }


    /// Renders the tree into `canvas` (offscreen: stills, the gallery, the judge), the window's top-left at the
    /// canvas's top-left plus `offset` points.
    pub(crate) fn render_into(&self, canvas: &raster::Canvas, offset: (f64, f64)) {
        let ctx = &canvas.ctx;
        objc2_core_graphics::CGContext::save_g_state(Some(ctx));
        // The canvas is y down and the root's geometry is flipped (y down): no further flip.
        objc2_core_graphics::CGContext::translate_ctm(Some(ctx), offset.0, offset.1);
        self.root.renderInContext(ctx);
        objc2_core_graphics::CGContext::restore_g_state(Some(ctx));
    }

    /// Lab check (`--island-loops`): where the first looping slot's own animation is in its cycle now (0–1), by
    /// Core Animation's clock and its `beginTime`, to compare with the model's phase on a live clock.
    pub(crate) fn loop_phase(&self) -> Option<f64> {
        let key = NSString::from_str(LOOP_KEY);
        self.slots.values().find_map(|sl| {
            let Body::Live(parts) = &sl.body else { return None };
            let begin = unsafe { parts.first()?.animationForKey(&key) }?.beginTime();
            let now = sl.layer.convertTime_fromLayer(CACurrentMediaTime(), None);
            Some(((now - begin) / LOOP_PERIOD).rem_euclid(1.0))
        })
    }

    /// Lab check (`--island-loops`): the tree as the render server shows it with every looping slot's own animation
    /// paused `elapsed` seconds into its cycle (the slot's layer at speed 0), over the checks' desktop grey, to
    /// `path`. False when no slot loops on its own.
    pub(crate) fn loop_still(&self, elapsed: f64, path: &std::path::Path) -> bool {
        let key = NSString::from_str(LOOP_KEY);
        let paused: Vec<&SlotLayer> = self
            .slots
            .values()
            .filter(|sl| matches!(&sl.body, Body::Live(parts) if parts.first().is_some_and(|l| unsafe { l.animationForKey(&key) }.is_some())))
            .collect();
        if paused.is_empty() {
            return false;
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for sl in &paused {
            let Body::Live(parts) = &sl.body else { continue };
            let begin = unsafe { parts[0].animationForKey(&key) }.map_or(0.0, |a| a.beginTime());
            sl.layer.setSpeed(0.0);
            sl.layer.setTimeOffset(begin + LOOP_PERIOD + elapsed);
        }
        CATransaction::commit();
        CATransaction::flush();
        let (w, h) = self.size;
        let drawn = raster::Canvas::new(w, h, self.scale).and_then(|c| {
            c.draw(|| {
                objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.36, 0.40, 0.50, 1.0).setFill();
                objc2_app_kit::NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
            });
            let shown = unsafe { self.root.presentationLayer() }?;
            shown.renderInContext(&c.ctx);
            c.png()
        });
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for sl in &paused {
            sl.layer.setSpeed(1.0);
            sl.layer.setTimeOffset(0.0);
        }
        CATransaction::commit();
        drawn.is_some_and(|png| std::fs::write(path, png).is_ok())
    }
}

fn class(name: &std::ffi::CStr) -> Option<&'static AnyClass> {
    AnyClass::get(name)
}

/// The image view's configuration for `content`: point size, weight and palette (as `raster::symbol` draws it).
fn symbol_config(size: f64, bold: bool, palette: &[crate::island::object::Rgba]) -> Retained<NSImageSymbolConfiguration> {
    let weight = unsafe { if bold { objc2_app_kit::NSFontWeightBold } else { objc2_app_kit::NSFontWeightSemibold } };
    let colors: Vec<Retained<objc2_app_kit::NSColor>> = palette.iter().map(|c| raster::color(*c)).collect();
    NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight)
        .configurationByApplyingConfiguration(&NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&colors)))
}

fn new_view(mtm: MainThreadMarker) -> Retained<NSImageView> {
    let view = NSImageView::new(mtm);
    view.setWantsLayer(true);
    view.setImageScaling(NSImageScaling::ScaleNone);
    view
}

/// A new image view showing `s`'s symbol at its natural size (plus the pad), primed and drawn.
fn setup_view(view: &NSImageView, s: &Slot) {
    let Content::Symbol { name, size, palette, bold } = &*s.content else { return };
    let (w0, h0) = (s.size.0 + 2.0 * PAD, s.size.1 + 2.0 * PAD);
    view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w0, h0)));
    view.setSymbolConfiguration(Some(&symbol_config(*size, *bold, palette)));
    let image = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(&objc2_foundation::NSString::from_str(name), None);
    view.setImage(image.as_deref());
    view.layoutSubtreeIfNeeded();
    // Out of any window nothing displays it: it builds its symbol layer when primed and draws now.
    prime(view);
    view.display();
}

/// Makes an image view out of any window build its symbol layer: an effect applied without animation (nothing
/// moves) and removed at once. Without it the view draws nothing until its first animated effect.
fn prime(view: &NSImageView) {
    let (Some(cls), Some(opts)) = (class(c"NSSymbolBounceEffect"), class(c"NSSymbolEffectOptions")) else { return };
    unsafe {
        let effect: Retained<AnyObject> = msg_send![cls, effect];
        let options: Retained<AnyObject> = msg_send![opts, optionsWithNonRepeating];
        let _: () = msg_send![view, addSymbolEffect: &*effect, options: &*options, animated: false];
        let _: () = msg_send![view, removeAllSymbolEffectsWithOptions: &*options, animated: false];
    }
}

/// The system's replace transition from the symbol `from` to `to`: the magic replace between a symbol and its slashed
/// twin ("mic" ⇄ "mic.slash": the slash is drawn or lifted, as Control Center does), down-up for anything else.
fn replace_transition(from: &str, to: &str) -> Option<Retained<AnyObject>> {
    let cls = class(c"NSSymbolReplaceContentTransition")?;
    let down_up: Retained<AnyObject> = unsafe { msg_send![cls, replaceDownUpTransition] };
    if !slash_pair(from, to) {
        return Some(down_up);
    }
    let magic = objc2::sel!(magicTransitionWithFallback:);
    let responds: bool = unsafe { msg_send![cls, respondsToSelector: magic] };
    if !responds {
        return Some(down_up);
    }
    Some(unsafe { msg_send![cls, magicTransitionWithFallback: &*down_up] })
}

/// Whether one symbol is the other slashed.
pub(crate) fn slash_pair(a: &str, b: &str) -> bool {
    a.strip_suffix(".slash") == Some(b) || b.strip_suffix(".slash") == Some(a)
}

/// The system's Reduce Motion setting (Accessibility ▸ Display), read when a symbol changes (no observer, no cost at rest).
pub(crate) fn reduce_motion() -> bool {
    let Some(cls) = class(c"NSWorkspace") else { return false };
    unsafe {
        let ws: Option<Retained<AnyObject>> = msg_send![cls, sharedWorkspace];
        ws.is_some_and(|ws| msg_send![&*ws, accessibilityDisplayShouldReduceMotion])
    }
}

/// Starts the system's effect `fx` on `view` (showing the symbol `name`): once (bounce, wiggle) or repeating (rotate,
/// breathe, variable colour).
fn play_effect(view: &NSImageView, fx: Fx, name: &str) {
    if fx == Fx::Variable {
        if let Some(effect) = variable_color(name) {
            if let Some(opts) = class(c"NSSymbolEffectOptions") {
                unsafe {
                    let options: Retained<AnyObject> = msg_send![opts, optionsWithRepeating];
                    let _: () = msg_send![view, addSymbolEffect: &*effect, options: &*options, animated: true];
                }
                return;
            }
        }
        return play_effect(view, Fx::Breathe, name);
    }
    let (cls, repeating) = match fx {
        Fx::None => return,
        Fx::Bounce => (class(c"NSSymbolBounceEffect"), false),
        Fx::Wiggle => (class(c"NSSymbolWiggleEffect").or_else(|| class(c"NSSymbolBounceEffect")), false),
        Fx::Rotate => (class(c"NSSymbolRotateEffect"), true),
        Fx::Breathe | Fx::Variable => (class(c"NSSymbolBreatheEffect").or_else(|| class(c"NSSymbolPulseEffect")), true),
    };
    let (Some(cls), Some(opts)) = (cls, class(c"NSSymbolEffectOptions")) else { return };
    unsafe {
        let effect: Retained<AnyObject> = msg_send![cls, effect];
        let options: Retained<AnyObject> = if repeating { msg_send![opts, optionsWithRepeating] } else { msg_send![opts, optionsWithNonRepeating] };
        let _: () = msg_send![view, addSymbolEffect: &*effect, options: &*options, animated: true];
    }
}

/// The system's variable colour for `name`, one layer lit after another and back, the others dimmed; `None` without
/// the class (or its options) or for a symbol without variable-colour layers (`VARIABLE_SYMBOLS`).
fn variable_color(name: &str) -> Option<Retained<AnyObject>> {
    if !crate::island::object::VARIABLE_SYMBOLS.contains(&name) {
        return None;
    }
    let cls = class(c"NSSymbolVariableColorEffect")?;
    let responds = |e: &AnyObject, sel: objc2::runtime::Sel| -> bool { unsafe { msg_send![e, respondsToSelector: sel] } };
    unsafe {
        let effect: Retained<AnyObject> = msg_send![cls, effect];
        if !responds(&effect, objc2::sel!(effectWithIterative)) {
            return None;
        }
        let effect: Retained<AnyObject> = msg_send![&*effect, effectWithIterative];
        if !responds(&effect, objc2::sel!(effectWithReversing)) {
            return None;
        }
        let effect: Retained<AnyObject> = msg_send![&*effect, effectWithReversing];
        if !responds(&effect, objc2::sel!(effectWithDimInactiveLayers)) {
            return None;
        }
        let effect: Retained<AnyObject> = msg_send![&*effect, effectWithDimInactiveLayers];
        Some(effect)
    }
}

/// One frame of a symbol slot: the image view placed, sized and faded; a new symbol under the same id turns into it
/// with the system's replace transition (down-up); the arrival effect starts once the slot has visibly arrived and
/// a held state's effect repeats while the content stays.
fn symbol_slot(sl: &mut SlotLayer, s: &Slot, t: f64, rasters: &mut Rasters, scale: f64) {
    let Content::Symbol { name, size, palette, bold } = &*s.content else { return };
    let Body::Symbol { view, blurred, has_blur, played, changed_at, ready, name: shown } = &mut sl.body else { return };
    let (w0, h0) = (s.size.0 + 2.0 * PAD, s.size.1 + 2.0 * PAD);
    if sl.key != s.key {
        let first = sl.key == u64::MAX;
        if first && !*ready {
            setup_view(view, s);
        } else if !first {
            let config = symbol_config(*size, *bold, palette);
            let image = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(&objc2_foundation::NSString::from_str(name), None);
            unsafe {
                let _: () = msg_send![&**view, removeAllSymbolEffects];
            }
            view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w0, h0)));
            view.setSymbolConfiguration(Some(&config));
            let transition = if reduce_motion() { None } else { replace_transition(shown, name) };
            match (&image, transition) {
                (Some(img), Some(tr)) => unsafe {
                    let _: () = msg_send![&**view, setSymbolImage: &**img, withContentTransition: &*tr];
                },
                _ => view.setImage(image.as_deref()),
            }
            view.display();
        }
        *shown = name.clone();
        // A replace is the new symbol's arrival: a held state's effect still starts below.
        *played = !first && !s.fx.repeats();
        *changed_at = t;
        let r = rasters.get(s, scale);
        set_image(blurred, r.as_ref().and_then(|r| r.blurred.as_deref()));
        *has_blur = r.as_ref().is_some_and(|r| r.blurred.is_some());
        sl.layer.setBounds(cg_rect(0.0, 0.0, w0, h0));
        blurred.setFrame(cg_rect(0.0, 0.0, w0, h0));
        if let Some(layer) = view.layer() {
            if layer.superlayer().is_none() {
                sl.layer.insertSublayer_atIndex(&layer, 0);
            }
        }
        sl.key = s.key;
    }
    if !*played && s.arrived && s.fx != Fx::None {
        // Reduce Motion: a one-off bounce or wiggle does not play, nor catching up's layers (its words and the still
        // stack say it); a restart's loop still says so.
        if !(reduce_motion() && matches!(s.fx, Fx::Bounce | Fx::Wiggle | Fx::Variable)) {
            play_effect(view, s.fx, name);
        }

        *played = true;
        *changed_at = t;
    }
    // Size by the container's scale (the view keeps its natural size; AppKit lays its layer out from its frame).
    let k = s.rect.w / s.size.0.max(1e-6);
    sl.layer.setAffineTransform(CGAffineTransform { a: k, b: 0.0, c: 0.0, d: k, tx: 0.0, ty: 0.0 });
    sl.layer.setOpacity(s.opacity as f32);
    let b = if *has_blur { (s.blur / BLUR).clamp(0.0, 1.0) } else { 0.0 };
    view.setAlphaValue(1.0 - b);
    blurred.setOpacity(b as f32);
    // Out of any window, the view draws (and steps a replace transition) only when told to.
    if t - *changed_at < 1.0 {
        view.displayIfNeeded();
    }
}

/// Dots (an iterative wave), the waveform's bars and the spinner's spokes as a few plain layers.
fn live_parts(host: &CALayer, parts: &mut Vec<Retained<CALayer>>, s: &Slot, w: f64, h: f64) {
    let (n, white) = match &*s.content {
        Content::Dots => (3, CGColor::new_srgb(1.0, 1.0, 1.0, 1.0)),
        Content::Waveform { .. } => (5, CGColor::new_srgb(1.0, 1.0, 1.0, 1.0)),
        Content::Spinner => (8, CGColor::new_srgb(1.0, 1.0, 1.0, 1.0)),
        _ => return,
    };
    while parts.len() < n {
        let l = CALayer::new();
        l.setBackgroundColor(Some(&white));
        host.addSublayer(&l);
        parts.push(l);
    }
    match &*s.content {
        Content::Dots => {
            // Three 8 pt dots, 6 apart; brightness 1 / 0.5 / 0.25 travelling along, 0.9 s a cycle (phase-locked).
            let d = 8.0f64.min(h);
            let gap = 6.0;
            let total = 3.0 * d + 2.0 * gap;
            for (i, l) in parts.iter().enumerate() {
                let x = w / 2.0 - total / 2.0 + i as f64 * (d + gap);
                l.setFrame(cg_rect(x, h / 2.0 - d / 2.0, d, d));
                l.setCornerRadius(d / 2.0);
                l.setOpacity(loop_opacity(&s.content, i, s.phase));
            }
        }
        Content::Waveform { level, flat } => {
            // Five 3 pt bars, 3 apart, 3–18 pt tall: the voice's level with a fixed per-bar shape; flat at 3 pt.
            let lv = if *flat { 0.0 } else { (*level as f64 / 1000.0).clamp(0.0, 1.0) };
            let shape = [0.45, 0.8, 1.0, 0.7, 0.4];
            for (i, l) in parts.iter().enumerate() {
                let bh = 3.0 + 15.0 * (lv * shape[i]).sqrt();
                let x = w / 2.0 - 13.5 + i as f64 * 6.0;
                l.setFrame(cg_rect(x, h / 2.0 - bh / 2.0, 3.0, bh));
                l.setCornerRadius(1.5);
                l.setOpacity(1.0);
            }
        }
        Content::Spinner => {
            let r = (w.min(h) / 2.0).max(1.0);
            for (i, l) in parts.iter().enumerate() {
                let a = i as f64 / 8.0 * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
                let (cx, cy) = (w / 2.0 + a.cos() * r * 0.75, h / 2.0 + a.sin() * r * 0.75);
                l.setBounds(cg_rect(0.0, 0.0, r * 0.5, 2.0));
                l.setPosition(CGPoint::new(cx, cy));
                l.setCornerRadius(1.0);
                l.setAffineTransform(CGAffineTransform { a: a.cos(), b: a.sin(), c: -a.sin(), d: a.cos(), tx: 0.0, ty: 0.0 });
                l.setOpacity(loop_opacity(&s.content, i, s.phase));
            }
        }
        _ => {}
    }
}

/// Part `i`'s opacity at `phase` (0–1 of `LOOP_PERIOD`) of a looping content: the dots' brightness 1 / 0.5 / 0.25
/// travelling along, the spinner's spokes fading round in eight steps.
fn loop_opacity(content: &Content, i: usize, phase: f64) -> f32 {
    match content {
        Content::Dots => {
            let step = (phase * 3.0).floor() as usize % 3;
            let k = phase * 3.0 - (phase * 3.0).floor();
            let level = |shift: usize| [1.0, 0.5, 0.25][(i + 3 - (step + shift) % 3) % 3];
            (level(0) + (level(1) - level(0)) * smooth01(k)) as f32
        }
        Content::Spinner => {
            let step = (phase * 8.0).floor() as usize % 8;
            (0.2 + 0.8 * ((i + 8 - step) % 8) as f64 / 7.0) as f32
        }
        _ => 1.0,
    }
}

/// The loop's cycle (the object's phase: `(t - since) / 0.9`).
const LOOP_PERIOD: f64 = 0.9;
/// The animation key of a part's loop.
const LOOP_KEY: &str = "loop";

/// Steps a cycle of each looping content: the dots at 30 a second (as the 30 fps timer that drove them drew them),
/// the spinner at its eight spoke steps.
fn loop_steps(content: &Content) -> Option<usize> {
    match content {
        Content::Dots => Some(27),
        Content::Spinner => Some(8),
        _ => None,
    }
}

/// The dots and the spinner loop on their own: each part gets one discrete opacity keyframe animation, repeating
/// forever, its steps the values `loop_opacity` gives, its cycle phase-locked to the slot's `since`. The render server
/// plays it, so while only a loop moves nothing is ticked or committed (GPU trace 28 Oct 20:05: the whole island tree
/// was committed 30 times a second while the agent worked). Made once per loop (a new `since` or part count remakes
/// it); the model values `live_parts` sets stay what an offscreen still draws.
fn loop_layers(host: &CALayer, parts: &[Retained<CALayer>], looped: &mut Option<(f64, usize)>, s: &Slot) {
    let key = NSString::from_str(LOOP_KEY);
    let Some(steps) = loop_steps(&s.content) else {
        if looped.take().is_some() {
            parts.iter().for_each(|l| l.removeAnimationForKey(&key));
        }
        return;
    };
    let n = parts.len();
    let want = Some((s.since, n));
    if *looped == want && parts.iter().all(|l| unsafe { l.animationForKey(&key) }.is_some()) {
        return;
    }
    *looped = want;
    // The cycle began `phase` periods ago, in the parts' time (their host's: they keep its timing).
    let now = host.convertTime_fromLayer(CACurrentMediaTime(), None);
    let begin = now - s.phase * LOOP_PERIOD;
    let times: Vec<Retained<NSNumber>> = (0..=steps).map(|j| NSNumber::new_f64(j as f64 / steps as f64)).collect();
    let times = NSArray::from_retained_slice(&times);
    let path = NSString::from_str("opacity");
    for (i, l) in parts.iter().enumerate() {
        let values: Vec<Retained<AnyObject>> = (0..steps).map(|j| Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_f32(loop_opacity(&s.content, i, j as f64 / steps as f64)))))).collect();
        let values: Retained<NSArray> = NSArray::from_retained_slice(&values);
        let a = CAKeyframeAnimation::animationWithKeyPath(Some(&path));
        unsafe { a.setValues(Some(&values)) };
        a.setKeyTimes(Some(&times));
        a.setCalculationMode(unsafe { kCAAnimationDiscrete });
        a.setDuration(LOOP_PERIOD);
        a.setRepeatCount(f32::INFINITY);
        a.setBeginTime(begin);
        // Discrete steps: the dots change 30 times a second at most, the spinner about nine.
        a.setPreferredFrameRateRange(CAFrameRateRange { minimum: 10.0, maximum: 30.0, preferred: 30.0 });
        l.addAnimation_forKey(&a, Some(&key));
    }
}

fn smooth01(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

// ------------------------------------------------------------------ the drawing light

/// Drawing mode's light: one continuous line round the island's outline and the screen's edge. It runs from the
/// island's flares along the top edge, down the sides and along the bottom to its centre (`run`), while the island's
/// outline is lit with it; one path, so the island's light and the screen's meet without a seam. A soft white glow
/// (a blurred shadow of the line) round a hairline rim; the capture's white flash over the screen.
pub(crate) struct LightLayers {
    pub root: Retained<CALayer>,
    glow: Retained<CAShapeLayer>,
    core: Retained<CAShapeLayer>,
    flash: Retained<CALayer>,
    size: (f64, f64),
    corner: f64,
    /// The shape the path was built for, and the path's lengths: (edge on one side, island outline, total).
    built: Option<(Shape, f64, f64, f64)>,
}

/// Length of a path given as points (flattened).
fn poly_len(pts: &[(f64, f64)]) -> f64 {
    pts.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).sum()
}

fn cubic(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), out: &mut Vec<(f64, f64)>) {
    for i in 1..=12 {
        let t = i as f64 / 12.0;
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        out.push((a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1));
    }
}

impl LightLayers {
    /// For a display `size` points large (y down from its top) with top corners of `corner` points.
    pub(crate) fn new(size: (f64, f64), corner: f64) -> LightLayers {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let root = CALayer::new();
        root.setGeometryFlipped(true);
        root.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        let white = CGColor::new_srgb(1.0, 1.0, 1.0, 1.0);
        let line = |width: f64, alpha: f64| {
            let l = CAShapeLayer::new();
            l.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
            l.setFillColor(None);
            l.setStrokeColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, alpha)));
            l.setLineWidth(width);
            unsafe {
                l.setLineCap(kCALineCapRound);
                l.setLineJoin(kCALineJoinRound);
            }
            l.setStrokeStart(0.5);
            l.setStrokeEnd(0.5);
            root.addSublayer(&l);
            l
        };
        // The glow: a faint line whose blurred shadow is the halo, and a hairline rim (the mockup's soft light, no
        // hard ring; half of each is under the black shape).
        let glow = line(5.0, 0.45);
        glow.setShadowColor(Some(&white));
        glow.setShadowRadius(10.0);
        glow.setShadowOpacity(1.0);
        glow.setShadowOffset(CGSize::new(0.0, 0.0));
        let core = line(1.0, 0.7);
        let flash = CALayer::new();
        flash.setFrame(cg_rect(0.0, 0.0, size.0, size.1));
        flash.setBackgroundColor(Some(&white));
        flash.setOpacity(0.0);
        root.addSublayer(&flash);
        CATransaction::commit();
        LightLayers { root, glow, core, flash, size, corner, built: None }
    }

    /// The loop as points: from the screen's bottom centre right, up the right side, along the top edge to the
    /// island's right flare, round the island's outline to its left flare, along the top edge, down the left side
    /// and back to the bottom centre. Returns the points and the lengths (one side's edge, the island's outline).
    fn points(&self, shape: &Shape) -> (Vec<(f64, f64)>, f64, f64) {
        let (w, h) = self.size;
        let cx = w / 2.0;
        let r = self.corner.clamp(0.0, 40.0);
        let i = 1.1;
        let mut right: Vec<(f64, f64)> = vec![(cx, h - i), (w - i, h - i), (w - i, r)];
        cubic((w - i, r), (w - i, r * 0.45), (w - r * 0.45, i), (w - r, i), &mut right);
        let fr = shape.right() + shape.flare.max(0.0);
        right.push((cx + fr, i));
        right.push((cx + fr, 0.0));
        let edge = poly_len(&right);
        // The island's outline from its right flare round to its left (`Shape::path` segments 3 to 9).
        let mut island: Vec<(f64, f64)> = vec![(cx + fr, 0.0)];
        let mut at = (cx + fr, 0.0);
        for seg in shape.path().into_iter().skip(3).take(7) {
            match seg {
                Seg::Line(a) => {
                    at = (cx + a.x, a.y);
                    island.push(at);
                }
                Seg::Curve(b, c, d) => {
                    cubic(at, (cx + b.x, b.y), (cx + c.x, c.y), (cx + d.x, d.y), &mut island);
                    at = (cx + d.x, d.y);
                }
                _ => {}
            }
        }
        let outline = poly_len(&island);
        // The left side: the right one mirrored, walked the other way.
        let left: Vec<(f64, f64)> = right.iter().rev().map(|(x, y)| (2.0 * cx - x, *y)).collect();
        let mut pts = right;
        pts.extend(island.into_iter().skip(1));
        pts.extend(left.into_iter().skip(1));
        (pts, edge, outline)
    }

    /// Paints the light of frame `f` round the island's `shape`.
    pub(crate) fn apply(&mut self, light: &Light, shape: &Shape) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let on = light.run > 0.0 && light.opacity > 0.0;
        if on && self.built.as_ref().is_none_or(|(s, ..)| s != shape) {
            let (pts, edge, outline) = self.points(shape);
            let p = CGMutablePath::new();
            unsafe {
                CGMutablePath::move_to_point(Some(&p), std::ptr::null(), pts[0].0, pts[0].1);
                for q in &pts[1..] {
                    CGMutablePath::add_line_to_point(Some(&p), std::ptr::null(), q.0, q.1);
                }
            }
            let path: &CGPath = &p;
            self.glow.setPath(Some(path));
            self.core.setPath(Some(path));
            self.built = Some((*shape, edge, outline, 2.0 * edge + outline));
        }
        if let Some((_, edge, outline, total)) = self.built {
            // Lit: the island's outline whole, and `run` of each side's edge out from the flares.
            let run = light.run.clamp(0.0, 1.0);
            let (start, end) = ((edge * (1.0 - run)) / total, (edge + outline + edge * run) / total);
            for l in [&self.glow, &self.core] {
                l.setStrokeStart(start);
                l.setStrokeEnd(end);
                l.setOpacity(light.opacity.clamp(0.0, 1.0) as f32);
                l.setHidden(!on);
            }
        }
        self.flash.setOpacity(light.flash.clamp(0.0, 1.0) as f32);
        CATransaction::commit();
    }
}
