//! Known voices: rename in place (click the name, type, Return), forget with the trash button. Voices the user chose not to
//! be asked about are listed dimmed with Ask Again (the undo of the prompt's Don't Ask Again). Each named voice has a
//! listen switch: on, Heimdall acts on their requests in calls until he turns it off; his own row
//! is on and locked.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSImage, NSScrollView, NSTextField, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use super::util::*;
use super::Controller;
use crate::model::*;

const WIDTH: f64 = 440.0;
const PAD: f64 = 20.0;
const ROW: f64 = 48.0;
const VISIBLE_ROWS: usize = 8;
/// The listen switch's column: its right edge 12 pt left of the pencil.
const SWITCH_RIGHT: f64 = PAD + 52.0 + 12.0;

/// The listen switch, drawn like the menu's switches (`controls::CheckRow`): NSSwitch draws On as a grey track
/// whenever its window is not key, which reads as Off.
const SWITCH: NSSize = NSSize::new(32.0, 18.0);

/// One state of the listen switch: the accent track with the knob right when on, a quiet track with it left when off.
fn switch_image(on: bool) -> Retained<NSImage> {
    let draw = block2::RcBlock::new(move |b: NSRect| -> objc2::runtime::Bool {
        let track = if on { NSColor::controlAccentColor() } else { NSColor::quaternaryLabelColor() };
        track.setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, b.size.height / 2.0, b.size.height / 2.0).fill();
        let d = b.size.height - 4.0;
        let x = if on { b.size.width - 2.0 - d } else { 2.0 };
        NSColor::whiteColor().setFill();
        NSBezierPath::bezierPathWithOvalInRect(rect(x, 2.0, d, d)).fill();
        objc2::runtime::Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(SWITCH, false, &draw)
}

/// The listen switch for a row: a toggle button showing the on image while its state is on.
pub fn listen_switch(on: bool, mtm: MainThreadMarker) -> Retained<NSButton> {
    let button = NSButton::new(mtm);
    button.setButtonType(NSButtonType::Toggle);
    button.setBordered(false);
    button.setImage(Some(&switch_image(false)));
    button.setAlternateImage(Some(&switch_image(true)));
    button.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageOnly);
    button.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    button
}

/// The listen switch's tooltip.
/// Whether an arriving list redraws the window: only a change, and only while it is on screen.
pub fn rebuilds(shown: &[KnownSpeaker], arriving: &[KnownSpeaker], visible: bool) -> bool {
    visible && shown != arriving
}

pub fn listen_tip(name: &str, listen: Listen) -> String {
    match listen {
        Listen::Always => format!("{} always listens to you", crate::identity::name()),
        Listen::On => format!("{} acts on {name}'s requests until you turn this off", crate::identity::name()),
        _ => format!("Let {} act on {name}'s requests", crate::identity::name()),
    }
}
/// Forget asks first, like Vella's model deletion.
pub fn forget_alert(name: &str, mtm: MainThreadMarker) -> Retained<objc2_app_kit::NSAlert> {
    let alert = objc2_app_kit::NSAlert::new(mtm);
    alert.setMessageText(&ns(&format!("Forget {name}?")));
    alert.setInformativeText(&ns(&format!("{} will ask who this is the next time it hears this voice.", crate::identity::name())));
    alert.addButtonWithTitle(&ns("Cancel"));
    alert.addButtonWithTitle(&ns("Forget"));
    alert_icon(&alert, "person.crop.circle.badge.minus");
    alert
}

pub struct SpeakersWindow {
    pub window: Retained<NSWindow>,
    scroll: Retained<NSScrollView>,
    empty: Retained<NSTextField>,
    names: Vec<Retained<NSTextField>>,
}

