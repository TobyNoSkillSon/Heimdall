//! Heimdall pointing at things for the user (the agent's `point` tool): marks drawn on the screen, display-only, in
//! Heimdall's blue (the user's own drawing is red). Clicks, scrolls and keys pass straight through to the apps below.
//! The marks are part of the island (one object, one clock): they arrive with it and hold while Heimdall speaks, and
//! fade when the voice ends (or after a few seconds without a voice) — their opacity is the island frame's
//! `pointing`, set by the island's host each frame (`set_opacity`). Nothing here animates on its own. One pointing
//! at a time: a new one replaces the last. The window exists from launch (ordered out) under `POINTER_WINDOW_TITLE`,
//! so Heimdall's screenshots exclude it.

use std::cell::RefCell;
use std::f64::consts::PI;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSBackingStoreType, NSBezierPath, NSColor, NSFont, NSFontWeightMedium, NSGraphicsContext,
    NSMainMenuWindowLevel, NSPanel, NSScreen, NSView, NSWindowAnimationBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGColor, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGMutablePath,
};
use objc2_foundation::{MainThreadMarker, NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{kCALineCapRound, kCALineJoinRound, CALayer, CAShapeLayer, CATransaction};

use crate::drawing::spec::{Mark, Rect, Seg, Shape, Side, Spec, P};
use crate::drawing::POINTER_WINDOW_TITLE;
use crate::island::object::Event;
use crate::mac::island;
use crate::mac::util::overlay_behavior;

/// Heimdall's blue (`#0A84FF`, DESIGN.md §5: info and Heimdall's pointing): the agent's marks.
pub(crate) const INK: (f64, f64, f64) = (10.0 / 255.0, 132.0 / 255.0, 1.0);
const WIDTH: f64 = 3.0;
const RIM_EXTRA: f64 = 3.0;
/// Stronger than the user's ink rim: the light accent needs it on white and on blue controls.
const RIM_ALPHA: f64 = 0.4;
/// Space between a target and the shape drawn round it, and between a shape and its label.
const OUTSET: f64 = 6.0;
const LABEL_GAP: f64 = 12.0;
/// Arrow labels sit a little further back from the tail.
const TAIL_GAP: f64 = 18.0;
const LABEL_H: f64 = 26.0;
const MARGIN: f64 = 8.0;
const ARROW_LENGTH: f64 = 110.0;
const ARROW_HEAD: f64 = 16.0;
/// Between one mark's end and the next mark's start.
const STAGGER: f64 = 0.08;

pub(crate) struct Display {
    /// 1-based, main display first.
    pub index: usize,
    pub screen: Retained<NSScreen>,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

pub(crate) fn displays(mtm: MainThreadMarker) -> Vec<Display> {
    NSScreen::screens(mtm)
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let f = s.frame();
            Display { index: i + 1, width: f.size.width, height: f.size.height, scale: s.backingScaleFactor(), screen: s }
        })
        .collect()
}

fn first_point(shape: &Shape) -> P {
    match shape {
        Shape::Circle(r) | Shape::Box(r) | Shape::Underline(r) => P { x: r.x + r.w / 2.0, y: r.y + r.h / 2.0 },
        Shape::Arrow { to, .. } => P { x: to.x + to.w / 2.0, y: to.y + to.h / 2.0 },
        Shape::Path(segs) => match segs[0] {
            Seg::Move(p) => p,
            _ => P { x: 0.0, y: 0.0 },
        },
    }
}

/// One mark's stroke in top-left display points, how long it takes to draw, and where its label goes.
struct Geometry {
    segs: Vec<Seg>,
    seconds: f64,
    /// The box the label should sit next to, and whether it prefers to sit below it.
    anchor: Rect,
    below: bool,
    /// For arrows: the tail and the direction from tip to tail, so the label sits behind the tail.
    tail: Option<(P, P)>,
}

fn bounds(segs: &[Seg]) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let points = segs.iter().flat_map(|s| match *s {
        Seg::Move(a) | Seg::Line(a) => vec![a],
        Seg::Quad(a, b) => vec![a, b],
        Seg::Cubic(a, b, c) => vec![a, b, c],
        Seg::Close => vec![],
    });
    for p in points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Rect { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
}

