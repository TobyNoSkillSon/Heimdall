//! The visual language (Swift `Look.swift`), taken from Heimdall's notch island: the notch's own black, radius
//! r = clamp(10 + (h − 32) × 0.4, 10, 24) with continuous corners, SF Pro, white text, colour only in icons; plus the
//! Core Animation helpers every part uses (springs, fades, text and symbol layers).

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{class, msg_send, AllocAnyThread, Message};
use objc2_app_kit::{
    NSColor, NSFont, NSFontWeight, NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold, NSImage,
    NSImageSymbolConfiguration, NSLineBreakMode, NSMutableParagraphStyle, NSTextAlignment,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSArray, NSAttributedString, NSDictionary, NSNumber, NSString, NSValue};
use objc2_quartz_core::{
    kCACornerCurveContinuous, kCAFillModeBackwards, kCAMediaTimingFunctionEaseIn, kCAMediaTimingFunctionEaseInEaseOut, kCAMediaTimingFunctionEaseOut,
    kCATruncationEnd, CAAnimation, CAAnimationGroup, CABasicAnimation, CAKeyframeAnimation, CALayer, CAMediaTiming,
    CAMediaTimingFunction, CASpringAnimation, CATextLayer, CATransaction,
};

use crate::model::geometry::Rect;

pub const PILL_H: f64 = 36.0;
pub const PILL_PAD: f64 = 15.0;
/// Expanded side padding.
pub const PAD: f64 = 16.0;
/// Icon to title.
pub const HEADER_GAP: f64 = 8.0;
pub const ICON_SIZE: f64 = 19.0;
pub const BODY_TOP: f64 = 48.0;
pub const BOTTOM_PAD: f64 = 16.0;
pub const PART_GAP: f64 = 12.0;
pub const EXPANDED_WIDTH: f64 = 360.0;
/// A title truncates only when it is truly out of room: the open island's header beside its icon and close button.
pub const MAX_PILL_TITLE: f64 = EXPANDED_WIDTH - 2.0 * PAD - 30.0 - ICON_SIZE - HEADER_GAP;
pub const HAIRLINE_WIDTH: f64 = 1.25;

pub fn radius(h: f64) -> f64 {
    (10.0 + (h - 32.0) * 0.4).clamp(10.0, 24.0)
}

// MARK: Colours

pub fn rgb(hex: u32, a: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        ((hex >> 16) & 255) as f64 / 255.0,
        ((hex >> 8) & 255) as f64 / 255.0,
        (hex & 255) as f64 / 255.0,
        a,
    )
}
pub fn green() -> Retained<NSColor> {
    rgb(0x30D158, 1.0)
}
pub fn red() -> Retained<NSColor> {
    rgb(0xFF453A, 1.0)
}
pub fn orange() -> Retained<NSColor> {
    rgb(0xFF9F0A, 1.0)
}
pub fn blue() -> Retained<NSColor> {
    rgb(0x0A84FF, 1.0)
}
pub fn white(a: f64) -> Retained<NSColor> {
    NSColor::colorWithWhite_alpha(1.0, a)
}
pub fn black() -> Retained<NSColor> {
    NSColor::colorWithWhite_alpha(0.0, 1.0)
}
pub fn gray(w: f64, a: f64) -> Retained<NSColor> {
    NSColor::colorWithWhite_alpha(w, a)
}
pub fn clear() -> Retained<NSColor> {
    NSColor::colorWithWhite_alpha(0.0, 0.0)
}
/// Text colours: primary, secondary (labels, times, values beside a label), done steps.
pub fn ink() -> Retained<NSColor> {
    white(0.96)
}
pub fn ink2() -> Retained<NSColor> {
    white(0.74)
}
/// A plan row's small project symbol: present but behind the title.
pub fn ink3() -> Retained<NSColor> {
    white(0.62)
}
pub fn ink_done() -> Retained<NSColor> {
    white(0.7)
}
/// The hairline that keeps a black island visible on black and dark backgrounds.
pub fn hairline() -> Retained<CGColor> {
    white(0.34).CGColor()
}
pub fn cg(c: &NSColor) -> Retained<CGColor> {
    c.CGColor()
}

// MARK: Fonts

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

