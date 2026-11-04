//! Setup's drawn pieces, after the approved prototype (`Projects/Heimdall/lab/setup-wizard`, its CSS): plates (cards,
//! model cards, fields), pads (tiles, voice tiles, mark cells, buttons, chips), the page dots and the app tile. Every
//! piece draws in `drawRect:` from its own state, so offscreen renders show exactly what the window shows, and nothing
//! draws at rest.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSAutoresizingMaskOptions as Mask, NSBezierPath, NSButton, NSColor,
    NSCompositingOperation, NSControlSize, NSFocusRingType, NSFont, NSGradient, NSGraphicsContext, NSImage,
    NSLineBreakMode, NSProgressIndicator, NSProgressIndicatorStyle, NSTextAlignment, NSTextField, NSView,
};
use objc2_core_graphics::CGContext;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use super::super::util::{attributed, ns, rect};
use crate::identity::Icon;

// ---- the prototype's palette (`:root`) ----

pub(crate) fn hex(rgb: u32, alpha: f64) -> Retained<NSColor> {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(c(16), c(8), c(0), alpha)
}
pub(crate) fn white(alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, alpha)
}
pub(crate) const BLUE: u32 = 0x0A84FF;
pub(crate) const GREEN: u32 = 0x30D158;
pub(crate) const ORANGE: u32 = 0xFF9F0A;
/// `.mcard .mr em`: "Suggested".
pub(crate) const YELLOW: u32 = 0xFFD60A;
/// The window (`#win`).
pub(crate) const BACKGROUND: u32 = 0x1F1F22;
pub(crate) fn t1() -> Retained<NSColor> { white(0.92) }
pub(crate) fn t2() -> Retained<NSColor> { white(0.62) }
pub(crate) fn t3() -> Retained<NSColor> { white(0.40) }
pub(crate) fn sep() -> Retained<NSColor> { white(0.09) }
pub(crate) fn fill() -> Retained<NSColor> { white(0.06) }
pub(crate) fn fill2() -> Retained<NSColor> { white(0.10) }
pub(crate) fn blue() -> Retained<NSColor> { hex(BLUE, 1.0) }

pub(crate) fn weight(size: f64, w: f64) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, w)
}
pub(crate) fn regular(size: f64) -> Retained<NSFont> { weight(size, 0.0) }
pub(crate) fn medium(size: f64) -> Retained<NSFont> { weight(size, unsafe { objc2_app_kit::NSFontWeightMedium }) }
pub(crate) fn semibold(size: f64) -> Retained<NSFont> { weight(size, unsafe { objc2_app_kit::NSFontWeightSemibold }) }
pub(crate) fn bold(size: f64) -> Retained<NSFont> { weight(size, unsafe { objc2_app_kit::NSFontWeightBold }) }
pub(crate) fn tabular(size: f64, w: f64) -> Retained<NSFont> { NSFont::monospacedDigitSystemFontOfSize_weight(size, w) }

fn rounded(r: NSRect, radius: f64) -> Retained<NSBezierPath> {
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius)
}

fn inset(r: NSRect, d: f64) -> NSRect {
    rect(r.origin.x + d, r.origin.y + d, r.size.width - 2.0 * d, r.size.height - 2.0 * d)
}

/// `box-shadow: 0 0 0 Wpx color inset`: a ring inside the edge.
fn ring(r: NSRect, radius: f64, color: &NSColor, width: f64) {
    let path = rounded(inset(r, width / 2.0), (radius - width / 2.0).max(0.0));
    path.setLineWidth(width);
    color.setStroke();
    path.stroke();
}

/// `image` (a template symbol or mark) in `color`, aspect-fit and centred in `r`; a non-template image as it is.
pub(crate) fn draw_image(image: &NSImage, r: NSRect, color: Option<&NSColor>) {
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    let k = (r.size.width / size.width).min(r.size.height / size.height);
    let (w, h) = (size.width * k, size.height * k);
    let at = rect(r.origin.x + (r.size.width - w) / 2.0, r.origin.y + (r.size.height - h) / 2.0, w, h);
    let draw = || unsafe { image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(at, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0, true, None) };
    let Some(color) = color.filter(|_| image.isTemplate()) else {
        draw();
        return;
    };
    let Some(context) = NSGraphicsContext::currentContext() else { return };
    let cg = context.CGContext();
    CGContext::save_g_state(Some(&cg));
    CGContext::set_alpha(Some(&cg), color.alphaComponent());
    unsafe { CGContext::begin_transparency_layer(Some(&cg), None) };
    draw();
    color.colorWithAlphaComponent(1.0).set();
    objc2_app_kit::NSRectFillUsingOperation(at, NSCompositingOperation::SourceAtop);
    CGContext::end_transparency_layer(Some(&cg));
    CGContext::restore_g_state(Some(&cg));
}

