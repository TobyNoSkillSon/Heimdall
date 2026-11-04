//! Heimdall's mark: his helmet, a rounded face-plate under two horns that rise into a crescent. Our own drawing
//! of that silhouette, never traced from the comic art. One geometry serves the menu-bar glyph (a template image,
//! so the menu bar tints it for light and dark bars), its crossed-out Listening-off variant, and the app icon (the glyph in white on a graphite tile).
//!
//! Geometry lives in a 100-unit box, y up: horn tips at the top edge, the cheek guards' feet at y 3, ink x 7.5–92.5.

use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::AllocAnyThread;
use objc2_app_kit::{
    NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSCompositingOperation, NSGradient,
    NSGraphicsContext, NSImage, NSLineCapStyle, NSLineJoinStyle,
};
use objc2_core_graphics::CGContext;
use objc2_foundation::{NSArray, NSDictionary, NSPoint, NSRect, NSSize};

use super::util::{ns, rect};
use crate::identity::Icon;

/// Maps the 100-unit design box onto the page.
#[derive(Clone, Copy)]
struct Frame {
    x: f64,
    y: f64,
    unit: f64,
}

impl Frame {
    fn p(&self, x: f64, y: f64) -> NSPoint {
        NSPoint::new(self.x + x * self.unit, self.y + y * self.unit)
    }
}

fn curve(path: &NSBezierPath, f: Frame, to: (f64, f64), c1: (f64, f64), c2: (f64, f64)) {
    path.curveToPoint_controlPoint1_controlPoint2(f.p(to.0, to.1), f.p(c1.0, c1.1), f.p(c2.0, c2.1));
}

/// Horn widths, (mid, tip), in units: broad blades that taper to a narrow chisel, heavy enough to hold at menu-bar
/// size (as SF Symbols thickens small strokes). The app icon draws the same glyph.
const GLYPH_HORN: (f64, f64) = (22.0, 12.5);
/// The chisel at each horn tip falls this steeply toward the inside (outer corner highest).
const CHISEL_DEGREES: f64 = 57.0;

/// One cubic segment: (end, control 1, control 2).
type Seg = ((f64, f64), (f64, f64), (f64, f64));

/// The cheek guard's foot: cut at an angle, the outer corner lower, so the guards end square-ish and never in a
/// point (points read as fangs at menu-bar size).
const FOOT_OUTER: (f64, f64) = (22.0, 4.0);
const FOOT_INNER: (f64, f64) = (31.0, 10.0);

/// The left half of the casting's outline, from the crown down to the cheek guard's foot: across the crown to where
/// the horn rises, up the horn's inner edge, the chisel cut sloping down toward the inside (outer corner highest),
/// down the horn's outer edge and, without a break, on down the temple into the cheek guard's outer edge, then the
/// foot's angled cut.
fn left_half((mid, tip): (f64, f64)) -> ((f64, f64), Vec<Seg>) {
    let crown = (50.0, 57.0);
    let junction = (37.5, 53.0);
    let widest = (7.5, 66.0);
    let inner = (widest.0 + mid, 68.0);
    let outer_tip = (21.0, 100.0);
    let (c, s) = (CHISEL_DEGREES.to_radians().cos(), CHISEL_DEGREES.to_radians().sin());
    let inner_tip = (outer_tip.0 + tip * c, outer_tip.1 - tip * s);
    let temple = (19.0, 36.0);
    let line = |from: (f64, f64), to: (f64, f64)| (to, from, to);
    let segs = vec![
        (junction, (45.0, 57.0), (40.5, 56.0)),
        (inner, (33.5, 56.0), (inner.0, 60.0)),
        (inner_tip, (inner.0, 78.0), (inner_tip.0 + 0.5, inner_tip.1 - 7.0)),
        line(inner_tip, outer_tip),
        (widest, (16.0, 93.0), (7.0, 82.0)),
        // Tangent-continuous through the temple: the horn's outer edge is the cheek guard's.
        (temple, (7.5, 52.0), (12.0, 41.0)),
        (FOOT_OUTER, (22.5, 33.5), (22.5, 14.0)),
        line(FOOT_OUTER, FOOT_INNER),
    ];
    (crown, segs)
}

/// One step of an outline in the 100-unit box: what both the AppKit paths and the SVG master are built from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Move((f64, f64)),
    Line((f64, f64)),
    /// (end, control 1, control 2).
    Curve((f64, f64), (f64, f64), (f64, f64)),
    Close,
}

