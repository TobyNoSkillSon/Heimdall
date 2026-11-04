//! Menu controls that act without closing the menu: the switch tiles at the top and a native-looking
//! check row (Launch at Login). A view inside an NSMenuItem receives the pointer's clicks itself, so the menu keeps
//! tracking; the view then calls the controller in Rust and redraws in place. Arrow keys highlight these rows like
//! native ones. On macOS 27 a view item gets nothing on Return (no item action, no key event to the view, no local
//! monitor call while the menu tracks), so these rows toggle by pointer and accessibility press only.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAccessibilityCheckBoxRole, NSBezierPath, NSColor, NSEvent, NSFont, NSFontWeightMedium, NSImageSymbolConfiguration,
    NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_app_kit::NSAttributedStringNSStringDrawing as _;
use objc2_foundation::{MainThreadMarker, NSArray, NSCopying, NSNumber, NSPoint, NSRect, NSSize, NSString};

use super::util::{attributed, label, menu_font, ns, rect, symbol, symbol_sized};
use crate::model::{Tile, TileLook};

// Tile geometry, in points. The block matches the native rows' content inset (separators run 16 pt in from both
// edges). Two rows, Control Center style: the passive switches share the top row, the active three the bottom
// row, each row filling the menu's width; every tile is the same height.
const TILE_ROWS: f64 = 2.0;
const TILE_TOP: f64 = 6.0;
const TILE_HEIGHT: f64 = 58.0;
const TILE_GAP: f64 = 8.0;
const TILE_INSET: f64 = 16.0;
const TILE_RADIUS: f64 = 12.0;
const TILE_ROW_HEIGHT: f64 = 2.0 * TILE_TOP + TILE_ROWS * TILE_HEIGHT + (TILE_ROWS - 1.0) * TILE_GAP;

/// The menu's width before AppKit stretches the block to the widest item: three bottom tiles of 88 pt.
pub const TILE_ROW_WIDTH: f64 = 3.0 * 88.0 + 2.0 * TILE_GAP + 2.0 * TILE_INSET;

pub struct TileIvars {
    tile: Cell<usize>,
    look: Cell<TileLook>,
    /// False while Listening is off: Seeing, Speaking and Agent do nothing without it.
    enabled: Cell<bool>,
    hover: Cell<bool>,
    pressed: Cell<bool>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    /// While loading: the native spinner in the glyph's place (a ring when the progress is known).
    spinner: RefCell<Option<Retained<objc2_app_kit::NSProgressIndicator>>>,
}

define_class!(
    /// One switch: a rounded box, glyph over label; accent blue with white ink when on, a quiet fill when off. The
    /// Engine tile has three more looks, never blended ("its states must be obvious at a glance"): on its
    /// way is the blue with a native spinner in the glyph's place (a filling ring when Splash reports progress), on
    /// with no model loaded the soft blue the Engine section shares, failed solid red with a warning glyph.
    /// Unavailable (Listening off), a tile keeps showing its saved state, faded, and takes no press or hover.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallTile"]
    #[ivars = TileIvars]
    pub struct TileView;

    impl TileView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        /// Acts on press: an open menu swallows the first mouse-up after it opens, so a
        /// release-driven toggle silently did nothing on the first click.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().pressed.set(false);
            self.setNeedsDisplay(true);
            if self.is_enabled() && self.contains(event) {
                self.press();
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, _event: &NSEvent) {}

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {}

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hover.set(self.is_enabled());
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hover.set(false);
            self.ivars().pressed.set(false);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            track_hover(self, &self.ivars().tracking);
            unsafe { msg_send![super(self), updateTrackingAreas] }
        }

        // VoiceOver and accessibility clients see checkboxes (Starting is mixed); AXPress toggles like a click.
        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Retained<NSString> {
            unsafe { NSAccessibilityCheckBoxRole }.copy()
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Retained<NSString> {
            ns(self.tile().title())
        }

        #[unsafe(method_id(accessibilityValue))]
        fn ax_value(&self) -> Retained<AnyObject> {
            // On its way reads as mixed; on, waiting and failed are all asked for (a press turns them off).
            let value = match self.ivars().look.get() {
                TileLook::On | TileLook::Waiting | TileLook::Problem => 1,
                TileLook::Loading(_) => 2,
                TileLook::Off => 0,
            };
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::numberWithInt(value))))
        }

        #[unsafe(method(isAccessibilityEnabled))]
        fn ax_enabled(&self) -> bool {
            self.is_enabled()
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn ax_press(&self) -> Bool {
            if !self.is_enabled() {
                return Bool::NO;
            }
            self.press();
            Bool::YES
        }
    }
);

