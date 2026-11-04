//! Small AppKit helpers and Vella's visual constants.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSButton, NSColor, NSFont, NSFontWeightMedium,
    NSImage, NSImageSymbolConfiguration, NSMenu, NSMenuItem, NSTextField, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// How every always-visible overlay (the island, its click target, the question card, the speaker prompt) joins
/// Spaces: on every Space including other apps' full-screen Spaces (`CanJoinAllApplications` is AppKit's flag for
/// "floating windows and system overlays" there; `FullScreenAuxiliary` alone covers only our own full-screen
/// windows), unmoved by Mission Control, out of the window cycle. One definition, so the surfaces cannot drift.
pub fn overlay_behavior() -> objc2_app_kit::NSWindowCollectionBehavior {
    use objc2_app_kit::NSWindowCollectionBehavior as B;
    B::CanJoinAllSpaces | B::CanJoinAllApplications | B::Stationary | B::FullScreenAuxiliary | B::IgnoresCycle
}

/// The display the user is on: the one under the pointer (`NSMouseInRect`'s convention for a point on a shared edge:
/// x in [minX, maxX), y in (minY, maxY]), else the nearest to it. `None` only with no displays.
pub fn screen_under(frames: &[NSRect], pointer: NSPoint) -> Option<usize> {
    let inside = |r: &NSRect| {
        pointer.x >= r.origin.x
            && pointer.x < r.origin.x + r.size.width
            && pointer.y > r.origin.y
            && pointer.y <= r.origin.y + r.size.height
    };
    if let Some(i) = frames.iter().position(inside) {
        return Some(i);
    }
    let distance = |r: &NSRect| {
        let dx = (r.origin.x - pointer.x).max(pointer.x - (r.origin.x + r.size.width)).max(0.0);
        let dy = (r.origin.y - pointer.y).max(pointer.y - (r.origin.y + r.size.height)).max(0.0);
        dx * dx + dy * dy
    };
    (0..frames.len()).min_by(|&a, &b| distance(&frames[a]).total_cmp(&distance(&frames[b])))
}

/// The screen under the pointer now (see `screen_under`), else the main screen.
pub fn pointer_screen(mtm: MainThreadMarker) -> Option<Retained<objc2_app_kit::NSScreen>> {
    let screens = objc2_app_kit::NSScreen::screens(mtm);
    let frames: Vec<NSRect> = screens.iter().map(|s| s.frame()).collect();
    match screen_under(&frames, objc2_app_kit::NSEvent::mouseLocation()) {
        Some(i) => Some(screens.objectAtIndex(i)),
        None => objc2_app_kit::NSScreen::mainScreen(mtm),
    }
}

// Vella's palette and type (Projects/Vella: AppDelegate menu header, ModelTable, TierControl).

/// Status line, healthy: `NSColor.systemGreen`.
pub fn green() -> Retained<NSColor> {
    NSColor::systemGreenColor()
}
/// Status line, needs attention: `NSColor.systemOrange`.
pub fn orange() -> Retained<NSColor> {
    NSColor::systemOrangeColor()
}
pub fn secondary() -> Retained<NSColor> {
    NSColor::secondaryLabelColor()
}
pub fn tertiary() -> Retained<NSColor> {
    NSColor::tertiaryLabelColor()
}
pub fn label() -> Retained<NSColor> {
    NSColor::labelColor()
}
/// Vella's bolt blue (TierControl.boltTint): Heimdall's bracketed action notes.
pub fn note_blue() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(142.0 / 255.0, 175.0 / 255.0, 246.0 / 255.0, 1.0)
}
/// The Models table's hairline: the separator colour at 35 %.
pub fn hairline() -> Retained<NSColor> {
    NSColor::separatorColor().colorWithAlphaComponent(0.35)
}

pub fn menu_font() -> Retained<NSFont> {
    NSFont::menuFontOfSize(0.0)
}
pub fn font(size: f64) -> Retained<NSFont> {
    NSFont::systemFontOfSize(size)
}
pub fn medium(size: f64) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, unsafe { NSFontWeightMedium })
}
pub fn digits(size: f64) -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(size, 0.0)
}

pub fn symbol(name: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None)
}

/// An SF Symbol at a point size and weight, as a template image.
pub fn symbol_sized(name: &str, size: f64, weight: f64) -> Option<Retained<NSImage>> {
    let image = symbol(name)?;
    let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight);
    image.imageWithSymbolConfiguration(&config)
}

pub fn attributed(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    unsafe {
        let keys: [&NSString; 2] = [objc2_app_kit::NSFontAttributeName, objc2_app_kit::NSForegroundColorAttributeName];
        let values: [&AnyObject; 2] = [font.as_ref(), color.as_ref()];
        let attrs = NSDictionary::from_slices(&keys, &values);
        NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &ns(text), Some(&attrs))
    }
}

