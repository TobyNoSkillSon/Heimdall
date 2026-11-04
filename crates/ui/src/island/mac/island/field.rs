//! Native text fields in the island ("a little text input so I can type a message to you"): the Other
//! row of a question card, and the composer at the foot of the pulled-down conversation.
//!
//! A field is a real NSTextField in the click target's window, placed on its slot's rect (`Content::Field`), so a click
//! on it takes the keyboard. That window is the only one of the island's that may become key (`KeyPanel`), and only
//! when a field asks (`becomesKeyOnlyIfNeeded`): a card on screen never takes focus from the app the user types in.
//! Option plus a digit answers the card on screen while it is open: its Carbon keys exist only for that time
//! (`sync_digits`), and a press goes to the island as `Event::AskDigit`.

use std::collections::{HashMap, HashSet};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2::runtime::{Bool, ProtocolObject};
use objc2_app_kit::{NSBackingStoreType, NSColor, NSControlTextEditingDelegate, NSPanel, NSTextField, NSTextFieldDelegate, NSView, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSRect};

use super::super::draw::hotkey;
use super::host::with_host;
use super::measure::ns_font;
use crate::island::object::{Content, Event, FrameModel, Rect, Target};
use crate::mac::util::{ns, rect};

/// Carbon's key codes for 1 to 4 (kVK_ANSI_1 to kVK_ANSI_4) and Option's modifier mask.
const DIGIT_CODES: [u32; 4] = [0x12, 0x13, 0x14, 0x15];
const OPTION: u32 = 0x0800;
/// Hot-key ids of the card's digits: `SIGNATURE` keys are Heimdall's, and `shortcuts` routes these ids here.
pub(crate) const DIGIT_IDS: [u32; 4] = [101, 102, 103, 104];

define_class!(
    /// The click target's window. It may become key, and does only when a field in it asks (`becomesKeyOnlyIfNeeded`).
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallIslandKeyPanel"]
    pub(crate) struct KeyPanel;

    impl KeyPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        /// ⌘C while the island has the keyboard (`copy::command_c`).
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &objc2_app_kit::NSEvent) -> bool {
            super::copy::command_c(self, event) || unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }
    }
);

impl KeyPanel {
    /// The panel as an `NSPanel`, built like the island's other windows (`host::make_panel`).
    pub(crate) fn new(frame: NSRect, style: NSWindowStyleMask, mtm: MainThreadMarker) -> Retained<NSPanel> {
        let this = Self::alloc(mtm).set_ivars(());
        let panel: Retained<Self> = unsafe { msg_send![super(this), initWithContentRect: frame, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false] };
        panel.into_super()
    }
}

/// What a field's typing reports to: its owner, the island's `Target` for it.
pub(crate) struct FieldIvars {
    owner: Target,
}

define_class!(
    /// The target of one field: its text changes and Return go to the island as the owner's events.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallIslandFieldTarget"]
    #[ivars = FieldIvars]
    pub(crate) struct FieldTarget;

    impl FieldTarget {
        /// The text changed (typing, paste, delete).
        #[unsafe(method(fieldChanged:))]
        fn field_changed(&self, note: &NSNotification) {
            let Some(object) = note.object() else { return };
            let Some(field) = object.downcast_ref::<NSTextField>() else { return };
            changed(&self.ivars().owner, field.stringValue().to_string());
        }

        /// Return in the field.
        #[unsafe(method(fieldReturned:))]
        fn field_returned(&self, sender: &AnyObject) {
            let Some(field) = sender.downcast_ref::<NSTextField>() else { return };
            returned(&self.ivars().owner, field);
        }

        /// A command the field editor cannot handle itself: Esc (cancelOperation:) in an Other row closes its card as
        /// dismissed, as the x does. Only while the field has focus, so no other app's Esc is taken.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn command_by_selector(&self, _control: &AnyObject, _text: &AnyObject, selector: objc2::runtime::Sel) -> Bool {
            let Target::AskOther { id, .. } = &self.ivars().owner else { return Bool::NO };
            if selector != sel!(cancelOperation:) {
                return Bool::NO;
            }
            let id = id.clone();
            with_host(|h| h.apply(Event::AskDismiss(id)));
            Bool::YES
        }
    }
);