fn geometry(shape: &Shape, w: f64, h: f64) -> Geometry {
    match shape {
        Shape::Circle(r) => {
            // An ellipse through the target's corners, started upper-left and closed with a short overlap.
            let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            let (a, b) = ((r.w / 2.0 * 2f64.sqrt() + OUTSET).max(14.0), (r.h / 2.0 * 2f64.sqrt() + OUTSET).max(14.0));
            let (start, turn, steps) = (-PI * 0.8, 2.0 * PI * 1.03, 96);
            let segs = (0..=steps)
                .map(|i| {
                    let t = i as f64 / steps as f64;
                    let angle = start + turn * t;
                    let p = P { x: cx + a * angle.cos(), y: cy + b * angle.sin() };
                    if i == 0 { Seg::Move(p) } else { Seg::Line(p) }
                })
                .collect();
            Geometry {
                segs,
                seconds: 0.5,
                anchor: Rect { x: cx - a, y: cy - b, w: 2.0 * a, h: 2.0 * b },
                below: false,
                tail: None,
            }
        }
        Shape::Box(r) => {
            let (x0, y0, x1, y1) = (r.x - OUTSET, r.y - OUTSET, r.x + r.w + OUTSET, r.y + r.h + OUTSET);
            let k = 6.0f64.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
            let segs = vec![
                Seg::Move(P { x: x0 + k, y: y0 }),
                Seg::Line(P { x: x1 - k, y: y0 }),
                Seg::Quad(P { x: x1, y: y0 }, P { x: x1, y: y0 + k }),
                Seg::Line(P { x: x1, y: y1 - k }),
                Seg::Quad(P { x: x1, y: y1 }, P { x: x1 - k, y: y1 }),
                Seg::Line(P { x: x0 + k, y: y1 }),
                Seg::Quad(P { x: x0, y: y1 }, P { x: x0, y: y1 - k }),
                Seg::Line(P { x: x0, y: y0 + k }),
                Seg::Quad(P { x: x0, y: y0 }, P { x: x0 + k, y: y0 }),
            ];
            Geometry { segs, seconds: 0.45, anchor: Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }, below: false, tail: None }
        }
        Shape::Underline(r) => {
            let y = r.y + r.h + 4.0;
            let segs = vec![Seg::Move(P { x: r.x - 3.0, y }), Seg::Line(P { x: r.x + r.w + 3.0, y })];
            Geometry { segs, seconds: 0.3, anchor: Rect { x: r.x, y, w: r.w, h: 0.0 }, below: true, tail: None }
        }
        Shape::Arrow { from, to } => {
            // Aim at the target's centre; without a tail, come in from the display's centre side so the arrow
            // stays on screen. The tip stops just outside the target's box (or just short of a point).
            let c = P { x: to.x + to.w / 2.0, y: to.y + to.h / 2.0 };
            let toward = from.unwrap_or(P { x: w / 2.0, y: h / 2.0 });
            let (mut ux, mut uy) = (toward.x - c.x, toward.y - c.y);
            let len = ux.hypot(uy);
            if len < 1.0 {
                (ux, uy) = (-1.0, 1.0);
            }
            let len = ux.hypot(uy);
            (ux, uy) = (ux / len, uy / len);
            let exit = |half: f64, u: f64| if u.abs() < 1e-9 { f64::MAX } else { half / u.abs() };
            let edge = exit(to.w / 2.0, ux).min(exit(to.h / 2.0, uy));
            let gap = if to.w > 0.0 || to.h > 0.0 { OUTSET } else { 4.0 };
            let tip = P { x: c.x + ux * (edge + gap), y: c.y + uy * (edge + gap) };
            let tail = from.unwrap_or(P { x: tip.x + ux * ARROW_LENGTH, y: tip.y + uy * ARROW_LENGTH });
            let wing = |sign: f64| {
                let a = uy.atan2(ux) + sign * 0.5;
                P { x: tip.x + ARROW_HEAD * a.cos(), y: tip.y + ARROW_HEAD * a.sin() }
            };
            let segs = vec![
                Seg::Move(tail),
                Seg::Line(tip),
                Seg::Move(wing(1.0)),
                Seg::Line(tip),
                Seg::Line(wing(-1.0)),
            ];
            Geometry {
                segs,
                seconds: 0.4,
                anchor: Rect { x: tail.x, y: tail.y, w: 0.0, h: 0.0 },
                below: false,
                tail: Some((tail, P { x: ux, y: uy })),
            }
        }
        Shape::Path(segs) => {
            let b = bounds(segs);
            let length = b.w + b.h;
            Geometry { segs: segs.clone(), seconds: (length / 900.0).clamp(0.25, 0.9), anchor: b, below: false, tail: None }
        }
    }
}