impl TileView {
    pub fn new(tile: usize, look: TileLook, enabled: bool, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TileIvars {
            tile: Cell::new(tile),
            look: Cell::new(look),
            enabled: Cell::new(enabled),
            hover: Cell::new(false),
            pressed: Cell::new(false),
            tracking: RefCell::new(None),
            spinner: RefCell::new(None),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 64.0, TILE_HEIGHT)] };
        this.sync_spinner();
        this
    }

    /// The spinner while the look is Loading, gone otherwise (nothing animates at rest).
    fn sync_spinner(&self) {
        let look = self.ivars().look.get();
        let mut slot = self.ivars().spinner.borrow_mut();
        let TileLook::Loading(percent) = look else {
            if let Some(old) = slot.take() { unsafe { old.stopAnimation(None) }; old.removeFromSuperview(); }
            return;
        };
        let spinner = slot.get_or_insert_with(|| {
            let s = spinner(GLYPH_SPIN, false, self.mtm());
            s.setHidden(STILL_SPINNERS.with(|c| c.get()));
            s.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxXMargin);
            self.addSubview(&s);
            s
        });
        set_progress(spinner, percent.map(|p| f64::from(p) / 100.0));
        let mid = self.bounds().size.width / 2.0;
        spinner.setFrame(rect((mid - GLYPH_SPIN / 2.0).round(), (GLYPH_Y - GLYPH_SPIN / 2.0).round(), GLYPH_SPIN, GLYPH_SPIN));
    }

    /// The live spinner (checks and renders).
    pub fn spinner(&self) -> Option<Retained<objc2_app_kit::NSProgressIndicator>> {
        self.ivars().spinner.borrow().clone()
    }

    pub fn tile(&self) -> Tile {
        Tile::ALL[self.ivars().tile.get()]
    }

    pub fn is_on(&self) -> bool {
        self.ivars().look.get() == TileLook::On
    }

    pub fn look(&self) -> TileLook {
        self.ivars().look.get()
    }

    pub fn is_enabled(&self) -> bool {
        self.ivars().enabled.get()
    }

    /// Updates the state in place (the menu may be open).
    pub fn set_look(&self, look: TileLook) {
        if self.ivars().look.replace(look) != look {
            self.sync_spinner();
            self.setNeedsDisplay(true);
        }
    }

    /// Makes the tile available or not in place; an unavailable tile drops any hover it had.
    pub fn set_enabled(&self, enabled: bool) {
        if self.ivars().enabled.replace(enabled) != enabled {
            if !enabled {
                self.ivars().hover.set(false);
                self.ivars().pressed.set(false);
            }
            self.setNeedsDisplay(true);
        }
    }

    fn contains(&self, event: &NSEvent) -> bool {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        let b = self.bounds();
        p.x >= 0.0 && p.y >= 0.0 && p.x <= b.size.width && p.y <= b.size.height
    }

    fn press(&self) {
        super::tile_clicked(self.ivars().tile.get());
    }

    fn draw(&self) {
        let ivars = self.ivars();
        let look = ivars.look.get();
        let on = look == TileLook::On;
        let enabled = ivars.enabled.get();
        let bounds = self.bounds();
        // Unavailable: the whole tile (fill, label and glyph together) at one opacity, so blue still reads as its saved
        // On while plainly not working, in the light and the dark menu alike. One transparency layer, so overlaps
        // never darken.
        let faded = (!enabled).then(|| objc2_app_kit::NSGraphicsContext::currentContext()).flatten().map(|c| c.CGContext());
        if let Some(cg) = &faded {
            objc2_core_graphics::CGContext::set_alpha(Some(cg), UNAVAILABLE_ALPHA);
            unsafe { objc2_core_graphics::CGContext::begin_transparency_layer(Some(cg), None) };
        }
        // Loading is not yet on (review 30 Oct): the off fill with the spinner in the glyph's place, a filling ring when
        // Splash reports progress; full blue is ready. No part-blue fill: a split tile read as a glitch.
        let fill = match look {
            TileLook::On => NSColor::controlAccentColor(),
            TileLook::Loading(_) => NSColor::quaternaryLabelColor(),
            TileLook::Waiting => waiting_fill(),
            TileLook::Problem => NSColor::systemRedColor(),
            TileLook::Off => NSColor::quaternaryLabelColor(),
        };
        let _ = on;
        // Hover lifts the box a little, pressing sinks it; both stay within the same hue.
        let fill = match (enabled && ivars.pressed.get(), enabled && ivars.hover.get()) {
            (true, _) => fill.blendedColorWithFraction_ofColor(0.18, &NSColor::blackColor()).unwrap_or(fill),
            (false, true) => fill.blendedColorWithFraction_ofColor(0.12, &NSColor::whiteColor()).unwrap_or(fill),
            _ => fill,
        };
        fill.setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, TILE_RADIUS, TILE_RADIUS).fill();

        let ink = match look {
            TileLook::On | TileLook::Problem => NSColor::whiteColor(),
            TileLook::Loading(_) => label(),
            TileLook::Waiting | TileLook::Off => label(),
        };
        let glyph_ink = ink.clone();
        let title = self.tile().title();
        let text = attributed(title, &NSFont::systemFontOfSize_weight(11.0, unsafe { NSFontWeightMedium }), &ink);
        let size = text.size();
        let mid = bounds.size.width / 2.0;
        text.drawAtPoint(NSPoint::new((mid - size.width / 2.0).round(), bounds.size.height - 10.0 - size.height));
        // Loading: the spinner stands in the glyph's place. Failed: the warning glyph, so red never reads as a colour
        // choice.
        let glyph = match look {
            TileLook::Loading(_) => None,
            TileLook::Problem => Some("exclamationmark.triangle"),
            _ => Some(self.tile().symbol()),
        };
        if let Some(image) = glyph.and_then(|g| tile_glyph(g, &glyph_ink)) {
            let s = image.size();
            image.drawInRect(rect((mid - s.width / 2.0).round(), (GLYPH_Y - s.height / 2.0).round(), s.width, s.height));
        }
        // Offscreen renders draw the spinner's own frame: its layer is not captured with the tile.
        if let Some(sp) = self.ivars().spinner.borrow().as_ref() { draw_spinner_still(sp); }
        if let Some(cg) = &faded {
            objc2_core_graphics::CGContext::end_transparency_layer(Some(cg));
            objc2_core_graphics::CGContext::set_alpha(Some(cg), 1.0);
        }
    }
}

/// An unavailable tile's opacity, the whole tile at once.
const UNAVAILABLE_ALPHA: f64 = 0.5;
/// Where a tile's glyph (or its spinner) is centred, from the top, and the spinner's size.
const GLYPH_Y: f64 = 22.0;
const GLYPH_SPIN: f64 = 18.0;

/// The Engine's "on, waiting for a model" fill: the accent, soft. The Engine section's header and rows use the same.
pub(crate) fn waiting_fill() -> Retained<NSColor> {
    NSColor::controlAccentColor().colorWithAlphaComponent(0.42)
}

/// A native spinner `size` points square: indeterminate and spinning, or a filling ring for a known progress
/// (`set_progress`). `light`: drawn light, for a blue fill.
pub(crate) fn spinner(size: f64, light: bool, mtm: objc2_foundation::MainThreadMarker) -> Retained<objc2_app_kit::NSProgressIndicator> {
    use objc2::MainThreadOnly as _;
    let s = objc2_app_kit::NSProgressIndicator::initWithFrame(objc2_app_kit::NSProgressIndicator::alloc(mtm), rect(0.0, 0.0, size, size));
    s.setStyle(objc2_app_kit::NSProgressIndicatorStyle::Spinning);
    s.setControlSize(if size < 15.0 { objc2_app_kit::NSControlSize::Mini } else { objc2_app_kit::NSControlSize::Small });
    s.setDisplayedWhenStopped(true);
    s.setMinValue(0.0);
    s.setMaxValue(1.0);
    if light {
        let dark = unsafe { objc2_app_kit::NSAppearanceNameDarkAqua };
        use objc2_app_kit::NSAppearanceCustomization as _;
        s.setAppearance(objc2_app_kit::NSAppearance::appearanceNamed(dark).as_deref());
    }
    s
}

/// A spinner's progress: `None` spins (an honest spinner: nothing is known), `Some` fills its ring.
pub(crate) fn set_progress(s: &objc2_app_kit::NSProgressIndicator, progress: Option<f64>) {
    match progress {
        Some(p) => {
            unsafe { s.stopAnimation(None) };
            s.setIndeterminate(false);
            s.setDoubleValue(p.clamp(0.0, 1.0));
        }
        None => {
            s.setIndeterminate(true);
            unsafe { s.startAnimation(None) };
        }
    }
}

thread_local! {
    /// Offscreen renders only: spinners draw a still frame of themselves (a capture does not see their layer).
    pub(crate) static STILL_SPINNERS: Cell<bool> = const { Cell::new(false) };
}

/// A spinner's still frame for renders: a three-quarter arc (spinning) or the ring filled to its value, in its ink.
pub(crate) fn draw_spinner_still(s: &objc2_app_kit::NSProgressIndicator) {
    if !STILL_SPINNERS.with(|c| c.get()) { return; }
    let f = s.frame();
    use objc2_app_kit::NSAppearanceCustomization as _;
    let light = s.appearance().is_some();
    let ink = if light { NSColor::whiteColor() } else { NSColor::secondaryLabelColor() };
    let (cx, cy, r) = (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0, f.size.width / 2.0 - 1.5);
    let track = NSBezierPath::bezierPath();
    track.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle(NSPoint::new(cx, cy), r, 0.0, 360.0);
    track.setLineWidth(2.0);
    ink.colorWithAlphaComponent(0.25).setStroke();
    track.stroke();
    let sweep = if s.isIndeterminate() { 270.0 } else { 360.0 * s.doubleValue() };
    if sweep <= 0.0 { return; }
    let arc = NSBezierPath::bezierPath();
    // Flipped views: clockwise from 12 o'clock.
    arc.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(NSPoint::new(cx, cy), r, -90.0, -90.0 + sweep, false);
    // (In a flipped view, increasing angles run clockwise on screen.)
    arc.setLineWidth(2.0);
    arc.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
    ink.setStroke();
    arc.stroke();
}

/// The glyph at 18 pt, medium, in the tile's ink.
fn tile_glyph(name: &str, ink: &NSColor) -> Option<Retained<objc2_app_kit::NSImage>> {
    let image = symbol_sized(name, 18.0, unsafe { NSFontWeightMedium })?;
    let colours = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&[ink.retain()]));
    image.imageWithSymbolConfiguration(&colours)
}

