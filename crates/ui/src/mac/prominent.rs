//! [chrome] The one menu row that rests on a slightly lighter background: Support the Developer (
//! "highlighted like Vella's 'Copy last transcript'"). Vella's ProminentMenuItemView, in Heimdall's row metrics: the
//! label colour at 13 % fills the rounded row at rest; under the pointer or the keyboard it takes the accent
//! highlight every other Heimdall row draws, with white ink. A press closes the menu and sends the item's action
//! (Heimdall's rows act on press: the menu can swallow the first mouse-up after it opens); Return on the highlighted
//! item and an accessibility press go through the native item action.
//!
//! `ProminentRow::in_place` is the same row for an action that happens inside the menu (Add Provider ▸ Built-in
//! Splash Engine, Remove, Disconnect, Refresh Voices, Revoke): no resting fill, the item's own image, and a press
//! sends the action with the menu left open ("I don't want the menu to disappear whenever I'm
//! clicking something inside the menu"). A native item cannot do that: AppKit closes the menu on its click. Return on
//! the highlighted row still runs the native item action, which closes the menu, as Return does on any menu.
//!
//! The Engine section's model rows are in-place rows too (30 Oct): the loaded one rests on the same lighter fill as
//! Support the Developer (`set_resting`), a row may carry a grey detail after its title (the precision), and the
//! section tints them all (grey while the engine is off, the soft accent while it waits for a pick).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_foundation::NSCopying as _;
use objc2_app_kit::NSAttributedStringNSStringDrawing as _;
use objc2_app_kit::{NSAccessibilityMenuItemRole, NSBezierPath, NSColor, NSEvent, NSImageSymbolConfiguration, NSImageView, NSMenuItem, NSTrackingArea, NSView};
use objc2_foundation::{MainThreadMarker, NSArray, NSPoint, NSRect, NSString};

use super::controls::{event_inside, hover_from, hovered, set_hover, track_hover};
use super::util::{attributed, label, menu_font, ns, rect, symbol, text_width};

/// Heimdall's native-row metrics (controls.rs): 24 pt rows, icon centre 22.25, title at 39, highlight inset 5,
/// radius 6.
const ROW_HEIGHT: f64 = 24.0;
const ICON_X: f64 = 22.25;
const TITLE_X: f64 = 39.0;
const INSET: f64 = 5.0;
const RADIUS: f64 = 6.0;
/// Vella's resting background: the label colour at 13 %.
pub const RESTING_ALPHA: f64 = 0.13;

pub struct ProminentIvars {
    title: RefCell<String>,
    icon: RefCell<Option<String>>,
    hover: Cell<bool>,
    /// Renders only: drawn highlighted without a menu tracking it.
    forced: Cell<bool>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    /// An in-menu action: no resting fill, and the menu stays open.
    in_place: Cell<bool>,
    /// The item's own image (a provider's mark), tinted with the ink, where an in-place row has one.
    mark: RefCell<Option<Retained<NSImageView>>>,
    /// An in-place row resting on the lighter fill all the same (the Engine section's loaded model).
    resting: Cell<bool>,
    /// The ink at rest instead of the label colour, and the resting fill instead of the label at 13 %.
    tint: RefCell<Option<Retained<NSColor>>>,
    fill: RefCell<Option<Retained<NSColor>>>,
    /// Grey text after the title ("UD-Q4_K_XL").
    detail: RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallProminentRow"]
    #[ivars = ProminentIvars]
    pub struct ProminentRow;

