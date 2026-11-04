//! Rasters: every distinct piece of island content (a line of text, a symbol, a button, a note's body) drawn once
//! with AppKit into a bitmap at the display's scale, and a blurred twin for the arrival blur. The layer tree only
//! moves and fades these images; nothing is drawn per frame (DESIGN.md §6: "text rasterised once per state").

use std::collections::HashMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::AnyThread as _;
use objc2_app_kit::{
    NSAttributedStringNSExtendedStringDrawing as _, NSAttributedStringNSStringDrawing as _, NSBezierPath, NSColor, NSCompositingOperation, NSFont, NSFontWeightBold, NSFontWeightRegular,
    NSFontWeightSemibold, NSGraphicsContext, NSImage, NSImageSymbolConfiguration, NSLineCapStyle, NSStringDrawingOptions,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetData, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSArray, NSData, NSPoint, NSRect, NSSize, NSString};

use super::super::util::{attributed, rect};
use super::measure::ns_font;
use crate::agent_notice::Detail;
use crate::island::object::{Capsule, Content, Font, Mark, Placed, Rgba, NOTE_HEAD, NOTE_PIN_GAP};

/// Points of transparent margin round every raster: room for anti-aliasing, a hover ring and the blur.
pub(crate) const PAD: f64 = 6.0;
/// The arrival blur's strongest radius (points); weaker blurs blend the sharp and the blurred raster.
pub(crate) const BLUR: f64 = 8.0;

/// An sRGB, premultiplied RGBA bitmap drawn in points, y down from the top.
pub(crate) struct Canvas {
    pub ctx: CFRetained<CGContext>,
    pub pw: usize,
    pub ph: usize,
    pub scale: f64,
}

impl Canvas {
    pub(crate) fn new(w: f64, h: f64, scale: f64) -> Option<Canvas> {
        let (pw, ph) = (((w * scale).ceil() as usize).max(1), ((h * scale).ceil() as usize).max(1));
        if pw > 16384 || ph > 16384 {
            return None;
        }
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
        let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
        let ctx = unsafe { CGBitmapContextCreate(std::ptr::null_mut(), pw, ph, 8, pw * 4, Some(&space), info) }?;
        CGContext::translate_ctm(Some(&ctx), 0.0, ph as f64);
        CGContext::scale_ctm(Some(&ctx), scale, -scale);
        Some(Canvas { ctx, pw, ph, scale })
    }

    /// Runs `paint` with this bitmap as the current flipped AppKit context.
    pub(crate) fn draw(&self, paint: impl FnOnce()) {
        CGContext::save_g_state(Some(&self.ctx));
        let ns = NSGraphicsContext::graphicsContextWithCGContext_flipped(&self.ctx, true);
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ns));
        paint();
        ns.flushGraphics();
        NSGraphicsContext::restoreGraphicsState_class();
        CGContext::restore_g_state(Some(&self.ctx));
    }

    pub(crate) fn clear(&self) {
        CGContext::save_g_state(Some(&self.ctx));
        CGContext::clear_rect(Some(&self.ctx), CGRect::new(CGPoint::new(-1e5, -1e5), CGSize::new(2e5, 2e5)));
        CGContext::restore_g_state(Some(&self.ctx));
    }

    pub(crate) fn image(&self) -> Option<CFRetained<CGImage>> {
        CGBitmapContextCreateImage(Some(&self.ctx))
    }

    pub(crate) fn pixels(&self) -> &[u8] {
        let data = CGBitmapContextGetData(Some(&self.ctx));
        unsafe { std::slice::from_raw_parts(data as *const u8, self.pw * self.ph * 4) }
    }

    pub(crate) fn pixels_mut(&mut self) -> &mut [u8] {
        let data = CGBitmapContextGetData(Some(&self.ctx));
        unsafe { std::slice::from_raw_parts_mut(data as *mut u8, self.pw * self.ph * 4) }
    }

    /// The bitmap as PNG bytes.
    pub(crate) fn png(&self) -> Option<Vec<u8>> {
        let image = self.image()?;
        let rep = objc2_app_kit::NSBitmapImageRep::initWithCGImage(objc2_app_kit::NSBitmapImageRep::alloc(), &image);
        let data = unsafe { rep.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &objc2_foundation::NSDictionary::new()) }?;
        Some(data.to_vec())
    }
}

pub(crate) fn color(c: Rgba) -> Retained<NSColor> {
    let [r, g, b, a] = c.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, a as f64 / 255.0)
}

/// An SF Symbol configured at `size` points, semibold (bold when asked), in its palette.
pub(crate) fn symbol(name: &str, size: f64, bold: bool, palette: &[Rgba]) -> Option<Retained<NSImage>> {
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    let weight = unsafe { if bold { NSFontWeightBold } else { NSFontWeightSemibold } };
    let colors: Vec<Retained<NSColor>> = palette.iter().map(|c| color(*c)).collect();
    let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight)
        .configurationByApplyingConfiguration(&NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&colors)));
    base.imageWithSymbolConfiguration(&config)
}

fn draw_symbol(name: &str, size: f64, bold: bool, palette: &[Rgba], cx: f64, cy: f64, alpha: f64) {
    let Some(img) = symbol(name, size, bold, palette) else { return };
    let s = img.size();
    let r = rect(cx - s.width / 2.0, cy - s.height / 2.0, s.width, s.height);
    unsafe { img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(r, NSRect::ZERO, NSCompositingOperation::SourceOver, alpha, true, None) };
}

