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
            // Checks copy to their lab pasteboard (`set_check_pasteboard`), never the user's clipboard.
            if CHECK_BOARD.with(|b| b.borrow().is_some()) {
                return edit_command(window, event);
            }
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

// ---- Night 1111 [input]: the fields' edit keys (bug note 2026-11-10T16-30-03: "copy paste doesn't work") ----
//
// Heimdall is an accessory app with no main menu, so no Edit menu carries ⌘V, ⌘X, ⌘A, ⌘Z or ⇧⌘Z to the field
// editor: AppKit offers those keys to the key window's `performKeyEquivalent:` and then to the (missing) menu, and the
// text view never sees them. The key panel sends them on itself, along the field editor's responder chain, as the
// Edit menu would. ⌘C keeps `command_c` (the field's selection, else the reply on screen).

/// An Edit-menu key the island's key panel performs itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKey {
    Paste,
    Cut,
    Copy,
    SelectAll,
    Undo,
    Redo,
}

impl EditKey {
    /// The Edit menu's action for the key, sent to the first responder.
    pub(crate) fn selector(self) -> objc2::runtime::Sel {
        match self {
            EditKey::Paste => sel!(paste:),
            EditKey::Cut => sel!(cut:),
            EditKey::Copy => sel!(copy:),
            EditKey::SelectAll => sel!(selectAll:),
            EditKey::Undo => sel!(undo:),
            EditKey::Redo => sel!(redo:),
        }
    }
}

/// The edit key a press is, from its modifiers and `key` (the characters ignoring modifiers, any case): ⌘ alone with
/// V, X, C, A or Z, and ⇧⌘Z for redo. Option, Control or a missing ⌘ make it some other key.
pub(crate) fn edit_key(command: bool, shift: bool, option: bool, control: bool, key: &str) -> Option<EditKey> {
    if !command || option || control {
        return None;
    }
    let key = key.to_lowercase();
    Some(match (key.as_str(), shift) {
        ("v", false) => EditKey::Paste,
        ("x", false) => EditKey::Cut,
        ("c", false) => EditKey::Copy,
        ("a", false) => EditKey::SelectAll,
        ("z", false) => EditKey::Undo,
        ("z", true) => EditKey::Redo,
        _ => return None,
    })
}

thread_local! {
    /// Checks only: the pasteboard ⌘V, ⌘X and ⌘C use instead of the general one, so a check never touches the user's
    /// clipboard (`set_check_pasteboard`).
    static CHECK_BOARD: RefCell<Option<Retained<objc2_app_kit::NSPasteboard>>> = const { RefCell::new(None) };
}

/// Checks: ⌘V, ⌘X and ⌘C read and write `board` (a named lab pasteboard) instead of the general one; `None` restores it.
pub(crate) fn set_check_pasteboard(board: Option<Retained<objc2_app_kit::NSPasteboard>>) {
    CHECK_BOARD.with(|b| *b.borrow_mut() = board);
}

/// An edit key in the island's key panel, while a field edits (its field editor is the first responder): performed
/// along the responder chain, as the Edit menu would. Whether the key was taken.
pub(crate) fn edit_command(window: &NSWindow, event: &NSEvent) -> bool {
    let flags = event.modifierFlags();
    let key = event.charactersIgnoringModifiers().map(|c| c.to_string()).unwrap_or_default();
    let Some(edit) = edit_key(
        flags.contains(NSEventModifierFlags::Command),
        flags.contains(NSEventModifierFlags::Shift),
        flags.contains(NSEventModifierFlags::Option),
        flags.contains(NSEventModifierFlags::Control),
        &key,
    ) else {
        return false;
    };
    let Some(editor) = window.firstResponder().and_then(|r| r.downcast::<NSTextView>().ok()) else { return false };
    if let Some(board) = CHECK_BOARD.with(|b| b.borrow().clone()) {
        let secure = objc2::runtime::AnyClass::get(c"NSSecureTextView").is_some_and(|c| unsafe { objc2::msg_send![&*editor, isKindOfClass: c] });
        let string = unsafe { objc2_app_kit::NSPasteboardTypeString };
        match edit {
            EditKey::Paste => return editor.readSelectionFromPasteboard(&board),
            EditKey::Copy | EditKey::Cut if secure => return true,
            EditKey::Copy | EditKey::Cut => {
                if editor.selectedRange().length == 0 {
                    return true;
                }
                let range = editor.selectedRange();
                let all = editor.string();
                let picked = all.substringWithRange(range);
                board.clearContents();
                let wrote = board.setString_forType(&picked, string);
                if wrote && edit == EditKey::Cut {
                    unsafe { editor.delete(None) };
                }
                return true;
            }
            _ => {}
        }
    }
    unsafe { editor.tryToPerform_with(edit.selector(), Some(window)) }
}

#[cfg(test)]
mod edit_key_tests {
    use super::{edit_key, EditKey};

    #[test]
    fn command_v_pastes_in_any_case() {
        assert_eq!(edit_key(true, false, false, false, "v"), Some(EditKey::Paste));
        assert_eq!(edit_key(true, false, false, false, "V"), Some(EditKey::Paste));
    }

    #[test]
    fn the_edit_menu_keys_map_to_their_actions() {
        assert_eq!(edit_key(true, false, false, false, "x"), Some(EditKey::Cut));
        assert_eq!(edit_key(true, false, false, false, "c"), Some(EditKey::Copy));
        assert_eq!(edit_key(true, false, false, false, "a"), Some(EditKey::SelectAll));
        assert_eq!(edit_key(true, false, false, false, "z"), Some(EditKey::Undo));
        assert_eq!(edit_key(true, true, false, false, "Z"), Some(EditKey::Redo));
    }

    #[test]
    fn other_modifiers_and_keys_are_not_edit_keys() {
        assert_eq!(edit_key(false, false, false, false, "v"), None, "plain v types");
        assert_eq!(edit_key(true, false, true, false, "v"), None, "⌥⌘V is not paste");
        assert_eq!(edit_key(true, false, false, true, "v"), None, "⌃⌘V is not paste");
        assert_eq!(edit_key(true, true, false, false, "v"), None, "⇧⌘V is not paste");
        assert_eq!(edit_key(true, false, false, false, "h"), None);
    }
}