/// A half outline, `start` then `half`, closed by its mirror image walked backwards; `apex` (the nasal), when
/// given, is visited between the halves.
fn mirrored(start: (f64, f64), half: &[Seg], apex: Option<(f64, f64)>) -> Vec<Op> {
    let mut ops = vec![Op::Move(start)];
    ops.extend(half.iter().map(|(to, c1, c2)| Op::Curve(*to, *c1, *c2)));
    if let Some(apex) = apex {
        ops.push(Op::Line(apex));
    }
    let mirror = |(x, y): (f64, f64)| (100.0 - x, y);
    let mut points = vec![start];
    points.extend(half.iter().map(|s| s.0));
    ops.push(Op::Line(mirror(*points.last().unwrap())));
    for (i, (_, c1, c2)) in half.iter().enumerate().rev() {
        ops.push(Op::Curve(mirror(points[i]), mirror(*c2), mirror(*c1)));
    }
    ops.push(Op::Close);
    ops
}

/// The whole helmet as one casting: horns, dome and cheek guards in a single outline (the face is cut after). The
/// right half is the left mirrored and walked backwards, from its foot up to the crown.
fn casting_ops(horn: (f64, f64)) -> Vec<Op> {
    let (start, segs) = left_half(horn);
    mirrored(start, &segs, None)
}

fn bezier(f: Frame, ops: &[Op]) -> Retained<NSBezierPath> {
    let path = NSBezierPath::bezierPath();
    for op in ops {
        match *op {
            Op::Move(p) => path.moveToPoint(f.p(p.0, p.1)),
            Op::Line(p) => path.lineToPoint(f.p(p.0, p.1)),
            Op::Curve(to, c1, c2) => curve(&path, f, to, c1, c2),
            Op::Close => path.closePath(),
        }
    }
    path
}

fn casting(f: Frame, horn: (f64, f64)) -> Retained<NSBezierPath> {
    bezier(f, &casting_ops(horn))
}

/// The nasal's point: where the brow's V meets over the nose. High enough that the 16 pt glyph never shows a spike.
const NASAL: (f64, f64) = (50.0, 26.0);

/// The face, cut from the casting: a real window, about 32 units wide at the eyes under a brow that starts near
/// y 40 and falls in a V to the nose; the cheek guards' inner edges flare outward down to their feet. Open at the
/// bottom. From below the foot (the inner edge carried on, outside the casting) up the cheek guard's inner edge,
/// round the eye's outer corner and along the brow down to the nasal.
fn face_ops() -> Vec<Op> {
    let half: [Seg; 4] = [
        (FOOT_INNER, (28.6, -3.0), FOOT_INNER),
        ((34.0, 37.0), (32.6, 17.0), (34.0, 28.0)),
        ((36.0, 40.0), (34.0, 39.0), (35.0, 40.0)),
        ((NASAL.0 - 1.0, NASAL.1 + 1.0), (40.5, 35.5), (45.5, 30.5)),
    ];
    mirrored((28.6, -3.0), &half, Some(NASAL))
}

fn face(f: Frame) -> Retained<NSBezierPath> {
    bezier(f, &face_ops())
}

fn fill(path: &NSBezierPath, context: &NSGraphicsContext, op: NSCompositingOperation) {
    context.setCompositingOperation(op);
    path.fill();
    context.setCompositingOperation(NSCompositingOperation::SourceOver);
}

fn stroke(path: &NSBezierPath, context: &NSGraphicsContext, op: NSCompositingOperation, width: f64) {
    context.setCompositingOperation(op);
    path.setLineWidth(width);
    path.setLineJoinStyle(NSLineJoinStyle::Round);
    path.stroke();
    context.setCompositingOperation(NSCompositingOperation::SourceOver);
}