/// One line of text with its top-left at `(x, y)` (flipped views).
pub(crate) fn draw_text(text: &str, font: &NSFont, color: &NSColor, x: f64, y: f64) -> NSSize {
    let s = attributed(text, font, color);
    s.drawAtPoint(NSPoint::new(x, y));
    s.size()
}

/// One line in `r`, cut with an ellipsis where it would pass `r`'s right edge.
pub(crate) fn draw_truncated(text: &str, font: &NSFont, color: &NSColor, r: NSRect) {
    let style = objc2_app_kit::NSMutableParagraphStyle::new();
    style.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let s = objc2_foundation::NSMutableAttributedString::initWithAttributedString(objc2_foundation::NSMutableAttributedString::alloc(), &attributed(text, font, color));
    let range = objc2_foundation::NSRange::new(0, text.encode_utf16().count());
    unsafe { s.addAttribute_value_range(objc2_app_kit::NSParagraphStyleAttributeName, AsRef::<AnyObject>::as_ref(&*style), range) };
    s.drawInRect(r);
}

/// One line centred on `cx`, its top at `y`; clipped to `max` wide by truncation.
pub(crate) fn draw_centered(text: &str, font: &NSFont, color: &NSColor, cx: f64, y: f64, max: f64) {
    let s = attributed(text, font, color);
    let size = s.size();
    let w = size.width.min(max);
    s.drawInRect(rect((cx - w / 2.0).round(), y, w.ceil() + 1.0, size.height.ceil()));
}

/// A symbol as a template image at `size` points.
/// One colour: a symbol with a hierarchical layer (the laptop's screen) would tint two-tone.
pub(crate) fn symbol(name: &str, size: f64, w: f64) -> Option<Retained<NSImage>> {
    let image = super::super::util::symbol_sized(name, size, w)?;
    let mono = objc2_app_kit::NSImageSymbolConfiguration::configurationPreferringMonochrome();
    image.imageWithSymbolConfiguration(&mono).or(Some(image))
}

/// An app's own icon from its bundle (its `.icns`), as Finder shows it.
pub(crate) fn app_icon(path: &std::path::Path) -> Retained<NSImage> {
    objc2_app_kit::NSWorkspace::sharedWorkspace().iconForFile(&ns(&path.to_string_lossy()))
}

// ---- plates: cards, model cards, fields ----

pub struct PlateIvars {
    fill: RefCell<Option<Retained<NSColor>>>,
    /// Top and bottom colours of a vertical gradient (under the fill).
    gradient: RefCell<Option<(Retained<NSColor>, Retained<NSColor>)>>,
    ring: RefCell<Option<(Retained<NSColor>, f64)>>,
    radius: Cell<f64>,
    /// Hairlines between rows: their y, from `line_inset` to the right edge.
    lines: RefCell<Vec<f64>>,
    line_inset: Cell<f64>,
}

