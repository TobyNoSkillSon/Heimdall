//! Bug Notes (the list was a wall of times and paragraphs): one card per note, the open ones
//! first, then those resolved in the last days. A card's first line is its cause class as a coloured chip, its time,
//! and its status at the right (open, or fixed with the commit); then a short title, the likely cause, the files the
//! cause names, and the evidence folded under a row that a click opens and closes. One flipped view draws every card
//! (`BugCards`) inside a scroll view: nothing draws at rest, and a resize lays the cards out again at the new width.

use std::cell::RefCell;
use std::collections::HashSet;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAttributedStringNSExtendedStringDrawing as _, NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSColor,
    NSCursor, NSEvent, NSFont, NSImage, NSLineBreakMode, NSMutableParagraphStyle, NSScrollView, NSStringDrawingOptions,
    NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSAttributedString, NSDictionary, NSNotification, NSPoint, NSRect, NSSize, NSString};

use super::util::*;
use crate::model::*;

/// The window's edge to a card, the gap between cards, a card's inside.
const MARGIN: f64 = 16.0;
const GAP: f64 = 10.0;
const PAD: f64 = 14.0;
const RADIUS: f64 = 11.0;
/// Cards grow with the window up to this width, then sit centred (lines stay readable).
const MAX_CARD_W: f64 = 760.0;
/// The chip row: the chip's height, then the gap to the title.
const HEAD_H: f64 = 20.0;
const HEAD_GAP: f64 = 8.0;
/// Between the title, the cause, the files and the evidence row.
const STEP: f64 = 6.0;
const PILL_H: f64 = 22.0;
const TOGGLE_H: f64 = 20.0;
/// Lines shown at most: the title, the cause (or the note when no cause is named yet).
const TITLE_LINES: usize = 3;
const BODY_LINES: usize = 5;

pub struct BugNotesWindow {
    pub window: Retained<NSWindow>,
    pub cards: Retained<BugCards>,
    empty: Retained<NSTextField>,
}

impl BugNotesWindow {
    /// `autosave`: remember the frame between launches (off for renders, which size it).
    pub fn new(mtm: MainThreadMarker, autosave: bool) -> Self {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, 560.0, 640.0),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns("Bug Notes"));
        window.setContentMinSize(NSSize::new(360.0, 240.0));
        window.center();
        if autosave {
            window.setFrameAutosaveName(&ns("HeimdallBugNotes"));
        }

        let content = window.contentView().unwrap();
        let bounds = content.bounds();
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), bounds);
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        let cards = BugCards::new(rect(0.0, 0.0, bounds.size.width, bounds.size.height), mtm);
        cards.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        scroll.setDocumentView(Some(&cards));
        content.addSubview(&scroll);
        cards.observe(&window, &scroll.contentView());

        let empty = text_label("No bug notes", &font(13.0), &tertiary(), mtm);
        empty.sizeToFit();
        let size = empty.frame().size;
        empty.setFrameOrigin(NSPoint::new((bounds.size.width - size.width) / 2.0, (bounds.size.height - size.height) / 2.0));
        empty.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        content.addSubview(&empty);
        BugNotesWindow { window, cards, empty }
    }

    pub fn show(&self, notes: &[BugNote]) {
        self.cards.set_notes(notes);
        self.empty.setHidden(!notes.is_empty());
    }
}

/// One card laid out at a width: what it draws and where, in the cards view (flipped, y down).
struct Card {
    frame: NSRect,
    chip: Option<(NSRect, &'static str, Retained<NSColor>)>,
    time: NSRect,
    status: (NSRect, NSRect),
    title: NSRect,
    body: Option<(NSRect, String)>,
    pills: Vec<(NSRect, String)>,
    toggle: Option<NSRect>,
    /// The opened evidence: its box and each paragraph's rect and text (`true`: an evidence item, set in mono).
    evidence: Option<(NSRect, Vec<(NSRect, String, bool)>)>,
}

pub struct CardsIvars {
    /// Bumped on every change to the notes or the opened cards; with the width, what the last layout was for.
    version: std::cell::Cell<u64>,
    laid: std::cell::Cell<Option<(u64, u64)>>,
    notes: RefCell<Vec<BugNote>>,
    open: RefCell<HashSet<String>>,
    cards: RefCell<Vec<Card>>,
}

define_class!(
    /// Every card, drawn in one view. A click on a card's evidence row opens or closes it.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallBugCards"]
    #[ivars = CardsIvars]
    pub struct BugCards;

    impl BugCards {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.draw(dirty);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            self.click(p);
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            for card in self.ivars().cards.borrow().iter() {
                if let Some(r) = card.toggle {
                    self.addCursorRect_cursor(r, &NSCursor::pointingHandCursor());
                }
            }
        }

        #[unsafe(method(bugWindowResized:))]
        fn window_resized(&self, _note: &NSNotification) {
            self.relayout();
        }
    }
);