/// The one-colour glyph in `ink`, filling `r` (its height is the 100-unit box). `off` crosses it out the way
/// SF Symbols draws its `.slash` variants: a 1 pt round-capped stroke from top left to bottom right with a 0.75 pt
/// gap cut on both sides, the geometry the crossed-out ear used (`mic.slash`'s, measured against `mic`).
pub fn draw_glyph(r: NSRect, off: bool, ink: &NSColor) {
    let Some(context) = NSGraphicsContext::currentContext() else { return };
    let unit = r.size.height / 100.0;
    let f = Frame { x: r.origin.x + (r.size.width - 100.0 * unit) / 2.0, y: r.origin.y, unit };
    let cg = context.CGContext();
    CGContext::save_g_state(Some(&cg));
    // In a flipped view, turn the y-up geometry right way up about the rect.
    if context.isFlipped() {
        CGContext::translate_ctm(Some(&cg), 0.0, 2.0 * r.origin.y + r.size.height);
        CGContext::scale_ctm(Some(&cg), 1.0, -1.0);
    }
    // Every cut happens inside a transparency layer, so it erases the helmet and never what is behind it. The ink
    // draws opaque inside it and its alpha applies to the layer once, so overlaps never darken.
    CGContext::set_alpha(Some(&cg), ink.alphaComponent());
    unsafe { CGContext::begin_transparency_layer(Some(&cg), None) };
    ink.colorWithAlphaComponent(1.0).set();
    fill(&casting(f, GLYPH_HORN), &context, NSCompositingOperation::SourceOver);
    fill(&face(f), &context, NSCompositingOperation::DestinationOut);
    if off {
        slash(f, &context);
    }
    CGContext::end_transparency_layer(Some(&cg));
    CGContext::restore_g_state(Some(&cg));
}

/// Menu-bar glyph size, in points: the helmet 16 pt tall, horn tip to jaw.
pub const GLYPH: NSSize = NSSize { width: 16.0, height: 16.0 };

/// The configured mark (`identity::current().icon`) in `ink`, filling `r` as the helmet does. Every place the helmet
/// stood draws this: the menu-bar item, the notch island, the gallery's menu bar.
pub fn draw_mark(r: NSRect, off: bool, ink: &NSColor) {
    draw_icon(&crate::identity::current().icon, r, off, ink);
}

/// One mark in `ink`, filling `r`: the helmet; or an SF Symbol or an image file drawn as a one-colour mask (its alpha)
/// in the helmet's square box, aspect-fit and centred. `off` crosses any of them out with the helmet's slash. A
/// symbol the system does not have, or a file that will not load, draws the helmet.
pub fn draw_icon(icon: &Icon, r: NSRect, off: bool, ink: &NSColor) {
    match icon_image(icon) {
        Some(image) => draw_masked(&image, r, off, ink),
        None => draw_glyph(r, off, ink),
    }
}

/// The image behind a non-helmet mark: the symbol, or the file (cached per path and modification time, so a mark the
/// agent redraws shows its new drawing). `None` for the helmet and for anything that does not load.
pub(crate) fn icon_image(icon: &Icon) -> Option<Retained<NSImage>> {
    match icon {
        Icon::Helmet => None,
        // Semibold: at 16 pt it holds beside the solid helmet and the bar's own symbols.
        Icon::Symbol { name } => super::util::symbol_sized(name, 64.0, unsafe { objc2_app_kit::NSFontWeightSemibold }),
        Icon::Image { path } => file_image(path),
    }
}

thread_local! {
    /// Loaded mark files: path → (modification time, image or `None` when it did not load).
    static FILES: std::cell::RefCell<std::collections::HashMap<std::path::PathBuf, (Option<std::time::SystemTime>, Option<Retained<NSImage>>)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A PNG, PDF or SVG mark (NSImage reads all three natively; SVG through `_NSSVGImageRep`). Unreadable or empty: `None`.
fn file_image(path: &Path) -> Option<Retained<NSImage>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok();
    FILES.with(|files| {
        if let Some((at, image)) = files.borrow().get(path) {
            if *at == modified {
                return image.clone();
            }
        }
        let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &ns(&path.to_string_lossy()))
            .filter(|i| i.isValid() && i.size().width > 0.0 && i.size().height > 0.0);
        let mut files = files.borrow_mut();
        if files.len() > 16 {
            files.clear();
        }
        files.insert(path.to_path_buf(), (modified, image.clone()));
        image
    })
}