pub fn text_width(text: &str, font: &NSFont) -> f64 {
    attributed(text, font, &label()).size().width
}

/// A one-line label.
pub fn text_label(text: &str, font: &NSFont, color: &NSColor, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&ns(text), mtm);
    field.setFont(Some(font));
    field.setTextColor(Some(color));
    field
}

/// A label sized to its text, placed with its baseline area at `(x, y)` in a flipped parent.
pub fn place_label(
    parent: &NSView,
    text: &str,
    font: &NSFont,
    color: &NSColor,
    x: f64,
    y: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = text_label(text, font, color, mtm);
    field.sizeToFit();
    let size = field.frame().size;
    field.setFrameOrigin(NSPoint::new(x, y));
    let _ = size;
    parent.addSubview(&field);
    field
}

/// A plain menu item. Vella's `item(_:_:_:)`: title, SF Symbol, action, key equivalent.
pub fn menu_item(
    title: &str,
    icon: Option<&str>,
    action: Option<Sel>,
    key: &str,
    target: Option<&AnyObject>,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), action, &ns(key))
    };
    unsafe { item.setTarget(target) };
    if let Some(icon) = icon {
        item.setImage(symbol(icon).as_deref());
    }
    item
}

/// A disabled grey line (Vella's fact line and captions).
pub fn caption_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let item = menu_item(title, None, None, "", None, mtm);
    item.setEnabled(false);
    item
}

/// macOS 27 hides menu-item SF Symbols for apps linked against SDK 26+ unless `preferredImageVisibility` is
/// visible (1). Set it through KVC on every image-bearing item, submenus included (V-family rule, 6 Oct).
pub fn show_menu_images(menu: &NSMenu) {
    let sel = objc2::sel!(setPreferredImageVisibility:);
    for item in menu.itemArray().iter() {
        if item.image().is_some() && item.respondsToSelector(sel) {
            let one = NSNumber::numberWithInteger(1);
            unsafe {
                let _: () = msg_send![&*item, setValue: &*one, forKey: &*ns("preferredImageVisibility")];
            }
        }
        if let Some(sub) = item.submenu() {
            show_menu_images(&sub);
        }
    }
}

/// NSAlert shows the app icon, as Vella's alerts do. The unbundled binary (the demo) has none, so `symbol` stands in:
/// white, with its badge in Vella's blue.
pub fn alert_icon(alert: &objc2_app_kit::NSAlert, symbol_name: &str) {
    let bundle = objc2_foundation::NSBundle::mainBundle();
    let bundled = bundle.objectForInfoDictionaryKey(&ns("CFBundleIconFile")).is_some()
        || bundle.objectForInfoDictionaryKey(&ns("CFBundleIconName")).is_some();
    if bundled {
        return;
    }
    let Some(image) = symbol(symbol_name) else { return };
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(44.0, 0.0);
    let colours = NSImageSymbolConfiguration::configurationWithPaletteColors(&objc2_foundation::NSArray::from_retained_slice(&[
        note_blue(),
        NSColor::colorWithWhite_alpha(1.0, 0.85),
    ]));
    let config = size.configurationByApplyingConfiguration(&colours);
    if let Some(image) = image.imageWithSymbolConfiguration(&config) {
        unsafe { alert.setIcon(Some(&image)) };
    }
}

/// A push button with Vella's (native) look.
pub fn push_button(
    title: &str,
    target: &AnyObject,
    action: Sel,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let button = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(action), mtm) };
    button.setFont(Some(&font(13.0)));
    button
}

/// A 1 pt hairline across a flipped parent at `y`.
pub fn hairline_view(parent: &NSView, x: f64, y: f64, width: f64, mtm: MainThreadMarker) {
    let line = Hairline::new(rect(x, y, width, 1.0), mtm);
    parent.addSubview(&line);
}

define_class!(
    /// An NSView whose y axis runs down, so rows lay out top to bottom.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallFlippedView"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    pub fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

define_class!(
    /// The Models table's divider: the separator colour at 35 %.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallHairline"]
    pub struct Hairline;

    impl Hairline {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            hairline().setFill();
            objc2_app_kit::NSRectFillUsingOperation(self.bounds(), objc2_app_kit::NSCompositingOperation::SourceOver);
        }
    }
);