impl BugCards {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CardsIvars { version: Default::default(), laid: Default::default(), notes: RefCell::new(vec![]), open: RefCell::new(HashSet::new()), cards: RefCell::new(vec![]) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Lays the cards out again whenever the window resizes, or the visible width changes (a scroller that takes
    /// room when scroll bars always show).
    fn observe(&self, window: &NSWindow, clip: &NSView) {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        clip.setPostsFrameChangedNotifications(true);
        unsafe {
            center.addObserver_selector_name_object(self, sel!(bugWindowResized:), Some(objc2_app_kit::NSWindowDidResizeNotification), Some(window.as_ref()));
            center.addObserver_selector_name_object(self, sel!(bugWindowResized:), Some(objc2_app_kit::NSViewFrameDidChangeNotification), Some(clip.as_ref()));
        }
    }

    pub fn set_notes(&self, notes: &[BugNote]) {
        *self.ivars().notes.borrow_mut() = notes.to_vec();
        // An opened card stays open while its note is listed.
        self.ivars().open.borrow_mut().retain(|id| notes.iter().any(|n| n.id == *id));
        self.changed();
        self.relayout();
    }

    /// Opens or closes note `id`'s evidence (a click on its row; checks call it directly).
    pub fn toggle(&self, id: &str) {
        {
            let mut open = self.ivars().open.borrow_mut();
            if !open.remove(id) {
                open.insert(id.to_owned());
            }
        }
        self.changed();
        self.relayout();
    }

    /// Checks: the evidence rows' rects in this view, by note id.
    pub fn toggles(&self) -> Vec<(String, NSRect)> {
        let notes = self.ivars().notes.borrow();
        self.ivars().cards.borrow().iter().zip(notes.iter()).filter_map(|(c, n)| c.toggle.map(|r| (n.id.clone(), r))).collect()
    }

    /// The notes shown.
    pub fn notes(&self) -> Vec<BugNote> {
        self.ivars().notes.borrow().clone()
    }

    /// Checks: each card's frame in this view.
    pub fn frames(&self) -> Vec<NSRect> {
        self.ivars().cards.borrow().iter().map(|c| c.frame).collect()
    }

    pub fn is_open(&self, id: &str) -> bool {
        self.ivars().open.borrow().contains(id)
    }

    fn changed(&self) {
        self.ivars().version.set(self.ivars().version.get() + 1);
    }

    fn click(&self, p: NSPoint) {
        let hit = self.toggles().into_iter().find(|(_, r)| contains(*r, p)).map(|(id, _)| id);
        if let Some(id) = hit {
            self.toggle(&id);
        }
    }

    /// Lays every card out at the view's width and sizes the view to hold them (at least the visible height).
    fn relayout(&self) {
        let width = self.enclosingScrollView().map(|s| s.contentSize().width).unwrap_or(self.frame().size.width);
        // Unchanged width and notes: nothing to lay out again (a frame change this relayout caused, or a height-only
        // resize).
        let visible = self.enclosingScrollView().map(|s| s.contentSize().height).unwrap_or(0.0);
        let key = (width.to_bits() ^ visible.to_bits().rotate_left(32), self.ivars().version.get());
        if self.ivars().laid.get() == Some(key) {
            return;
        }
        self.ivars().laid.set(Some(key));
        let notes = self.ivars().notes.borrow();
        let open = self.ivars().open.borrow();
        let mut y = MARGIN;
        let mut cards = Vec::with_capacity(notes.len());
        let card_w = (width - 2.0 * MARGIN).clamp(200.0, MAX_CARD_W);
        let x = ((width - card_w) / 2.0).max(MARGIN).floor();
        for note in notes.iter() {
            let card = layout_card(note, x, y, card_w, open.contains(&note.id));
            y += card.frame.size.height + GAP;
            cards.push(card);
        }
        let height = if cards.is_empty() { 0.0 } else { y - GAP + MARGIN };
        drop((notes, open));
        *self.ivars().cards.borrow_mut() = cards;
        self.setFrameSize(NSSize::new(width, height.max(visible)));
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.setNeedsDisplay(true);
    }

    fn draw(&self, dirty: NSRect) {
        let notes = self.ivars().notes.borrow();
        let cards = self.ivars().cards.borrow();
        for (card, note) in cards.iter().zip(notes.iter()) {
            if !intersects(card.frame, dirty) {
                continue;
            }
            draw_card(card, note);
        }
    }
}

fn contains(r: NSRect, p: NSPoint) -> bool {
    p.x >= r.origin.x && p.x <= r.origin.x + r.size.width && p.y >= r.origin.y && p.y <= r.origin.y + r.size.height
}

fn intersects(a: NSRect, b: NSRect) -> bool {
    a.origin.x < b.origin.x + b.size.width && b.origin.x < a.origin.x + a.size.width && a.origin.y < b.origin.y + b.size.height && b.origin.y < a.origin.y + a.size.height
}

fn title_font() -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(14.0, unsafe { objc2_app_kit::NSFontWeightSemibold })
}
fn body_font() -> Retained<NSFont> {
    font(12.5)
}
fn chip_font() -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(11.0, unsafe { objc2_app_kit::NSFontWeightSemibold })
}
fn mono(size: f64) -> Retained<NSFont> {
    NSFont::monospacedSystemFontOfSize_weight(size, 0.0)
}