    impl ProminentRow {
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

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if event_inside(self, event) {
                // The press closes the menu: no exit follows it, so the hover goes now (night 1027, the row stayed lit).
                // An in-place row's menu stays open under the pointer, so its hover stays.
                if !self.ivars().in_place.get() { set_hover(self, &self.ivars().hover, false); }
                self.perform();
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

        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Retained<NSString> {
            unsafe { NSAccessibilityMenuItemRole }.copy()
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Retained<NSString> {
            ns(&self.ivars().title.borrow())
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn ax_press(&self) -> Bool {
            self.perform();
            Bool::YES
        }

        #[unsafe(method(isAccessibilityEnabled))]
        fn ax_enabled(&self) -> bool {
            self.enclosingMenuItem().is_none_or(|i| i.isEnabled())
        }
    }
);

impl ProminentRow {
    pub fn new(title: &str, icon: Option<&str>, mtm: MainThreadMarker) -> Retained<Self> {
        let width = TITLE_X + text_width(title, &menu_font()) + 24.0 + 16.0;
        let this = Self::alloc(mtm).set_ivars(ProminentIvars {
            title: RefCell::new(title.into()),
            icon: RefCell::new(icon.map(Into::into)),
            hover: Cell::new(false),
            forced: Cell::new(false),
            tracking: RefCell::new(None),
            in_place: Cell::new(false),
            mark: RefCell::new(None),
            resting: Cell::new(false),
            tint: RefCell::new(None),
            fill: RefCell::new(None),
            detail: RefCell::new(None),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, width, ROW_HEIGHT)] };
        this.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }

    /// `item` as a row whose press acts inside the menu and leaves it open: its title and image, at rest like a
    /// native row. Set after the item's image.
    pub fn in_place(item: &NSMenuItem, mtm: MainThreadMarker) -> Retained<Self> {
        let title = item.title().to_string();
        let image = item.image();
        let this = Self::new(&title, None, mtm);
        this.ivars().in_place.set(true);
        this.setFrameSize(objc2_foundation::NSSize::new(TITLE_X + text_width(&title, &menu_font()) + 24.0 + 16.0, ROW_HEIGHT));
        if let Some(image) = image {
            let view = NSImageView::imageViewWithImage(&image, mtm);
            let s = image.size();
            view.setFrame(rect((ICON_X - s.width / 2.0).round(), ((ROW_HEIGHT - s.height) / 2.0).round(), s.width, s.height));
            this.addSubview(&view);
            *this.ivars().mark.borrow_mut() = Some(view);
        }
        this
    }

    pub fn is_in_place(&self) -> bool {
        self.ivars().in_place.get()
    }

    /// An in-place row resting on the lighter fill (or not), in place.
    pub fn set_resting(&self, on: bool) {
        if self.ivars().resting.replace(on) != on { self.setNeedsDisplay(true); }
    }

    pub fn is_resting(&self) -> bool {
        self.ivars().resting.get()
    }

    /// The ink and resting fill at rest (`None`: the label colour, the label at 13 %); the highlight stays the accent.
    pub fn set_tint(&self, ink: Option<Retained<NSColor>>, fill: Option<Retained<NSColor>>) {
        *self.ivars().tint.borrow_mut() = ink;
        *self.ivars().fill.borrow_mut() = fill;
        self.setNeedsDisplay(true);
    }

    /// Grey text after the title, and room for it.
    pub fn set_detail(&self, detail: Option<&str>) {
        *self.ivars().detail.borrow_mut() = detail.map(Into::into);
        let w = TITLE_X + text_width(&self.ivars().title.borrow(), &menu_font()) + detail.map_or(0.0, |d| 6.0 + text_width(d, &menu_font())) + 24.0 + 16.0;
        if self.frame().size.width < w { self.setFrameSize(objc2_foundation::NSSize::new(w.ceil(), ROW_HEIGHT)); }
        self.setNeedsDisplay(true);
    }

    pub fn title(&self) -> String {
        self.ivars().title.borrow().clone()
    }

    /// Renders: draw the row highlighted, as the pointer would.
    pub fn force_highlight(&self, on: bool) {
        self.ivars().forced.set(on);
        self.setNeedsDisplay(true);
    }

    /// Forced (renders), under the pointer (`controls::hovered`: the pointer must be inside), or the menu's keyboard
    /// highlight.
    pub fn highlighted(&self) -> bool {
        self.ivars().forced.get() || hovered(self, &self.ivars().hover) || self.enclosingMenuItem().is_some_and(|item| item.isHighlighted())
    }

    /// The menu closed (`controls::menu_closed`): no hover survives it.
    pub(crate) fn drop_hover(&self) {
        set_hover(self, &self.ivars().hover, false);
    }

    /// Checks: (hover, forced, the item's own highlight), as `menuWillOpen:` logs them.
    pub(crate) fn highlight_parts(&self) -> (bool, bool, bool) {
        (self.ivars().hover.get(), self.ivars().forced.get(), self.enclosingMenuItem().is_some_and(|item| item.isHighlighted()))
    }

    /// What the resting row fills, and its ink: (fill, ink) as (colour, alpha) pairs the tests read.
    pub fn look(&self) -> (Retained<NSColor>, Retained<NSColor>) {
        if self.ivars().in_place.get() && !self.enclosingMenuItem().is_none_or(|i| i.isEnabled()) {
            return (NSColor::clearColor(), NSColor::tertiaryLabelColor());
        }
        if self.highlighted() {
            (NSColor::controlAccentColor(), NSColor::whiteColor())
        } else {
            let ivars = self.ivars();
            let lit = !ivars.in_place.get() || ivars.resting.get();
            let rest = if lit { label().colorWithAlphaComponent(RESTING_ALPHA) } else { NSColor::clearColor() };
            let rest = ivars.fill.borrow().clone().filter(|_| ivars.in_place.get()).unwrap_or(rest);
            (rest, ivars.tint.borrow().clone().unwrap_or_else(label))
        }
    }

    /// Closes the menu, then sends the item's action through NSMenu, as a native click does; an in-place row only
    /// sends it, the menu left open.
    fn perform(&self) {
        let Some(item) = self.enclosingMenuItem() else { return };
        let Some(menu) = (unsafe { item.menu() }) else { return };
        if !item.isEnabled() { return; }
        let index = menu.indexOfItem(&item);
        if index < 0 { return; }
        if !self.ivars().in_place.get() {
            menu.cancelTracking();
            menu.performActionForItemAtIndex(index);
            return;
        }
        // In place: on the next main-queue turn, once the press has left this row, so an action that rebuilds the
        // rows around it (a provider added, removed or disconnected) never takes the row out mid-event.
        let item = super::MainOnly(item);
        dispatch2::DispatchQueue::main().exec_async(move || {
            let item = item;
            let Some(menu) = (unsafe { item.0.menu() }) else { return };
            let index = menu.indexOfItem(&item.0);
            if index >= 0 && item.0.isEnabled() { menu.performActionForItemAtIndex(index); }
        });
    }

    fn draw(&self) {
        let ivars = self.ivars();
        let b = self.bounds();
        let (fill, ink) = self.look();
        fill.setFill();
        let r = rect(INSET, 0.0, b.size.width - 2.0 * INSET, b.size.height);
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, RADIUS, RADIUS).fill();
        let font = menu_font();
        if let Some(name) = ivars.icon.borrow().as_deref() {
            if let Some(image) = symbol(name) {
                let weight = unsafe { objc2_app_kit::NSFontWeightRegular };
                let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(font.pointSize(), weight);
                let colours = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&[ink.clone()]));
                if let Some(image) = image.imageWithSymbolConfiguration(&size.configurationByApplyingConfiguration(&colours)) {
                    let s = image.size();
                    let y = b.size.height / 2.0;
                    image.drawInRect(rect((ICON_X - s.width / 2.0).round(), (y - s.height / 2.0).round(), s.width, s.height));
                }
            }
        }
        let mark = ivars.mark.borrow();
        if let Some(view) = mark.as_ref() { view.setContentTintColor(Some(&ink)); }
        // With or without an image the title starts where a native row's does (macOS 27 keeps the image column).
        let text = attributed(&ivars.title.borrow(), &font, &ink);
        let size = text.size();
        text.drawAtPoint(NSPoint::new(TITLE_X, ((b.size.height - size.height) / 2.0).round()));
        if let Some(detail) = ivars.detail.borrow().as_deref() {
            // Dimmer than the title by the same step whatever its ink: a greyed (tertiary) title keeps a greyer detail.
            let alpha = ink.colorUsingColorSpace(&objc2_app_kit::NSColorSpace::sRGBColorSpace()).map_or(1.0, |c| c.alphaComponent());
            let grey = if self.highlighted() { NSColor::whiteColor().colorWithAlphaComponent(0.75) } else { ink.colorWithAlphaComponent(alpha * 0.55) };
            let d = attributed(detail, &font, &grey);
            let ds = d.size();
            d.drawAtPoint(NSPoint::new((TITLE_X + size.width + 6.0).round(), ((b.size.height - ds.height) / 2.0).round()));
        }
        // Offscreen renders: a spinner child's still frame.
        for v in self.subviews().iter() {
            if let Ok(sp) = v.downcast::<objc2_app_kit::NSProgressIndicator>() {
                if sp.style() == objc2_app_kit::NSProgressIndicatorStyle::Spinning { super::controls::draw_spinner_still(&sp); }
            }
        }
    }
}