/// Makes a view act on the first click in a window that is not key (the speaker popup never takes focus). The
/// view is re-classed to a runtime subclass of its own class, as KVO does, so private classes such as NSAlert's
/// buttons keep all their behaviour and only gain the override.
pub fn accept_first_click(view: &NSView) {
    use objc2::runtime::{AnyClass, Bool, ClassBuilder};
    unsafe extern "C-unwind" fn yes(_this: *mut AnyObject, _sel: Sel, _event: *mut AnyObject) -> Bool {
        Bool::YES
    }
    let original = view.class();
    let name = format!("HeimdallFirstClick_{}", original.name().to_string_lossy());
    let cname = std::ffi::CString::new(name).expect("class name");
    let class: &AnyClass = match AnyClass::get(&cname) {
        Some(class) => class,
        None => {
            let Some(mut builder) = ClassBuilder::new(&cname, original) else { return };
            unsafe {
                builder.add_method(
                    objc2::sel!(acceptsFirstMouse:),
                    yes as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
                )
            };
            builder.register()
        }
    };
    unsafe { objc2::ffi::object_setClass(view as *const NSView as *mut AnyObject, class) };
}

impl Hairline {
    pub fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::NSWindowCollectionBehavior as B;

    // Fake display lists in AppKit's global coordinates (y up, origin at the primary display's bottom-left).
    const BUILT_IN: (f64, f64, f64, f64) = (0.0, 0.0, 1728.0, 1117.0);
    const RIGHT: (f64, f64, f64, f64) = (1728.0, -143.0, 2560.0, 1440.0);
    const ABOVE: (f64, f64, f64, f64) = (-96.0, 1117.0, 1920.0, 1080.0);

    fn frames(list: &[(f64, f64, f64, f64)]) -> Vec<NSRect> {
        list.iter().map(|&(x, y, w, h)| rect(x, y, w, h)).collect()
    }

    #[test]
    fn the_island_goes_to_the_display_under_the_pointer_not_the_default_one() {
        // The built-in (notched) display first, as NSScreen lists the primary display; the pointer on the right one.
        let two = frames(&[BUILT_IN, RIGHT]);
        assert_eq!(screen_under(&two, NSPoint::new(2500.0, 700.0)), Some(1));
        assert_eq!(screen_under(&two, NSPoint::new(400.0, 700.0)), Some(0));
        // Order does not matter: the external display listed first.
        let swapped = frames(&[RIGHT, BUILT_IN]);
        assert_eq!(screen_under(&swapped, NSPoint::new(400.0, 700.0)), Some(1));
        // A display above, the pointer at its top edge (the menu bar row).
        let stacked = frames(&[BUILT_IN, ABOVE]);
        assert_eq!(screen_under(&stacked, NSPoint::new(500.0, 2197.0)), Some(1));
        // One display: always it.
        assert_eq!(screen_under(&frames(&[BUILT_IN]), NSPoint::new(10.0, 10.0)), Some(0));
        assert_eq!(screen_under(&[], NSPoint::new(10.0, 10.0)), None);
    }

    #[test]
    fn a_pointer_on_a_shared_edge_or_off_every_display_still_picks_one() {
        let two = frames(&[BUILT_IN, RIGHT]);
        // x = 1728 is the right display's first column (NSMouseInRect: x in [minX, maxX)).
        assert_eq!(screen_under(&two, NSPoint::new(1728.0, 500.0)), Some(1));
        assert_eq!(screen_under(&two, NSPoint::new(1727.5, 500.0)), Some(0));
        // y = 1117 is the built-in's top row, not the display above it (y in (minY, maxY]).
        let stacked = frames(&[BUILT_IN, ABOVE]);
        assert_eq!(screen_under(&stacked, NSPoint::new(500.0, 1117.0)), Some(0));
        assert_eq!(screen_under(&stacked, NSPoint::new(500.0, 1117.5)), Some(1));
        // In the gap below the built-in, beside the taller right display: the nearest display.
        assert_eq!(screen_under(&two, NSPoint::new(1700.0, -100.0)), Some(1));
        assert_eq!(screen_under(&two, NSPoint::new(100.0, -5.0)), Some(0));
    }

    #[test]
    fn overlays_join_every_space_including_other_apps_full_screen_spaces() {
        let b = overlay_behavior();
        for flag in [B::CanJoinAllSpaces, B::CanJoinAllApplications, B::FullScreenAuxiliary, B::Stationary, B::IgnoresCycle] {
            assert!(b.contains(flag), "{flag:?} missing");
        }
        // AppKit takes at most one of each group; a second one raises at runtime.
        assert!(!b.contains(B::MoveToActiveSpace), "CanJoinAllSpaces and MoveToActiveSpace are exclusive");
        assert!(!b.intersects(B::Primary | B::Auxiliary), "CanJoinAllApplications excludes Primary and Auxiliary");
        assert!(!b.intersects(B::Managed | B::Transient), "Stationary excludes Managed and Transient");
        assert!(!b.intersects(B::FullScreenPrimary | B::FullScreenNone));
    }
}