impl Weight {
    pub fn ns(self) -> NSFontWeight {
        // SAFETY: framework constants.
        unsafe {
            match self {
                Weight::Regular => NSFontWeightRegular,
                Weight::Medium => NSFontWeightMedium,
                Weight::Semibold => NSFontWeightSemibold,
                Weight::Bold => NSFontWeightBold,
            }
        }
    }
}

pub fn system(size: f64, w: Weight) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, w.ns())
}
pub fn digits(size: f64, w: Weight) -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(size, w.ns())
}
pub fn mono(size: f64, w: Weight) -> Retained<NSFont> {
    NSFont::monospacedSystemFontOfSize_weight(size, w.ns())
}

pub fn title_font() -> Retained<NSFont> {
    system(17.0, Weight::Semibold)
}
pub fn trailing_font() -> Retained<NSFont> {
    digits(17.0, Weight::Semibold)
}
pub fn body_font() -> Retained<NSFont> {
    system(15.0, Weight::Regular)
}
pub fn body_strong() -> Retained<NSFont> {
    system(15.0, Weight::Semibold)
}
pub fn status_font() -> Retained<NSFont> {
    system(15.0, Weight::Semibold)
}
pub fn small_font() -> Retained<NSFont> {
    system(13.0, Weight::Medium)
}
/// A plan item's explanation, under its title once the row is open: smaller and quieter than the title.
pub fn note_font() -> Retained<NSFont> {
    system(13.0, Weight::Regular)
}
pub fn group_header_font() -> Retained<NSFont> {
    system(12.0, Weight::Bold)
}
pub fn small_mono() -> Retained<NSFont> {
    digits(13.0, Weight::Medium)
}
pub fn button_font() -> Retained<NSFont> {
    system(15.0, Weight::Semibold)
}
pub fn stat_font() -> Retained<NSFont> {
    digits(28.0, Weight::Semibold)
}

// MARK: Springs

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub response: f64,
    pub damping: f64,
}

impl Spring {
    /// Dynamic Island pace: quick, with one small living overshoot on growth.
    pub const GROW: Spring = Spring { response: 0.3, damping: 0.74 };
    pub const CALM: Spring = Spring { response: 0.24, damping: 1.0 };
    /// Settling into a snap point: one small overshoot.
    pub const SNAP: Spring = Spring { response: 0.3, damping: 0.7 };
    /// A viewer growing out of the clicked picture or row (Quick Look zoom): critically damped
    /// enough that nothing bounces at the end, about 0.4 s.
    pub const ZOOM: Spring = Spring { response: 0.32, damping: 0.96 };
    /// A thrown island landing in its dock (iPhone picture in picture).
    pub const TOSS: Spring = Spring { response: 0.4, damping: 0.78 };

    /// `velocity`: the starting speed as a fraction of the distance per second (positive toward the target).
    pub fn animation(&self, key: &str, from: &AnyObject, to: &AnyObject, velocity: f64) -> Retained<CASpringAnimation> {
        let a = CASpringAnimation::animationWithKeyPath(Some(&ns(key)));
        let w0 = 2.0 * std::f64::consts::PI / self.response;
        a.setMass(1.0);
        a.setStiffness(w0 * w0);
        a.setDamping(2.0 * self.damping * w0);
        a.setInitialVelocity(velocity);
        // SAFETY: values are NSNumber/NSValue matching the key path.
        unsafe {
            a.setFromValue(Some(from));
            a.setToValue(Some(to));
        }
        a.setDuration(a.settlingDuration());
        a
    }
}

// MARK: Values

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Any Objective-C object as `AnyObject`.
pub fn any<T: Message>(x: &T) -> &AnyObject {
    // SAFETY: every Objective-C object is an AnyObject.
    unsafe { &*(x as *const T as *const AnyObject) }
}

pub fn num(v: f64) -> Retained<NSNumber> {
    NSNumber::new_f64(v)
}
pub fn pt(x: f64, y: f64) -> CGPoint {
    CGPoint::new(x, y)
}
pub fn cgr(r: Rect) -> CGRect {
    CGRect::new(CGPoint::new(r.x, r.y), CGSize::new(r.w, r.h))
}
pub fn rect_of(r: CGRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}
pub fn point_val(p: CGPoint) -> Retained<NSValue> {
    // SAFETY: plain value boxing.
    unsafe { NSValue::valueWithPoint(p) }
}
pub fn rect_val(r: CGRect) -> Retained<NSValue> {
    // SAFETY: plain value boxing.
    unsafe { NSValue::valueWithRect(r) }
}