unsafe impl NSObjectProtocol for FieldTarget {}
unsafe impl NSControlTextEditingDelegate for FieldTarget {}
unsafe impl NSTextFieldDelegate for FieldTarget {}

impl FieldTarget {
    fn new(owner: Target, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(FieldIvars { owner });
        unsafe { msg_send![super(this), init] }
    }
}

/// One field and its target. Kept while its card (or the composer) lives, so typed text stays with it when its
/// question is off the tab on screen.
struct FieldView {
    view: Retained<NSTextField>,
    /// The card (its request id) the field belongs to; `None` for the composer.
    card: Option<String>,
    /// The model's text the last time the field was synced, so a change the model made is written once.
    seen: String,
    _target: Retained<FieldTarget>,
}

/// The fields of the island, by owner.
#[derive(Default)]
pub(crate) struct Fields {
    views: HashMap<String, FieldView>,
    /// ⌃⌘H asked for the keyboard in the composer: it takes it as soon as it is placed.
    focus: bool,
    /// The cards whose field asked for the keyboard (`OtherField::focus`) and had it: each takes it once.
    focused: HashSet<String>,
    /// A field that had the keyboard left the screen (`take_released`).
    released: bool,
}

/// Where the native field sits against its slot: its cell draws text 1.75 pt in and 1.5 pt higher than the island draws
/// the same font on the same rect (measured in `checks::open_chat`), so the field moves by that and typed text takes
/// the drawn placeholder's place exactly.
const TEXT_SHIFT: (f64, f64) = (-1.75, 1.5);

/// The composer's owner key (`format!("{owner:?}")` of `Target::Composer`).
const COMPOSER: &str = "Composer";

impl Fields {
    /// Places the frame's fields on their slots (`parent` is the click target's content view, `size` the window's
    /// size: island points, y down from the top). Fields not on screen are taken off the view, keeping their text;
    /// fields of cards that are gone are dropped. `cards` are the request ids of the cards the island still holds.
    pub(crate) fn sync(&mut self, frame: &FrameModel, parent: &NSView, size: (f64, f64), cards: &HashSet<String>, mtm: MainThreadMarker) {
        let mut shown: HashSet<String> = HashSet::new();
        for s in &frame.slots {
            // A leaving copy (`~`) of a field is not the field: its window is removed as the copy leaves, never kept
            // (the last frame of a motion may still hold it, and nothing would place it again).
            if s.id.contains('~') {
                continue;
            }
            let Content::Field { font, owner, text, focus_until_ms } = &*s.content else { continue };
            let key = format!("{owner:?}");
            let entry = self.views.entry(key.clone()).or_insert_with(|| new_view(owner, *font, mtm));
            // The model's text, written only when the model changed it (a pick dropped the Other answer): what the user
            // types is already in the model, so an unchanged text is never written back over his typing.
            if let Some(text) = text {
                if *text != entry.seen {
                    entry.seen = text.clone();
                    if entry.view.stringValue().to_string() != *text {
                        entry.view.setStringValue(&ns(text));
                    }
                }
            }
            entry.view.setFrame(local(s.rect.offset(TEXT_SHIFT.0, TEXT_SHIFT.1), size));
            if unsafe { entry.view.superview() }.is_none() {
                parent.addSubview(&entry.view);
            }
            // A name field that answers the user's click takes the keyboard once, as it is first placed, and only while the
            // card is still the answer to that click (not after it waited behind another card or tucked away).
            let card_focus = now_ms() < *focus_until_ms && entry.card.as_ref().is_some_and(|c| self.focused.insert(c.clone()));
            if (self.focus && key == COMPOSER) || card_focus {
                if key == COMPOSER {
                    self.focus = false;
                }
                if let Some(window) = parent.window() {
                    // The click target is a non-activating panel: it takes the keyboard without bringing Heimdall
                    // forward, as a click on the field does.
                    window.makeKeyWindow();
                    window.makeFirstResponder(Some(&entry.view));
                }
            }
            shown.insert(key);
        }
        for (key, fv) in &self.views {
            if !shown.contains(key) && unsafe { fv.view.superview() }.is_some() {
                self.released |= remove(&fv.view);
            }
        }
        // A dropped field leaves the view too: a card can go while its leaving frame still places the field, and a
        // field forgotten on the view stays there invisible, taking the clicks meant for what is drawn under it
        // (day 26: an expired card's Dismiss and the Connect T3 button went dead after a card with an Other row).
        self.focused.retain(|c| cards.contains(c));
        self.views.retain(|_, fv| {
            let keep = fv.card.as_ref().is_none_or(|c| cards.contains(c));
            if !keep && unsafe { fv.view.superview() }.is_some() {
                fv.view.removeFromSuperview();
            }
            keep
        });
    }