/// The label's frame (top-left points) next to its mark, kept on the display. Arrows put it behind the tail;
/// other marks to the right of the target (a target's own title usually sits above it), then left, below, above,
/// taking the first side with room, unless the spec names a side.
fn label_frame(g: &Geometry, side: Option<Side>, lw: f64, w: f64, h: f64) -> Rect {
    let at = |side: Side| {
        let a = g.anchor;
        match side {
            Side::Right => (a.x + a.w + LABEL_GAP, a.y + a.h / 2.0 - LABEL_H / 2.0),
            Side::Left => (a.x - LABEL_GAP - lw, a.y + a.h / 2.0 - LABEL_H / 2.0),
            Side::Below => (a.x + a.w / 2.0 - lw / 2.0, a.y + a.h + LABEL_GAP),
            Side::Above => (a.x + a.w / 2.0 - lw / 2.0, a.y - LABEL_GAP - LABEL_H),
        }
    };
    // A side fits when there is room along it; the other axis is clamped onto the display afterwards.
    let fits = |side: Side| {
        let (x, y) = at(side);
        match side {
            Side::Right | Side::Left => x >= MARGIN && x + lw <= w - MARGIN,
            Side::Above | Side::Below => y >= MARGIN && y + LABEL_H <= h - MARGIN,
        }
    };
    let (mut x, mut y) = match (side, g.tail) {
        (Some(s), _) => at(s),
        (None, Some((t, u))) if u.x.abs() > u.y.abs() => {
            (if u.x > 0.0 { t.x + TAIL_GAP } else { t.x - TAIL_GAP - lw }, t.y - LABEL_H / 2.0)
        }
        (None, Some((t, u))) => (t.x - lw / 2.0, if u.y > 0.0 { t.y + TAIL_GAP } else { t.y - TAIL_GAP - LABEL_H }),
        (None, None) => {
            let order: &[Side] = if g.below {
                &[Side::Below, Side::Above, Side::Right, Side::Left]
            } else {
                &[Side::Right, Side::Left, Side::Below, Side::Above]
            };
            at(order.iter().copied().find(|&s| fits(s)).unwrap_or(order[0]))
        }
    };
    x = x.clamp(MARGIN, (w - MARGIN - lw).max(MARGIN));
    y = y.clamp(MARGIN, (h - MARGIN - LABEL_H).max(MARGIN));
    Rect { x, y, w: lw, h: LABEL_H }
}

fn cg_path(segs: &[Seg], h: f64) -> CFRetained<CGMutablePath> {
    let path = CGMutablePath::new();
    let m = Some(&*path);
    let t = std::ptr::null();
    unsafe {
        for s in segs {
            match *s {
                Seg::Move(a) => CGMutablePath::move_to_point(m, t, a.x, h - a.y),
                Seg::Line(a) => CGMutablePath::add_line_to_point(m, t, a.x, h - a.y),
                Seg::Quad(a, b) => CGMutablePath::add_quad_curve_to_point(m, t, a.x, h - a.y, b.x, h - b.y),
                Seg::Cubic(a, b, c) => {
                    CGMutablePath::add_curve_to_point(m, t, a.x, h - a.y, b.x, h - b.y, c.x, h - c.y)
                }
                Seg::Close => CGMutablePath::close_subpath(m),
            }
        }
    }
    path
}

fn attributed(text: &str) -> Retained<NSAttributedString> {
    let font = NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightMedium });
    let color = NSColor::colorWithWhite_alpha(1.0, 0.95);
    unsafe {
        let keys: [&NSString; 2] = [objc2_app_kit::NSFontAttributeName, objc2_app_kit::NSForegroundColorAttributeName];
        let values: [&AnyObject; 2] = [font.as_ref(), color.as_ref()];
        let attrs = NSDictionary::from_slices(&keys, &values);
        NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &NSString::from_str(text), Some(&attrs))
    }
}

fn label_width(text: &str) -> f64 {
    (attributed(text).size().width + 22.0).ceil()
}