/// One line of text whose cap height is centred on `cy`, starting at `x` (left aligned).
fn draw_line(text: &str, font: &NSFont, ink: &NSColor, x: f64, cy: f64) -> f64 {
    let s = attributed(text, font, ink);
    let top = cy + font.capHeight() / 2.0 - font.ascender();
    s.drawAtPoint(NSPoint::new(x, top));
    s.size().width
}

fn width(text: &str, font: &NSFont) -> f64 {
    attributed(text, font, &NSColor::whiteColor()).size().width
}

fn fill_capsule(r: NSRect, ink: &NSColor) {
    ink.setFill();
    let rad = r.size.height.min(r.size.width) / 2.0;
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, rad, rad).fill();
}

fn fill_circle(cx: f64, cy: f64, d: f64, ink: &NSColor) {
    ink.setFill();
    NSBezierPath::bezierPathWithOvalInRect(rect(cx - d / 2.0, cy - d / 2.0, d, d)).fill();
}

/// A drawn piece of content: its image (`size` + `PAD` on every side) and the blurred twin.
pub(crate) struct Raster {
    pub image: CFRetained<CGImage>,
    pub blurred: Option<CFRetained<CGImage>>,
}

/// A note's body, drawn once when it arrives.
pub(crate) struct NoteDoc {
    pub image: CFRetained<CGImage>,
    pub w: f64,
    pub h: f64,
    /// Cut at `MAX_NOTE_H`, ending with a line that says so.
    pub cut: bool,
    /// The backing scale it was drawn at, and what it was drawn from (`note`'s arguments), so a move to a display of
    /// another scale draws it again rather than stretching it (bug note 2026-10-24T14-01-17).
    pub scale: f64,
    pub source: Rc<(Detail, bool, f64)>,
}

impl NoteDoc {
    /// The same note drawn at `scale` (itself when it already is).
    pub(crate) fn at_scale(self: &Rc<Self>, scale: f64) -> Rc<NoteDoc> {
        if (self.scale - scale).abs() < 1e-9 {
            return self.clone();
        }
        let (detail, log, width) = &*self.source;
        match note(detail, *log, *width, scale) {
            Some(doc) => Rc::new(doc),
            None => self.clone(),
        }
    }
}

