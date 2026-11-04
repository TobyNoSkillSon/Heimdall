//! Bitmaps: CoreGraphics bitmap contexts, ImageIO reads (sizes, thumbnails) and PNG writes, NSImage drawing. Raw
//! CoreGraphics/ImageIO calls with an owning `Cf` handle; typed objc2 references are made from them where an
//! Objective-C method takes one (`renderInContext:`, layer contents).

use std::ffi::c_void;
use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{NSColor, NSImage};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGContext, CGImage};
use objc2_foundation::{NSData, NSDictionary, NSNumber, NSString, NSURL};

use super::look::{any, ns};

pub type Ref = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorSpaceSRGB: Ref;
    fn CGColorSpaceCreateWithName(name: Ref) -> Ref;
    fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bpc: usize, bpr: usize, space: Ref, info: u32) -> Ref;
    fn CGBitmapContextCreateImage(c: Ref) -> Ref;
    fn CGContextScaleCTM(c: Ref, sx: f64, sy: f64);
    fn CGContextTranslateCTM(c: Ref, tx: f64, ty: f64);
    fn CGContextSaveGState(c: Ref);
    fn CGContextRestoreGState(c: Ref);
    fn CGContextSetFillColorWithColor(c: Ref, color: Ref);
    fn CGContextSetStrokeColorWithColor(c: Ref, color: Ref);
    fn CGContextSetLineWidth(c: Ref, w: f64);
    fn CGContextFillRect(c: Ref, r: CGRect);
    fn CGContextSetBlendMode(c: Ref, mode: i32);
    fn CGContextDrawImage(c: Ref, r: CGRect, img: Ref);
    fn CGContextClipToRect(c: Ref, r: CGRect);
    fn CGContextAddPath(c: Ref, p: Ref);
    fn CGContextFillPath(c: Ref);
    fn CGContextStrokePath(c: Ref);
    fn CGContextClip(c: Ref);
    fn CGPathCreateWithRoundedRect(r: CGRect, cw: f64, ch: f64, t: *const c_void) -> Ref;
    fn CGImageGetWidth(img: Ref) -> usize;
    fn CGImageGetHeight(img: Ref) -> usize;
    fn CGImageCreateWithImageInRect(img: Ref, r: CGRect) -> Ref;
    fn CGGradientCreateWithColors(space: Ref, colors: Ref, locations: *const f64) -> Ref;
    fn CGContextDrawLinearGradient(c: Ref, g: Ref, start: CGPoint, end: CGPoint, options: u32);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(r: Ref);
    fn CFRetain(r: Ref) -> Ref;
}

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageSourceCreateWithURL(url: Ref, options: Ref) -> Ref;
    fn CGImageSourceCopyPropertiesAtIndex(src: Ref, index: usize, options: Ref) -> Ref;
    fn CGImageSourceCreateThumbnailAtIndex(src: Ref, index: usize, options: Ref) -> Ref;
    fn CGImageDestinationCreateWithURL(url: Ref, kind: Ref, count: usize, options: Ref) -> Ref;
    fn CGImageDestinationAddImage(dst: Ref, img: Ref, props: Ref);
    fn CGImageDestinationFinalize(dst: Ref) -> bool;
}

/// An owned CoreFoundation object (released on drop). Never NULL: the only way to make one is `Cf::new`, which takes
/// a +1 reference from a Create or Copy call (or an explicit retain) and turns NULL into `None`.
pub struct Cf(Ref);

impl Cf {
    pub fn new(r: Ref) -> Option<Cf> {
        // Not `then_some(Cf(r))`: its argument is built eagerly and dropped when the test is false, so a NULL result
        // (an unreadable image, say) would reach `CFRelease(NULL)` and trap. That was the 24 Oct thumbnail crash.
        if r.is_null() {
            None
        } else {
            Some(Cf(r))
        }
    }
    pub fn raw(&self) -> Ref {
        self.0
    }
}