/// The label as a picture: the Send capsule's dark material with an accent hairline, so it reads as Heimdall's.
fn label_image(text: &str, w: f64, scale: f64) -> Option<CFRetained<CGImage>> {
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
    let ctx = unsafe {
        CGBitmapContextCreate(std::ptr::null_mut(), (w * scale) as usize, (LABEL_H * scale) as usize, 8, 0, Some(&space), info)
    }?;
    CGContext::scale_ctm(Some(&ctx), scale, scale);
    let ns = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ns));
    let r = NSRect::new(NSPoint::new(0.5, 0.5), NSSize::new(w - 1.0, LABEL_H - 1.0));
    let capsule = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, r.size.height / 2.0, r.size.height / 2.0);
    // Blue label capsules with white words (DESIGN.md §3, pointing).
    NSColor::colorWithSRGBRed_green_blue_alpha(INK.0, INK.1, INK.2, 1.0).setFill();
    capsule.fill();
    let s = attributed(text);
    let size = s.size();
    s.drawAtPoint(NSPoint::new((w - size.width) / 2.0, (LABEL_H - size.height) / 2.0));
    NSGraphicsContext::restoreGraphicsState_class();
    CGBitmapContextCreateImage(Some(&ctx))
}

/// The marks moved into the chosen display's own points (top-left origin).
pub(crate) struct Resolved {
    pub display: Display,
    pub marks: Vec<Mark>,
}

/// Maps the spec onto a display: `display N`, else the display the user is on (under the pointer).
pub(crate) fn resolve(spec: &Spec, mtm: MainThreadMarker) -> Result<Resolved, String> {
    let mut all = displays(mtm);
    if all.is_empty() {
        return Err("no display".into());
    }
    let count = all.len();
    let index = match spec.display {
        Some(n) if n > count => return Err(format!("display {n} does not exist ({count} connected)")),
        Some(n) => n,
        None => {
            let id = crate::mac::util::pointer_screen(mtm).map(|s| s.CGDirectDisplayID());
            all.iter().position(|d| Some(d.screen.CGDirectDisplayID()) == id).map_or(1, |i| i + 1)
        }
    };
    let display = all.swap_remove(index - 1);
    let (sx, sy) = (display.width / spec.space.0, display.height / spec.space.1);
    let p = |p: P| P { x: p.x * sx, y: p.y * sy };
    let r = |r: Rect| {
        let o = p(P { x: r.x, y: r.y });
        Rect { x: o.x, y: o.y, w: r.w * sx, h: r.h * sy }
    };
    let mut marks = vec![];
    for (i, m) in spec.marks.iter().enumerate() {
        let shape = match &m.shape {
            Shape::Circle(x) => Shape::Circle(r(*x)),
            Shape::Box(x) => Shape::Box(r(*x)),
            Shape::Underline(x) => Shape::Underline(r(*x)),
            Shape::Arrow { from, to } => Shape::Arrow { from: from.map(p), to: r(*to) },
            Shape::Path(segs) => Shape::Path(
                segs.iter()
                    .map(|s| match *s {
                        Seg::Move(a) => Seg::Move(p(a)),
                        Seg::Line(a) => Seg::Line(p(a)),
                        Seg::Quad(a, b) => Seg::Quad(p(a), p(b)),
                        Seg::Cubic(a, b, c) => Seg::Cubic(p(a), p(b), p(c)),
                        Seg::Close => Seg::Close,
                    })
                    .collect(),
            ),
        };
        let at = first_point(&shape);
        if at.x < 0.0 || at.y < 0.0 || at.x > display.width || at.y > display.height {
            let bounds = format!("0,0 to {},{} in your screenshot", spec.space.0, spec.space.1);
            return Err(format!("mark {} is off display {} ({bounds})", i + 1, display.index));
        }
        marks.push(Mark { shape, label: m.label.clone(), side: m.side });
    }
    Ok(Resolved { display, marks })
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// The pointing, built: the container layer.
pub(crate) struct Built {
    pub container: Retained<CALayer>,
}

/// Builds the layers for a w×h display, in their final state (the island's frame fades the container in and out).
pub(crate) fn build(r: &Resolved, _begin: Option<f64>) -> Built {
    let d = &r.display;
    let (w, h) = (d.width, d.height);
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    let container = CALayer::new();
    container.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w, h)));
    let ink = CGColor::new_srgb(INK.0, INK.1, INK.2, 1.0);
    let rim = CGColor::new_srgb(0.0, 0.0, 0.0, RIM_ALPHA);
    let mut t = 0.0;
    for mark in &r.marks {
        let g = geometry(&mark.shape, w, h);
        let path = cg_path(&g.segs, h);
        for (i, (color, width)) in [(&rim, WIDTH + RIM_EXTRA), (&ink, WIDTH)].into_iter().enumerate() {
            let layer = CAShapeLayer::new();
            layer.setFrame(container.bounds());
            layer.setPath(Some(&path));
            layer.setFillColor(None);
            layer.setStrokeColor(Some(color));
            layer.setLineWidth(width);
            unsafe {
                layer.setLineCap(kCALineCapRound);
                layer.setLineJoin(kCALineJoinRound);
            }
            if i == 1 {
                // The marks glow in their own blue (DESIGN.md: 3 pt with glow).
                layer.setShadowColor(Some(color));
                layer.setShadowRadius(6.0);
                layer.setShadowOpacity(0.8);
                layer.setShadowOffset(CGSize::new(0.0, 0.0));
            }
            container.addSublayer(&layer);
        }
        t += g.seconds;
        if let Some(text) = &mark.label {
            let lw = label_width(text);
            let f = label_frame(&g, mark.side, lw, w, h);
            if let Some(image) = label_image(text, lw, d.scale) {
                let layer = CALayer::new();
                layer.setFrame(CGRect::new(CGPoint::new(f.x, h - f.y - f.h), CGSize::new(f.w, f.h)));
                layer.setContentsScale(d.scale);
                unsafe { layer.setContents(Some(&*(CFRetained::as_ptr(&image).as_ptr() as *const AnyObject))) };
                container.addSublayer(&layer);
            }
        }
        t += STAGGER;
    }
    CATransaction::commit();
    let _ = t;
    Built { container }
}