define_class!(
    /// A rounded plate: the prototype's `.card`, `.mcard`, `.perm`, `.acts`, the fields' wells and the model icons.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupPlate"]
    #[ivars = PlateIvars]
    pub struct Plate;

    impl Plate {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl Plate {
    pub(crate) fn new(frame: NSRect, radius: f64, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PlateIvars {
            fill: RefCell::new(Some(fill())),
            gradient: RefCell::new(None),
            ring: RefCell::new(None),
            radius: Cell::new(radius),
            lines: RefCell::new(vec![]),
            line_inset: Cell::new(16.0),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// `.card`: white 6% with a 5% inner ring.
    pub(crate) fn card(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let p = Self::new(frame, 12.0, mtm);
        p.set_ring(Some((white(0.05), 1.0)));
        p
    }

    pub(crate) fn set_fill(&self, color: Option<Retained<NSColor>>) {
        *self.ivars().fill.borrow_mut() = color;
        self.setNeedsDisplay(true);
    }
    pub(crate) fn set_gradient(&self, colors: Option<(Retained<NSColor>, Retained<NSColor>)>) {
        *self.ivars().gradient.borrow_mut() = colors;
        self.setNeedsDisplay(true);
    }
    pub(crate) fn set_ring(&self, ring: Option<(Retained<NSColor>, f64)>) {
        *self.ivars().ring.borrow_mut() = ring;
        self.setNeedsDisplay(true);
    }
    /// Selected (`.mcard.on`, `.tile.on`): blue 14% with a 2 pt blue ring; otherwise a card.
    pub(crate) fn set_selected(&self, on: bool) {
        if on {
            self.set_fill(Some(hex(BLUE, 0.14)));
            self.set_ring(Some((blue(), 2.0)));
        } else {
            self.set_fill(Some(fill()));
            self.set_ring(Some((white(0.06), 1.0)));
        }
    }
    pub(crate) fn set_lines(&self, lines: Vec<f64>, inset: f64) {
        *self.ivars().lines.borrow_mut() = lines;
        self.ivars().line_inset.set(inset);
        self.setNeedsDisplay(true);
    }

    fn draw(&self) {
        let b = self.bounds();
        let radius = self.ivars().radius.get();
        let path = rounded(b, radius);
        if let Some((top, bottom)) = self.ivars().gradient.borrow().as_ref() {
            // Flipped: 90 degrees runs from the top down.
            if let Some(g) = NSGradient::initWithStartingColor_endingColor(NSGradient::alloc(), top, bottom) {
                g.drawInBezierPath_angle(&path, 90.0);
            }
        }
        if let Some(f) = self.ivars().fill.borrow().as_ref() {
            f.setFill();
            path.fill();
        }
        let inset = self.ivars().line_inset.get();
        for y in self.ivars().lines.borrow().iter() {
            sep().setFill();
            objc2_app_kit::NSRectFillUsingOperation(rect(inset, *y, b.size.width - inset, 1.0), NSCompositingOperation::SourceOver);
        }
        if let Some((color, width)) = self.ivars().ring.borrow().as_ref() {
            ring(b, radius, color, *width);
        }
    }
}

/// A model card's icon (`.mcard .mic`): a 40-pt rounded square in a blue-to-indigo gradient with a white symbol.
pub(crate) fn model_icon(parent: &NSView, symbol_name: &str, x: f64, y: f64, side: f64, mtm: MainThreadMarker) -> Retained<Plate> {
    let plate = Plate::new(rect(x, y, side, side), side / 4.0, mtm);
    plate.set_fill(None);
    plate.set_gradient(Some((hex(0x4F7BFF, 1.0), hex(0x5E3BEE, 1.0))));
    plate.set_ring(Some((white(0.15), 0.5)));
    let glyph = side * 0.6;
    let image = image_view(rect((side - glyph) / 2.0, (side - glyph) / 2.0, glyph, glyph), symbol(symbol_name, side * 0.5, unsafe { objc2_app_kit::NSFontWeightMedium }), Some(&NSColor::whiteColor()), mtm);
    plate.addSubview(&image);
    parent.addSubview(&plate);
    plate
}

pub(crate) fn image_view(frame: NSRect, image: Option<Retained<NSImage>>, tint: Option<&NSColor>, mtm: MainThreadMarker) -> Retained<objc2_app_kit::NSImageView> {
    let view = objc2_app_kit::NSImageView::new(mtm);
    view.setFrame(frame);
    view.setImage(image.as_deref());
    view.setContentTintColor(tint);
    view.setImageScaling(objc2_app_kit::NSImageScaling::ScaleProportionallyUpOrDown);
    view
}

// ---- labels ----

/// A one-line label at `(x, y)`, `width` wide (0: its own width); `align` left, centre or right.
pub(crate) fn text(parent: &NSView, s: &str, font: &NSFont, color: &NSColor, x: f64, y: f64, width: f64, align: NSTextAlignment, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&ns(s), mtm);
    field.setFont(Some(font));
    field.setTextColor(Some(color));
    field.setAlignment(align);
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    field.sizeToFit();
    let size = field.frame().size;
    field.setFrame(rect(x, y, if width > 0.0 { width } else { size.width }, size.height));
    parent.addSubview(&field);
    field
}

/// Sets a label's text and, when it hugs its text, its width (keeping the left or right edge).
pub(crate) fn retext(field: &NSTextField, s: &str, keep_right: bool) {
    field.setStringValue(&ns(s));
    let old = field.frame();
    field.sizeToFit();
    let size = field.frame().size;
    let x = if keep_right { old.origin.x + old.size.width - size.width } else { old.origin.x };
    field.setFrame(rect(x, old.origin.y, size.width, size.height));
}

/// A label's text height for `font` (one line).
pub(crate) fn line_height(font: &NSFont) -> f64 {
    attributed("Ag", font, &t1()).size().height.ceil()
}

/// The page's title: `.page h1` (28 pt bold) or `h1.q` (24 pt, two lines), centred across `width`.
pub(crate) fn heading(parent: &NSView, s: &str, size: f64, y: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::wrappingLabelWithString(&ns(s), mtm);
    field.setFont(Some(&bold(size)));
    field.setTextColor(Some(&t1()));
    field.setAlignment(NSTextAlignment::Center);
    let w = parent.frame().size.width;
    let lines = s.matches('\n').count() as f64 + 1.0;
    let h = (size * 1.25 * lines).ceil();
    field.setFrame(rect(0.0, y, w, h));
    field.setAutoresizingMask(Mask::ViewWidthSizable);
    parent.addSubview(&field);
    field
}

pub(crate) fn spinner(frame: NSRect, mtm: MainThreadMarker) -> Retained<NSProgressIndicator> {
    let spinner = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), frame);
    spinner.setStyle(NSProgressIndicatorStyle::Spinning);
    spinner.setControlSize(NSControlSize::Small);
    spinner.setDisplayedWhenStopped(false);
    spinner
}

// ---- pads: tiles, voices, marks, buttons ----

/// What a pad draws.
pub(crate) enum Look {
    /// `.tile`, 132 × 96: a mark (a template tinted white, or an app icon as it is), the title, a caption under it.
    Tile { image: Option<Retained<NSImage>>, image_side: f64, title: String, caption: Option<(String, Retained<NSColor>)> },
    /// `.vt`, a voice: the initial on its colour, the name.
    Voice { color: Retained<NSColor>, name: String },
    /// `.mk`, 40 × 40: one mark.
    Mark(Icon),
    /// `.btn`: primary (blue) or secondary; `small` is `.btn.sm`.
    Button { title: String, primary: bool, small: bool },
}

pub struct PadIvars {
    look: RefCell<Look>,
    selected: Cell<bool>,
}

define_class!(
    /// A clickable drawn control: a button whose cell never draws (the look is `Look`); target, action, tag, key
    /// equivalent and accessibility are the button's own.
    #[unsafe(super(NSButton, objc2_app_kit::NSControl, NSView, objc2_app_kit::NSResponder, objc2::runtime::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupPad"]
    #[ivars = PadIvars]
    pub struct Pad;

    impl Pad {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&objc2_app_kit::NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl Pad {
    pub(crate) fn new(frame: NSRect, look: Look, target: &AnyObject, action: Sel, tag: isize, mtm: MainThreadMarker) -> Retained<Self> {
        let title = match &look {
            Look::Tile { title, .. } | Look::Voice { name: title, .. } | Look::Button { title, .. } => title.clone(),
            Look::Mark(icon) => super::super::personalise::mark_title(icon),
        };
        let this = Self::alloc(mtm).set_ivars(PadIvars { look: RefCell::new(look), selected: Cell::new(false) });
        let pad: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        pad.setTitle(&ns(&title));
        pad.setBordered(false);
        pad.setFocusRingType(NSFocusRingType::None);
        pad.setTag(tag);
        unsafe {
            pad.setTarget(Some(target));
            pad.setAction(Some(action));
        }
        pad
    }

    /// A footer or card button sized to its title (`min` wide at least).
    pub(crate) fn button(title: &str, primary: bool, small: bool, min: f64, target: &AnyObject, action: Sel, mtm: MainThreadMarker) -> Retained<Self> {
        let pad = Self::new(rect(0.0, 0.0, 10.0, 10.0), Look::Button { title: title.into(), primary, small }, target, action, 0, mtm);
        pad.fit(min);
        pad
    }

    /// A button's width from its title: padding 18 (14 small) a side, at least `min`.
    pub(crate) fn fit(&self, min: f64) {
        let Look::Button { title, small, .. } = &*self.ivars().look.borrow() else { return };
        let (h, pad, size) = if *small { (28.0, 14.0, 13.0) } else { (32.0, 18.0, 14.0) };
        let w = (attributed(title, &medium(size), &t1()).size().width + 2.0 * pad).ceil().max(min);
        let f = self.frame();
        self.setFrame(rect(f.origin.x, f.origin.y, w, h));
    }

    pub(crate) fn retitle(&self, title: &str, min: f64) {
        if let Look::Button { title: t, .. } = &mut *self.ivars().look.borrow_mut() {
            if t == title {
                return;
            }
            *t = title.into();
        }
        self.setTitle(&ns(title));
        let right = self.frame().origin.x + self.frame().size.width;
        self.fit(min);
        let f = self.frame();
        self.setFrameOrigin(NSPoint::new(right - f.size.width, f.origin.y));
        self.redraw();
    }

    pub(crate) fn set_primary(&self, primary: bool) {
        if let Look::Button { primary: p, .. } = &mut *self.ivars().look.borrow_mut() {
            *p = primary;
        }
        self.redraw();
    }

    fn redraw(&self) {
        AsRef::<NSView>::as_ref(self).setNeedsDisplay(true);
    }

    pub(crate) fn label(&self) -> String {
        self.title().to_string()
    }

    pub(crate) fn is_selected(&self) -> bool {
        self.ivars().selected.get()
    }

    pub(crate) fn set_selected(&self, on: bool) {
        if self.ivars().selected.replace(on) != on {
            self.redraw();
        }
    }

    pub(crate) fn set_look(&self, look: Look) {
        // VoiceOver reads the button's title: it follows the drawn one.
        if let Look::Tile { title, .. } | Look::Voice { name: title, .. } | Look::Button { title, .. } = &look {
            self.setTitle(&ns(title));
        }
        *self.ivars().look.borrow_mut() = look;
        self.redraw();
    }

    fn draw(&self) {
        let b = self.bounds();
        let on = self.ivars().selected.get();
        let enabled = self.isEnabled();
        let pressed = self.isHighlighted();
        let Some(context) = NSGraphicsContext::currentContext() else { return };
        let cg = context.CGContext();
        CGContext::save_g_state(Some(&cg));
        match &*self.ivars().look.borrow() {
            Look::Button { .. } if !enabled => CGContext::set_alpha(Some(&cg), 0.35),
            Look::Tile { .. } | Look::Voice { .. } if !enabled => CGContext::set_alpha(Some(&cg), 0.45),
            _ => {}
        }
        unsafe { CGContext::begin_transparency_layer(Some(&cg), None) };
        match &*self.ivars().look.borrow() {
            Look::Tile { image, image_side, title, caption } => {
                let path = rounded(b, 12.0);
                (if on { hex(BLUE, 0.16) } else { fill() }).setFill();
                path.fill();
                if on { ring(b, 12.0, &blue(), 2.0) } else { ring(b, 12.0, &white(0.06), 1.0) }
                let title_font = medium(13.5);
                let caption_font = medium(11.5);
                let title_h = line_height(&title_font);
                // The icon and title sit where a plain tile has them; a caption hangs under the title.
                let gap = 9.0;
                let total = 30.0 + gap + title_h;
                let top = ((b.size.height - total) / 2.0).round();
                if let Some(image) = image {
                    let side = *image_side;
                    draw_image(image, rect((b.size.width - side) / 2.0, top + (30.0 - side) / 2.0, side, side), Some(&white(0.92)));
                }
                let cx = b.size.width / 2.0;
                draw_centered(title, &title_font, &*if on { NSColor::whiteColor() } else { t1() }, cx, top + 30.0 + gap, b.size.width - 12.0);
                if let Some((caption, color)) = caption {
                    draw_centered(caption, &caption_font, color, cx, top + 30.0 + gap + title_h, b.size.width - 12.0);
                }
            }
            Look::Voice { color, name } => {
                let path = rounded(b, 11.0);
                (if on { hex(BLUE, 0.14) } else { fill() }).setFill();
                path.fill();
                if on { ring(b, 11.0, &blue(), 2.0) } else { ring(b, 11.0, &white(0.05), 1.0) }
                let side = 22.0;
                let av = rect(8.0, ((b.size.height - side) / 2.0).round(), side, side);
                color.setFill();
                NSBezierPath::bezierPathWithOvalInRect(av).fill();
                let initial: String = name.chars().take(1).collect();
                let font = semibold(11.5);
                draw_centered(&initial, &font, &NSColor::whiteColor(), av.origin.x + side / 2.0, av.origin.y + (side - line_height(&font)) / 2.0, side);
                let font = medium(12.5);
                let x = av.origin.x + side + 8.0;
                draw_truncated(name, &font, &t1(), rect(x, ((b.size.height - line_height(&font)) / 2.0).round(), (b.size.width - x - 8.0).max(0.0), line_height(&font)));
            }
            Look::Mark(icon) => {
                let path = rounded(b, 10.0);
                if on {
                    blue().setFill();
                    path.fill();
                } else {
                    fill().setFill();
                    path.fill();
                    ring(b, 10.0, &white(0.06), 1.0);
                }
                let side = 20.0;
                super::super::helmet::draw_icon(icon, rect(0.0, ((b.size.height - side) / 2.0).round(), b.size.width, side), false, &NSColor::whiteColor());
            }
            Look::Button { title, primary, small } => {
                let radius = if *small { 7.0 } else { 9.0 };
                let path = rounded(b, radius);
                if *primary {
                    blue().setFill();
                    path.fill();
                } else {
                    fill2().setFill();
                    path.fill();
                    ring(b, radius, &white(0.08), 0.5);
                }
                if pressed {
                    white(0.14).setFill();
                    path.fill();
                }
                let font = medium(if *small { 13.0 } else { 14.0 });
                let color = if *primary { NSColor::whiteColor() } else { t1() };
                draw_centered(title, &font, &color, b.size.width / 2.0, ((b.size.height - line_height(&font)) / 2.0).round(), b.size.width - 8.0);
            }
        }
        CGContext::end_transparency_layer(Some(&cg));
        CGContext::restore_g_state(Some(&cg));
    }
}

// ---- the page dots ----

pub struct DotsIvars {
    count: Cell<usize>,
    current: Cell<usize>,
    /// The footer's width the buttons take at the right: the dots stay centred unless that would reach them.
    reserve: Cell<f64>,
}

define_class!(
    /// `#dots`: one 7-pt dot a page, 8 pt apart, the current one white 85%, the others 18%.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupDots"]
    #[ivars = DotsIvars]
    pub struct Dots;

    impl Dots {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let (n, current) = (self.ivars().count.get(), self.ivars().current.get());
            let dw = Self::width(n);
            let x0 = ((b.size.width - dw) / 2.0).min(b.size.width - self.ivars().reserve.get() - 24.0 - dw).max(22.0).round();
            for i in 0..n {
                (if i == current { white(0.85) } else { white(0.18) }).setFill();
                NSBezierPath::bezierPathWithOvalInRect(rect(x0 + i as f64 * (DOT + DOT_GAP), ((b.size.height - DOT) / 2.0).round(), DOT, DOT)).fill();
            }
        }
    }
);

const DOT: f64 = 7.0;
const DOT_GAP: f64 = 8.0;

impl Dots {
    pub(crate) fn new(frame: NSRect, count: usize, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DotsIvars { count: Cell::new(count), current: Cell::new(0), reserve: Cell::new(0.0) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
    pub(crate) fn width(n: usize) -> f64 {
        n as f64 * DOT + n.saturating_sub(1) as f64 * DOT_GAP
    }
    pub(crate) fn set_current(&self, i: usize) {
        self.ivars().current.set(i);
        self.setNeedsDisplay(true);
    }
    pub(crate) fn set_reserve(&self, w: f64) {
        self.ivars().reserve.set(w);
        self.setNeedsDisplay(true);
    }
}

// ---- the app tile ----

pub struct TileIvars {
    icon: RefCell<Icon>,
    /// The tile's side and corner radius; the view is larger by `margin` a side for the shadow.
    side: Cell<f64>,
    radius: Cell<f64>,
    margin: Cell<f64>,
    /// `.halo` (Customise): a radial graphite; otherwise `.apptile`'s vertical one.
    radial: Cell<bool>,
    /// The mark's share of the side.
    mark: Cell<f64>,
    /// An image drawn in place of the mark (the Vella diagram's app icon).
    image: RefCell<Option<Retained<NSImage>>>,
}

define_class!(
    /// The app tile (`.apptile`, `.halo`): the configured mark in white on graphite, with its shadow and edge.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupAppTile"]
    #[ivars = TileIvars]
    pub struct AppTile;

    impl AppTile {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl AppTile {
    /// A tile `side` points square at `(x, y)` (its own corner, not the view's).
    pub(crate) fn new(x: f64, y: f64, side: f64, radius: f64, radial: bool, mtm: MainThreadMarker) -> Retained<Self> {
        let margin = (side * 0.3).round();
        let this = Self::alloc(mtm).set_ivars(TileIvars {
            icon: RefCell::new(crate::identity::current().icon),
            side: Cell::new(side),
            radius: Cell::new(radius),
            margin: Cell::new(margin),
            radial: Cell::new(radial),
            mark: Cell::new(0.56),
            image: RefCell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: rect(x - margin, y - margin, side + 2.0 * margin, side + 2.0 * margin)] }
    }
    pub(crate) fn set_icon(&self, icon: &Icon) {
        *self.ivars().icon.borrow_mut() = icon.clone();
        self.setNeedsDisplay(true);
    }
    pub(crate) fn icon(&self) -> Icon {
        self.ivars().icon.borrow().clone()
    }
    pub(crate) fn set_mark_share(&self, share: f64) {
        self.ivars().mark.set(share);
    }
    pub(crate) fn set_image(&self, image: Option<Retained<NSImage>>) {
        *self.ivars().image.borrow_mut() = image;
        self.setNeedsDisplay(true);
    }
    pub(crate) fn margin(&self) -> f64 {
        self.ivars().margin.get()
    }

    fn draw(&self) {
        let (side, radius, m) = (self.ivars().side.get(), self.ivars().radius.get(), self.ivars().margin.get());
        let tile = rect(m, m, side, side);
        let path = rounded(tile, radius);
        // `0 18px 40px rgba(0,0,0,.45)`, scaled to the tile.
        NSGraphicsContext::saveGraphicsState_class();
        let shadow = objc2_app_kit::NSShadow::new();
        shadow.setShadowColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.45)));
        shadow.setShadowBlurRadius(side * 0.2);
        shadow.setShadowOffset(NSSize::new(0.0, -side * 0.09));
        shadow.set();
        hex(0x161618, 1.0).setFill();
        path.fill();
        NSGraphicsContext::restoreGraphicsState_class();
        if self.ivars().radial.get() {
            if let Some(g) = NSGradient::initWithStartingColor_endingColor(NSGradient::alloc(), &hex(0x2C2C30, 1.0), &hex(0x161618, 1.0)) {
                g.drawInBezierPath_relativeCenterPosition(&path, NSPoint::new(0.0, 0.3));
            }
            ring(tile, radius, &white(0.08), 1.0);
        } else {
            if let Some(g) = NSGradient::initWithStartingColor_endingColor(NSGradient::alloc(), &hex(0x2E2E31, 1.0), &hex(0x121214, 1.0)) {
                g.drawInBezierPath_angle(&path, 90.0);
            }
            // `0 1px 0 rgba(255,255,255,.12) inset` and the .5 px edge at 22%.
            NSGraphicsContext::saveGraphicsState_class();
            path.addClip();
            let light = rounded(tile, radius);
            light.appendBezierPath(&rounded(rect(m, m + (side / 156.0).max(0.5), side, side), radius));
            light.setWindingRule(objc2_app_kit::NSWindingRule::EvenOdd);
            white(0.12).setFill();
            light.fill();
            NSGraphicsContext::restoreGraphicsState_class();
            ring(tile, radius, &white(0.22), 0.5);
        }
        if let Some(image) = self.ivars().image.borrow().as_ref() {
            let s = side * self.ivars().mark.get();
            draw_image(image, rect(m + (side - s) / 2.0, m + (side - s) / 2.0, s, s), Some(&NSColor::whiteColor()));
            return;
        }
        let s = side * self.ivars().mark.get();
        super::super::helmet::draw_icon(&self.ivars().icon.borrow(), rect(m, m + (side - s) / 2.0, side, s), false, &NSColor::whiteColor());
    }
}

// ---- the band: a centred child at most `cap` wide ----

pub struct BandIvars {
    cap: Cell<f64>,
    side: Cell<f64>,
}

define_class!(
    /// A full-width row that keeps its one child centred, `cap` wide at most and `side` in from both edges (the
    /// prototype's `max-width` cards in a page padded 48 pt a side). The child's own subviews follow by autoresizing.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupBand"]
    #[ivars = BandIvars]
    pub struct Band;

    impl Band {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, _old: NSSize) {
            self.place();
        }
    }
);

impl Band {
    /// A band at `y` across `width`, holding `child` (its height is the band's).
    pub(crate) fn wrap(parent: &NSView, child: &NSView, y: f64, cap: f64, side: f64, mtm: MainThreadMarker) -> Retained<Self> {
        let width = parent.frame().size.width;
        let h = child.frame().size.height;
        let this = Self::alloc(mtm).set_ivars(BandIvars { cap: Cell::new(cap), side: Cell::new(side) });
        let band: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, y, width, h)] };
        band.setAutoresizingMask(Mask::ViewWidthSizable);
        band.addSubview(child);
        band.place();
        parent.addSubview(&band);
        band
    }

    /// The child's width at the band's width now.
    pub(crate) fn child_width(&self) -> f64 {
        let w = self.bounds().size.width;
        self.ivars().cap.get().min(w - 2.0 * self.ivars().side.get()).max(0.0)
    }

    pub(crate) fn place(&self) {
        let w = self.bounds().size.width;
        let cw = self.child_width();
        for v in self.subviews().iter() {
            let f = v.frame();
            v.setFrame(rect(((w - cw) / 2.0).round(), f.origin.y, cw, f.size.height));
        }
    }

    /// Resizes the band and its child to `h` tall.
    pub(crate) fn set_height(&self, h: f64) {
        let f = self.frame();
        self.setFrame(rect(f.origin.x, f.origin.y, f.size.width, h));
        for v in self.subviews().iter() {
            let c = v.frame();
            v.setFrame(rect(c.origin.x, c.origin.y, c.size.width, h));
        }
    }
}