    /// Checks: types `text` into the field whose owner key contains `needle` (the native field shows it; the island's
    /// model takes it through `Event::AskOther`, which the check sends too).
    pub(crate) fn type_for_check(&self, needle: &str, text: &str) -> bool {
        let mut typed = false;
        for (key, fv) in &self.views {
            if key.contains(needle) && unsafe { fv.view.superview() }.is_some() {
                fv.view.setStringValue(&ns(text));
                typed = true;
            }
        }
        typed
    }

    /// Every field off the screen (the island is at rest, or the window hides).
    pub(crate) fn hide(&mut self) {
        for fv in self.views.values() {
            if unsafe { fv.view.superview() }.is_some() {
                self.released |= remove(&fv.view);
            }
        }
    }

    /// Builds the composer's field and readies AppKit's text editing ahead of time (at install, while the click
    /// target is not on screen): the first ⌃⌘H then only places and focuses it. The first `makeFirstResponder` of a
    /// text field in the process costs 25-28 ms (the field editor and the text system set up), and it used to land on
    /// the opening motion's fourth frame, three dropped frames on the first open (audit 27 Oct). The window is never
    /// key here, so no keyboard is taken; the field leaves the view at once.
    pub(crate) fn prepare_composer(&mut self, window: &objc2_app_kit::NSWindow, mtm: MainThreadMarker) {
        let fv = self.views.entry(COMPOSER.to_string()).or_insert_with(|| new_view(&Target::Composer, crate::island::object::Font::Field, mtm));
        let Some(parent) = window.contentView() else { return };
        if window.isVisible() || unsafe { fv.view.superview() }.is_some() {
            return;
        }
        parent.addSubview(&fv.view);
        window.makeFirstResponder(Some(&fv.view));
        window.makeFirstResponder(None);
        fv.view.removeFromSuperview();
    }

    /// The composer takes the keyboard the next time it is placed (now, if it is on screen).
    pub(crate) fn focus_composer(&mut self) {
        self.focus = true;
    }

    /// Nothing opened: a later pull-down by click does not take the keyboard.
    pub(crate) fn cancel_focus(&mut self) {
        self.focus = false;
    }

    /// Whether the composer is on screen and edits (it has the keyboard's field editor).
    pub(crate) fn composer_focused(&self) -> bool {
        self.views.get(COMPOSER).is_some_and(|fv| unsafe { fv.view.superview() }.is_some() && fv.view.currentEditor().is_some())
    }

    /// Whether a field that had the keyboard left the screen since the last call.
    pub(crate) fn take_released(&mut self) -> bool {
        std::mem::take(&mut self.released)
    }

    /// Checks: the composer's native field (placed or not).
    pub(crate) fn composer(&self) -> Option<Retained<NSTextField>> {
        self.views.get(COMPOSER).map(|fv| fv.view.clone())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Takes a field off the screen. True when it was typing in (it had the keyboard): the host gives the keyboard back.
fn remove(view: &NSTextField) -> bool {
    let editing = view.currentEditor().is_some();
    if editing {
        if let Some(window) = view.window() {
            window.makeFirstResponder(None);
        }
    }
    view.removeFromSuperview();
    editing
}

/// An island rect in the click target's window: its origin at the top-centre of the window, y down.
fn local(r: Rect, (w, h): (f64, f64)) -> NSRect {
    rect(r.x + w / 2.0, h - r.y - r.h, r.w, r.h)
}

fn new_view(owner: &Target, font: crate::island::object::Font, mtm: MainThreadMarker) -> FieldView {
    let target = FieldTarget::new(owner.clone(), mtm);
    let view = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(0.0, 0.0, 10.0, 10.0));
    view.setBezeled(false);
    view.setBordered(false);
    view.setDrawsBackground(false);
    view.setEditable(true);
    view.setSelectable(true);
    view.setUsesSingleLineMode(true);
    // One line that scrolls with the caret: a long message never wraps inside the one-line field (its top line was
    // cut in half).
    if let Some(cell) = view.cell() {
        cell.setWraps(false);
        cell.setScrollable(true);
    }
    view.setFocusRingType(objc2_app_kit::NSFocusRingType::None);
    view.setFont(Some(&ns_font(font)));
    view.setTextColor(Some(&NSColor::whiteColor()));
    unsafe {
        view.setTarget(Some(&target));
        view.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        view.setAction(Some(sel!(fieldReturned:)));
        objc2_foundation::NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &target,
            sel!(fieldChanged:),
            Some(objc2_app_kit::NSControlTextDidChangeNotification),
            Some(&view),
        );
    }
    let card = match owner {
        Target::AskOther { id, .. } => Some(id.clone()),
        _ => None,
    };
    FieldView { view, card, seen: String::new(), _target: target }
}