struct State {
    panel: Retained<NSPanel>,
    shown: Option<Retained<CALayer>>,
    /// The marks have been seen (their first frame is transparent: they fade in); at 0 after that they are gone.
    seen: bool,
    /// When they were put up: marks never seen within a second (the island dropped them at once) go too.
    shown_at: std::time::Instant,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok()?.as_mut().map(f))
}

/// Creates the (ordered-out) window at launch.
/// The pointing window's level at rest: one under the island.
pub(crate) const LEVEL: isize = NSMainMenuWindowLevel + 2;

pub(crate) fn install(mtm: MainThreadMarker) {
    if STATE.with(|s| s.borrow().is_some()) {
        return;
    }
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0));
    let panel: Retained<NSPanel> = unsafe {
        msg_send![NSPanel::alloc(mtm), initWithContentRect: frame, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
    };
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setTitle(&ns(POINTER_WINDOW_TITLE));
    panel.setSharingType(crate::mac::streams::sharing());
    // Under the island; while drawing mode is up, with its overlay above menus (`island::pin`).
    panel.setLevel(LEVEL);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    // Display-only: every click, scroll and key goes to the apps below.
    panel.setIgnoresMouseEvents(true);
    panel.setHidesOnDeactivate(false);
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    panel.setCollectionBehavior(overlay_behavior());
    let view: Retained<NSView> = unsafe { msg_send![NSView::alloc(mtm), initWithFrame: frame] };
    view.setWantsLayer(true);
    panel.setContentView(Some(&view));
    STATE.with(|s| *s.borrow_mut() = Some(State { panel, shown: None, seen: false, shown_at: std::time::Instant::now() }));
}

/// The window (checks read its properties).
pub(crate) fn panel() -> Option<Retained<NSPanel>> {
    with_state(|s| s.panel.clone())
}

/// Shows a pointing on screen, replacing any still showing. Returns the one-line answer for the agent.
pub(crate) fn show(spec: &Spec, alpha: f64, mtm: MainThreadMarker) -> Result<String, String> {
    install(mtm);
    let resolved = resolve(spec, mtm)?;
    let built = build(&resolved, None);
    let d = &resolved.display;
    let frame = d.screen.frame();
    let Some((panel, old)) = with_state(|s| (s.panel.clone(), s.shown.take())) else {
        return Err("pointing is not installed".into());
    };
    if let Some(old) = old {
        old.removeFromSuperlayer();
    }
    panel.setFrame_display(frame, false);
    panel.setAlphaValue(alpha);
    if let Some(view) = panel.contentView() {
        view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        built.container.setOpacity(0.0);
        view.layer().expect("layer-backed").addSublayer(&built.container);
        CATransaction::commit();
    }
    with_state(|s| {
        s.shown = Some(built.container.clone());
        s.seen = false;
        s.shown_at = std::time::Instant::now();
    });
    panel.orderFrontRegardless();
    // Still while Spaces slide, like the island; checks show pointing at alpha 0 and never reach SkyLight.
    if alpha > 0.0 {
        crate::mac::overlay_space::join(&panel, POINTER_WINDOW_TITLE);
    }
    let n = resolved.marks.len();
    // The island holds the marks while Heimdall speaks and fades them with the voice.
    island::apply(Event::Point(n));
    Ok(format!("showing {n} mark{} on display {} ({}x{} pt)", if n == 1 { "" } else { "s" }, d.index, d.width, d.height))
}