// MARK: Layers

/// The actions every island layer turns off, so only explicit animations move anything.
pub fn no_implicit(l: &CALayer) {
    let keys = [
        "position", "bounds", "opacity", "contents", "transform", "backgroundColor", "path", "strokeEnd", "sublayers", "cornerRadius",
        "frame", "hidden", "mask", "borderColor", "string", "foregroundColor", "zPosition", "anchorPoint", "colors", "locations",
        "startPoint", "endPoint", "lineWidth",
    ];
    let null: Retained<AnyObject> = unsafe { msg_send![class!(NSNull), null] };
    let ks: Vec<Retained<NSString>> = keys.iter().map(|k| ns(k)).collect();
    let kr: Vec<&NSString> = ks.iter().map(|k| &**k).collect();
    let vs: Vec<Retained<AnyObject>> = keys.iter().map(|_| null.clone()).collect();
    let d = NSDictionary::<NSString, AnyObject>::from_retained_objects(&kr, &vs);
    // SAFETY: NSNull is a valid CAAction (it means "no action").
    let _: () = unsafe { msg_send![l, setActions: &*d] };
}

pub fn layer() -> Retained<CALayer> {
    let l = CALayer::new();
    no_implicit(&l);
    l
}

pub fn continuous(l: &CALayer) {
    // SAFETY: a framework constant.
    l.setCornerCurve(unsafe { kCACornerCurveContinuous });
}

/// A capsule's corners: circular, so an outline meets its straight edges cleanly (a continuous curve at radius h/2
/// leaves square stubs on a bordered pill).
pub fn circular(l: &CALayer) {
    // SAFETY: a framework constant.
    l.setCornerCurve(unsafe { objc2_quartz_core::kCACornerCurveCircular });
}

/// The layer as it is on screen now (its presentation), else the model.
pub fn shown(l: &CALayer) -> Retained<CALayer> {
    // SAFETY: presentationLayer is safe to read on the main thread.
    unsafe { l.presentationLayer() }.unwrap_or_else(|| l.retain())
}

pub fn set_bg(l: &CALayer, c: &NSColor) {
    l.setBackgroundColor(Some(&c.CGColor()));
}

/// A plain box: frame, fill, radius, continuous corners, top-left anchor.
pub fn boxed(x: f64, y: f64, w: f64, h: f64, color: &NSColor, radius: f64) -> Retained<CALayer> {
    let l = layer();
    l.setAnchorPoint(pt(0.0, 0.0));
    l.setFrame(cgr(Rect::new(x, y, w, h)));
    set_bg(&l, color);
    l.setCornerRadius(radius);
    continuous(&l);
    l
}

pub fn ease_out() -> Retained<CAMediaTimingFunction> {
    // SAFETY: a framework constant.
    CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseOut })
}
pub fn ease_in() -> Retained<CAMediaTimingFunction> {
    // SAFETY: a framework constant.
    CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseIn })
}
pub fn ease_in_out() -> Retained<CAMediaTimingFunction> {
    // SAFETY: a framework constant.
    CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseInEaseOut })
}

pub fn fill_backwards(a: &CAAnimation) {
    // SAFETY: a framework constant.
    a.setFillMode(unsafe { kCAFillModeBackwards });
}

pub fn add(l: &CALayer, a: &CAAnimation, key: &str) {
    l.add_anim(a, key);
}

/// `addAnimation:forKey:` with a Rust key.
pub trait LayerExt {
    fn add_anim(&self, a: &CAAnimation, key: &str);
    fn remove_anim(&self, key: &str);
    fn has_anim(&self, key: &str) -> bool;
    fn set_key_value(&self, key_path: &str, v: &AnyObject);
    fn key_value(&self, key_path: &str) -> Option<Retained<AnyObject>>;
}

impl LayerExt for CALayer {
    #[allow(non_snake_case)]
    fn add_anim(&self, a: &CAAnimation, key: &str) {
        self.addAnimation_forKey(a, Some(&ns(key)));
    }
    fn remove_anim(&self, key: &str) {
        self.removeAnimationForKey(&ns(key));
    }
    fn has_anim(&self, key: &str) -> bool {
        // SAFETY: reading the layer's own animation list.
        unsafe { self.animationForKey(&ns(key)) }.is_some()
    }
    fn set_key_value(&self, key_path: &str, v: &AnyObject) {
        let _: () = unsafe { msg_send![self, setValue: v, forKeyPath: &*ns(key_path)] };
    }
    fn key_value(&self, key_path: &str) -> Option<Retained<AnyObject>> {
        unsafe { msg_send![self, valueForKeyPath: &*ns(key_path)] }
    }
}