/// `image`'s alpha as a mask in `ink`, aspect-fit in the helmet's square box (side `r`'s height, centred in `r`), with
/// the helmet's slash when `off`. One transparency layer, as the helmet: cuts erase only the mark.
fn draw_masked(image: &NSImage, r: NSRect, off: bool, ink: &NSColor) {
    let Some(context) = NSGraphicsContext::currentContext() else { return };
    let unit = r.size.height / 100.0;
    let f = Frame { x: r.origin.x + (r.size.width - 100.0 * unit) / 2.0, y: r.origin.y, unit };
    let side = 100.0 * unit;
    let s = image.size();
    if s.width <= 0.0 || s.height <= 0.0 {
        return draw_glyph(r, off, ink);
    }
    let k = (side / s.width).min(side / s.height);
    let (w, h) = (s.width * k, s.height * k);
    let dst = rect(f.x + (side - w) / 2.0, f.y + (side - h) / 2.0, w, h);
    let cg = context.CGContext();
    CGContext::save_g_state(Some(&cg));
    if context.isFlipped() {
        CGContext::translate_ctm(Some(&cg), 0.0, 2.0 * r.origin.y + r.size.height);
        CGContext::scale_ctm(Some(&cg), 1.0, -1.0);
    }
    CGContext::set_alpha(Some(&cg), ink.alphaComponent());
    unsafe { CGContext::begin_transparency_layer(Some(&cg), None) };
    // y-up now (the flip above undone by hand), so the image draws without AppKit's own flip.
    unsafe {
        image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(dst, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, false, None)
    };
    ink.colorWithAlphaComponent(1.0).set();
    objc2_app_kit::NSRectFillUsingOperation(dst, NSCompositingOperation::SourceIn);
    if off {
        slash(f, &context);
    }
    CGContext::end_transparency_layer(Some(&cg));
    CGContext::restore_g_state(Some(&cg));
}

/// The crossed-out stroke over the 100-unit box, in the current colour (see `draw_glyph`).
fn slash(f: Frame, context: &NSGraphicsContext) {
    let line = NSBezierPath::bezierPath();
    line.moveToPoint(f.p(10.0, 95.0));
    line.lineToPoint(f.p(90.0, 7.0));
    line.setLineCapStyle(NSLineCapStyle::Round);
    stroke(&line, context, NSCompositingOperation::DestinationOut, 2.5);
    stroke(&line, context, NSCompositingOperation::SourceOver, 1.0);
}

/// A key for the configured mark (its kind, name or path and the file's modification time): caches of drawn marks
/// (the island's rasters) mix it in, so a new mark draws anew.
pub fn mark_key() -> u64 {
    use std::hash::{Hash, Hasher};
    let icon = crate::identity::current().icon;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{icon:?}").hash(&mut h);
    if let Icon::Image { path } = &icon {
        std::fs::metadata(path).and_then(|m| m.modified()).ok().hash(&mut h);
    }
    h.finish()
}

/// What VoiceOver says for the menu-bar item: the name, and the state when it is not plain listening.
pub fn bar_description(name: &str, state: BarState) -> String {
    match state {
        BarState::On => name.to_string(),
        BarState::Off => format!("{name}, listening off"),
        BarState::Attention => format!("{name}, needs attention"),
    }
}