define_class!(
    /// The tiles' block: lays each row's tiles out equally across whatever width the menu gives it.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallTileRow"]
    pub struct TileRow;

    impl TileRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, _old: NSSize) {
            self.layout_tiles();
        }
    }
);

impl TileRow {
    pub fn new(tiles: &[Retained<TileView>], mtm: MainThreadMarker) -> Retained<Self> {
        let this: Retained<Self> =
            unsafe { msg_send![Self::alloc(mtm), initWithFrame: rect(0.0, 0.0, TILE_ROW_WIDTH, TILE_ROW_HEIGHT)] };
        this.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        for tile in tiles {
            this.addSubview(tile);
        }
        this.layout_tiles();
        this
    }

    fn layout_tiles(&self) {
        let width = self.bounds().size.width;
        let tiles: Vec<Retained<TileView>> = self.subviews().iter().filter_map(|v| v.downcast::<TileView>().ok()).collect();
        for row in 0..TILE_ROWS as usize {
            let in_row: Vec<_> = tiles.iter().filter(|t| t.tile().row() == row).collect();
            let n = in_row.len() as f64;
            if n == 0.0 {
                continue;
            }
            // Whole points; the last tile takes the remainder, so both rows end on the same edge.
            let tile = ((width - 2.0 * TILE_INSET - (n - 1.0) * TILE_GAP) / n).floor();
            let y = TILE_TOP + row as f64 * (TILE_HEIGHT + TILE_GAP);
            for (i, view) in in_row.iter().enumerate() {
                let x = TILE_INSET + i as f64 * (tile + TILE_GAP);
                let w = if i + 1 == in_row.len() { width - TILE_INSET - x } else { tile };
                view.setFrame(rect(x, y, w, TILE_HEIGHT));
            }
        }
    }
}

// Native row geometry on macOS 27, measured from the real menu (lab/ui-renders/menu-keyboard-*.png): 24 pt rows;
// with no native checkmark item the menu has no state column, so the symbol column is centred at x 22.25 and
// titles start at x 39. The keyboard highlight is the accent colour, the full row tall, 5 pt in from each edge,
// with 6 pt corners. The state sits on the right as a small switch, 16 pt in, where chevrons end: accent with the
// knob right when on, a quiet track with the knob left when off (the tiles' colours). NSSwitch is not used: in a
// menu its window is never key, so it draws On as a grey track that reads as Off.
const ROW_HEIGHT: f64 = 24.0;
const ICON_X: f64 = 22.25;
const TITLE_X: f64 = 39.0;
const HIGHLIGHT_INSET: f64 = 5.0;
const HIGHLIGHT_RADIUS: f64 = 6.0;
const SWITCH_RIGHT: f64 = 16.0;
const SWITCH_WIDTH: f64 = 26.0;
const SWITCH_HEIGHT: f64 = 15.0;

pub struct CheckRowIvars {
    row: Cell<usize>,
    on: Cell<bool>,
    title: RefCell<String>,
    icon: RefCell<Option<String>>,
    hover: Cell<bool>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    /// [menu lane] A section's disclosure (Add Provider): a chevron where submenu arrows sit instead of the switch.
    chevron: Cell<bool>,
    /// Greyed and inert (Background Tasks in emergency mode): no highlight, clicks and presses do nothing.
    dimmed: Cell<bool>,
    /// No control drawn at all: dimmed rows standing in for rows whose controls are elsewhere (climb 2).
    plain: Cell<bool>,
    /// A section header's live state at its trailing edge, before the chevron (the Engine's "Ready", "Loading").
    status: RefCell<Option<(String, StatusTone)>>,
    /// Beside the status while something is under way: a native spinner, or its ring when the progress is known.
    spinner: RefCell<Option<Retained<objc2_app_kit::NSProgressIndicator>>>,
}

/// A disclosure's round chip, in points.
const CHIP: f64 = 16.0;

/// A header status's colour: grey (plain facts), the soft accent (on, waiting for the user), red (failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone { Grey, Accent, Alert, Warning }

impl StatusTone {
    pub(crate) fn color(self) -> Retained<NSColor> {
        match self {
            StatusTone::Grey => NSColor::secondaryLabelColor(),
            StatusTone::Accent => NSColor::controlAccentColor(),
            StatusTone::Alert => NSColor::systemRedColor(),
            // Emergency mode's orange (the island's token, the status line's problems).
            StatusTone::Warning => NSColor::systemOrangeColor(),
        }
    }
}

/// A header's status column ends here, from the row's right edge (the chevron's own column is to its right).
pub(crate) const STATUS_RIGHT: f64 = 38.0;

thread_local! {
    /// Where the native rows' values end, from the right edge (`menu::set_value`, measured with AppKit): headers put
    /// their status on the same column (climb 2), so every value in the menu lines up.
    static VALUE_RIGHT: Cell<f64> = const { Cell::new(0.0) };
}

pub(crate) fn set_value_column(right: f64) {
    VALUE_RIGHT.with(|v| v.set(right));
}
const STATUS_SPIN: f64 = 14.0;

define_class!(
    /// A menu row drawn like a native item (symbol, title, highlight on hover or keyboard focus) with a small switch
    /// on the right (or, as a disclosure, a chevron pointing down while open); a click anywhere on it toggles in place.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallCheckRow"]
    #[ivars = CheckRowIvars]
    pub struct CheckRow;

    impl CheckRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        /// The whole row is the click target.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let local = match unsafe { self.superview() } {
                Some(parent) => self.convertPoint_fromView(point, Some(&parent)),
                None => point,
            };
            let b = self.bounds();
            if local.x >= 0.0 && local.y >= 0.0 && local.x <= b.size.width && local.y <= b.size.height {
                self as *const Self as *mut NSView
            } else {
                std::ptr::null_mut()
            }
        }

        /// Taken here, so the menu never starts its own click on the item (which would close it on release).
        /// Acts on press: the menu can swallow the first mouse-up after it opens. The rest of the press is this
        /// row's too (`finish_press`).
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let b = self.bounds();
            hover_from(self, &self.ivars().hover, event);
            if p.x >= 0.0 && p.y >= 0.0 && p.x <= b.size.width && p.y <= b.size.height && !self.ivars().dimmed.get() {
                super::check_row_clicked(self.ivars().row.get());
            }
            finish_press(self, &self.ivars().hover);
        }

        /// A held press carried out of the row drops its highlight; carried back, it returns.
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            hover_from(self, &self.ivars().hover, event);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            hover_from(self, &self.ivars().hover, event);
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            set_hover(self, &self.ivars().hover, true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            set_hover(self, &self.ivars().hover, false);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            track_hover(self, &self.ivars().tracking);
            unsafe { msg_send![super(self), updateTrackingAreas] }
        }

        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Retained<NSString> {
            if self.ivars().chevron.get() { unsafe { objc2_app_kit::NSAccessibilityDisclosureTriangleRole }.copy() } else { unsafe { NSAccessibilityCheckBoxRole }.copy() }
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Retained<NSString> {
            ns(&self.ivars().title.borrow())
        }

        #[unsafe(method_id(accessibilityValue))]
        fn ax_value(&self) -> Retained<AnyObject> {
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::numberWithBool(self.ivars().on.get()))))
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn ax_press(&self) -> Bool {
            if self.ivars().dimmed.get() { return Bool::NO }
            super::check_row_clicked(self.ivars().row.get());
            Bool::YES
        }

        #[unsafe(method(isAccessibilityEnabled))]
        fn ax_enabled(&self) -> bool {
            !self.ivars().dimmed.get()
        }
    }
);