impl Clone for Cf {
    fn clone(&self) -> Cf {
        // SAFETY: retaining a live CF object.
        Cf(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { CFRelease(self.0) }
    }
}

/// A CGImage (owned).
#[derive(Clone)]
pub struct Image(pub Cf);

impl Image {
    pub fn width(&self) -> usize {
        // SAFETY: a live CGImage.
        unsafe { CGImageGetWidth(self.0.raw()) }
    }
    pub fn height(&self) -> usize {
        // SAFETY: a live CGImage.
        unsafe { CGImageGetHeight(self.0.raw()) }
    }
    /// As a typed CGImage reference (layer contents, Objective-C calls).
    pub fn cg(&self) -> &CGImage {
        // SAFETY: the pointer is a live CGImage for as long as `self`.
        unsafe { &*(self.0.raw() as *const CGImage) }
    }
    pub fn object(&self) -> &AnyObject {
        // SAFETY: CF objects are Objective-C objects.
        unsafe { &*(self.0.raw() as *const AnyObject) }
    }
    /// A crop in pixel space (y down).
    pub fn crop(&self, r: CGRect) -> Option<Image> {
        // SAFETY: a live image; the result is owned.
        Cf::new(unsafe { CGImageCreateWithImageInRect(self.0.raw(), r) }).map(Image)
    }
}

/// An RGBA bitmap context in sRGB, `w` × `h` pixels.
pub struct Bitmap {
    pub ctx: Cf,
    pub w: usize,
    pub h: usize,
}

impl Bitmap {
    pub fn new(w: usize, h: usize) -> Option<Bitmap> {
        if w == 0 || h == 0 {
            return None;
        }
        // SAFETY: plain CoreGraphics creation; both results are owned.
        unsafe {
            let space = Cf::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB))?;
            let ctx = Cf::new(CGBitmapContextCreate(std::ptr::null_mut(), w, h, 8, 0, space.raw(), 1 /* premultiplied last */))?;
            Some(Bitmap { ctx, w, h })
        }
    }
    pub fn raw(&self) -> Ref {
        self.ctx.raw()
    }
    /// As a typed CGContext reference (`renderInContext:`, NSGraphicsContext).
    pub fn cg(&self) -> &CGContext {
        // SAFETY: the pointer is a live CGContext for as long as `self`.
        unsafe { &*(self.ctx.raw() as *const CGContext) }
    }
    pub fn image(&self) -> Option<Image> {
        // SAFETY: a live context; the image is owned.
        Cf::new(unsafe { CGBitmapContextCreateImage(self.raw()) }).map(Image)
    }
    pub fn scale(&self, sx: f64, sy: f64) {
        unsafe { CGContextScaleCTM(self.raw(), sx, sy) }
    }
    pub fn translate(&self, tx: f64, ty: f64) {
        unsafe { CGContextTranslateCTM(self.raw(), tx, ty) }
    }
    pub fn save(&self) {
        unsafe { CGContextSaveGState(self.raw()) }
    }
    pub fn restore(&self) {
        unsafe { CGContextRestoreGState(self.raw()) }
    }
    pub fn fill(&self, r: CGRect, c: &CGColor) {
        unsafe {
            CGContextSetFillColorWithColor(self.raw(), c as *const CGColor as Ref);
            CGContextFillRect(self.raw(), r);
        }
    }
    pub fn set_fill(&self, c: &CGColor) {
        unsafe { CGContextSetFillColorWithColor(self.raw(), c as *const CGColor as Ref) }
    }
    /// Source-atop: paint only where something is already drawn (silhouettes).
    pub fn blend_source_atop(&self) {
        unsafe { CGContextSetBlendMode(self.raw(), 20) }
    }
    pub fn draw(&self, img: &Image, r: CGRect) {
        unsafe { CGContextDrawImage(self.raw(), r, img.0.raw()) }
    }
    pub fn clip(&self, r: CGRect) {
        unsafe { CGContextClipToRect(self.raw(), r) }
    }
    pub fn rounded_path(&self, r: CGRect, radius: f64) {
        unsafe {
            if let Some(p) = Cf::new(CGPathCreateWithRoundedRect(r, radius, radius, std::ptr::null())) {
                CGContextAddPath(self.raw(), p.raw());
            }
        }
    }
    pub fn fill_path(&self, c: &CGColor) {
        unsafe {
            CGContextSetFillColorWithColor(self.raw(), c as *const CGColor as Ref);
            CGContextFillPath(self.raw());
        }
    }
    pub fn stroke_path(&self, c: &CGColor, width: f64) {
        unsafe {
            CGContextSetStrokeColorWithColor(self.raw(), c as *const CGColor as Ref);
            CGContextSetLineWidth(self.raw(), width);
            CGContextStrokePath(self.raw());
        }
    }
    pub fn clip_path(&self) {
        unsafe { CGContextClip(self.raw()) }
    }
    /// A vertical linear gradient from `top` (at y = h) to `bottom` (at y = 0), in user space.
    pub fn vertical_gradient(&self, top: &CGColor, bottom: &CGColor, h: f64) {
        let cols: Retained<objc2_foundation::NSArray<AnyObject>> = objc2_foundation::NSArray::from_slice(&[
            // SAFETY: CGColors are Objective-C objects.
            unsafe { &*(top as *const CGColor as *const AnyObject) },
            unsafe { &*(bottom as *const CGColor as *const AnyObject) },
        ]);
        let locs = [0.0f64, 1.0];
        unsafe {
            let Some(space) = Cf::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB)) else { return };
            if let Some(g) = Cf::new(CGGradientCreateWithColors(space.raw(), &*cols as *const _ as Ref, locs.as_ptr())) {
                CGContextDrawLinearGradient(self.raw(), g.raw(), CGPoint::new(0.0, h), CGPoint::new(0.0, 0.0), 0);
            }
        }
    }

    /// Runs `f` with this context as the current NSGraphicsContext (AppKit drawing: NSImage, NSString).
    pub fn with_appkit(&self, flipped: bool, f: impl FnOnce()) {
        let g: Option<Retained<AnyObject>> =
            unsafe { msg_send![class!(NSGraphicsContext), graphicsContextWithCGContext: self.cg(), flipped: flipped] };
        let Some(g) = g else { return };
        unsafe {
            let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
            let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: &*g];
        }
        f();
        unsafe {
            let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
        }
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&ns(&path.to_string_lossy()))
}