/// The menu-bar item's image: the mark, crossed out when nothing is heard. A template image, so the menu bar draws it
/// in its own ink on light and dark bars.
pub fn menu_bar_image(off: bool) -> Retained<NSImage> {
    let handler = block2::RcBlock::new(move |r: NSRect| -> Bool {
        draw_mark(r, off, &NSColor::blackColor());
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(GLYPH, false, &handler);
    image.setTemplate(true);
    let state = if off { BarState::Off } else { BarState::On };
    image.setAccessibilityDescription(Some(&ns(&bar_description(&crate::identity::name(), state))));
    image
}

/// The menu-bar item's "needs attention" image: the mark, not crossed out, in the system's bright red, for when
/// Heimdall cannot recover by itself (the user: "the icon should be also bright red"). Not a template, so the menu bar
/// keeps the colour; `systemRed` resolves per bar appearance when the image draws. The status button shows it while
/// the island holds a health problem (docs/resilience.md).
pub fn menu_bar_attention_image() -> Retained<NSImage> {
    let handler = block2::RcBlock::new(move |r: NSRect| -> Bool {
        draw_mark(r, false, &NSColor::systemRedColor());
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(GLYPH, false, &handler);
    image.setTemplate(false);
    image.setAccessibilityDescription(Some(&ns(&bar_description(&crate::identity::name(), BarState::Attention))));
    image
}

/// `icon` as a template image `size` points square with the mark `mark` points tall (centred): a menu row's image
/// (Personalise) and the Personalise grid's cells.
pub fn icon_menu_image(icon: &Icon, size: f64, mark: f64) -> Retained<NSImage> {
    let icon = icon.clone();
    let handler = block2::RcBlock::new(move |r: NSRect| -> Bool {
        let y = r.origin.y + ((r.size.height - mark) / 2.0);
        draw_icon(&icon, rect(r.origin.x, y, r.size.width, mark), false, &NSColor::blackColor());
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(size, size), false, &handler);
    image.setTemplate(true);
    image
}

/// A menu-bar state, for renders and the item's description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarState {
    On,
    Off,
    Attention,
}

fn rgb(r: f64, g: f64, b: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)
}

fn gradient(colors: &[Retained<NSColor>]) -> Option<Retained<NSGradient>> {
    NSGradient::initWithColors(NSGradient::alloc(), &NSArray::from_retained_slice(colors))
}

/// macOS's icon shape: a continuous-corner square on the 1024 grid's 824-point body, as a superellipse.
fn squircle_points(x: f64, y: f64, w: f64, h: f64) -> Vec<(f64, f64)> {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (a, b) = (w / 2.0, h / 2.0);
    let n = 5.0;
    let steps = 720;
    (0..steps)
        .map(|i| {
            let t = i as f64 / steps as f64 * std::f64::consts::TAU;
            let (c, s) = (t.cos(), t.sin());
            (cx + a * c.signum() * c.abs().powf(2.0 / n), cy + b * s.signum() * s.abs().powf(2.0 / n))
        })
        .collect()
}

fn squircle(r: NSRect) -> Retained<NSBezierPath> {
    let path = NSBezierPath::bezierPath();
    for (i, (x, y)) in squircle_points(r.origin.x, r.origin.y, r.size.width, r.size.height).into_iter().enumerate() {
        if i == 0 { path.moveToPoint(NSPoint::new(x, y)) } else { path.lineToPoint(NSPoint::new(x, y)) }
    }
    path.closePath();
    path
}

/// The app icon's tile, top and bottom of its gradient (the setup prototype's `.apptile`, 28 Oct).
const TILE_TOP: (f64, f64, f64) = (0x2e as f64 / 255.0, 0x2e as f64 / 255.0, 0x31 as f64 / 255.0);
const TILE_BOTTOM: (f64, f64, f64) = (0x12 as f64 / 255.0, 0x12 as f64 / 255.0, 0x14 as f64 / 255.0);
/// The tile's edge (white at 22%) and top highlight (white at 12%), in points of a 156-point tile.
const TILE_EDGE: (f64, f64) = (0.5, 0.22);
const TILE_LIGHT: (f64, f64) = (1.0, 0.12);
/// The helmet's box, as a share of the tile (the prototype's mark at 56% of its tile).
const TILE_MARK: f64 = 0.56;

/// The plate on Apple's 1024 grid: the 824-point body at 100, 100.
const PLATE: (f64, f64) = (100.0, 824.0);

/// The app icon at `px` pixels square (the 1024 grid scaled): the white helmet glyph on a dark graphite tile, as the
/// setup prototype draws it. `app_icon_svg` is the same drawing.
fn draw_app_icon(px: f64) {
    if NSGraphicsContext::currentContext().is_none() {
        return;
    }
    let k = px / 1024.0;
    let body = PLATE.1 * k;
    let plate = squircle(rect(PLATE.0 * k, PLATE.0 * k, body, body));
    // The plate's own shadow, as on Apple's grid; below 64 px it only blurs the edge.
    NSGraphicsContext::saveGraphicsState_class();
    if px >= 64.0 {
        let shadow = objc2_app_kit::NSShadow::new();
        shadow.setShadowColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.35)));
        shadow.setShadowBlurRadius(24.0 * k);
        shadow.setShadowOffset(NSSize::new(0.0, -10.0 * k));
        shadow.set();
    }
    rgb(TILE_BOTTOM.0, TILE_BOTTOM.1, TILE_BOTTOM.2).setFill();
    plate.fill();
    NSGraphicsContext::restoreGraphicsState_class();
    // y up: 90 degrees runs from the bottom colour to the top one.
    if let Some(g) = gradient(&[rgb(TILE_BOTTOM.0, TILE_BOTTOM.1, TILE_BOTTOM.2), rgb(TILE_TOP.0, TILE_TOP.1, TILE_TOP.2)]) {
        g.drawInBezierPath_angle(&plate, 90.0);
    }
    let pt = body / 156.0;
    NSGraphicsContext::saveGraphicsState_class();
    plate.addClip();
    // The top highlight: the plate less itself moved down, inside the plate.
    let light = squircle(rect(PLATE.0 * k, PLATE.0 * k, body, body));
    light.appendBezierPath(&squircle(rect(PLATE.0 * k, PLATE.0 * k - TILE_LIGHT.0 * pt, body, body)));
    light.setWindingRule(objc2_app_kit::NSWindingRule::EvenOdd);
    NSColor::colorWithWhite_alpha(1.0, TILE_LIGHT.1).setFill();
    light.fill();
    // The edge: a stroke centred on the outline, its outer half clipped away.
    NSColor::colorWithWhite_alpha(1.0, TILE_EDGE.1).setStroke();
    plate.setLineWidth((2.0 * TILE_EDGE.0 * pt).max(1.0));
    plate.stroke();
    NSGraphicsContext::restoreGraphicsState_class();
    // The helmet glyph in white, centred.
    let side = TILE_MARK * body;
    draw_glyph(rect(512.0 * k - side / 2.0, 512.0 * k - side / 2.0, side, side), false, &NSColor::whiteColor());
}