/// The class's colour: one hue per class, so the list reads at a glance.
fn class_color(class: &str) -> Retained<NSColor> {
    match class {
        "code" => NSColor::systemBlueColor(),
        "decider" => NSColor::systemPurpleColor(),
        "agent-prompt" => NSColor::systemTealColor(),
        "model-quality" => NSColor::systemPinkColor(),
        "environment" => NSColor::systemYellowColor(),
        _ => NSColor::systemGrayColor(),
    }
}

fn wrapping() -> Retained<NSMutableParagraphStyle> {
    let p = NSMutableParagraphStyle::new();
    p.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    p.setLineSpacing(1.5);
    p
}

fn styled(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let paragraph = wrapping();
    unsafe {
        let keys: [&NSString; 3] = [objc2_app_kit::NSFontAttributeName, objc2_app_kit::NSForegroundColorAttributeName, objc2_app_kit::NSParagraphStyleAttributeName];
        let values: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), paragraph.as_ref()];
        let attrs = NSDictionary::from_slices(&keys, &values);
        NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &ns(text), Some(&attrs))
    }
}

/// The height `text` wraps to at `width`, at most `lines` lines (0: no limit).
fn wrapped_height(text: &str, font: &NSFont, width: f64, lines: usize) -> f64 {
    let s = styled(text, font, &label());
    let r = s.boundingRectWithSize_options_context(NSSize::new(width, f64::MAX), NSStringDrawingOptions::UsesLineFragmentOrigin, None);
    let h = r.size.height.ceil();
    if lines == 0 {
        h
    } else {
        h.min(line_height(font) * lines as f64)
    }
}

fn line_height(font: &NSFont) -> f64 {
    (font.ascender() - font.descender() + font.leading()).ceil() + 1.5
}

fn draw_text(text: &str, font: &NSFont, color: &NSColor, r: NSRect) {
    let s = styled(text, font, color);
    let options = NSStringDrawingOptions::UsesLineFragmentOrigin | NSStringDrawingOptions::TruncatesLastVisibleLine;
    s.drawWithRect_options_context(r, options, None);
}