fn cfdict(pairs: &[(&str, Retained<AnyObject>)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let ks: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| ns(k)).collect();
    let kr: Vec<&NSString> = ks.iter().map(|k| &**k).collect();
    let vs: Vec<Retained<AnyObject>> = pairs.iter().map(|(_, v)| v.clone()).collect();
    NSDictionary::from_retained_objects(&kr, &vs)
}

fn yes() -> Retained<AnyObject> {
    let n = NSNumber::new_bool(true);
    any(&*n).retain()
}

fn number(v: f64) -> Retained<AnyObject> {
    let n = NSNumber::new_f64(v);
    any(&*n).retain()
}

use objc2::Message;

/// Pixel size of an image file, orientation applied (header read only).
pub fn image_pixel_size(path: &Path) -> Option<CGSize> {
    let url = file_url(path);
    // SAFETY: CF calls with toll-free bridged NSURL; results owned.
    unsafe {
        let src = Cf::new(CGImageSourceCreateWithURL(&*url as *const NSURL as Ref, std::ptr::null_mut()))?;
        let props = Cf::new(CGImageSourceCopyPropertiesAtIndex(src.raw(), 0, std::ptr::null_mut()))?;
        let d: &NSDictionary<NSString, AnyObject> = &*(props.raw() as *const NSDictionary<NSString, AnyObject>);
        let get = |k: &str| -> Option<f64> {
            let v = d.objectForKey(&ns(k))?;
            let n: f64 = msg_send![&*v, doubleValue];
            Some(n)
        };
        let (w, h) = (get("PixelWidth")?, get("PixelHeight")?);
        let o = get("Orientation").unwrap_or(1.0) as i64;
        Some(if o >= 5 { CGSize::new(h, w) } else { CGSize::new(w, h) })
    }
}