/// Opacity fade from where it is on screen now (content follows the shape, so fades are short and start slightly
/// after the shape moves).
pub fn fade(l: &CALayer, to: f32, duration: f64, delay: f64) {
    fade_from(l, shown(l).opacity(), to, duration, delay)
}

/// The same from a given opacity (a layer made invisible in this transaction still shows its old value).
pub fn fade_from(l: &CALayer, from: f32, to: f32, duration: f64, delay: f64) {
    l.setOpacity(to);
    let a = CABasicAnimation::animationWithKeyPath(Some(&ns("opacity")));
    // SAFETY: NSNumbers for an opacity animation.
    unsafe {
        a.setFromValue(Some(any(&*num(from as f64))));
        a.setToValue(Some(any(&*num(to as f64))));
    }
    a.setDuration(duration);
    a.setBeginTime(if delay > 0.0 { super::sys::media_time() + delay } else { 0.0 });
    fill_backwards(&a);
    a.setTimingFunction(Some(&ease_out()));
    add(l, &a, "opacity");
}

/// Set a property and spring it from where it is on screen now.
pub fn spring_to(l: &CALayer, key_path: &str, target: &AnyObject, s: Spring) {
    let from = shown(l).key_value(key_path).or_else(|| l.key_value(key_path));
    l.set_key_value(key_path, target);
    if let Some(from) = from {
        add(l, &s.animation(key_path, &from, target, 0.0), key_path);
    }
}

pub fn basic(key: &str, from: Option<&AnyObject>, to: Option<&AnyObject>, duration: f64) -> Retained<CABasicAnimation> {
    let a = CABasicAnimation::animationWithKeyPath(Some(&ns(key)));
    // SAFETY: values match the key path at the call sites.
    unsafe {
        a.setFromValue(from);
        a.setToValue(to);
    }
    a.setDuration(duration);
    a
}

pub fn keyframes(key: &str, values: &[f64], times: Option<&[f64]>, duration: f64) -> Retained<CAKeyframeAnimation> {
    let a = CAKeyframeAnimation::animationWithKeyPath(Some(&ns(key)));
    let vs: Vec<Retained<NSNumber>> = values.iter().map(|v| num(*v)).collect();
    let arr = NSArray::from_retained_slice(&vs);
    // SAFETY: an array of NSNumbers is an array of objects.
    unsafe { a.setValues(Some(&Retained::cast_unchecked::<NSArray>(arr))) };
    if let Some(t) = times {
        let ts: Vec<Retained<NSNumber>> = t.iter().map(|v| num(*v)).collect();
        a.setKeyTimes(Some(&NSArray::from_retained_slice(&ts)));
    }
    a.setDuration(duration);
    a
}

/// Keyframes of arbitrary values (points).
pub fn keyframe_values(key: &str, values: &[Retained<AnyObject>], times: &[f64], duration: f64) -> Retained<CAKeyframeAnimation> {
    let a = CAKeyframeAnimation::animationWithKeyPath(Some(&ns(key)));
    let arr = NSArray::from_retained_slice(values);
    // SAFETY: values match the key path at the call sites.
    unsafe { a.setValues(Some(arr.as_ref())) };
    let ts: Vec<Retained<NSNumber>> = times.iter().map(|v| num(*v)).collect();
    a.setKeyTimes(Some(&NSArray::from_retained_slice(&ts)));
    a.setDuration(duration);
    a
}

pub fn group(anims: &[&CAAnimation], duration: f64) -> Retained<CAAnimationGroup> {
    let g = CAAnimationGroup::new();
    let rs: Vec<Retained<CAAnimation>> = anims.iter().map(|a| (*a).retain()).collect();
    g.setAnimations(Some(&NSArray::from_retained_slice(&rs)));
    g.setDuration(duration);
    g
}

/// Starts now on the layer's own clock (an explicit begin time, so the lab's at-rest check sees it while it runs).
pub fn timed(a: &CAAnimation, l: &CALayer) {
    a.setBeginTime(l.convertTime_fromLayer(super::sys::media_time(), None));
    fill_backwards(a);
}

