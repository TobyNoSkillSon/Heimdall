//! The one CoreGraphics routine that draws the user's strokes, and the image helpers built on it. The overlay's stroke
//! layers take `path` and `styles`; the screenshot composite calls `draw` with the same path, so what the user sees is
//! what the agent receives. Thread-safe: every call owns its bitmap context.

use std::io::Cursor;

use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow, CGBitmapContextGetData,
    CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality, CGLineCap,
    CGLineJoin, CGMutablePath,
};

use crate::drawing::{Marks, Pt};

/// Stroke width in points (8 px on a Retina display).
pub(crate) const WIDTH: f64 = 4.0;
/// sRGB ink: system red, the colour people already read as "marked up". The agent's own marks are Heimdall's blue.
pub(crate) const INK: (f64, f64, f64) = (1.0, 59.0 / 255.0, 48.0 / 255.0);
/// A dark rim under the ink keeps it legible on white, red and busy pages alike.
const RIM_ALPHA: f64 = 0.22;
const RIM_EXTRA: f64 = 1.5;

/// One stroke as a smooth path: quadratic curves through the midpoints of successive points. A single point
/// becomes a zero-length line, which the round cap draws as a dot.
pub(crate) fn path(stroke: &[Pt]) -> CFRetained<CGMutablePath> {
    let p = CGMutablePath::new();
    let m = Some(&*p);
    let t = std::ptr::null();
    let Some(first) = stroke.first() else { return p };
    unsafe {
        CGMutablePath::move_to_point(m, t, first.x, first.y);
        if stroke.len() == 1 {
            CGMutablePath::add_line_to_point(m, t, first.x, first.y);
            return p;
        }
        for pair in stroke[1..].windows(2) {
            let (a, b) = (pair[0], pair[1]);
            CGMutablePath::add_quad_curve_to_point(m, t, a.x, a.y, (a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        }
        let last = stroke[stroke.len() - 1];
        CGMutablePath::add_line_to_point(m, t, last.x, last.y);
    }
    p
}

/// The rim's and the ink's line widths and colours (sRGB, alpha): rim first.
pub(crate) fn styles() -> [(f64, (f64, f64, f64, f64)); 2] {
    [(WIDTH + RIM_EXTRA, (0.0, 0.0, 0.0, RIM_ALPHA)), (WIDTH, (INK.0, INK.1, INK.2, 1.0))]
}

/// Draws every stroke into `c`, whose user space is the overlay view's (points, y up). All rims go down first so a
/// later stroke's rim never darkens an earlier stroke's ink (the overlay stacks its layers the same way).
pub(crate) fn draw(c: &CGContext, strokes: &[Vec<Pt>]) {
    let cx = Some(c);
    CGContext::save_g_state(cx);
    CGContext::set_line_cap(cx, CGLineCap::Round);
    CGContext::set_line_join(cx, CGLineJoin::Round);
    for (width, (r, g, b, a)) in styles() {
        CGContext::set_rgb_stroke_color(cx, r, g, b, a);
        CGContext::set_line_width(cx, width);
        for stroke in strokes {
            CGContext::add_path(cx, Some(&path(stroke)));
            CGContext::stroke_path(cx);
        }
    }
    CGContext::restore_g_state(cx);
}

fn rgba_context(w: usize, h: usize) -> Result<CFRetained<CGContext>, String> {
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or("no sRGB colour space")?;
    let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
    unsafe { CGBitmapContextCreate(std::ptr::null_mut(), w, h, 8, 0, Some(&space), info) }.ok_or_else(|| "no bitmap context".into())
}

/// Packed RGB8 as an opaque sRGB image.
fn rgb_image(rgb: &[u8], w: u32, h: u32) -> Result<CFRetained<CGImage>, String> {
    let (wu, hu) = (w as usize, h as usize);
    if wu == 0 || hu == 0 || rgb.len() != wu * hu * 3 {
        return Err(format!("bad image: {w}x{h} with {} bytes", rgb.len()));
    }
    let ctx = rgba_context(wu, hu)?;
    let row = CGBitmapContextGetBytesPerRow(Some(&ctx));
    let data = CGBitmapContextGetData(Some(&ctx)) as *mut u8;
    if data.is_null() {
        return Err("bitmap has no data".into());
    }
    for y in 0..hu {
        let line = unsafe { std::slice::from_raw_parts_mut(data.add(y * row), wu * 4) };
        for (px, src) in line.chunks_exact_mut(4).zip(rgb[y * wu * 3..(y + 1) * wu * 3].chunks_exact(3)) {
            px[..3].copy_from_slice(src);
            px[3] = 255;
        }
    }
    CGBitmapContextCreateImage(Some(&ctx)).ok_or_else(|| "no image".into())
}

/// The context's pixels as packed RGB8, top row first (the screenshot is opaque, so dropping alpha loses nothing).
fn context_rgb(ctx: &CGContext, w: usize, h: usize) -> Result<Vec<u8>, String> {
    let row = CGBitmapContextGetBytesPerRow(Some(ctx));
    let data = CGBitmapContextGetData(Some(ctx)) as *const u8;
    if data.is_null() {
        return Err("bitmap has no data".into());
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let line = unsafe { std::slice::from_raw_parts(data.add(y * row), w * 4) };
        for px in line.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
    }
    Ok(rgb)
}

/// `image` with the strokes drawn on top, as packed RGB8 at the image's size. `view` is the overlay's size in
/// points; the strokes scale from it to the image's pixels (a native capture, or a downscaled recorded frame).
pub(crate) fn composite_image(image: &CGImage, view: (f64, f64), strokes: &[Vec<Pt>]) -> Result<Vec<u8>, String> {
    let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    if w == 0 || h == 0 || view.0 <= 0.0 || view.1 <= 0.0 {
        return Err("empty image or view".into());
    }
    let ctx = rgba_context(w, h)?;
    let c = Some(&*ctx);
    CGContext::set_interpolation_quality(c, CGInterpolationQuality::None);
    CGContext::draw_image(c, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)), Some(image));
    CGContext::scale_ctm(c, w as f64 / view.0, h as f64 / view.1);
    draw(&ctx, strokes);
    context_rgb(&ctx, w, h)
}