/// Draws `content` at its natural size `size` (points).
pub(crate) fn draw(content: &Content, size: (f64, f64), scale: f64, notes: &HashMap<String, Rc<NoteDoc>>) -> Option<Raster> {
    // A body taller than its slot (the pulled-down turn): every line of the reply, then each note under its
    // header, drawn once; the slot shows part of it.
    if let body @ Content::Body { lines, font, color: c, line_h, notes: placed, marks, veil, .. } = content {
        let w = size.0.max(1.0);
        let full = body.body_full().max(1.0);
        // The veil is blurred: drawn at `VEIL_SCALE` (a pixel per two points), cheap to draw and to blur.
        // Its blur spreads past the body's sides: `VEIL_PAD` either side, so it fades out rather than ending on an edge.
        let side = if *veil > 0.0 { VEIL_PAD } else { 0.0 };
        let canvas = Canvas::new(w + 2.0 * side, full, if *veil > 0.0 { VEIL_SCALE } else { scale })?;
        let (f, strong) = (ns_font(*font), ns_font(crate::island::object::prose::STRONG));
        let ink = color(*c);
        canvas.draw(|| {
            if side > 0.0 {
                let t = objc2_foundation::NSAffineTransform::transform();
                t.translateXBy_yBy(side, 0.0);
                objc2_app_kit::NSAffineTransformNSAppKitAdditions::concat(&*t);
            }
            for m in marks {
                draw_mark(m);
            }
            for row in lines {
                draw_line(&row.text, if row.strong { &strong } else { &f }, &ink, row.x, row.y + line_h / 2.0);
            }
            for p in placed {
                note_header(p, w);
                if let Some(doc) = notes.get(&p.id) {
                    let img = NSImage::initWithCGImage_size(NSImage::alloc(), &doc.image, NSSize::new(doc.w, doc.h));
                    unsafe { img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(rect(0.0, p.y + NOTE_HEAD, p.w.min(w), p.h), NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, true, None) };
                }
            }
        });
        if *veil > 0.0 {
            let canvas = blur_px(canvas, VEIL_BLUR * VEIL_SCALE)?;
            return Some(Raster { image: canvas.image()?, blurred: None });
        }
        return Some(Raster { image: canvas.image()?, blurred: None });
    }
    let (w, h) = (size.0.max(1.0), size.1.max(1.0));
    let canvas = Canvas::new(w + 2.0 * PAD, h + 2.0 * PAD, scale)?;
    let (cx, cy) = (PAD + w / 2.0, PAD + h / 2.0);
    canvas.draw(|| match content {
        Content::Symbol { name, size, palette, bold } => draw_symbol(name, *size, *bold, palette, cx, cy, 1.0),
        Content::Helmet { size, color: c } => {
            let r = rect(cx - size / 2.0, cy - size / 2.0, *size, *size);
            super::super::helmet::draw_mark(r, false, &color(*c));
        }
        Content::Text { lines, font, color: c, line_h } => {
            let f = ns_font(*font);
            let ink = color(*c);
            for (i, line) in lines.iter().enumerate() {
                let lw = width(line, &f);
                draw_line(line, &f, &ink, cx - lw / 2.0, PAD + line_h * (i as f64 + 0.5));
            }
        }
        Content::Unit { text, focus, size } => {
            let f = ns_font(Font::Reader(*size as u8));
            let chars: Vec<char> = text.chars().collect();
            let i = (*focus).min(chars.len().saturating_sub(1));
            let pre: String = chars[..i.min(chars.len())].iter().collect();
            let mid: String = chars.get(i).map(|c| c.to_string()).unwrap_or_default();
            let post: String = chars.get(i + 1..).map(|s| s.iter().collect()).unwrap_or_default();
            let (pw, mw) = (width(&pre, &f), width(&mid, &f));
            // The focus letter's centre sits where the layout put the centre line (its left extent from the slot's
            // left edge), the rest extends either side of it.
            let left = (pw + mw / 2.0).min(crate::island::object::CHUNK_SIDE + 30.0);
            let focus_x = PAD + left;
            let white = NSColor::whiteColor();
            draw_line(&pre, &f, &white, focus_x - mw / 2.0 - pw, cy);
            draw_line(&mid, &f, &color(Rgba::RED), focus_x - mw / 2.0, cy);
            draw_line(&post, &f, &white, focus_x + mw / 2.0, cy);
        }
        Content::Split { read, rest } => {
            let mut size = Font::Sentence.size();
            let mut f = NSFont::systemFontOfSize_weight(size, Font::Sentence.weight());
            while width(&format!("{read}{rest}"), &f) > w && size > 11.0 {
                size -= 1.0;
                f = NSFont::systemFontOfSize_weight(size, Font::Sentence.weight());
            }
            let (rw, sw) = (width(read, &f), width(rest, &f));
            let x0 = cx - (rw + sw) / 2.0;
            draw_line(read, &f, &color(Rgba::RED), x0, cy);
            draw_line(rest, &f, &NSColor::whiteColor(), x0 + rw, cy);
        }
        Content::Capsule { label, key, symbol: sym, style } => {
            let r = rect(PAD, PAD, w, h);
            let (fill, ink) = match style {
                Capsule::White | Capsule::Hover | Capsule::Chosen => (NSColor::whiteColor(), NSColor::blackColor()),
                Capsule::Pressed => (color(Rgba::PRESSED), NSColor::blackColor()),
                Capsule::Dim => (NSColor::colorWithWhite_alpha(1.0, 0.12), NSColor::colorWithWhite_alpha(1.0, 0.35)),
            };
            fill_capsule(r, &fill);
            if *style == Capsule::Hover {
                // The hover ring: the capsule's own outline lifted 2 pt.
                let ring = rect(PAD - 2.0, PAD - 2.0, w + 4.0, h + 4.0);
                let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(ring, ring.size.height / 2.0, ring.size.height / 2.0);
                p.setLineWidth(1.5);
                NSColor::colorWithWhite_alpha(1.0, 0.55).setStroke();
                p.stroke();
            }
            let label_font = ns_font(Font::Button);
            let key_font = ns_font(Font::Key);
            let mut parts: Vec<(f64, Box<dyn Fn(f64)>)> = vec![];
            if let Some(s) = sym.clone() {
                let ink = ink.clone();
                parts.push((16.0, Box::new(move |x| draw_symbol(&s, 13.0, true, &[rgba_of(&ink)], x + 8.0, cy, 1.0))));
            }
            if !label.is_empty() {
                let (l, f, ink) = (label.clone(), label_font.clone(), ink.clone());
                parts.push((width(&l, &f), Box::new(move |x| {
                    draw_line(&l, &f, &ink, x, cy);
                })));
            }
            if let Some(k) = key.clone() {
                let kf = key_font.clone();
                let kc = color(Rgba::GREY);
                parts.push((width(&k, &kf), Box::new(move |x| {
                    draw_line(&k, &kf, &kc, x, cy);
                })));
            }
            let gap = 6.0;
            let total: f64 = parts.iter().map(|p| p.0).sum::<f64>() + gap * parts.len().saturating_sub(1) as f64;
            let mut x = cx - total / 2.0;
            for (pw, d) in parts {
                d(x);
                x += pw + gap;
            }
        }
        Content::Dot { color: c, d } => {
            if *d > 0.0 {
                fill_circle(cx, cy, *d, &color(*c));
            }
        }
        // A countdown's ring: the track, then the share left as a round-capped arc ending at twelve o'clock. The
        // canvas is flipped, so twelve is -90° and a rising angle turns clockwise on screen.
        Content::Ring { left_milli, d } => {
            let width = 2.0;
            let r = d / 2.0 - width / 2.0;
            let track = NSBezierPath::bezierPathWithOvalInRect(rect(cx - r, cy - r, 2.0 * r, 2.0 * r));
            track.setLineWidth(width);
            NSColor::colorWithWhite_alpha(1.0, 0.24).setStroke();
            track.stroke();
            let left = (*left_milli).min(1000) as f64 / 1000.0;
            if left > 0.0 {
                let arc = NSBezierPath::bezierPath();
                arc.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(NSPoint::new(cx, cy), r, 270.0 - 360.0 * left, 270.0, false);
                arc.setLineWidth(width);
                arc.setLineCapStyle(NSLineCapStyle::Round);
                NSColor::colorWithWhite_alpha(1.0, 0.86).setStroke();
                arc.stroke();
            }
        }
        Content::Tick => fill_capsule(rect(cx - 1.0, cy - 4.5, 2.0, 9.0), &color(Rgba::RED)),
        // A rounded rectangle: its fill, then its ring inset by half its width so the stroke stays inside the rect.
        Content::Panel { fill, ring, radius } => {
            if fill.alpha() > 0.0 {
                let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(PAD, PAD, w, h), *radius, *radius);
                color(*fill).setFill();
                p.fill();
            }
            if let Some((c, width)) = ring {
                let inset = width / 2.0;
                let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(PAD + inset, PAD + inset, w - 2.0 * inset, h - 2.0 * inset), *radius, *radius);
                p.setLineWidth(*width);
                color(*c).setStroke();
                p.stroke();
            }
        }
        // A native text field: the window places and draws it (`paint.rs`); nothing to rasterise.
        Content::Field { .. } => {}
        // Black from transparent at its top to opaque at its bottom, in 1 pt steps.
        Content::Fade { on } => {
            if *on {
                let steps = h.ceil().max(1.0) as usize;
                for k in 0..steps {
                    let a = ((k as f64 + 0.5) / steps as f64).powf(1.6);
                    NSColor::colorWithWhite_alpha(0.0, a).setFill();
                    NSBezierPath::fillRect(rect(PAD, PAD + k as f64, w, 1.0));
                }
            }
        }
        Content::FadeTop { on } => {
            if *on {
                let steps = h.ceil().max(1.0) as usize;
                for k in 0..steps {
                    let a = (1.0 - (k as f64 + 0.5) / steps as f64).powf(1.6);
                    NSColor::colorWithWhite_alpha(0.0, a).setFill();
                    NSBezierPath::fillRect(rect(PAD, PAD + k as f64, w, 1.0));
                }
            }
        }
        Content::Grabber { hot } => fill_capsule(rect(cx - 18.0, cy - 2.5, 36.0, 5.0), &NSColor::colorWithWhite_alpha(1.0, if *hot { 0.6 } else { 0.32 })),
        Content::Vella { size } => {
            if let Some(img) = vella_mark() {
                let r = rect(cx - size / 2.0, cy - size / 2.0, *size, *size);
                unsafe { img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(r, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, true, None) };
            }
        }
        Content::Thumb { path, image, symbol: sym, radius } => draw_thumb(path.as_deref(), *image, sym, *radius, rect(PAD, PAD, w, h)),
        // Live layers (`paint.rs`), never rasterised.
        Content::Dots | Content::Waveform { .. } | Content::Spinner | Content::Body { .. } => {}
    });
    let image = canvas.image()?;
    let blurred = half(&canvas, w + 2.0 * PAD, h + 2.0 * PAD).and_then(|c| blur(c, BLUR)).and_then(|c| c.image());
    Some(Raster { image, blurred })
}