/// The user typed: the owner's text. An Other row answers its question's text; the composer reports whether it is empty
/// (its placeholder), and sends on Return only.
fn changed(owner: &Target, text: String) {
    match owner {
        Target::AskOther { id, question } => {
            let (id, question) = (id.clone(), *question);
            with_host(|h| h.apply(Event::AskOther { id, question, text }));
        }
        Target::Composer => {
            let empty = text.is_empty();
            with_host(|h| h.apply(Event::ComposerEmpty(empty)));
        }
        _ => {}
    }
}

/// Return in a field: an Other row brings up its card's next open question, or sends the card once none is left;
/// the composer sends its text and clears itself.
fn returned(owner: &Target, field: &NSTextField) {
    match owner {
        Target::AskOther { id, .. } => {
            let id = id.clone();
            with_host(|h| h.apply(Event::AskReturn(id)));
        }
        Target::Composer => {
            let text = field.stringValue().to_string();
            field.setStringValue(&ns(""));
            with_host(|h| h.apply(Event::ComposerSent(text)));
        }
        _ => {}
    }
}

/// Option plus digit `id`: answers the card on screen with its option. `false` for an id that is not a card's digit.
pub(crate) fn digit_pressed(id: u32) -> bool {
    let Some(n) = DIGIT_IDS.iter().position(|d| *d == id) else { return false };
    with_host(|h| h.apply(Event::AskDigit(n + 1)));
    true
}

/// The digits a card with `want` options (at most four) needs, against the `registered` ones now: the indices to
/// register and the indices to release (index `k` is digit `k + 1`). A closed card wants none.
pub(crate) fn digit_change(registered: usize, want: usize) -> (Vec<usize>, Vec<usize>) {
    let want = want.min(DIGIT_IDS.len());
    if want >= registered {
        ((registered..want).collect(), Vec::new())
    } else {
        (Vec::new(), (want..registered).collect())
    }
}

/// Registers the digits of the card on screen (`want`: its current tab's options, at most four) and releases the rest.
/// Returns the count registered now.
pub(crate) fn sync_digits(registered: usize, want: usize, mtm: MainThreadMarker) -> usize {
    let (register, release) = digit_change(registered, want);
    for k in register {
        if let Err(e) = hotkey::register(DIGIT_IDS[k], DIGIT_CODES[k], OPTION, mtm) {
            eprintln!("Island: Option+{} for a question: {e}", k + 1);
        }
    }
    for k in release {
        hotkey::unregister(DIGIT_IDS[k]);
    }
    want.min(DIGIT_IDS.len())
}

#[cfg(test)]
mod digit_tests {
    use super::digit_change;

    #[test]
    fn a_two_option_tab_registers_exactly_one_and_two() {
        assert_eq!(digit_change(0, 2), (vec![0, 1], vec![]));
    }

    #[test]
    fn a_four_option_tab_registers_one_to_four() {
        assert_eq!(digit_change(0, 4), (vec![0, 1, 2, 3], vec![]));
    }

    #[test]
    fn a_closed_card_registers_none_and_releases_what_it_held() {
        assert_eq!(digit_change(0, 0), (vec![], vec![]));
        assert_eq!(digit_change(4, 0), (vec![], vec![0, 1, 2, 3]));
    }

    #[test]
    fn a_tab_change_releases_the_digits_it_no_longer_has() {
        assert_eq!(digit_change(4, 2), (vec![], vec![2, 3]));
        assert_eq!(digit_change(2, 4), (vec![2, 3], vec![]));
    }
}