/// The status at the card's right: a glyph and its word ("Open"; the fix's commit; "Closed").
fn status_look(status: &BugStatus) -> (&'static str, String, Retained<NSColor>) {
    match status {
        BugStatus::Open => ("circle", "Open".into(), secondary()),
        BugStatus::Fixed(Some(commit)) => ("checkmark.circle.fill", commit.clone(), NSColor::systemGreenColor()),
        BugStatus::Fixed(None) => ("checkmark.circle.fill", "Fixed".into(), NSColor::systemGreenColor()),
        BugStatus::Closed => ("minus.circle", "Closed".into(), secondary()),
    }
}

fn status_font(status: &BugStatus) -> Retained<NSFont> {
    match status {
        BugStatus::Fixed(Some(_)) => NSFont::monospacedSystemFontOfSize_weight(11.5, unsafe { objc2_app_kit::NSFontWeightMedium }),
        _ => medium(11.5),
    }
}

fn layout_card(note: &BugNote, x: f64, y: f64, w: f64, open: bool) -> Card {
    let inner = w - 2.0 * PAD;
    let (left, mut top) = (x + PAD, y + PAD);
    // The head: chip, time, status.
    let chip = note.class.as_deref().map(|class| {
        let label = bug_class_label(class);
        let cw = text_width(label, &chip_font()).ceil() + 16.0;
        (rect(left, top, cw, HEAD_H), label, class_color(class))
    });
    let time_x = chip.as_ref().map_or(left, |(r, ..)| r.origin.x + r.size.width + 10.0);
    let time_w = text_width(&note.time, &digits(11.5)).ceil() + 2.0;
    let time = rect(time_x, top + 2.5, time_w, 16.0);
    let (_, word, _) = status_look(&note.status);
    let word_w = text_width(&word, &status_font(&note.status)).ceil() + 2.0;
    let word_r = rect(left + inner - word_w, top + 2.5, word_w, 16.0);
    let glyph_r = rect(word_r.origin.x - 18.0, top + 2.0, 14.0, 16.0);
    top += HEAD_H + HEAD_GAP;
    // The title.
    let th = wrapped_height(&note.title, &title_font(), inner, TITLE_LINES);
    let title = rect(left, top, inner, th);
    top += th;
    // The likely cause, or what the note says when no one named a cause yet.
    let rest = bug_rest(&note.text);
    let body_text = note.cause.clone().filter(|c| !c.trim().is_empty()).or_else(|| (!rest.is_empty()).then(|| rest.clone()));
    let body = body_text.map(|text| {
        top += STEP;
        let h = wrapped_height(&text, &body_font(), inner, BODY_LINES);
        let r = rect(left, top, inner, h);
        top += h;
        (r, text)
    });
    // The files, as pills that wrap.
    let mut pills = vec![];
    if !note.files.is_empty() {
        top += STEP + 4.0;
        let mut px = left;
        for file in &note.files {
            let pw = (text_width(file, &mono(11.0)).ceil() + 34.0).min(inner);
            if px > left && px + pw > left + inner {
                px = left;
                top += PILL_H + 6.0;
            }
            pills.push((rect(px, top, pw, PILL_H), file.clone()));
            px += pw + 6.0;
        }
        top += PILL_H;
    }
    // The evidence: the rest of the note when the cause took its place, then each item.
    let mut paragraphs: Vec<(String, bool)> = vec![];
    if note.cause.is_some() && !rest.is_empty() {
        paragraphs.push((rest, false));
    }
    paragraphs.extend(note.evidence.iter().filter(|e| !e.trim().is_empty()).map(|e| (e.trim().to_owned(), true)));
    let toggle = (!paragraphs.is_empty()).then(|| {
        top += STEP + 2.0;
        let r = rect(left - 4.0, top, text_width("Evidence", &medium(12.0)).ceil() + 30.0, TOGGLE_H);
        top += TOGGLE_H;
        r
    });
    let evidence = (open && toggle.is_some()).then(|| {
        top += 6.0;
        let box_top = top;
        let mut py = top + 10.0;
        let tw = inner - 24.0;
        let mut items = vec![];
        for (text, item) in paragraphs {
            let font = if item { mono(11.0) } else { body_font() };
            let h = wrapped_height(&text, &font, tw, 0);
            items.push((rect(left + 12.0, py, tw, h), text, item));
            py += h + 8.0;
        }
        let r = rect(left, box_top, inner, py - 8.0 + 10.0 - box_top);
        top = r.origin.y + r.size.height;
        (r, items)
    });
    top += PAD;
    Card { frame: rect(x, y, w, top - y), chip, time, status: (glyph_r, word_r), title, body, pills, toggle, evidence }
}