fn rgba_of(c: &NSColor) -> Rgba {
    let c = c.colorUsingColorSpace(&objc2_app_kit::NSColorSpace::sRGBColorSpace()).unwrap_or_else(|| objc2::Message::retain(c));
    Rgba([c.redComponent(), c.greenComponent(), c.blueComponent(), c.alphaComponent()].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8))
}

/// `c` at half its pixel size (2 × 2 averages): the blurred twin is blurred at half resolution, a quarter of the
/// work, and its layer scales it up (a blur has nothing finer to lose).
fn half(c: &Canvas, w: f64, h: f64) -> Option<Canvas> {
    let mut out = Canvas::new(w, h, c.scale * 0.5)?;
    let (sw, sh, dw, dh) = (c.pw, c.ph, out.pw, out.ph);
    let src = c.pixels().to_vec();
    let dst = out.pixels_mut();
    for y in 0..dh {
        for x in 0..dw {
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for (sx, sy) in [(2 * x, 2 * y), (2 * x + 1, 2 * y), (2 * x, 2 * y + 1), (2 * x + 1, 2 * y + 1)] {
                if sx < sw && sy < sh {
                    let i = (sy * sw + sx) * 4;
                    for ch in 0..4 {
                        acc[ch] += src[i + ch] as u32;
                    }
                    n += 1;
                }
            }
            let o = (y * dw + x) * 4;
            for ch in 0..4 {
                dst[o + ch] = (acc[ch] / n.max(1)) as u8;
            }
        }
    }
    Some(out)
}

/// The veil's drawing scale (pixels per point) and its blur (sigma, points).
pub(crate) const VEIL_SCALE: f64 = 0.5;
pub(crate) const VEIL_BLUR: f64 = 6.0;
/// The veil's margin either side of its body (its blur's reach, about three sigma).
pub(crate) const VEIL_PAD: f64 = 18.0;