impl CheckRow {
    /// `row` identifies the row to `check_row_clicked`; `width` is the title's room (the menu may widen it).
    pub fn new(row: usize, title: &str, icon: Option<&str>, on: bool, mtm: MainThreadMarker) -> Retained<Self> {
        let width = TITLE_X + super::util::text_width(title, &menu_font()) + 24.0 + SWITCH_WIDTH + SWITCH_RIGHT;
        let this = Self::alloc(mtm).set_ivars(CheckRowIvars {
            row: Cell::new(row),
            on: Cell::new(on),
            title: RefCell::new(title.into()),
            icon: RefCell::new(icon.map(Into::into)),
            hover: Cell::new(false),
            tracking: RefCell::new(None),
            chevron: Cell::new(false),
            plain: Cell::new(false),
            dimmed: Cell::new(false),
            status: RefCell::new(None),
            spinner: RefCell::new(None),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, width, ROW_HEIGHT)] };
        this.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }

    /// [menu lane] A collapsible section's header row ("Add Provider"): `open` points the chevron down.
    pub fn disclosure(row: usize, title: &str, icon: Option<&str>, open: bool, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::new(row, title, icon, open, mtm);
        this.ivars().chevron.set(true);
        this
    }

    pub fn is_on(&self) -> bool {
        self.ivars().on.get()
    }

    pub fn set_on(&self, on: bool) {
        if self.ivars().on.replace(on) != on {
            self.setNeedsDisplay(true);
        }
    }

    /// Greyed and inert (`dimmed`), or back to normal.
    pub fn set_dimmed(&self, dimmed: bool) {
        if self.ivars().dimmed.replace(dimmed) != dimmed {
            self.setNeedsDisplay(true);
        }
    }

    /// The row draws no switch or chevron (`plain`).
    pub fn set_plain(&self, plain: bool) {
        if self.ivars().plain.replace(plain) != plain { self.setNeedsDisplay(true); }
    }

    pub fn is_dimmed(&self) -> bool {
        self.ivars().dimmed.get()
    }

    /// A section header's live state in place: its text at the trailing edge in `tone`, and a spinner beside it while
    /// `progress` is `Some` (`None` inside: an honest spinner; `Some(f)`: a ring filled to `f`). Nothing animates
    /// without one.
    pub fn set_status(&self, status: Option<(&str, StatusTone)>, progress: Option<Option<f64>>) {
        let next = status.map(|(t, tone)| (t.to_string(), tone));
        let text_w = next.as_ref().map_or(0.0, |(t, _)| super::util::text_width(t, &menu_font()).ceil());
        if *self.ivars().status.borrow() != next {
            *self.ivars().status.borrow_mut() = next;
            self.setNeedsDisplay(true);
        }
        // Room for the title, the status and its spinner.
        let title_w = super::util::text_width(&self.ivars().title.borrow(), &menu_font());
        let need = TITLE_X + title_w + 24.0 + text_w + if progress.is_some() { STATUS_SPIN + 6.0 } else { 0.0 } + self.status_right();
        // A header never widens the menu for its status (climb 2: one width in every state); its callers cut it to fit.
        if self.frame().size.width < need && !self.ivars().chevron.get() && !self.ivars().plain.get() { self.setFrameSize(NSSize::new(need.ceil(), self.frame().size.height)); }
        let mut slot = self.ivars().spinner.borrow_mut();
        let Some(progress) = progress else {
            if let Some(old) = slot.take() { unsafe { old.stopAnimation(None) }; old.removeFromSuperview(); }
            return;
        };
        let spinner = slot.get_or_insert_with(|| {
            let s = spinner(STATUS_SPIN, false, self.mtm());
            s.setHidden(STILL_SPINNERS.with(|c| c.get()));
            s.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin);
            self.addSubview(&s);
            s
        });
        set_progress(spinner, progress);
        let b = self.bounds();
        let x = b.size.width - self.status_right() - text_w - 6.0 - STATUS_SPIN;
        spinner.setFrame(rect(x.round(), ((b.size.height - STATUS_SPIN) / 2.0).round(), STATUS_SPIN, STATUS_SPIN));
    }

    /// Where the status ends from the right edge: on the native rows' value column (`set_value_column`) before a
    /// header's chevron, or before a switch.
    fn status_right(&self) -> f64 {
        // A plain row (emergency mode's stand-ins) puts its value in the native rows' value column too.
        if self.ivars().chevron.get() || self.ivars().plain.get() { VALUE_RIGHT.with(|v| v.get()).max(STATUS_RIGHT) } else { SWITCH_RIGHT + SWITCH_WIDTH + 10.0 }
    }

    /// The status text and tone (checks).
    pub fn status(&self) -> Option<(String, StatusTone)> {
        self.ivars().status.borrow().clone()
    }

    /// The header's spinner (checks).
    pub fn status_spinner(&self) -> Option<Retained<objc2_app_kit::NSProgressIndicator>> {
        self.ivars().spinner.borrow().clone()
    }

    /// The pointer over the row, for offscreen renders (no event reaches a row drawn off screen).
    pub(crate) fn hover_for_render(&self) {
        set_hover(self, &self.ivars().hover, true);
    }

    /// The menu highlights the item for keyboard focus; the pointer's highlight needs the pointer inside the row.
    pub(crate) fn highlighted(&self) -> bool {
        !self.ivars().dimmed.get() && (hovered(self, &self.ivars().hover) || self.enclosingMenuItem().is_some_and(|item| item.isHighlighted()))
    }

    fn draw(&self) {
        let ivars = self.ivars();
        let bounds = self.bounds();
        let highlighted = self.highlighted();
        if highlighted {
            NSColor::controlAccentColor().setFill();
            let r = rect(HIGHLIGHT_INSET, 0.0, bounds.size.width - 2.0 * HIGHLIGHT_INSET, bounds.size.height);
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, HIGHLIGHT_RADIUS, HIGHLIGHT_RADIUS).fill();
        }
        let dimmed = ivars.dimmed.get();
        // Full-strength text, as AppKit's own rows draw theirs through the menu's vibrancy: `labelColor` (85%) in a view
        // without vibrancy read dimmer than the native rows around it (climb 2: headers looked disabled).
        let ink = if highlighted { NSColor::whiteColor() } else if dimmed { NSColor::tertiaryLabelColor() } else { NSColor::textColor() };
        let font = menu_font();
        if let Some(icon) = ivars.icon.borrow().as_deref() {
            draw_symbol(icon, ICON_X, bounds.size.height / 2.0, &font, &ink);
        }
        let text = attributed(&ivars.title.borrow(), &font, &ink);
        let size = text.size();
        text.drawAtPoint(NSPoint::new(TITLE_X, ((bounds.size.height - size.height) / 2.0).round()));

        let on = ivars.on.get();
        if let Some((status, tone)) = ivars.status.borrow().as_ref() {
            // A dimmed row's value keeps its grey (climb 3 Nov round 4: paused rows' models were too dim to read); the
            // dimmed title and the header's Paused say the row is idle.
            let ink = if highlighted { NSColor::whiteColor() } else { tone.color() };
            let text = attributed(status, &font, &ink);
            let size = text.size();
            text.drawAtPoint(NSPoint::new((bounds.size.width - self.status_right() - size.width).round(), ((bounds.size.height - size.height) / 2.0).round()));
        }
        if let Some(sp) = ivars.spinner.borrow().as_ref() { draw_spinner_still(sp); }
        if ivars.plain.get() { return }
        if ivars.chevron.get() {
            // A disclosure, not a submenu arrow (review 30 Oct): a small chevron on a round chip, pointing right while
            // the section is folded and down while it is open (the macOS Wi-Fi menu's Other Networks), smaller and
            // greyer than the white arrows of the rows that open submenus.
            // Its right edge on the toggles' and separators' line (UX climb, 30 Oct).
            let (cx, cy) = (bounds.size.width - SWITCH_RIGHT - CHIP / 2.0, bounds.size.height / 2.0);
            let chip = if highlighted { NSColor::whiteColor().colorWithAlphaComponent(0.25) } else { NSColor::quaternaryLabelColor() };
            chip.setFill();
            NSBezierPath::bezierPathWithOvalInRect(rect(cx - CHIP / 2.0, cy - CHIP / 2.0, CHIP, CHIP)).fill();
            let ink = if highlighted { NSColor::whiteColor() } else { NSColor::secondaryLabelColor() };
            let small = super::util::font(8.0);
            draw_symbol_weighted(if on { "chevron.down" } else { "chevron.right" }, cx, cy, &small, &ink, unsafe { objc2_app_kit::NSFontWeightBold });
            return;
        }
        let track = rect(
            bounds.size.width - SWITCH_RIGHT - SWITCH_WIDTH,
            ((bounds.size.height - SWITCH_HEIGHT) / 2.0).round(),
            SWITCH_WIDTH,
            SWITCH_HEIGHT,
        );
        // On a highlighted row the accent track would vanish into the highlight: white track, accent knob.
        let (track_fill, knob_fill) = match (on, highlighted) {
            (true, false) => (NSColor::controlAccentColor(), NSColor::whiteColor()),
            (true, true) => (NSColor::whiteColor(), NSColor::controlAccentColor()),
            (false, false) => (NSColor::quaternaryLabelColor(), NSColor::whiteColor()),
            (false, true) => (NSColor::whiteColor().colorWithAlphaComponent(0.35), NSColor::whiteColor()),
        };
        // Greyed: the switch keeps its position, at a fraction of its ink.
        let (track_fill, knob_fill) = if dimmed {
            (if on { NSColor::tertiaryLabelColor() } else { NSColor::quaternaryLabelColor() }, NSColor::whiteColor().colorWithAlphaComponent(0.45))
        } else { (track_fill, knob_fill) };
        track_fill.setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(track, SWITCH_HEIGHT / 2.0, SWITCH_HEIGHT / 2.0).fill();
        let d = SWITCH_HEIGHT - 3.0;
        let x = if on { track.origin.x + track.size.width - 1.5 - d } else { track.origin.x + 1.5 };
        knob_fill.setFill();
        NSBezierPath::bezierPathWithOvalInRect(rect(x, track.origin.y + 1.5, d, d)).fill();
    }
}