/// A thumbnail no larger than `max_pixel` on its long side, orientation applied.
pub fn thumbnail(path: &Path, max_pixel: usize, cache_now: bool) -> Option<Image> {
    let url = file_url(path);
    let mut pairs = vec![
        ("kCGImageSourceCreateThumbnailFromImageAlways", yes()),
        ("kCGImageSourceThumbnailMaxPixelSize", number(max_pixel as f64)),
        ("kCGImageSourceCreateThumbnailWithTransform", yes()),
    ];
    if cache_now {
        pairs.push(("kCGImageSourceShouldCacheImmediately", yes()));
    }
    let opts = cfdict(&pairs);
    // SAFETY: as above.
    unsafe {
        let src = Cf::new(CGImageSourceCreateWithURL(&*url as *const NSURL as Ref, std::ptr::null_mut()))?;
        Cf::new(CGImageSourceCreateThumbnailAtIndex(src.raw(), 0, &*opts as *const _ as Ref)).map(Image)
    }
}

/// Write a PNG marked 144 dpi (a 2× render).
pub fn write_png(img: &Image, path: &Path) -> bool {
    let url = file_url(path);
    let kind = ns("public.png");
    let props = cfdict(&[("DPIWidth", number(144.0)), ("DPIHeight", number(144.0))]);
    // SAFETY: as above.
    unsafe {
        let Some(d) = Cf::new(CGImageDestinationCreateWithURL(&*url as *const NSURL as Ref, &*kind as *const NSString as Ref, 1, std::ptr::null_mut()))
        else {
            return false;
        };
        CGImageDestinationAddImage(d.raw(), img.0.raw(), &*props as *const _ as Ref);
        CGImageDestinationFinalize(d.raw())
    }
}

/// A tiny inline SVG drawn as a silhouette in `color`, aspect fit a hair inside a `size`-point box.
pub fn svg_silhouette(svg: &str, size: f64, color: &NSColor, scale: f64) -> Option<Image> {
    let data = NSData::with_bytes(svg.as_bytes());
    let img = NSImage::initWithData(NSImage::alloc(), &data)?;
    let s = img.size();
    if s.width <= 0.0 || s.height <= 0.0 {
        return None;
    }
    let px = (size * scale).ceil() as usize;
    let b = Bitmap::new(px, px)?;
    let full = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(px as f64, px as f64));
    let inset = px as f64 * 0.05;
    let boxw = px as f64 - 2.0 * inset;
    let k = (boxw / s.width).min(boxw / s.height);
    let fit = CGRect::new(
        CGPoint::new(px as f64 / 2.0 - s.width * k / 2.0, px as f64 / 2.0 - s.height * k / 2.0),
        CGSize::new(s.width * k, s.height * k),
    );
    b.with_appkit(false, || {
        let _: () = unsafe { msg_send![&*img, drawInRect: fit] };
    });
    b.blend_source_atop();
    b.fill(full, &color.CGColor());
    b.image()
}

use objc2::AllocAnyThread;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPathCreateMutable() -> Ref;
    fn CGPathMoveToPoint(p: Ref, t: *const c_void, x: f64, y: f64);
    fn CGPathAddLineToPoint(p: Ref, t: *const c_void, x: f64, y: f64);
    fn CGPathAddArc(p: Ref, t: *const c_void, x: f64, y: f64, r: f64, start: f64, end: f64, clockwise: bool);
    fn CGPathAddCurveToPoint(p: Ref, t: *const c_void, c1x: f64, c1y: f64, c2x: f64, c2y: f64, x: f64, y: f64);
}

/// A mutable CGPath (lines, arcs) for shape layers.
pub struct CgPath(Cf);