/// A Gaussian-like blur of sigma `sigma` pixels: three box passes each way on the premultiplied pixels.
pub(crate) fn blur_px(mut c: Canvas, sigma: f64) -> Option<Canvas> {
    let box_r = (((12.0 * sigma * sigma / 3.0 + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    let (w, h) = (c.pw, c.ph);
    let px = c.pixels_mut();
    let mut tmp = vec![0u8; px.len()];
    for _ in 0..3 {
        box_pass(px, &mut tmp, w, h, box_r, true);
        box_pass(&tmp.clone(), px, w, h, box_r, false);
    }
    Some(c)
}

/// A Gaussian-like blur of radius `radius` points: three box passes each way on the premultiplied pixels.
pub(crate) fn blur(mut c: Canvas, radius: f64) -> Option<Canvas> {
    let sigma = radius * c.scale * 0.5;
    let box_r = (((12.0 * sigma * sigma / 3.0 + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    let (w, h) = (c.pw, c.ph);
    let px = c.pixels_mut();
    let mut tmp = vec![0u8; px.len()];
    for _ in 0..3 {
        box_pass(px, &mut tmp, w, h, box_r, true);
        box_pass(&tmp.clone(), px, w, h, box_r, false);
    }
    Some(c)
}

fn box_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horizontal: bool) {
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    let idx = |o: usize, i: usize| if horizontal { (o * w + i) * 4 } else { (i * w + o) * 4 };
    let n = (2 * r + 1) as u32;
    for o in 0..outer {
        let mut acc = [0u32; 4];
        for k in 0..=r.min(inner.saturating_sub(1)) {
            for ch in 0..4 {
                acc[ch] += src[idx(o, k) + ch] as u32;
            }
        }
        for i in 0..inner {
            for ch in 0..4 {
                dst[idx(o, i) + ch] = (acc[ch] / n) as u8;
            }
            let add = i + r + 1;
            if add < inner {
                for ch in 0..4 {
                    acc[ch] += src[idx(o, add) + ch] as u32;
                }
            }
            if i >= r {
                let sub = i - r;
                for ch in 0..4 {
                    acc[ch] -= src[idx(o, sub) + ch] as u32;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ notes

/// The X a note's header shows at its right end: a grey disc with a dark cross (macOS's own close look).
pub(crate) const CLOSE_SYMBOL: &str = "xmark.circle.fill";
pub(crate) const CLOSE_PALETTE: [Rgba; 2] = [Rgba([0x1C, 0x1C, 0x1E, 255]), Rgba([0x8E, 0x8E, 0x93, 255])];

/// A note's header row in the pulled-down turn (`NOTE_HEAD` tall at `p.y`, `w` wide): its icon and title on the
/// left, its close X at the right end (the layout's `Target::NoteClose` sits over it).
fn note_header(p: &Placed, w: f64) {
    let cy = p.y + NOTE_HEAD / 2.0;
    draw_symbol(&p.icon, 15.0, false, &p.palette, 8.0, cy, 1.0);
    let f = ns_font(Font::Label);
    let title = ellipsize_in(&p.title, &f, (w - 23.0 - 34.0 - NOTE_PIN_GAP).max(10.0));
    draw_line(&title, &f, &NSColor::whiteColor(), 23.0, cy);
    // Its pin (this note alone leaves as a floating island), then its close X.
    draw_symbol("pin.fill", 11.0, false, &[Rgba::white(0.55)], w - 9.0 - NOTE_PIN_GAP, cy, 1.0);
    draw_symbol(CLOSE_SYMBOL, 16.0, false, &CLOSE_PALETTE, w - 9.0, cy, 1.0);
}

/// One piece of the conversation around the turn (`object::chat_layout`).
fn draw_mark(m: &Mark) {
    match m {
        Mark::Fill { x, y, w, h, r, color: c } => {
            color(*c).setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(*x, *y, *w, *h), *r, *r).fill();
        }
        Mark::Text { text, x, y, line_h, font, color: c } => {
            draw_line(text, &ns_font(*font), &color(*c), *x, y + line_h / 2.0);
        }
        Mark::Symbol { name, size, palette, cx, cy } => draw_symbol(name, *size, false, palette, *cx, *cy, 1.0),
    }
}

/// A note's body at most `width` points wide: a table, wrapped text, a monospaced log, an SVG or an image. Each is
/// drawn as wide as its content needs (bug note 2026-10-19T18-46-34; a table's columns whole, bug note
/// 2026-10-20T23-12-49), and its height is the content's own (the island scrolls it when taller than half the
/// screen).
pub(crate) fn note(detail: &Detail, log: bool, width: f64, scale: f64) -> Option<NoteDoc> {
    let width = width.max(40.0);
    let mut natural = width;
    let mut cut = false;
    let (h, paint): (f64, Box<dyn Fn()>) = match detail {
        Detail::Table { headers, rows } => {
            let t = TableLayout::new(headers, rows, width);
            natural = t.width().max(40.0);
            cut = t.more > 0;
            let h = t.height();
            (h, Box::new(move || t.draw()))
        }
        Detail::Text(text) => {
            let text = text.trim().to_string();
            if log {
                let width = width.min(LOG_W);
                let f = NSFont::monospacedSystemFontOfSize_weight(11.5, unsafe { NSFontWeightRegular });
                // At most what `MAX_NOTE_H` holds; a longer log ends with how many lines it left out.
                let all = text.lines().count();
                let room = ((MAX_NOTE_H - 6.0) / 18.0).floor() as usize;
                let keep = if all > room { room - 1 } else { all };
                let lines: Vec<String> = text.lines().take(keep).map(str::to_string).collect();
                let more = all - keep;
                cut = more > 0;
                let h = 18.0 * (lines.len() + usize::from(cut)).max(1) as f64 + 6.0;
                // As wide as its longest line (a longer one is cut at the widest a log is drawn).
                natural = lines.iter().map(|l| self::width(l, &f)).fold(40.0, f64::max).ceil().min(width);
                (h, Box::new(move || {
                    for (i, l) in lines.iter().enumerate() {
                        let ink = NSColor::colorWithWhite_alpha(1.0, if i == 0 { 0.95 } else { 0.62 });
                        let s = ellipsize_in(l, &f, width);
                        draw_line(&s, &f, &ink, 0.0, 9.0 + 18.0 * i as f64);
                    }
                    if more > 0 {
                        let words = if more == 1 { "1 more line".to_string() } else { format!("{more} more lines") };
                        draw_line(&words, &f, &NSColor::colorWithWhite_alpha(1.0, 0.45), 0.0, 9.0 + 18.0 * lines.len() as f64);
                    }
                }))
            } else {
                let width = width.min(TEXT_W);
                let f = NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightRegular });
                let s = attributed(&text, &f, &NSColor::colorWithWhite_alpha(1.0, 0.92));
                let bounds = s.boundingRectWithSize_options_context(NSSize::new(width, 100_000.0), NSStringDrawingOptions::UsesLineFragmentOrigin, None);
                natural = bounds.size.width.ceil().clamp(40.0, width);
                if bounds.size.height.ceil() <= MAX_NOTE_H {
                    let h = bounds.size.height.ceil().max(18.0);
                    (h, Box::new(move || s.drawWithRect_options_context(rect(0.0, 0.0, width, 100_000.0), NSStringDrawingOptions::UsesLineFragmentOrigin, None)))
                } else {
                    // Longer than `MAX_NOTE_H`: the whole lines that fit, then a line saying where the rest is.
                    cut = true;
                    let body_h = MAX_NOTE_H - 26.0;
                    (MAX_NOTE_H, Box::new(move || {
                        s.drawWithRect_options_context(rect(0.0, 0.0, width, body_h), NSStringDrawingOptions::UsesLineFragmentOrigin | NSStringDrawingOptions::TruncatesLastVisibleLine, None);
                        draw_line(crate::island::object::REPLY_CUT, &f, &NSColor::colorWithWhite_alpha(1.0, 0.45), 0.0, body_h + 13.0);
                    }))
                }
            }
        }
        Detail::Svg(svg) => picture(NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(svg.as_bytes())), width.min(PICTURE_W), &mut natural),
        Detail::Image { archive_id } => {
            let bytes = crate::attachment_loader().and_then(|l| l(archive_id));
            let image = bytes.and_then(|b| NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&b)));
            picture(image, width.min(PICTURE_W), &mut natural)
        }
    };
    let canvas = Canvas::new(natural, h, scale)?;
    canvas.draw(|| paint());
    Some(NoteDoc { image: canvas.image()?, w: natural, h, cut, scale, source: Rc::new((detail.clone(), log, width)) })
}

/// Text notes wrap at the reply's own width; logs and pictures may use a little more.
const TEXT_W: f64 = 420.0;
const LOG_W: f64 = 560.0;
const PICTURE_W: f64 = 560.0;
/// The tallest note drawing (the whole turn is one bitmap below Core Animation's limit): a longer table ends with
/// a count of the rows left out.
pub(crate) const MAX_NOTE_H: f64 = 3000.0;

/// A picture at most `width` wide (and 420 tall); `natural` becomes the width it is drawn at.
fn picture(image: Option<Retained<NSImage>>, width: f64, natural: &mut f64) -> (f64, Box<dyn Fn()>) {
    match image.filter(|i| i.size().width > 0.0 && i.size().height > 0.0) {
        Some(img) => {
            let s = img.size();
            let k = (width / s.width).min(1.0).min(420.0 / s.height);
            let (w, h) = (s.width * k, s.height * k);
            let canvas_w = w.ceil().clamp(40.0, width);
            *natural = canvas_w;
            (h, Box::new(move || unsafe {
                img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(rect((canvas_w - w) / 2.0, 0.0, w, h), NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, true, None)
            }))
        }
        None => (24.0, Box::new(move || {
            let f = NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightRegular });
            draw_line("Not available", &f, &NSColor::colorWithWhite_alpha(1.0, 0.55), 0.0, 12.0);
        })),
    }
}

fn numeric(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '+' || c == '$') && t.chars().filter(|c| c.is_ascii_digit()).count() * 2 >= t.chars().filter(|c| !c.is_whitespace()).count()
}

/// Space between two columns.
const COL_GAP: f64 = 18.0;
/// The narrowest a column is squeezed to before the table is simply wider than its room.
const COL_MIN: f64 = 44.0;
/// Room above and below a row's text.
const CELL_PAD: f64 = 5.0;
const ROW_MIN: f64 = 26.0;
const HEAD_H: f64 = 22.0;

/// The pure part of a table's layout: each column's width from its natural width and the room (columns that fit
/// keep their width; the rest share what is left equally, and their cells wrap).
pub(crate) fn column_widths(natural: &[f64], room: f64) -> Vec<f64> {
    let n = natural.len();
    if n == 0 {
        return vec![];
    }
    let avail = (room - COL_GAP * (n - 1) as f64).max(COL_MIN * n as f64);
    if natural.iter().sum::<f64>() <= avail {
        return natural.to_vec();
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| natural[*a].total_cmp(&natural[*b]));
    let mut out = vec![0.0; n];
    let mut left = avail;
    for (k, &i) in order.iter().enumerate() {
        let share = left / (n - k) as f64;
        let w = natural[i].min(share).max(COL_MIN);
        out[i] = w;
        left -= w;
    }
    out
}

/// A table laid out at most `room` wide: column widths, each row's height (cells wrap), the rows that fit
/// `MAX_NOTE_H`.
struct TableLayout {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    widths: Vec<f64>,
    numeric: Vec<bool>,
    head_h: f64,
    row_h: Vec<f64>,
    /// Rows left out past `MAX_NOTE_H`.
    more: usize,
}

impl TableLayout {
    fn fonts() -> (Retained<NSFont>, Retained<NSFont>, Retained<NSFont>) {
        (
            NSFont::systemFontOfSize_weight(11.0, unsafe { NSFontWeightSemibold }),
            NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightRegular }),
            NSFont::monospacedDigitSystemFontOfSize_weight(13.0, unsafe { NSFontWeightRegular }),
        )
    }

    fn new(headers: &[String], rows: &[Vec<String>], room: f64) -> TableLayout {
        let (head, body, digits) = Self::fonts();
        let n = headers.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0)).max(1);
        let cell = |r: &Vec<String>, i: usize| r.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
        // A column of numbers (every filled cell numeric) is right aligned in monospaced digits.
        let numeric: Vec<bool> = (0..n).map(|i| {
            let filled: Vec<String> = rows.iter().map(|r| cell(r, i)).filter(|c| !c.is_empty()).collect();
            !filled.is_empty() && filled.iter().all(|c| numeric(c))
        }).collect();
        let natural: Vec<f64> = (0..n).map(|i| {
            let f = if numeric[i] { &digits } else { &body };
            let h = headers.get(i).map_or(0.0, |s| width(&s.to_uppercase(), &head));
            rows.iter().map(|r| width(&cell(r, i), f)).fold(h, f64::max).ceil() + 1.0
        }).collect();
        let widths = column_widths(&natural, room);
        let wrapped = |text: &str, f: &NSFont, w: f64| -> f64 {
            if text.is_empty() {
                return 0.0;
            }
            let s = attributed(text, f, &NSColor::whiteColor());
            s.boundingRectWithSize_options_context(NSSize::new(w, 100_000.0), NSStringDrawingOptions::UsesLineFragmentOrigin, None).size.height.ceil()
        };
        let head_h = (0..n).map(|i| wrapped(&headers.get(i).map_or(String::new(), |s| s.to_uppercase()), &head, widths[i]) + 9.0).fold(HEAD_H, f64::max);
        let mut row_h = vec![];
        let mut total = head_h;
        let mut kept = vec![];
        for r in rows {
            let h = (0..n).map(|i| {
                let c = cell(r, i);
                // Numbers stay on one line (cut, never wrapped).
                if numeric[i] { 16.0 } else { wrapped(&c, &body, widths[i]) }
            }).fold(0.0, f64::max) + 2.0 * CELL_PAD;
            let h = h.max(ROW_MIN);
            if total + h > MAX_NOTE_H - ROW_MIN && !kept.is_empty() {
                break;
            }
            total += h;
            row_h.push(h);
            kept.push(r.clone());
        }
        let more = rows.len() - kept.len();
        TableLayout { headers: headers.to_vec(), rows: kept, widths, numeric, head_h, row_h, more }
    }

    fn width(&self) -> f64 {
        self.widths.iter().sum::<f64>() + COL_GAP * self.widths.len().saturating_sub(1) as f64
    }

    fn height(&self) -> f64 {
        self.head_h + self.row_h.iter().sum::<f64>() + if self.more > 0 { ROW_MIN } else { 0.0 }
    }

    fn x(&self, i: usize) -> f64 {
        self.widths[..i].iter().sum::<f64>() + COL_GAP * i as f64
    }

    fn draw(&self) {
        let (head, body, digits) = Self::fonts();
        let dim = NSColor::colorWithWhite_alpha(1.0, 0.45);
        let white = NSColor::whiteColor();
        let full_w = self.width();
        // Text left aligned and wrapped in its column; a number right aligned on one line.
        let put = |text: &str, i: usize, f: &NSFont, ink: &NSColor, top: f64| {
            let w = self.widths[i];
            if self.numeric[i] {
                let t = ellipsize_in(text, f, w);
                let tw = width(&t, f);
                draw_line(&t, f, ink, self.x(i) + w - tw, top + f.ascender() - f.capHeight() / 2.0);
            } else {
                attributed(text, f, ink).drawWithRect_options_context(rect(self.x(i), top, w, 100_000.0), NSStringDrawingOptions::UsesLineFragmentOrigin, None);
            }
        };
        for (i, hd) in self.headers.iter().enumerate().take(self.widths.len()) {
            put(&hd.to_uppercase(), i, &head, &dim, 0.0);
        }
        let mut y = self.head_h;
        for (row, h) in self.rows.iter().zip(&self.row_h) {
            NSColor::colorWithWhite_alpha(1.0, 0.1).setFill();
            NSBezierPath::fillRect(rect(0.0, y, full_w, 0.5));
            for (i, c) in row.iter().enumerate().take(self.widths.len()) {
                let f = if self.numeric[i] { &digits } else { &body };
                put(c.trim(), i, f, &white, y + CELL_PAD);
            }
            y += h;
        }
        if self.more > 0 {
            NSColor::colorWithWhite_alpha(1.0, 0.1).setFill();
            NSBezierPath::fillRect(rect(0.0, y, full_w, 0.5));
            let words = if self.more == 1 { "1 more row".to_string() } else { format!("{} more rows", self.more) };
            draw_line(&words, &body, &dim, 0.0, y + ROW_MIN / 2.0);
        }
    }
}