/// A menu symbol centred on (`x`, `y`) at the menu font's size, in `ink`.
fn draw_symbol(name: &str, x: f64, y: f64, font: &NSFont, ink: &NSColor) {
    draw_symbol_weighted(name, x, y, font, ink, unsafe { objc2_app_kit::NSFontWeightRegular });
}

fn draw_symbol_weighted(name: &str, x: f64, y: f64, font: &NSFont, ink: &NSColor, weight: f64) {
    let Some(image) = symbol(name) else { return };
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(font.pointSize(), weight);
    let colours = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&[ink.retain()]));
    let Some(image) = image.imageWithSymbolConfiguration(&size.configurationByApplyingConfiguration(&colours)) else { return };
    let s = image.size();
    image.drawInRect(rect((x - s.width / 2.0).round(), (y - s.height / 2.0).round(), s.width, s.height));
}

/// Hover tracking that works inside a menu's tracking loop, and while the button is held: a press on a row takes
/// the mouse, and without `EnabledDuringMouseDrag` no exit arrives until release (night 1027, a row left blue).
pub(crate) fn track_hover(view: &NSView, slot: &RefCell<Option<Retained<NSTrackingArea>>>) {
    if let Some(old) = slot.borrow_mut().take() {
        view.removeTrackingArea(&old);
    }
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect
        | NSTrackingAreaOptions::EnabledDuringMouseDrag;
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            view.bounds(),
            options,
            Some(AsRef::<AnyObject>::as_ref(view)),
            None,
        )
    };
    view.addTrackingArea(&area);
    *slot.borrow_mut() = Some(area);
}

// [menu lane, night 1027] The pointer's hover over a menu row, one rule for CheckRow, the status LabelRow and
// ProminentRow (audit items 12-13: a row stayed blue after a press-drag out of it, and Support the Developer stayed
// lit with the pointer elsewhere after its press closed the menu). Every pointer event the row gets (enter, exit,
// press, drag, release) sets the hover from where the event is; a row draws the pointer's highlight only while the
// pointer is inside it; a closing menu drops every row's hover (`menu_closed`).

/// Whether `event` happened inside `view`'s bounds.
pub(crate) fn event_inside(view: &NSView, event: &NSEvent) -> bool {
    inside(view, view.convertPoint_fromView(event.locationInWindow(), None))
}

fn inside(view: &NSView, p: NSPoint) -> bool {
    let b = view.bounds();
    p.x >= 0.0 && p.y >= 0.0 && p.x <= b.size.width && p.y <= b.size.height
}

/// The hover follows `on`; the row redraws only when it changes.
pub(crate) fn set_hover(view: &NSView, hover: &Cell<bool>, on: bool) {
    if hover.replace(on) != on {
        view.setNeedsDisplay(true);
    }
}

/// The hover, from a pointer event the row got: inside it or not.
/// A press a menu row acted on is taken to its end here: its drags and its mouse-up go to the row, never to the menu's
/// own tracking. A section header's press moves the rows under the pointer at once (the open section above it folds),
/// and the menu took the mouse-up for a release on whatever row had come under the pointer, or, the menu shorter by
/// then, for a click outside it, and closed (31 Oct, posted clicks on the live menu). As a native control in a menu
/// tracks its own press.
pub(crate) fn finish_press(view: &NSView, hover: &Cell<bool>) {
    let Some(window) = view.window() else { return };
    let mask = objc2_app_kit::NSEventMask::LeftMouseUp | objc2_app_kit::NSEventMask::LeftMouseDragged;
    loop {
        // In short waits: a mouse-up lost elsewhere (a system gesture, the screen locking) ends the press once the
        // button is seen up, instead of holding the menu in this loop (code review, 31 Oct).
        let until = objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.25);
        let next = window.nextEventMatchingMask_untilDate_inMode_dequeue(mask, Some(&until), unsafe { objc2_app_kit::NSEventTrackingRunLoopMode }, true);
        let Some(next) = next else {
            if NSEvent::pressedMouseButtons() & 1 == 0 { return }
            continue;
        };
        hover_from(view, hover, &next);
        if next.r#type() == objc2_app_kit::NSEventType::LeftMouseUp { return }
    }
}

pub(crate) fn hover_from(view: &NSView, hover: &Cell<bool>, event: &NSEvent) {
    set_hover(view, hover, event_inside(view, event));
}

/// Whether the row shows the pointer's highlight: hovered, and the pointer is inside it now. A row on screen asks
/// where the pointer is (no event may have told it); one off screen (renders, checks) goes by its events.
pub(crate) fn hovered(view: &NSView, hover: &Cell<bool>) -> bool {
    if !hover.get() {
        return false;
    }
    match view.window() {
        Some(window) if window.isVisible() => {
            let at = window.convertPointFromScreen(NSEvent::mouseLocation());
            inside(view, view.convertPoint_fromView(at, None))
        }
        _ => true,
    }
}