/// One CATransaction with an optional completion.
pub fn transaction(disable: bool, completion: Option<Box<dyn Fn()>>, body: impl FnOnce()) {
    CATransaction::begin();
    if disable {
        CATransaction::setDisableActions(true);
    }
    if let Some(c) = completion {
        let b = block2::RcBlock::new(move || c());
        // SAFETY: the block is copied by Core Animation.
        unsafe { CATransaction::setCompletionBlock(Some(&b)) };
    }
    body();
    CATransaction::commit();
}

/// Changes without implicit animations.
pub fn quiet(body: impl FnOnce()) {
    transaction(true, None, body)
}

// MARK: Text

pub fn line_height(f: &NSFont) -> f64 {
    (f.ascender() - f.descender() + f.leading()).ceil()
}

pub fn attrs(font: &NSFont, color: &NSColor, para: Option<&NSMutableParagraphStyle>, strike: bool) -> Retained<NSDictionary<NSString, AnyObject>> {
    use objc2_app_kit::{
        NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName, NSStrikethroughColorAttributeName,
        NSStrikethroughStyleAttributeName,
    };
    // SAFETY: framework constants.
    unsafe {
        let mut keys: Vec<&NSString> = vec![NSFontAttributeName, NSForegroundColorAttributeName];
        let mut vals: Vec<Retained<AnyObject>> = vec![any(font).retain(), any(color).retain()];
        if let Some(p) = para {
            keys.push(NSParagraphStyleAttributeName);
            vals.push(any(p).retain());
        }
        if strike {
            keys.push(NSStrikethroughStyleAttributeName);
            vals.push(any(&*NSNumber::new_isize(1)).retain());
            keys.push(NSStrikethroughColorAttributeName);
            vals.push(any(&*color.colorWithAlphaComponent(0.8)).retain());
        }
        NSDictionary::from_retained_objects(&keys, &vals)
    }
}

pub fn attributed(s: &str, a: &NSDictionary<NSString, AnyObject>) -> Retained<NSAttributedString> {
    // SAFETY: a plain string with attribute values of the right types.
    unsafe { NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &ns(s), Some(a)) }
}

/// The width of one line of `s` in `font`.
pub fn text_width(s: &str, font: &NSFont) -> f64 {
    let a = attrs(font, &white(1.0), None, false);
    let size: CGSize = unsafe { msg_send![&*ns(s), sizeWithAttributes: &*a] };
    size.width
}