/// `text` cut with an ellipsis to fit `max` points in font `f`.
fn ellipsize_in(text: &str, f: &NSFont, max: f64) -> String {
    if width(text, f) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&cand, f) <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

#[cfg(test)]
mod note_tests {
    use super::{note, MAX_NOTE_H};
    use crate::agent_notice::Detail;

    /// Review of 063424a, findings 3 and 4: a long text note and a long error log stop at `MAX_NOTE_H` (one bitmap
    /// holds the whole turn) and say so at their end, as a long table's "N more rows" does.
    #[test]
    fn long_text_and_logs_stop_at_the_cap_and_say_so() {
        let text = note(&Detail::Text("A sentence that keeps going on. ".repeat(4000)), false, 600.0, 2.0).expect("drawn");
        assert!(text.h <= MAX_NOTE_H, "text {} pt", text.h);
        assert!(text.cut, "a cut text ends with a marker");
        let log_text: String = (0..400).map(|i| format!("line {i}: error[E0308]\n")).collect();
        let log = note(&Detail::Text(log_text), true, 600.0, 2.0).expect("drawn");
        assert!(log.h <= MAX_NOTE_H, "log {} pt", log.h);
        assert!(log.cut, "a cut log ends with a marker");
        let short = note(&Detail::Text("Saved.".into()), false, 600.0, 2.0).expect("drawn");
        assert!(!short.cut);
        let rows: Vec<Vec<String>> = (0..200).map(|k| vec![format!("crate-{k}"), format!("{k}")]).collect();
        let table = note(&Detail::Table { headers: vec!["A".into(), "B".into()], rows }, false, 600.0, 2.0).expect("drawn");
        assert!(table.h <= MAX_NOTE_H && table.cut, "table {} pt", table.h);
    }
}
#[cfg(test)]
mod scale_tests {
    use std::rc::Rc;