/// Packed RGB8 with the strokes drawn on top.
pub(crate) fn composite_rgb(rgb: &[u8], w: u32, h: u32, view: (f64, f64), strokes: &[Vec<Pt>]) -> Result<Vec<u8>, String> {
    let image = rgb_image(rgb, w, h)?;
    composite_image(&image, view, strokes)
}

/// Packed RGB8 scaled to `to` pixels (high-quality interpolation).
pub(crate) fn resize_rgb(rgb: &[u8], w: u32, h: u32, to: (u32, u32)) -> Result<Vec<u8>, String> {
    let image = rgb_image(rgb, w, h)?;
    let (tw, th) = (to.0 as usize, to.1 as usize);
    if tw == 0 || th == 0 {
        return Err("empty size".into());
    }
    let ctx = rgba_context(tw, th)?;
    let c = Some(&*ctx);
    CGContext::set_interpolation_quality(c, CGInterpolationQuality::High);
    CGContext::draw_image(c, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(tw as f64, th as f64)), Some(&image));
    context_rgb(&ctx, tw, th)
}

/// Packed RGB8 as an sRGB PNG (fast compression: these images are sent at once).
pub(crate) fn encode_png(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut out), w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.set_compression(png::Compression::Fast);
        encoder.write_header().and_then(|mut w| w.write_image_data(rgb)).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// An 8-bit RGB or RGBA PNG as packed RGB8.
pub(crate) fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = &buf[..info.buffer_size()];
    let rgb = match info.color_type {
        png::ColorType::Rgb => data.to_vec(),
        png::ColorType::Rgba => data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&v| [v, v, v]).collect(),
        png::ColorType::GrayscaleAlpha => data.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0]]).collect(),
        other => return Err(format!("unsupported PNG colour type {other:?}")),
    };
    Ok((info.width, info.height, rgb))
}

/// A PNG screenshot of `marks.display` with the user's strokes composited, re-encoded.
pub(crate) fn mark_png(bytes: &[u8], marks: &Marks) -> Result<Vec<u8>, String> {
    let (w, h, rgb) = decode_png(bytes)?;
    let marked = composite_rgb(&rgb, w, h, marks.view, &marks.strokes)?;
    encode_png(w, h, &marked)
}

/// A CGImage from PNG bytes (renders and checks).
#[allow(dead_code)]
pub(crate) fn png_image(bytes: &[u8]) -> Option<CFRetained<CGImage>> {
    let (w, h, rgb) = decode_png(bytes).ok()?;
    rgb_image(&rgb, w, h).ok()
}

/// Reads one pixel (RGB) of a packed buffer; y counts from the top row, as in the PNG.
#[allow(dead_code)]
pub(crate) fn pixel(rgb: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = (y as usize * w as usize + x as usize) * 3;
    [rgb[i], rgb[i + 1], rgb[i + 2]]
}
