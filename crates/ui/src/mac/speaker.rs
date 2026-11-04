//! The floating name entry (`UiUpdate::VoiceNameEntry`): what the voice said and a name field, as a floating panel.
//! The island's voice cards type the name inline now (3 Nov 18:50), so the app no longer opens it; it never opens by itself; Return or Name answers, Esc or
//! Cancel closes it. A new entry replaces an open one.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationOptions, NSPanel, NSRunningApplication, NSTextField, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::util::*;

#[derive(Default)]
pub struct NameEntry {
    /// Render mode: the panel stays invisible and click-through.
    pub hidden: bool,
    current: Option<Current>,
}

/// What the name prompt is for: a voice's name (its code), the name of a friend to invite, or the reply to a friend's
/// ask (the card's ID).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    Voice(String),
    Friend,
    Reply(String),
}

struct Current {
    purpose: Purpose,
    /// The app the user was in: it gets focus back when the panel closes.
    previous: Option<Retained<NSRunningApplication>>,
    alert: Retained<NSAlert>,
    field: Retained<NSTextField>,
}

/// The quote as the panel shows it: whole words, at most about 80 characters.
pub fn short_quote(quote: &str) -> String {
    let quote = quote.trim();
    if quote.chars().count() <= 80 {
        return quote.to_string();
    }
    let mut out = String::new();
    for word in quote.split_whitespace() {
        if out.chars().count() + word.chars().count() + 1 > 78 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.push('…');
    out
}

impl NameEntry {
    /// Asks for the name of `code`; typing goes to the field at once (the user clicked for it).
    pub fn show(&mut self, code: String, quote: &str, target: &AnyObject, mtm: MainThreadMarker) {
        self.close();
        let (alert, field) = build_alert("Who is this?", Some(quote), "Name", target, mtm);
        self.present(Purpose::Voice(code), alert, field, mtm);
    }

    /// Friends ▸ Invite a friend…: asks for the friend's name, with `suggested` in the field for the user to keep or type
    /// over.
    pub fn show_friend(&mut self, suggested: &str, target: &AnyObject, mtm: MainThreadMarker) {
        self.close();
        let (alert, field) = build_alert("Invite a friend", None, "Invite", target, mtm);
        field.setStringValue(&ns(suggested));
        self.present(Purpose::Friend, alert, field, mtm);
    }

    /// Friends ▸ an ask's Answer…: the user types the reply, with the friend's `question` as the quote. The card is `card_id`.
    pub fn show_reply(&mut self, card_id: &str, question: &str, target: &AnyObject, mtm: MainThreadMarker) {
        self.close();
        let (alert, field) = build_alert("Answer a friend", Some(question), "Send", target, mtm);
        field.setPlaceholderString(Some(&ns("Reply")));
        self.present(Purpose::Reply(card_id.into()), alert, field, mtm);
    }

    fn present(&mut self, purpose: Purpose, alert: Retained<NSAlert>, field: Retained<NSTextField>, mtm: MainThreadMarker) {
        let window = alert.window();
        float(&window);
        // Under the notch, where the card was.
        if let Some(screen) = pointer_screen(mtm) {
            let frame = screen.frame();
            let size = window.frame().size;
            window.setFrameOrigin(NSPoint::new(
                frame.origin.x + (frame.size.width - size.width) / 2.0,
                frame.origin.y + frame.size.height - size.height - 90.0,
            ));
        }
        let app = NSApplication::sharedApplication(mtm);
        let previous = if app.isActive() { None } else { NSWorkspace::sharedWorkspace().frontmostApplication() };
        if self.hidden {
            window.setAlphaValue(0.0);
            window.setIgnoresMouseEvents(true);
            window.orderFrontRegardless();
        } else {
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            window.makeKeyAndOrderFront(None);
            window.makeFirstResponder(Some(&field));
        }
        self.current = Some(Current { purpose, previous, alert, field });
    }

    /// Name: (what it was for, typed name); an empty name counts as Cancel.
    pub fn answer(&mut self) -> Option<(Purpose, String)> {
        let current = self.current.as_ref()?;
        let name = current.field.stringValue().to_string().trim().to_string();
        let purpose = current.purpose.clone();
        self.close();
        (!name.is_empty()).then_some((purpose, name))
    }

    pub fn close(&mut self) {
        if let Some(current) = self.current.take() {
            current.alert.window().orderOut(None);
            let mtm = current.alert.mtm();
            if NSApplication::sharedApplication(mtm).isActive() {
                if let Some(previous) = current.previous {
                    previous.activateWithOptions(NSApplicationActivationOptions::empty());
                }
            }
        }
    }

    /// The open panel's Name button and field: (class, accepts first click) each (render-mode check).
    pub fn first_click_check(&self) -> Vec<(String, bool)> {
        let Some(current) = &self.current else { return Vec::new() };
        let mut views: Vec<Retained<objc2_app_kit::NSView>> = current.alert.buttons().iter().map(|b| Retained::into_super(Retained::into_super(b))).collect();
        views.push(Retained::into_super(Retained::into_super(current.field.clone())));
        views.iter().map(|v| (v.class().name().to_string_lossy().to_string(), v.acceptsFirstMouse(None))).collect()
    }

    pub fn confirm_button(&self) -> Option<Retained<objc2_app_kit::NSButton>> {
        self.current.as_ref().and_then(|c| c.alert.buttons().firstObject())
    }

    /// The open panel's field (render mode types into it).
    pub fn field(&self) -> Option<Retained<NSTextField>> {
        self.current.as_ref().map(|c| c.field.clone())
    }

    /// The open panel's window (render mode).
    pub fn visible_window(&self) -> Option<Retained<objc2_app_kit::NSWindow>> {
        self.current.as_ref().map(|c| c.alert.window())
    }

    /// The active Space changed: an open panel is ordered in again on the Space the user is now on.
    pub fn space_changed(&self) {
        if let Some(current) = &self.current {
            let window = current.alert.window();
            if window.isVisible() {
                window.orderFrontRegardless();
            }
        }
    }
}

/// A floating panel on every Space (other apps' full-screen ones included), like the island.
pub fn float(window: &objc2_app_kit::NSWindow) {
    if let Some(panel) = window.downcast_ref::<NSPanel>() {
        panel.setStyleMask(panel.styleMask() | NSWindowStyleMask::NonactivatingPanel);
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(false);
    }
    window.setCollectionBehavior(overlay_behavior());
}

pub fn build_alert(
    title: &str,
    quote: Option<&str>,
    confirm_title: &str,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> (Retained<NSAlert>, Retained<NSTextField>) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&ns(title));
    if let Some(quote) = quote {
        alert.setInformativeText(&ns(&format!("\u{201c}{}\u{201d}", short_quote(quote))));
    }
    alert_icon(&alert, "person.crop.circle.badge.questionmark");
    let confirm = alert.addButtonWithTitle(&ns(confirm_title));
    let cancel = alert.addButtonWithTitle(&ns("Cancel"));
    cancel.setKeyEquivalent(&ns("\u{1b}"));
    accept_first_click(&confirm);
    accept_first_click(&cancel);
    unsafe {
        confirm.setTarget(Some(target));
        confirm.setAction(Some(sel!(voiceNameConfirm:)));
        cancel.setTarget(Some(target));
        cancel.setAction(Some(sel!(voiceNameCancel:)));
    }
    let field = NSTextField::textFieldWithString(&ns(""), mtm);
    accept_first_click(&field);
    field.setFrame(rect(0.0, 0.0, 228.0, 24.0));
    field.setFont(Some(&font(13.0)));
    field.setPlaceholderString(Some(&ns("Name")));
    // Return in the field reaches Name through its key equivalent; the field itself sends nothing.
    alert.setAccessoryView(Some(&field));
    alert.layout();
    (alert, field)
}