/// A menu closed (`menuDidClose:`): no row in it, or in its submenus, keeps a hover. A press that closes the menu
/// (Support the Developer, Set Up…) gets no exit to follow.
pub(crate) fn menu_closed(menu: &objc2_app_kit::NSMenu) {
    for item in menu.itemArray().iter() {
        if let Some(view) = item.view() {
            if let Some(row) = view.downcast_ref::<CheckRow>() {
                set_hover(row, &row.ivars().hover, false);
            } else if let Some(row) = view.downcast_ref::<LabelRow>() {
                set_hover(row, &row.ivars().hover, false);
            } else if let Some(row) = view.downcast_ref::<super::prominent::ProminentRow>() {
                row.drop_hover();
            }
        }
        if let Some(sub) = item.submenu() {
            menu_closed(&sub);
        }
    }
}

define_class!(
    /// The host of a submenu radio/checkbox row (Vella's SettingsMenuItem): the whole row is the click target. The
    /// press is taken here, so the menu never ends tracking, and the release clicks the row's button, which flips
    /// its own state and sends its action.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallControlRow"]
    pub struct ControlRow;

    impl ControlRow {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let local = match unsafe { self.superview() } {
                Some(parent) => self.convertPoint_fromView(point, Some(&parent)),
                None => point,
            };
            let b = self.bounds();
            if local.x >= 0.0 && local.y >= 0.0 && local.x <= b.size.width && local.y <= b.size.height {
                self as *const Self as *mut NSView
            } else {
                std::ptr::null_mut()
            }
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Acts on press: the menu can swallow the first mouse-up after it opens.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let b = self.bounds();
            if p.x < 0.0 || p.y < 0.0 || p.x > b.size.width || p.y > b.size.height {
                return;
            }
            // A second button in the row (a model's disclosure) takes the press over it; the rest is the row's own.
            let buttons: Vec<_> = self.subviews().iter().filter_map(|v| v.downcast::<objc2_app_kit::NSButton>().ok()).collect();
            let over = |b: &objc2_app_kit::NSButton| { let f = b.frame(); p.x >= f.origin.x && p.x <= f.origin.x + f.size.width };
            let target = buttons.iter().skip(1).find(|b| over(b)).or(buttons.first());
            if let Some(button) = target {
                if button.isEnabled() {
                    unsafe { button.performClick(None) };
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {}
    }
);

impl ControlRow {
    pub fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

pub struct LabelRowIvars {
    clickable: Cell<bool>,
    hover: Cell<bool>,
    alert: Cell<bool>,
    /// A failure (the Engine's reason): red, as its tile and header say it, instead of the problems' orange.
    failure: Cell<bool>,
    /// Where the text starts; with `wraps`, the text width is the menu's narrowest (tile row) width less this and
    /// the right inset.
    x: Cell<f64>,
    /// The status line under the tiles: every sentence wraps and the row grows to fit them all,
    /// so no problem is cut off (23 Oct). Other lines stay one line, truncated in the middle.
    wraps: Cell<bool>,
    /// One line's height, measured before wrapping (a wrapping field's intrinsic size is already its whole text).
    line: Cell<f64>,
    /// Offscreen renders only: the grey sampled from the real menu, in place of `secondaryLabelColor`.
    render_grey: RefCell<Option<Retained<NSColor>>>,
    field: RefCell<Option<Retained<objc2_app_kit::NSTextField>>>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
}

define_class!(
    /// A one-line grey (or orange) line in the menu: the status under the tiles and the Agent session. Drawn by
    /// the view, so every path shows the same colour; a long text truncates in the middle at the menu's own width
    /// (never widening it) with the whole text in the tooltip, and only then: a line read whole has none (a stray "Claude Haiku 5.5 · Anthropic" over the menu). When clickable (Set Up…) it highlights like a row and
    /// a click opens Setup.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallLabelRow"]
    #[ivars = LabelRowIvars]
    pub struct LabelRow;

    impl LabelRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let local = match unsafe { self.superview() } {
                Some(parent) => self.convertPoint_fromView(point, Some(&parent)),
                None => point,
            };
            let b = self.bounds();
            if local.x >= 0.0 && local.y >= 0.0 && local.x <= b.size.width && local.y <= b.size.height {
                self as *const Self as *mut NSView
            } else {
                std::ptr::null_mut()
            }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let highlighted = self.highlighted();
            if highlighted {
                NSColor::controlAccentColor().setFill();
                let b = self.bounds();
                let r = rect(HIGHLIGHT_INSET, 0.0, b.size.width - 2.0 * HIGHLIGHT_INSET, b.size.height);
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, HIGHLIGHT_RADIUS, HIGHLIGHT_RADIUS).fill();
            }
            if let Some(field) = self.ivars().field.borrow().as_ref() {
                field.setTextColor(Some(&self.ink(highlighted)));
            }
        }

        /// Acts on press: the menu can swallow the first mouse-up after it opens.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if self.ivars().clickable.get() {
                // The press closes the menu: no exit follows it.
                set_hover(self, &self.ivars().hover, false);
                super::status_clicked();
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            hover_from(self, &self.ivars().hover, event);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            hover_from(self, &self.ivars().hover, event);
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            set_hover(self, &self.ivars().hover, true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            set_hover(self, &self.ivars().hover, false);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            track_hover(self, &self.ivars().tracking);
            unsafe { msg_send![super(self), updateTrackingAreas] }
        }

        /// The menu stretched (or narrowed) the row: whether the text still fits decides the tooltip.
        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, old: NSSize) {
            unsafe { let _: () = msg_send![super(self), resizeSubviewsWithOldSize: old]; }
            self.sync_tooltip();
        }
    }
);