fn draw_card(card: &Card, note: &BugNote) {
    let f = card.frame;
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(f.origin.x + 0.5, f.origin.y + 0.5, f.size.width - 1.0, f.size.height - 1.0), RADIUS, RADIUS);
    NSColor::colorWithWhite_alpha(1.0, 0.05).setFill();
    path.fill();
    NSColor::colorWithWhite_alpha(1.0, 0.07).setStroke();
    path.setLineWidth(1.0);
    path.stroke();
    if let Some((r, label, color)) = &card.chip {
        color.colorWithAlphaComponent(0.20).setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(*r, r.size.height / 2.0, r.size.height / 2.0).fill();
        draw_text(label, &chip_font(), color, rect(r.origin.x + 8.0, r.origin.y + 3.0, r.size.width - 16.0 + 2.0, 15.0));
    }
    draw_text(&note.time, &digits(11.5), &tertiary(), card.time);
    let (glyph, word, color) = status_look(&note.status);
    draw_symbol(glyph, &color, card.status.0, 12.0);
    draw_text(&word, &status_font(&note.status), &color, card.status.1);
    draw_text(&note.title, &title_font(), &label(), card.title);
    if let Some((r, text)) = &card.body {
        draw_text(text, &body_font(), &secondary(), *r);
    }
    for (r, file) in &card.pills {
        NSColor::colorWithWhite_alpha(1.0, 0.07).setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(*r, 6.0, 6.0).fill();
        draw_symbol("doc.text", &tertiary(), rect(r.origin.x + 7.0, r.origin.y + 4.0, 14.0, 14.0), 10.5);
        draw_text(file, &mono(11.0), &secondary(), rect(r.origin.x + 25.0, r.origin.y + 4.0, r.size.width - 31.0, 15.0));
    }
    if let Some(r) = card.toggle {
        let open = card.evidence.is_some();
        draw_symbol(if open { "chevron.down" } else { "chevron.right" }, &tertiary(), rect(r.origin.x + 4.0, r.origin.y + 3.0, 14.0, 14.0), 10.0);
        draw_text("Evidence", &medium(12.0), &tertiary(), rect(r.origin.x + 22.0, r.origin.y + 2.0, r.size.width - 22.0, 16.0));
    }
    if let Some((r, items)) = &card.evidence {
        NSColor::colorWithWhite_alpha(0.0, 0.22).setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(*r, 8.0, 8.0).fill();
        for (ir, text, item) in items {
            let font = if *item { mono(11.0) } else { body_font() };
            draw_text(text, &font, &secondary(), *ir);
        }
    }
}

/// An SF Symbol tinted `color`, centred in `r` at `size` points.
fn draw_symbol(name: &str, color: &NSColor, r: NSRect, size: f64) {
    let Some(image) = symbol_sized(name, size, unsafe { objc2_app_kit::NSFontWeightMedium }) else { return };
    let tinted = tint(&image, color);
    let s = tinted.size();
    let at = rect(r.origin.x + (r.size.width - s.width) / 2.0, r.origin.y + (r.size.height - s.height) / 2.0, s.width, s.height);
    unsafe { tinted.drawInRect_fromRect_operation_fraction_respectFlipped_hints(at, NSRect::ZERO, objc2_app_kit::NSCompositingOperation::SourceOver, 1.0, true, None) };
}

fn tint(image: &NSImage, color: &NSColor) -> Retained<NSImage> {
    let config = objc2_app_kit::NSImageSymbolConfiguration::configurationWithHierarchicalColor(color);
    image.imageWithSymbolConfiguration(&config).unwrap_or_else(|| image.retain())
}