impl CgPath {
    pub fn new() -> CgPath {
        // SAFETY: an owned new path.
        CgPath(Cf::new(unsafe { CGPathCreateMutable() }).expect("CGPath"))
    }
    pub fn move_to(&self, x: f64, y: f64) {
        unsafe { CGPathMoveToPoint(self.0.raw(), std::ptr::null(), x, y) }
    }
    pub fn line_to(&self, x: f64, y: f64) {
        unsafe { CGPathAddLineToPoint(self.0.raw(), std::ptr::null(), x, y) }
    }
    pub fn arc(&self, x: f64, y: f64, r: f64, start: f64, end: f64, clockwise: bool) {
        unsafe { CGPathAddArc(self.0.raw(), std::ptr::null(), x, y, r, start, end, clockwise) }
    }
    /// A cubic Bézier to (x, y) through two control points.
    pub fn curve_to(&self, c1: (f64, f64), c2: (f64, f64), x: f64, y: f64) {
        unsafe { CGPathAddCurveToPoint(self.0.raw(), std::ptr::null(), c1.0, c1.1, c2.0, c2.1, x, y) }
    }
    pub fn cg(&self) -> &objc2_core_graphics::CGPath {
        // SAFETY: a live CGPath for as long as `self`.
        unsafe { &*(self.0.raw() as *const objc2_core_graphics::CGPath) }
    }
}

impl Default for CgPath {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NULL result is `None` and is never released (`CFRelease(NULL)` traps the process: the 24 Oct crash).
    #[test]
    fn cf_from_null_is_none_and_not_released() {
        assert!(Cf::new(std::ptr::null_mut()).is_none());
    }

    /// Files that are not readable images give no thumbnail and no crash: missing, empty, cut short, a folder, text.
    #[test]
    fn thumbnail_of_bad_files_is_none() {
        let dir = lab_dir("thumbs");
        let png = sample_png(&dir);
        let bytes = std::fs::read(&png).unwrap();
        std::fs::write(dir.join("empty.png"), b"").unwrap();
        std::fs::write(dir.join("cut.png"), &bytes[..bytes.len().min(40)]).unwrap();
        std::fs::write(dir.join("text.png"), b"not an image at all\n").unwrap();
        std::fs::write(dir.join("notes.txt"), b"plain words\n").unwrap();
        std::fs::create_dir_all(dir.join("folder.png")).unwrap();
        for name in ["missing.png", "empty.png", "cut.png", "text.png", "notes.txt", "folder.png"] {
            for cache in [true, false] {
                assert!(thumbnail(&dir.join(name), 400, cache).is_none(), "{name}");
            }
            assert!(image_pixel_size(&dir.join(name)).is_none(), "{name}");
        }
        // A real image (and the same one as HEIC, where this Mac can write it) still gives one, long side capped.
        let t = thumbnail(&png, 32, true).expect("png thumbnail");
        assert_eq!(t.width().max(t.height()), 32);
        let heic = dir.join("shot.heic");
        let made = std::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "heic"])
            .arg(&png)
            .arg("--out")
            .arg(&heic)
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            let t = thumbnail(&heic, 32, true).expect("heic thumbnail");
            assert_eq!(t.width().max(t.height()), 32);
            let bytes = std::fs::read(&heic).unwrap();
            std::fs::write(dir.join("cut.heic"), &bytes[..bytes.len() / 3]).unwrap();
            // Cut short, ImageIO may still decode part of it or nothing; either way, no crash.
            let _ = thumbnail(&dir.join("cut.heic"), 32, true);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn lab_dir(name: &str) -> std::path::PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/night/islands-lab").join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_png(dir: &Path) -> std::path::PathBuf {
        let b = Bitmap::new(64, 48).unwrap();
        b.fill(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(64.0, 48.0)), &super::super::look::cg(&super::super::look::rgb(0x3380e6, 1.0)));
        let p = dir.join("shot.png");
        assert!(write_png(&b.image().unwrap(), &p));
        p
    }
}