// ---- the island preview (Customise) ----

pub struct PreviewIvars {
    icon: RefCell<Icon>,
    greeting: RefCell<String>,
}

define_class!(
    /// `.pv`: a strip of desktop with the menu bar and the notch island greeting the user, as it will on first run.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupPreview"]
    #[ivars = PreviewIvars]
    pub struct Preview;

    impl Preview {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl Preview {
    pub(crate) fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PreviewIvars { icon: RefCell::new(Icon::Helmet), greeting: RefCell::new(String::new()) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub(crate) fn set(&self, icon: &Icon, greeting: &str) {
        *self.ivars().icon.borrow_mut() = icon.clone();
        *self.ivars().greeting.borrow_mut() = greeting.into();
        self.setNeedsDisplay(true);
    }

    pub(crate) fn greeting(&self) -> String {
        self.ivars().greeting.borrow().clone()
    }

    fn draw(&self) {
        let b = self.bounds();
        let card = rounded(b, 12.0);
        NSGraphicsContext::saveGraphicsState_class();
        card.addClip();
        // A dusk desktop: warm at the horizon, deep blue above (the prototype's wallpaper, as colour only).
        let colors = [hex(0x2B3A5C, 1.0), hex(0x6B5A7A, 1.0), hex(0xC98A6B, 1.0)];
        if let Some(g) = NSGradient::initWithColors(NSGradient::alloc(), &objc2_foundation::NSArray::from_retained_slice(&colors)) {
            g.drawInRect_angle(b, 90.0);
        }
        // The menu bar.
        hex(0x121216, 0.55).setFill();
        objc2_app_kit::NSRectFillUsingOperation(rect(0.0, 0.0, b.size.width, 16.0), NSCompositingOperation::SourceOver);
        let greeting = self.ivars().greeting.borrow().clone();
        let font = medium(12.5);
        let text_w = attributed(&greeting, &font, &t1()).size().width.ceil();
        let (mark, wave, gap, pad) = (14.0, 22.0, 12.0, 16.0);
        let w = (pad + mark + gap + text_w + gap + wave + pad).min(b.size.width - 24.0);
        let island = rect(((b.size.width - w) / 2.0).round(), 0.0, w, 46.0);
        let shape = NSBezierPath::bezierPath();
        let (x0, x1, h, r) = (island.origin.x, island.origin.x + w, 46.0, 18.0);
        shape.moveToPoint(NSPoint::new(x0, 0.0));
        shape.lineToPoint(NSPoint::new(x1, 0.0));
        shape.lineToPoint(NSPoint::new(x1, h - r));
        shape.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(NSPoint::new(x1 - r, h - r), r, 0.0, 90.0, false);
        shape.lineToPoint(NSPoint::new(x0 + r, h));
        shape.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(NSPoint::new(x0 + r, h - r), r, 90.0, 180.0, false);
        shape.closePath();
        NSColor::blackColor().setFill();
        shape.fill();
        let mut x = x0 + pad;
        super::super::helmet::draw_icon(&self.ivars().icon.borrow(), rect(x, (h - mark) / 2.0, mark, mark), false, &NSColor::whiteColor());
        x += mark + gap;
        let lh = line_height(&font);
        attributed(&greeting, &font, &NSColor::whiteColor()).drawInRect(rect(x, ((h - lh) / 2.0).round(), text_w + 1.0, lh));
        x += text_w + gap;
        // The voice's level as five still bars (it talks on the island; here it holds still).
        NSColor::whiteColor().setFill();
        for (i, bar) in [5.0, 11.0, 16.0, 9.0, 6.0].iter().enumerate() {
            let bx = x + i as f64 * 5.0;
            rounded(rect(bx, (h - bar) / 2.0, 3.0, *bar), 1.5).fill();
        }
        NSGraphicsContext::restoreGraphicsState_class();
        ring(b, 12.0, &white(0.10), 1.0);
    }
}

/// A one-line editable field in its well (`.field.big`, `.namebig`): no bezel, the text centred, a blue focus ring
/// drawn by the well while `focused`.
pub(crate) fn big_field(parent: &NSView, frame: NSRect, size: f64, w: f64, placeholder: &str, mtm: MainThreadMarker) -> (Retained<Plate>, Retained<NSTextField>) {
    let well = Plate::new(frame, 10.0, mtm);
    well.set_fill(Some(NSColor::colorWithWhite_alpha(0.0, 0.25)));
    well.set_ring(Some((white(0.12), 1.0)));
    let font = weight(size, w);
    let field = NSTextField::new(mtm);
    field.setBezeled(false);
    field.setBordered(false);
    field.setDrawsBackground(false);
    field.setEditable(true);
    field.setSelectable(true);
    field.setFocusRingType(NSFocusRingType::None);
    field.setAlignment(NSTextAlignment::Center);
    field.setFont(Some(&font));
    field.setTextColor(Some(&NSColor::whiteColor()));
    field.setPlaceholderString(Some(&ns(placeholder)));
    if let Some(cell) = field.cell() {
        cell.setUsesSingleLineMode(true);
        cell.setScrollable(true);
    }
    let h = line_height(&font) + 2.0;
    field.setFrame(rect(10.0, ((frame.size.height - h) / 2.0).round() + 1.0, frame.size.width - 20.0, h));
    field.setAutoresizingMask(Mask::ViewWidthSizable);
    well.addSubview(&field);
    parent.addSubview(&well);
    (well, field)
}

/// The well's focused look (`.field:focus`): a 1 pt blue ring with a 3 pt glow at 45%.
pub(crate) fn focus_well(well: &Plate, on: bool) {
    well.set_ring(Some(if on { (blue(), 1.5) } else { (white(0.12), 1.0) }));
}