/// Measured with the same attributes it is drawn with (`usesLineFragmentOrigin | usesFontLeading`).
pub fn bounding(a: &NSAttributedString, width: f64) -> CGSize {
    let opts: isize = 1 | 2;
    let r: CGRect = unsafe { msg_send![a, boundingRectWithSize: CGSize::new(width, 10_000.0), options: opts, context: std::ptr::null::<AnyObject>()] };
    r.size
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A text layer measured with the attributes it is drawn with. One line that does not fit is cut at a character and
/// ends with an ellipsis (CATextLayer draws nothing for an over-long truncating attributed string).
#[allow(clippy::too_many_arguments)]
pub fn text_layer(s: &str, font: &NSFont, color: &NSColor, max_width: f64, lines: usize, align: Align, strike: bool, scale: f64) -> Retained<CATextLayer> {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(match align {
        Align::Left => NSTextAlignment::Left,
        Align::Center => NSTextAlignment::Center,
        Align::Right => NSTextAlignment::Right,
    });
    para.setLineBreakMode(if lines == 1 { NSLineBreakMode::ByClipping } else { NSLineBreakMode::ByWordWrapping });
    let mut text = s.to_owned();
    if lines == 1 && text_width(&text, font) > max_width {
        let chars: Vec<char> = s.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        let cut = |n: usize| format!("{}…", chars[..n].iter().collect::<String>().trim_matches(|c| c == ' ' || c == '\t'));
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if text_width(&cut(mid), font) <= max_width {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        text = cut(lo);
    }
    let a = attrs(font, color, Some(&para), strike);
    let attr = attributed(&text, &a);
    let lh = line_height(font);
    let r = bounding(&attr, if lines == 1 { 10_000.0 } else { max_width });
    let l = CATextLayer::new();
    // SAFETY: an attributed string for a text layer.
    unsafe { l.setString(Some(any(&*attr))) };
    l.setContentsScale(scale);
    l.setWrapped(lines > 1);
    // SAFETY: framework constants.
    unsafe {
        l.setTruncationMode(kCATruncationEnd);
        l.setAlignmentMode(match align {
            Align::Right => objc2_quartz_core::kCAAlignmentRight,
            Align::Center => objc2_quartz_core::kCAAlignmentCenter,
            Align::Left => objc2_quartz_core::kCAAlignmentLeft,
        });
    }
    l.setBounds(cgr(Rect::new(0.0, 0.0, (r.width.ceil() + 1.0).min(max_width), r.height.ceil().min(lh * lines as f64))));
    l.setAnchorPoint(pt(0.0, 0.0));
    no_implicit(&l);
    l
}

#[repr(C)]
struct CFRange {
    location: isize,
    length: isize,
}
type CFRef = *mut std::ffi::c_void;
#[link(name = "CoreText", kind = "framework")]
extern "C" {
    fn CTFramesetterCreateWithAttributedString(s: *const AnyObject) -> CFRef;
    fn CTFramesetterCreateFrame(f: CFRef, range: CFRange, path: CFRef, attrs: CFRef) -> CFRef;
    fn CTFrameGetLines(f: CFRef) -> CFRef;
    fn CTLineGetTypographicBounds(line: CFRef, ascent: *mut f64, descent: *mut f64, leading: *mut f64) -> f64;
}
extern "C" {
    fn CFArrayGetCount(a: CFRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFRef, i: isize) -> CFRef;
    fn CFRelease(o: CFRef);
    fn CGPathCreateWithRect(r: CGRect, t: CFRef) -> CFRef;
}

/// Wrapped text of any length. Its height is the sum of the lines Core Text breaks it into, each at its own height
/// (ascent, descent and leading, so a line with a taller fallback glyph counts as drawn): TextKit's measure runs about
/// a point taller per line at small sizes than CATextLayer draws, which left a gap under a long explanation.
pub fn text_block(s: &str, font: &NSFont, color: &NSColor, max_width: f64, scale: f64) -> Retained<CATextLayer> {
    let para = NSMutableParagraphStyle::new();
    para.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    let attr = attributed(s, &attrs(font, color, Some(&para), false));
    // SAFETY: Core Text objects made from a live attributed string, measured and released here; the lines array and
    // its lines belong to the frame (get rule).
    let height = unsafe {
        let f = CTFramesetterCreateWithAttributedString(any(&*attr));
        let path = CGPathCreateWithRect(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(max_width, 100_000.0)), std::ptr::null_mut());
        let frame = CTFramesetterCreateFrame(f, CFRange { location: 0, length: 0 }, path, std::ptr::null_mut());
        let lines = CTFrameGetLines(frame);
        let mut h = 0.0;
        for k in 0..CFArrayGetCount(lines) {
            let (mut a, mut d, mut l) = (0.0, 0.0, 0.0);
            CTLineGetTypographicBounds(CFArrayGetValueAtIndex(lines, k), &mut a, &mut d, &mut l);
            h += a + d + l;
        }
        CFRelease(frame);
        CFRelease(path);
        CFRelease(f);
        h.max(font.ascender() - font.descender())
    };
    let l = CATextLayer::new();
    // SAFETY: an attributed string for a text layer.
    unsafe { l.setString(Some(any(&*attr))) };
    l.setContentsScale(scale);
    l.setWrapped(true);
    l.setBounds(cgr(Rect::new(0.0, 0.0, max_width, height.ceil())));
    l.setAnchorPoint(pt(0.0, 0.0));
    no_implicit(&l);
    l
}

/// One line, left aligned, no strike: the common case.
pub fn text1(s: &str, font: &NSFont, color: &NSColor, max_width: f64, scale: f64) -> Retained<CATextLayer> {
    text_layer(s, font, color, max_width, 1, Align::Left, false, scale)
}

/// The string a text layer shows.
pub fn text_of(l: &CATextLayer) -> Option<String> {
    let s = l.string()?;
    let is_attr: bool = unsafe { msg_send![&*s, isKindOfClass: class!(NSAttributedString)] };
    if is_attr {
        let st: Retained<NSString> = unsafe { msg_send![&*s, string] };
        return Some(st.to_string());
    }
    let is_str: bool = unsafe { msg_send![&*s, isKindOfClass: class!(NSString)] };
    is_str.then(|| {
        let st: &NSString = unsafe { &*(&*s as *const AnyObject as *const NSString) };
        st.to_string()
    })
}

/// Replace a text layer's string keeping its attributes.
pub fn retext(l: &CATextLayer, s: &str) {
    let Some(old) = l.string() else { return };
    let is_attr: bool = unsafe { msg_send![&*old, isKindOfClass: class!(NSAttributedString)] };
    if !is_attr {
        return;
    }
    let oa: &NSAttributedString = unsafe { &*(&*old as *const AnyObject as *const NSAttributedString) };
    if oa.length() == 0 {
        return;
    }
    let a: Retained<NSDictionary<NSString, AnyObject>> =
        unsafe { msg_send![oa, attributesAtIndex: 0usize, effectiveRange: std::ptr::null_mut::<objc2_foundation::NSRange>()] };
    let n = attributed(s, &a);
    // SAFETY: an attributed string for a text layer.
    unsafe { l.setString(Some(any(&*n))) };
}

pub fn is_text_layer(l: &CALayer) -> bool {
    let c: &AnyClass = class!(CATextLayer);
    unsafe { msg_send![l, isKindOfClass: c] }
}

// MARK: Symbols

/// An SF Symbol image (palette colours: glyph first, then fill; one colour: hierarchical).
pub fn symbol_image(name: &str, point: f64, colors: &[Retained<NSColor>], weight: Weight) -> Option<Retained<NSImage>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None)
        .or_else(|| NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns("circle.fill"), None))?;
    let cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(point, weight.ns());
    let colour = if colors.len() > 1 {
        NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(colors))
    } else {
        NSImageSymbolConfiguration::configurationWithHierarchicalColor(&colors.first().cloned().unwrap_or_else(|| white(1.0)))
    };
    let cfg = cfg.configurationByApplyingConfiguration(&colour);
    img.imageWithSymbolConfiguration(&cfg)
}

