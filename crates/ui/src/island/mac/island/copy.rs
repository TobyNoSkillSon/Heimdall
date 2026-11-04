//! Copying the island's words (bug note 2026-10-30T22-16-36: "he couldn't select or copy it"). The words are drawn as
//! layers, not text views, so a message is copied whole: a right-click (or Control-click) on a message of the
//! pulled-down conversation, or on its reply, offers Copy; ⌘C while the island has the keyboard copies the field's
//! selection when there is one, else the reply. The copy glyph in the reply's band stays (`Target::ReplyCopy`).

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSMenu, NSMenuItem, NSTextView, NSView, NSWindow};
use objc2_foundation::{MainThreadMarker, NSPoint};

use crate::mac::util::ns;
use super::host::with_host;
use crate::island::object::Output;

/// The words the open menu's Copy takes, and the item's target (a menu holds its target weakly).
#[derive(Default)]
pub(crate) struct CopyIvars {
    text: RefCell<String>,
}

define_class!(
    /// Copy's target in the island's context menu.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallIslandCopyTarget"]
    #[ivars = CopyIvars]
    pub(crate) struct CopyTarget;

    impl CopyTarget {
        #[unsafe(method(islandCopy:))]
        fn island_copy(&self, _sender: &AnyObject) {
            let text = self.ivars().text.borrow().clone();
            if !text.is_empty() {
                super::host::dispatch(Output::Copy(text));
            }
        }
    }
);

thread_local! {
    static TARGET: RefCell<Option<Retained<CopyTarget>>> = const { RefCell::new(None) };
}

/// The menu for a right-click at `event` on the island's click target: Copy over words, nothing elsewhere.
pub(crate) fn menu_for(view: &NSView, event: &NSEvent) -> Option<Retained<NSMenu>> {
    let p = screen_point(view, event);
    let text = with_host(|h| h.copy_at(p)).flatten()?;
    let mtm = MainThreadMarker::from(view);
    let target = TARGET.with(|t| {
        t.borrow_mut()
            .get_or_insert_with(|| {
                let this = CopyTarget::alloc(mtm).set_ivars(CopyIvars::default());
                unsafe { msg_send![super(this), init] }
            })
            .clone()
    });
    *target.ivars().text.borrow_mut() = text;
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(""));
    menu.setAutoenablesItems(false);
    let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns("Copy"), Some(sel!(islandCopy:)), &ns("")) };
    unsafe { item.setTarget(Some(&target)) };
    menu.addItem(&item);
    Some(menu)
}

/// A Control-click is a right-click: shows the menu and says it took the click.
pub(crate) fn control_click(view: &NSView, event: &NSEvent) -> bool {
    if !event.modifierFlags().contains(NSEventModifierFlags::Control) {
        return false;
    }
    let Some(menu) = menu_for(view, event) else { return false };
    NSMenu::popUpContextMenu_withEvent_forView(&menu, event, view);
    true
}

/// ⌘C in the island's key panel: the focused field's selection when it has one (AppKit has no Edit menu here to send
/// it), else the reply on screen. Whether the key was taken.
pub(crate) fn command_c(window: &NSWindow, event: &NSEvent) -> bool {
    // Caps Lock (and Fn, numeric pad) are not modifiers here.
    let flags = event.modifierFlags() & (NSEventModifierFlags::Command | NSEventModifierFlags::Shift | NSEventModifierFlags::Option | NSEventModifierFlags::Control);
    if flags != NSEventModifierFlags::Command || event.charactersIgnoringModifiers().map(|c| c.to_string()).as_deref() != Some("c") {
        return false;
    }
    if let Some(editor) = window.firstResponder().and_then(|r| r.downcast::<NSTextView>().ok()) {
        if editor.selectedRange().length > 0 {
            unsafe { editor.copy(None) };
            return true;
        }
    }
    match with_host(|h| h.copy_shown()).flatten() {
        Some(text) => {
            super::host::dispatch(Output::Copy(text));
            true
        }
        None => false,
    }
}

/// Screen point of an event in a view's window.
fn screen_point(view: &NSView, event: &NSEvent) -> NSPoint {
    let p = event.locationInWindow();
    match view.window() {
        Some(w) => {
            let f = w.frame();
            NSPoint::new(f.origin.x + p.x, f.origin.y + p.y)
        }
        None => p,
    }
}