impl LabelRow {
    /// `x`: where the text starts (16 under the tiles, the title column 39 for the session line).
    pub fn new(x: f64, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(LabelRowIvars {
            clickable: Cell::new(false),
            hover: Cell::new(false),
            alert: Cell::new(false),
            failure: Cell::new(false),
            x: Cell::new(x),
            wraps: Cell::new(false),
            line: Cell::new(ROW_HEIGHT),
            render_grey: RefCell::new(None),
            field: RefCell::new(None),
            tracking: RefCell::new(None),
        });
        // Narrow on purpose: the menu sizes to its other rows and stretches this one.
        let width = 140.0;
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, width, ROW_HEIGHT)] };
        this.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        let field = super::util::text_label("", &menu_font(), &NSColor::secondaryLabelColor(), mtm);
        field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
        field.setMaximumNumberOfLines(1);
        let height = field.intrinsicContentSize().height.ceil();
        field.setFrame(rect(x, ((ROW_HEIGHT - height) / 2.0).round(), width - x - 16.0, height));
        this.ivars().line.set(height);
        field.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        this.addSubview(&field);
        *this.ivars().field.borrow_mut() = Some(field);
        this
    }

    /// Wrap whole sentences instead of truncating (the status line under the tiles).
    pub fn set_wraps(&self) {
        self.ivars().wraps.set(true);
        if let Some(field) = self.ivars().field.borrow().as_ref() {
            field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByWordWrapping);
            field.setMaximumNumberOfLines(0);
            field.setPreferredMaxLayoutWidth(self.text_width());
        }
    }

    fn text_width(&self) -> f64 {
        TILE_ROW_WIDTH - self.ivars().x.get() - 16.0
    }

    /// A failure's reason, whole and red (wraps when set to).
    pub fn set_failure(&self, text: &str) {
        self.ivars().failure.set(true);
        self.set(text, true, false);
    }

    /// Text, orange (`alert`) or grey, and whether a click opens Setup. The tooltip holds the whole text.
    pub fn set(&self, text: &str, alert: bool, clickable: bool) {
        self.ivars().alert.set(alert);
        self.ivars().clickable.set(clickable);
        if let Some(field) = self.ivars().field.borrow().as_ref() {
            field.setStringValue(&ns(text));
            field.setTextColor(Some(&self.ink(self.highlighted())));
            if self.ivars().wraps.get() {
                // Measured at the menu's narrowest width: a wider menu only leaves the last line shorter.
                let width = self.text_width();
                let one = self.ivars().line.get();
                let height = field.cell().map_or(one, |cell| cell.cellSizeForBounds(rect(0.0, 0.0, width, 10_000.0)).height.ceil()).max(one);
                // A wrapped sentence keeps a single line's insets, plus a little air above the separator.
                let row = if height > one { height + ROW_HEIGHT - one + 4.0 } else { ROW_HEIGHT };
                self.setFrameSize(NSSize::new(self.frame().size.width, row));
                field.setFrame(rect(self.ivars().x.get(), ((row - height) / 2.0).round(), self.frame().size.width - self.ivars().x.get() - 16.0, height));
            }
        }
        self.sync_tooltip();
        self.setNeedsDisplay(true);
    }

    /// The whole text as the tooltip only while the line shows less of it (truncated in the middle).
    fn sync_tooltip(&self) {
        let Some(field) = self.ivars().field.borrow().clone() else { return };
        let text = field.stringValue().to_string();
        // A wrapping line shows every sentence.
        let cut = !self.ivars().wraps.get() && !text.is_empty()
            && field.cell().is_some_and(|c| c.cellSizeForBounds(rect(0.0, 0.0, 100_000.0, 1000.0)).width.ceil() > field.frame().size.width + 0.5);
        self.setToolTip(cut.then(|| ns(&text)).as_deref());
    }

    /// Whether the line has a tooltip (its text is cut).
    pub fn has_tooltip(&self) -> bool {
        self.toolTip().is_some()
    }

    pub fn text(&self) -> String {
        self.ivars().field.borrow().as_ref().map(|f| f.stringValue().to_string()).unwrap_or_default()
    }

    /// Offscreen renders only: a grey line in `color` (see `offscreen::capture`); an orange line stays orange.
    pub(crate) fn render_grey(&self, color: &NSColor) {
        *self.ivars().render_grey.borrow_mut() = Some(color.retain());
        if let Some(field) = self.ivars().field.borrow().as_ref() {
            field.setTextColor(Some(&self.ink(false)));
        }
    }

    pub fn text_color(&self) -> Option<Retained<NSColor>> {
        self.ivars().field.borrow().as_ref().and_then(|f| f.textColor())
    }

    pub(crate) fn highlighted(&self) -> bool {
        self.ivars().clickable.get()
            && (hovered(self, &self.ivars().hover) || self.enclosingMenuItem().is_some_and(|item| item.isHighlighted()))
    }

    fn ink(&self, highlighted: bool) -> Retained<NSColor> {
        if highlighted {
            NSColor::whiteColor()
        } else if self.ivars().alert.get() && self.ivars().failure.get() {
            NSColor::systemRedColor()
        } else if self.ivars().alert.get() {
            NSColor::systemOrangeColor()
        } else if let Some(grey) = self.ivars().render_grey.borrow().as_ref() {
            grey.clone()
        } else {
            NSColor::secondaryLabelColor()
        }
    }
}

// Speaking ▸ Speed: a title line ("Speed", the value on the right) over a slider between a tortoise and a hare,
// Apple's own speaking-rate pair (Accessibility ▸ Spoken Content). Drawn here rather than an NSSlider for the same
// reason as the switch: a menu's window is never key, so a native slider's fill draws grey. Dragging shows the
// value live; the release commits it, and the app answers with a spoken sample at that speed.
pub const SPEED_ROW_HEIGHT: f64 = 46.0;
const SPEED_TRACK_Y: f64 = 33.0;
const SPEED_TRACK_HEIGHT: f64 = 4.0;
const SPEED_KNOB: f64 = 15.0;
/// How far from the knob's centre a press still takes it.
const SPEED_KNOB_GRAB: f64 = 12.0;
const SPEED_END_ICON: f64 = 11.0;
/// From each icon's centre to the track's end.
const SPEED_ICON_GAP: f64 = 20.0;

pub struct SpeedRowIvars {
    percent: Cell<u16>,
    dragging: Cell<bool>,
    /// Shown in place of the speed while the sample waits for Jarvis.
    note: std::cell::RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSpeedRow"]
    #[ivars = SpeedRowIvars]
    pub struct SpeedRow;

    impl SpeedRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        /// The whole row takes the pointer, so the menu never starts its own click on the item.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let local = match unsafe { self.superview() } {
                Some(parent) => self.convertPoint_fromView(point, Some(&parent)),
                None => point,
            };
            let b = self.bounds();
            if local.x >= 0.0 && local.y >= 0.0 && local.x <= b.size.width && local.y <= b.size.height {
                self as *const Self as *mut NSView
            } else {
                std::ptr::null_mut()
            }
        }

        /// A press on the knob takes it and follows the pointer until release. The drag is tracked here (the menu's
        /// tracking mode), so a release the menu would otherwise eat still commits. A press anywhere else on the row
        /// does nothing, and a release commits only a value the drag moved.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            use objc2_app_kit::{NSEventMask, NSEventType};
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let (x0, x1) = self.track();
            if !knob_hit(p.x, p.y, self.percent(), x0, x1) {
                return;
            }
            let start = self.percent();
            self.ivars().dragging.set(true);
            if let Some(window) = self.window() {
                while let Some(next) = window.nextEventMatchingMask(NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp) {
                    if next.r#type() == NSEventType::LeftMouseUp {
                        break;
                    }
                    self.follow(&next);
                }
            }
            self.ivars().dragging.set(false);
            self.setNeedsDisplay(true);
            if let Some(percent) = speed_release(start, self.percent()) {
                super::voice_speed_released(percent);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {}

        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Retained<NSString> {
            unsafe { objc2_app_kit::NSAccessibilitySliderRole }.copy()
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Retained<NSString> {
            ns("Voice speed")
        }

        #[unsafe(method_id(accessibilityValue))]
        fn ax_value(&self) -> Retained<AnyObject> {
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::numberWithInt(self.percent() as i32))))
        }

        #[unsafe(method_id(accessibilityValueDescription))]
        fn ax_value_description(&self) -> Retained<NSString> {
            match self.note() {
                Some(note) => ns(&format!("{}, {note}", speed_title(self.percent()))),
                None => ns(&speed_title(self.percent())),
            }
        }

        #[unsafe(method(accessibilityPerformIncrement))]
        fn ax_increment(&self) -> Bool {
            self.step(1);
            Bool::YES
        }

        #[unsafe(method(accessibilityPerformDecrement))]
        fn ax_decrement(&self) -> Bool {
            self.step(-1);
            Bool::YES
        }
    }
);

/// "1.15×", "1.2×", "1×".
pub fn speed_title(percent: u16) -> String {
    let text = format!("{:.2}", percent as f64 / 100.0);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}×")
}