/// The island's frame: the marks' opacity. At 0 they go and the window goes out.
pub(crate) fn set_opacity(opacity: f64) {
    let gone = with_state(|s| {
        let Some(c) = &s.shown else { return None };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        c.setOpacity(opacity.clamp(0.0, 1.0) as f32);
        CATransaction::commit();
        if opacity > 0.0 {
            s.seen = true;
        }
        let stale = !s.seen && s.shown_at.elapsed().as_secs_f64() > 1.0;
        (opacity <= 0.0 && (s.seen || stale)).then(|| (s.shown.take(), s.panel.clone()))
    })
    .flatten();
    if let Some((shown, panel)) = gone {
        if let Some(c) = shown {
            c.removeFromSuperlayer();
        }
        panel.orderOut(None);
    }
}

/// Nothing of a pointing is alive (checks).
pub(crate) fn at_rest() -> bool {
    with_state(|s| s.shown.is_none() && !s.panel.isVisible()).unwrap_or(true)
}

/// The pointing on screen now (checks sample its layers).
pub(crate) fn shown_container() -> Option<Retained<CALayer>> {
    with_state(|s| s.shown.clone()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_without_tail_comes_from_the_centre_side_and_stops_short() {
        let point = Rect { x: 1600.0, y: 100.0, w: 0.0, h: 0.0 };
        let g = geometry(&Shape::Arrow { from: None, to: point }, 1728.0, 1117.0);
        let Seg::Move(tail) = g.segs[0] else { panic!() };
        let Seg::Line(tip) = g.segs[1] else { panic!() };
        assert!(tail.x < 1600.0 && tail.y > 100.0, "tail toward the centre: {tail:?}");
        assert!(((tail.x - tip.x).hypot(tail.y - tip.y) - ARROW_LENGTH).abs() < 0.01);
        assert!(((tip.x - 1600.0).hypot(tip.y - 100.0) - 4.0).abs() < 0.01);
    }

    #[test]
    fn arrow_at_a_box_stops_outside_it() {
        let button = Rect { x: 1000.0, y: 800.0, w: 244.0, h: 44.0 };
        let g = geometry(&Shape::Arrow { from: None, to: button }, 1728.0, 1117.0);
        let Seg::Line(tip) = g.segs[1] else { panic!() };
        let inside = |p: P| p.x > button.x && p.x < button.x + button.w && p.y > button.y && p.y < button.y + button.h;
        assert!(!inside(tip), "tip {tip:?} must be outside the button");
        assert!(tip.y < button.y && tip.y > button.y - OUTSET - 1.0, "comes from above (the centre side): {tip:?}");
    }

    #[test]
    fn labels_stay_on_the_display() {
        let g = geometry(&Shape::Box(Rect { x: 1700.0, y: 2.0, w: 20.0, h: 20.0 }), 1728.0, 1117.0);
        let f = label_frame(&g, None, 120.0, 1728.0, 1117.0);
        assert!(f.x >= MARGIN && f.x + f.w <= 1728.0 - MARGIN + 0.01);
        assert!(f.x + f.w < 1700.0, "no room on the right, so it goes left: {f:?}");
        let g = geometry(&Shape::Box(Rect { x: 500.0, y: 500.0, w: 80.0, h: 20.0 }), 1728.0, 1117.0);
        let f = label_frame(&g, None, 120.0, 1728.0, 1117.0);
        assert!(f.x > 580.0, "right of the target by default: {f:?}");
        let f = label_frame(&g, Some(Side::Above), 120.0, 1728.0, 1117.0);
        assert!(f.y + f.h < 500.0, "a named side wins: {f:?}");
    }

    #[test]
    fn circle_encloses_its_target() {
        let r = Rect { x: 100.0, y: 100.0, w: 244.0, h: 44.0 };
        let g = geometry(&Shape::Circle(r), 1728.0, 1117.0);
        let (cx, cy) = (222.0, 122.0);
        let corner = |p: P| ((p.x - cx) / (g.anchor.w / 2.0)).powi(2) + ((p.y - cy) / (g.anchor.h / 2.0)).powi(2);
        assert!(corner(P { x: 100.0, y: 100.0 }) < 1.0);
    }
}