impl SpeakersWindow {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, WIDTH, 200.0),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns("Speakers"));
        let content = window.contentView().unwrap();
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), content.bounds());
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        content.addSubview(&scroll);
        let empty = text_label("No voices yet", &font(13.0), &tertiary(), mtm);
        empty.sizeToFit();
        content.addSubview(&empty);
        window.center();
        SpeakersWindow { window, scroll, empty, names: Vec::new() }
    }

    /// The pencil: start editing that name with its text selected; Return saves.
    pub fn edit(&self, index: usize) {
        if let Some(name) = self.names.get(index) {
            self.window.makeKeyAndOrderFront(None);
            unsafe { name.selectText(None) };
        }
    }

    pub fn show(&mut self, list: &[KnownSpeaker], controller: &Controller, mtm: MainThreadMarker) {
        let target: &AnyObject = controller.as_ref();
        let rows = list.len().clamp(3, VISIBLE_ROWS);
        let height = 8.0 + rows as f64 * ROW + 8.0;
        let frame = self.window.frame();
        let content = self.window.contentRectForFrameRect(frame);
        let top = frame.origin.y + frame.size.height;
        let mut new_frame = self.window.frameRectForContentRect(rect(content.origin.x, content.origin.y, WIDTH, height));
        new_frame.origin.y = top - new_frame.size.height;
        self.window.setFrame_display(new_frame, true);

        let doc_height = (8.0 + list.len() as f64 * ROW + 8.0).max(height);
        let doc = FlippedView::new(rect(0.0, 0.0, WIDTH, doc_height), mtm);
        self.names.clear();
        for (index, known) in list.iter().enumerate() {
            let y = 8.0 + index as f64 * ROW;
            let name = NSTextField::textFieldWithString(&ns(&known.name), mtm);
            name.setBordered(false);
            name.setDrawsBackground(false);
            name.setFocusRingType(objc2_app_kit::NSFocusRingType::None);
            name.setFont(Some(&font(13.0)));
            name.setTextColor(Some(&label()));
            name.setTag(index as isize);
            if known.ignored || known.listen == Listen::Always {
                name.setEditable(false);
                name.setSelectable(false);
                if known.ignored {
                    name.setTextColor(Some(&secondary()));
                }
            } else {
                unsafe {
                    name.setTarget(Some(target));
                    name.setAction(Some(sel!(speakerRenamed:)));
                }
                name.setToolTip(Some(&ns("Click to rename")));
            }
            // Text starts on the dividers' left edge (a field draws its text 2 pt in).
            name.setFrame(rect(PAD - 2.0, y + 7.0, WIDTH - SWITCH_RIGHT - PAD - 44.0, 18.0));
            if known.listen != Listen::None {
                let switch = listen_switch(matches!(known.listen, Listen::On | Listen::Always), mtm);
                switch.setTag(index as isize);
                switch.setToolTip(Some(&ns(&listen_tip(&known.name, known.listen))));
                switch.setAccessibilityLabel(Some(&ns(&format!("Listen to {}", known.name))));
                unsafe {
                    switch.setTarget(Some(target));
                    switch.setAction(Some(sel!(speakerListen:)));
                }
                // The user's: on, dimmed, and it does not move.
                switch.setEnabled(known.listen != Listen::Always);
                switch.setFrame(rect(WIDTH - SWITCH_RIGHT - SWITCH.width, y + ((ROW - SWITCH.height) / 2.0).round(), SWITCH.width, SWITCH.height));
                doc.addSubview(&switch);
            }
            doc.addSubview(&name);
            let clips = if known.clips == 1 { "1 clip".to_string() } else { format!("{} clips", known.clips) };
            let first = if known.ignored { "Not asked about".to_string() } else if known.listen == Listen::Always && known.clips == 0 { String::new() } else { clips };
            let detail = [known.heard_in.as_str(), first.as_str(), known.last_heard.as_str()].iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" · ");
            place_label(&doc, &detail, &digits(11.0), &secondary(), PAD - 2.0, y + 26.0, mtm);
            if known.ignored {
                self.names.push(name.clone());
                let again = unsafe {
                    NSButton::buttonWithImage_target_action(
                        &symbol_sized("arrow.uturn.backward", 13.0, 0.0).expect("arrow.uturn.backward symbol"),
                        Some(target),
                        Some(sel!(speakerUnignore:)),
                        mtm,
                    )
                };
                again.setBordered(false);
                again.setContentTintColor(Some(&secondary()));
                again.setTag(index as isize);
                again.setToolTip(Some(&ns("Ask about this voice again")));
                again.setFrame(rect(WIDTH - PAD - 22.0, y + (ROW - 22.0) / 2.0, 22.0, 22.0));
                doc.addSubview(&again);
                if index + 1 < list.len() {
                    hairline_view(&doc, PAD, y + ROW, WIDTH - 2.0 * PAD, mtm);
                }
                continue;
            }
            if known.listen == Listen::Always {
                self.names.push(name.clone());
                if index + 1 < list.len() {
                    hairline_view(&doc, PAD, y + ROW, WIDTH - 2.0 * PAD, mtm);
                }
                continue;
            }
            let rename = unsafe {
                NSButton::buttonWithImage_target_action(
                    &symbol_sized("pencil", 13.0, 0.0).expect("pencil symbol"),
                    Some(target),
                    Some(sel!(speakerEdit:)),
                    mtm,
                )
            };
            rename.setBordered(false);
            rename.setContentTintColor(Some(&secondary()));
            rename.setTag(index as isize);
            rename.setToolTip(Some(&ns("Rename")));
            rename.setFrame(rect(WIDTH - PAD - 52.0, y + (ROW - 22.0) / 2.0, 22.0, 22.0));
            doc.addSubview(&rename);
            self.names.push(name.clone());
            let trash = unsafe {
                NSButton::buttonWithImage_target_action(
                    &symbol_sized("trash", 13.0, 0.0).expect("trash symbol"),
                    Some(target),
                    Some(sel!(speakerForget:)),
                    mtm,
                )
            };
            trash.setBordered(false);
            trash.setContentTintColor(Some(&secondary()));
            trash.setTag(index as isize);
            trash.setToolTip(Some(&ns("Forget this voice")));
            trash.setFrame(rect(WIDTH - PAD - 22.0, y + (ROW - 22.0) / 2.0, 22.0, 22.0));
            doc.addSubview(&trash);
            if index + 1 < list.len() {
                hairline_view(&doc, PAD, y + ROW, WIDTH - 2.0 * PAD, mtm);
            }
        }
        self.scroll.setDocumentView(Some(&doc));
        let bounds = self.window.contentView().unwrap().bounds();
        self.scroll.setFrame(bounds);
        self.empty.setHidden(!list.is_empty());
        let size: NSSize = self.empty.frame().size;
        self.empty.setFrameOrigin(NSPoint::new(
            ((bounds.size.width - size.width) / 2.0).round(),
            ((bounds.size.height - size.height) / 2.0).round(),
        ));
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(name: &str, clips: u32) -> KnownSpeaker {
        KnownSpeaker { id: name.into(), name: name.into(), heard_in: "Discord".into(), clips, last_heard: String::new(), ignored: false, listen: Listen::Off }
    }

    #[test]
    fn same_list_or_hidden_window_is_not_rebuilt() {
        let list = vec![voice("Dave", 3)];
        assert!(!rebuilds(&list, &list.clone(), true));
        assert!(!rebuilds(&list, &[voice("Dave", 4)], false));
        assert!(rebuilds(&list, &[voice("Dave", 4)], true));
    }
}