/// Whether a press at (`x`, `y`) takes the knob: the knob and a little slop around it, on the track's line.
pub fn knob_hit(x: f64, y: f64, percent: u16, x0: f64, x1: f64) -> bool {
    use crate::model::{VOICE_SPEED_MAX_PERCENT as MAX, VOICE_SPEED_MIN_PERCENT as MIN};
    let fraction = (percent.clamp(MIN, MAX) - MIN) as f64 / (MAX - MIN) as f64;
    let knob_x = x0 + fraction * (x1 - x0);
    (x - knob_x).abs() <= SPEED_KNOB_GRAB && (y - SPEED_TRACK_Y).abs() <= SPEED_KNOB_GRAB
}
/// The value a drag commits on release: only if the drag moved it (a press and release in place commits nothing).
pub fn speed_release(start: u16, released: u16) -> Option<u16> {
    (start != released).then_some(released)
}
/// The percent under `x` on a track from `x0` to `x1`, snapped to the step.
pub fn speed_at(x: f64, x0: f64, x1: f64) -> u16 {
    use crate::model::{VOICE_SPEED_MAX_PERCENT as MAX, VOICE_SPEED_MIN_PERCENT as MIN, VOICE_SPEED_STEP_PERCENT as STEP};
    let fraction = ((x - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0);
    let steps = (fraction * (MAX - MIN) as f64 / STEP as f64).round() as u16;
    (MIN + steps * STEP).min(MAX)
}

impl SpeedRow {
    pub fn new(percent: u16, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SpeedRowIvars { percent: Cell::new(percent), dragging: Cell::new(false), note: Default::default() });
        // Narrow on purpose: the menu sizes to its other rows and stretches this one.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 200.0, SPEED_ROW_HEIGHT)] };
        this.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }

    pub fn percent(&self) -> u16 {
        self.ivars().percent.get()
    }

    /// "Loading Jarvis…" while the sample waits for him; `None` shows the speed.
    pub fn set_note(&self, note: Option<&str>) {
        if self.ivars().note.replace(note.map(str::to_owned)).as_deref() != note {
            self.setNeedsDisplay(true);
        }
    }

    pub fn note(&self) -> Option<String> {
        self.ivars().note.borrow().clone()
    }

    /// The saved value, unless the user is dragging it right now.
    pub fn set_percent(&self, percent: u16) {
        if !self.ivars().dragging.get() && self.ivars().percent.replace(percent) != percent {
            self.setNeedsDisplay(true);
        }
    }

    fn track(&self) -> (f64, f64) {
        let width = self.bounds().size.width;
        (TITLE_X + SPEED_ICON_GAP, width - SWITCH_RIGHT - 5.0 - SPEED_ICON_GAP)
    }

    fn follow(&self, event: &NSEvent) {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        let (x0, x1) = self.track();
        let percent = speed_at(p.x, x0, x1);
        if self.ivars().percent.replace(percent) != percent || self.ivars().dragging.get() {
            self.setNeedsDisplay(true);
            self.displayIfNeeded();
        }
    }

    fn step(&self, direction: i32) {
        use crate::model::{VOICE_SPEED_MAX_PERCENT as MAX, VOICE_SPEED_MIN_PERCENT as MIN, VOICE_SPEED_STEP_PERCENT as STEP};
        let next = (self.percent() as i32 + direction * STEP as i32).clamp(MIN as i32, MAX as i32) as u16;
        self.ivars().percent.set(next);
        self.setNeedsDisplay(true);
        super::voice_speed_released(next);
    }

    fn draw(&self) {
        use crate::model::{VOICE_SPEED_MAX_PERCENT as MAX, VOICE_SPEED_MIN_PERCENT as MIN};
        let bounds = self.bounds();
        let font = menu_font();
        // Full strength, as the other custom rows (climb 2).
        let ink = NSColor::textColor();
        let grey = NSColor::secondaryLabelColor();
        draw_symbol("speedometer", ICON_X, ROW_HEIGHT / 2.0, &font, &ink);
        let title = attributed("Speed", &font, &ink);
        let size = title.size();
        title.drawAtPoint(NSPoint::new(TITLE_X, ((ROW_HEIGHT - size.height) / 2.0).round()));
        // While the user drags, the speed; while the sample waits for Jarvis, the note.
        let note = self.ivars().note.borrow().clone().filter(|_| !self.ivars().dragging.get());
        let value = attributed(note.as_deref().unwrap_or(&speed_title(self.percent())), &font, &grey);
        let size = value.size();
        // On the menu's value column, as every other value (climb 2).
        value.drawAtPoint(NSPoint::new(
            (bounds.size.width - VALUE_RIGHT.with(|v| v.get()).max(SWITCH_RIGHT) - size.width).round(),
            ((ROW_HEIGHT - size.height) / 2.0).round(),
        ));

        let (x0, x1) = self.track();
        let small = NSFont::systemFontOfSize(SPEED_END_ICON);
        draw_symbol("tortoise.fill", TITLE_X + 2.0, SPEED_TRACK_Y, &small, &grey);
        // Its right edge on the margin the switches and chevrons end on.
        draw_symbol("hare.fill", bounds.size.width - SWITCH_RIGHT - 7.0, SPEED_TRACK_Y, &small, &grey);
        let fraction = (self.percent().clamp(MIN, MAX) - MIN) as f64 / (MAX - MIN) as f64;
        let knob_x = x0 + fraction * (x1 - x0);
        let track = rect(x0, SPEED_TRACK_Y - SPEED_TRACK_HEIGHT / 2.0, x1 - x0, SPEED_TRACK_HEIGHT);
        NSColor::tertiaryLabelColor().setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(track, SPEED_TRACK_HEIGHT / 2.0, SPEED_TRACK_HEIGHT / 2.0).fill();
        let filled = rect(x0, track.origin.y, knob_x - x0, SPEED_TRACK_HEIGHT);
        NSColor::controlAccentColor().setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(filled, SPEED_TRACK_HEIGHT / 2.0, SPEED_TRACK_HEIGHT / 2.0).fill();
        let knob = rect(knob_x - SPEED_KNOB / 2.0, SPEED_TRACK_Y - SPEED_KNOB / 2.0, SPEED_KNOB, SPEED_KNOB);
        let pressed = self.ivars().dragging.get();
        let knob_fill = if pressed { NSColor::colorWithWhite_alpha(0.86, 1.0) } else { NSColor::whiteColor() };
        knob_fill.setFill();
        let oval = NSBezierPath::bezierPathWithOvalInRect(knob);
        oval.fill();
        NSColor::colorWithWhite_alpha(0.0, 0.22).setStroke();
        oval.setLineWidth(0.5);
        oval.stroke();
    }
}

#[cfg(test)]
mod speed_tests {
    use super::*;
    #[test]
    fn speed_titles_read_like_apple_playback_rates() {
        assert_eq!(speed_title(115), "1.15×");
        assert_eq!(speed_title(120), "1.2×");
        assert_eq!(speed_title(100), "1×");
        assert_eq!(speed_title(80), "0.8×");
    }
    #[test]
    fn a_press_off_the_knob_takes_nothing() {
        // Track from 50 to 210; the knob at 115 sits at about x = 50 + 35/80 × 160 = 120.
        assert!(knob_hit(120.0, SPEED_TRACK_Y, 115, 50.0, 210.0));
        assert!(!knob_hit(210.0, SPEED_TRACK_Y, 115, 50.0, 210.0), "the right end of the row is not the knob");
        assert!(!knob_hit(120.0, 2.0, 115, 50.0, 210.0), "the title line above the track is not the knob");
    }
    #[test]
    fn a_press_and_release_in_place_commits_nothing_and_a_drag_commits_its_release_once() {
        assert_eq!(speed_release(115, 115), None);
        assert_eq!(speed_release(115, 160), Some(160));
        assert_eq!(speed_release(160, 80), Some(80));
    }
    #[test]
    fn the_track_maps_ends_and_snaps_to_five_percent_steps() {
        assert_eq!(speed_at(0.0, 50.0, 210.0), 80);
        assert_eq!(speed_at(50.0, 50.0, 210.0), 80);
        assert_eq!(speed_at(210.0, 50.0, 210.0), 160);
        assert_eq!(speed_at(999.0, 50.0, 210.0), 160);
        assert_eq!(speed_at(130.0, 50.0, 210.0), 120);
        assert_eq!(speed_at(131.9, 50.0, 210.0), 120);
        assert_eq!(speed_at(135.1, 50.0, 210.0), 125);
    }
}