/// The app icon as an SVG on the 1024 grid: the same plate, gradient, edge, highlight, shadow and helmet as
/// `draw_app_icon`, for the README, the site and anything outside the app (`assets/icon/`).
pub fn app_icon_svg() -> String {
    let (o, body) = PLATE;
    let pt = body / 156.0;
    let side = TILE_MARK * body;
    let (x0, y0) = (512.0 - side / 2.0, 512.0 - side / 2.0);
    let unit = side / 100.0;
    // SVG is y down: the 100-unit box flips about its middle.
    let p = |(x, y): (f64, f64)| format!("{:.2} {:.2}", x0 + x * unit, y0 + (100.0 - y) * unit);
    let d = |ops: &[Op]| {
        ops.iter()
            .map(|op| match *op {
                Op::Move(a) => format!("M{}", p(a)),
                Op::Line(a) => format!("L{}", p(a)),
                Op::Curve(to, c1, c2) => format!("C{} {} {}", p(c1), p(c2), p(to)),
                Op::Close => "Z".into(),
            })
            .collect::<String>()
    };
    let plate = |dy: f64| {
        squircle_points(o, o + dy, body, body)
            .iter()
            .enumerate()
            .map(|(i, (x, y))| format!("{}{:.2} {:.2}", if i == 0 { "M" } else { "L" }, x, y))
            .collect::<String>()
            + "Z"
    };
    let hex = |(r, g, b): (f64, f64, f64)| format!("#{:02x}{:02x}{:02x}", (r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8);
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <title>Heimdall</title>
  <defs>
    <linearGradient id="tile" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{top}"/>
      <stop offset="1" stop-color="{bottom}"/>
    </linearGradient>
    <clipPath id="plate"><path d="{plate}"/></clipPath>
    <filter id="shadow" x="-10%" y="-10%" width="120%" height="125%">
      <feDropShadow dx="0" dy="10" stdDeviation="12" flood-color="#000" flood-opacity="0.35"/>
    </filter>
    <mask id="helmet" maskUnits="userSpaceOnUse" x="0" y="0" width="1024" height="1024">
      <path d="{cast}" fill="#fff"/>
      <path d="{face}" fill="#000"/>
    </mask>
  </defs>
  <path d="{plate}" fill="url(#tile)" filter="url(#shadow)"/>
  <g clip-path="url(#plate)">
    <path d="{plate}{lowered}" fill="#fff" fill-opacity="{light}" fill-rule="evenodd"/>
    <path d="{plate}" fill="none" stroke="#fff" stroke-opacity="{edge}" stroke-width="{edge_w:.2}"/>
  </g>
  <rect width="1024" height="1024" fill="#fff" mask="url(#helmet)"/>
</svg>
"##,
        top = hex(TILE_TOP),
        bottom = hex(TILE_BOTTOM),
        plate = plate(0.0),
        lowered = plate(TILE_LIGHT.0 * pt),
        light = TILE_LIGHT.1,
        edge = TILE_EDGE.1,
        edge_w = 2.0 * TILE_EDGE.0 * pt,
        cast = d(&casting_ops(GLYPH_HORN)),
        face = d(&face_ops()),
    )
}