pub fn symbol_exists(name: &str) -> bool {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None).is_some()
}

/// An SF Symbol drawn into a layer at the screen's scale.
pub fn symbol_layer(name: &str, size: f64, colors: &[Retained<NSColor>], weight: Weight, scale: f64) -> Retained<CALayer> {
    let l = layer();
    l.setBounds(cgr(Rect::new(0.0, 0.0, size, size)));
    l.setAnchorPoint(pt(0.5, 0.5));
    // SAFETY: a framework constant.
    l.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspect });
    l.setContentsScale(scale);
    if let Some(img) = symbol_image(name, size * 0.88, colors, weight) {
        let c = img.layerContentsForContentsScale(scale);
        // SAFETY: layer contents from NSImage for this scale.
        unsafe { l.setContents(Some(&c)) };
    }
    l
}

pub fn sym(name: &str, size: f64, colors: &[Retained<NSColor>], scale: f64) -> Retained<CALayer> {
    symbol_layer(name, size, colors, Weight::Semibold, scale)
}

/// An island's own icon in `color`: an SF Symbol name, or a tiny inline SVG drawn as a silhouette. `None` when it
/// cannot be drawn: the state's own icon shows instead.
pub fn icon_layer(icon: &str, size: f64, color: &NSColor, scale: f64) -> Option<Retained<CALayer>> {
    let t = icon.trim();
    if !t.starts_with("<svg") {
        if !symbol_exists(t) {
            return None;
        }
        return Some(sym(t, size, &[color.retain()], scale));
    }
    let lower = t.to_lowercase();
    if t.len() > 4096 || ["script", "href", "style", "url(", "foreignobject", "<image", "\\", "<!"].iter().any(|b| lower.contains(b)) {
        return None;
    }
    let img = super::draw::svg_silhouette(t, size, color, scale)?;
    let l = layer();
    l.setBounds(cgr(Rect::new(0.0, 0.0, size, size)));
    l.setAnchorPoint(pt(0.5, 0.5));
    // SAFETY: a framework constant.
    l.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspect });
    l.setContentsScale(scale);
    // SAFETY: a CGImage as layer contents.
    unsafe { l.setContents(Some(img.object())) };
    Some(l)
}

pub type Float = CGFloat;

/// Sanity for timing values read back from animations.
pub fn media(a: &CAAnimation) -> (f64, f64, f32, f64, bool) {
    (a.beginTime(), a.duration(), a.repeatCount(), a.repeatDuration(), a.autoreverses())
}