/// `heimdall-ui-demo --bug-notes <dir>`: the window offscreen (never ordered in) with synthesized notes, every state
/// at its real size, and a click on a card's evidence row through the window's own event path. Returns what failed.
pub(crate) fn render(dir: &std::path::Path) -> Vec<String> {
    use objc2_app_kit::{NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy, NSEventType};
    let mtm = MainThreadMarker::new().expect("the Bug Notes renders run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {} {name}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failures.push(name.to_owned());
        }
    };
    let notes = demo_bug_notes();
    let w = BugNotesWindow::new(mtm, false);
    let shot = |w: &BugNotesWindow, name: &str, size: (f64, f64)| -> bool {
        w.window.setContentSize(NSSize::new(size.0, size.1));
        w.show(&w.cards.notes());
        super::agent_render::capture_window(&w.window, &dir.join(format!("{name}.png")))
    };
    w.show(&notes);
    check("bug-notes: rendered at 560 × 640", shot(&w, "bug-notes", (560.0, 640.0)));
    check("bug-notes-narrow: rendered at the minimum width", shot(&w, "bug-notes-narrow", (360.0, 640.0)));
    check("bug-notes-wide: rendered at 960 × 560", shot(&w, "bug-notes-wide", (960.0, 560.0)));
    // The end of the list: the unknown class, a fix with its commit, a closed note.
    w.window.setContentSize(NSSize::new(560.0, 640.0));
    w.show(&notes);
    let end = w.cards.frame().size.height;
    w.cards.scrollPoint(NSPoint::new(0.0, end));
    check("bug-notes-end: rendered scrolled to the end", super::agent_render::capture_window(&w.window, &dir.join("bug-notes-end.png")));
    w.cards.scrollPoint(NSPoint::new(0.0, 0.0));
    w.window.setContentSize(NSSize::new(560.0, 820.0));
    w.show(&notes);
    // Every card is laid out inside the view's width and none overlaps the next.
    let cards = w.cards.frames();
    let width = w.cards.frame().size.width;
    check("cards: one per note", cards.len() == notes.len());
    check("cards: inside the view and in order without overlap", cards.windows(2).all(|p| p[0].origin.y + p[0].size.height <= p[1].origin.y) && cards.iter().all(|c| c.origin.x >= 0.0 && c.origin.x + c.size.width <= width + 0.5));
    // A click on the first card's evidence row, as AppKit delivers it: the card opens and grows.
    let first = notes[0].id.clone();
    let before = cards[0].size.height;
    let toggles = w.cards.toggles();
    let row = toggles.iter().find(|(id, _)| *id == first).map(|(_, r)| *r);
    check("the first card has an evidence row", row.is_some());
    if let Some(r) = row {
        w.window.setAlphaValue(0.0);
        w.window.setIgnoresMouseEvents(false);
        w.window.orderFrontRegardless();
        let local = NSPoint::new(r.origin.x + 30.0, r.origin.y + r.size.height / 2.0);
        let at = w.cards.convertPoint_toView(local, None);
        let n = w.window.windowNumber();
        w.window.sendEvent(&super::island::checks::mouse(NSEventType::LeftMouseDown, at, n));
        w.window.sendEvent(&super::island::checks::mouse(NSEventType::LeftMouseUp, at, n));
        w.window.orderOut(None);
    }
    let after = w.cards.frames().first().map_or(0.0, |c| c.size.height);
    check("a click on the evidence row opens it", w.cards.is_open(&first) && after > before + 40.0);
    // The decider card opened too, for the render.
    if let Some(n) = notes.iter().find(|n| n.class.as_deref() == Some("decider")) {
        w.cards.toggle(&n.id);
    }
    check("bug-notes-expanded: rendered with evidence open", super::agent_render::capture_window(&w.window, &dir.join("bug-notes-expanded.png")));
    w.cards.toggle(&first);
    check("a second click closes it", !w.cards.is_open(&first));
    // A refresh keeps an opened card open while its note is listed.
    let other = notes[3].id.clone();
    w.show(&notes);
    check("a refresh keeps an opened card open", w.cards.is_open(&other));
    let empty = BugNotesWindow::new(mtm, false);
    empty.show(&[]);
    empty.window.setContentSize(NSSize::new(560.0, 360.0));
    check("bug-notes-empty: rendered", super::agent_render::capture_window(&empty.window, &dir.join("bug-notes-empty.png")));
    failures
}