    use crate::agent_notice::Detail;

    /// Bug note 2026-10-24T14-01-17: a note drawn for a 1x display and then shown on the Retina display is drawn again
    /// at 2x (it was kept as it was and stretched), with the same size in points.
    #[test]
    fn a_note_follows_the_display_scale() {
        let rows: Vec<Vec<String>> = (0..7).map(|k| vec![format!("Aspect {k}"), "Swift".into(), "Rust".into()]).collect();
        let d = Detail::Table { headers: vec!["Aspect".into(), "Swift".into(), "Rust".into()], rows };
        let one = Rc::new(super::note(&d, false, 612.0, 1.0).expect("drawn"));
        let px = |n: &super::NoteDoc| (objc2_core_graphics::CGImage::width(Some(&n.image)), objc2_core_graphics::CGImage::height(Some(&n.image)));
        let two = one.at_scale(2.0);
        assert_eq!((two.w, two.h), (one.w, one.h), "the same size in points");
        assert_eq!(px(&two), (px(&one).0 * 2, px(&one).1 * 2), "twice the pixels");
        assert_eq!(two.scale, 2.0);
        assert!(Rc::ptr_eq(&two.at_scale(2.0), &two), "already at the scale: kept");
    }
}

/// Vella's mark: its app icon as Vella's own `scripts/icon.swift` renders it (assets/island/SOURCES.md), embedded so
/// the installed app has no source-path dependency. Loaded once per thread.
/// A composer attachment in `r`: the image at `path` cropped to fill the rounded tile with a hairline rim, or `sym` on
/// a dark tile. Only a local image of sensible size is read (an iCloud-only file would download on the main thread),
/// as the drop's own thumbnail does.
fn draw_thumb(path: Option<&std::path::Path>, image: bool, sym: &str, radius: f64, r: NSRect) {
    let readable = path.is_some_and(|p| {
        use std::os::macos::fs::MetadataExt;
        std::fs::metadata(p).is_ok_and(|m| m.len() <= 64 * 1024 * 1024 && m.st_flags() & 0x4000_0000 == 0)
    });
    let picture = match path {
        Some(p) if image && readable => NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&p.to_string_lossy())).filter(|i| i.size().width > 0.0 && i.size().height > 0.0),
        _ => None,
    };
    let tile = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
    match picture {
        Some(img) => {
            // Aspect fill: the part of the image the tile's shape shows, centred.
            let s = img.size();
            let k = (r.size.width / s.width).max(r.size.height / s.height);
            let (fw, fh) = (r.size.width / k, r.size.height / k);
            let from = rect((s.width - fw) / 2.0, (s.height - fh) / 2.0, fw, fh);
            NSGraphicsContext::saveGraphicsState_class();
            tile.addClip();
            unsafe { img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(r, from, NSCompositingOperation::SourceOver, 1.0, true, None) };
            NSGraphicsContext::restoreGraphicsState_class();
            let rim = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(r.origin.x + 0.25, r.origin.y + 0.25, r.size.width - 0.5, r.size.height - 0.5), radius, radius);
            rim.setLineWidth(0.5);
            NSColor::colorWithWhite_alpha(1.0, 0.22).setStroke();
            rim.stroke();
        }
        None => {
            NSColor::colorWithSRGBRed_green_blue_alpha(0.17, 0.17, 0.18, 1.0).setFill();
            tile.fill();
            let (cx, cy) = (r.origin.x + r.size.width / 2.0, r.origin.y + r.size.height / 2.0);
            draw_symbol(sym, 20.0, true, &[Rgba::white(0.85)], cx, cy, 1.0);
        }
    }
}

fn vella_mark() -> Option<objc2::rc::Retained<NSImage>> {
    thread_local! {
        static MARK: Option<objc2::rc::Retained<NSImage>> = {
            let data = objc2_foundation::NSData::with_bytes(include_bytes!("../../../../assets/island/vella.png"));
            NSImage::initWithData(NSImage::alloc(), &data)
        };
    }
    MARK.with(Clone::clone)
}