/// An offscreen RGBA bitmap `w` × `h` pixels with `scale` pixels per point, drawn by `paint` in points, y up.
pub(crate) fn png(path: &Path, w: f64, h: f64, scale: f64, paint: impl FnOnce()) -> bool {
    let (pw, ph) = ((w * scale).round() as isize, (h * scale).round() as isize);
    let Some(rep) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), pw, ph, 8, 4, true, false,
            objc2_app_kit::NSDeviceRGBColorSpace, 0, 0,
        )
    }) else {
        return false;
    };
    rep.setSize(NSSize::new(w, h));
    let Some(context) = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep) else { return false };
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    paint();
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    let Some(data) = (unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }) else {
        return false;
    };
    data.writeToFile_atomically(&ns(&path.to_string_lossy()), true)
}

/// The sizes macOS reads from an `.icns`: (file name in the `.iconset`, pixels).
pub const ICONSET: [(&str, u32); 10] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

/// Writes the app icon's `.iconset` (every size macOS needs) into `dir`. Returns the files that failed.
pub fn write_iconset(dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).ok();
    ICONSET
        .iter()
        .filter_map(|(name, px)| {
            let px = *px as f64;
            (!png(&dir.join(name), px, px, 1.0, || draw_app_icon(px))).then(|| name.to_string())
        })
        .collect()
}

/// Menu-bar strips: the glyph beside the system's Wi-Fi and battery symbols on a flat bar of the appearance's
/// colour, 24 pt tall, ink as the menu bar draws template images.
pub fn write_menu_bar(path: &Path, state: BarState, dark: bool, scale: f64) -> bool {
    let (w, h) = (96.0, 24.0);
    png(path, w, h, scale, || {
        let (bar, ink) = if dark {
            (rgb(0.13, 0.13, 0.14), NSColor::colorWithWhite_alpha(1.0, 1.0))
        } else {
            (rgb(0.93, 0.93, 0.94), NSColor::colorWithWhite_alpha(0.0, 0.85))
        };
        bar.setFill();
        objc2_app_kit::NSRectFill(rect(0.0, 0.0, w, h));
        let y = ((h - GLYPH.height) / 2.0).round();
        let at = rect(10.0, y, GLYPH.width, GLYPH.height);
        match state {
            BarState::On | BarState::Off => draw_mark(at, state == BarState::Off, &ink),
            // The real image, drawn under the bar's appearance so its red resolves as it will in the menu bar.
            BarState::Attention => {
                let name = unsafe {
                    if dark { objc2_app_kit::NSAppearanceNameDarkAqua } else { objc2_app_kit::NSAppearanceNameAqua }
                };
                if let Some(appearance) = objc2_app_kit::NSAppearance::appearanceNamed(name) {
                    let image = menu_bar_attention_image();
                    let block = block2::RcBlock::new(move || image.drawInRect(at));
                    appearance.performAsCurrentDrawingAppearance(&block);
                }
            }
        }
        let mut x = 40.0;
        for name in ["wifi", "battery.75percent"] {
            if let Some(image) = super::util::symbol_sized(name, 13.0, 0.0) {
                let colours = objc2_app_kit::NSImageSymbolConfiguration::configurationWithPaletteColors(
                    &NSArray::from_retained_slice(&[ink.clone()]),
                );
                if let Some(image) = image.imageWithSymbolConfiguration(&colours) {
                    let s = image.size();
                    image.drawInRect(rect(x, ((h - s.height) / 2.0).round(), s.width, s.height));
                    x += s.width + 14.0;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_svg_draws_the_glyph_the_menu_bar_draws_on_the_graphite_tile() {
        let svg = app_icon_svg();
        // The glyph's horn weights, not a second geometry: the cast path is the menu-bar casting laid out on the tile.
        let ops = casting_ops(GLYPH_HORN);
        assert_eq!(svg.matches("<path d=\"M").count(), 6, "clip, plate, highlight, edge, cast and face: {svg}");
        let cast = svg.split("mask id=\"helmet\"").nth(1).and_then(|m| m.split("d=\"").nth(1)).unwrap();
        let curves = ops.iter().filter(|o| matches!(o, Op::Curve(..))).count();
        assert_eq!(cast.split('"').next().unwrap().matches('C').count(), curves);
        assert!(svg.contains("#2e2e31") && svg.contains("#121214"));
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
    }
}
