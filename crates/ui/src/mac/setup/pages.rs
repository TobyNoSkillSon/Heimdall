//! Setup's pages, one builder and one `sync` each, laid out as the prototype's CSS (`lab/setup-wizard/src/index.src.html`):
//! a centred title, then tiles, cards and model cards at most 760 pt wide, 48 pt in from the window's edges. Every
//! number on a page comes from the app's state; the pages only draw it.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSAutoresizingMaskOptions as Mask, NSColor, NSControlTextEditingDelegate, NSImage, NSImageView, NSProgressIndicator,
    NSScrollView, NSSegmentSwitchTracking, NSSegmentedControl, NSTextAlignment, NSTextField, NSTextFieldDelegate, NSView,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint};

use super::kit::*;
use super::kit::t3 as kit_t3;
use super::super::util::{ns, rect, FlippedView};
use super::{size_text, tilde, ProviderStatus, CARD, PAGE_H, SIDE, W};
use crate::identity::{Icon, Identity};
use crate::model::*;
use crate::speech_voice::SpeechVoiceState;

/// Where content starts under a 28-pt title (`h1`: 4 pt page padding, 8 pt above, 22 pt below).
pub(super) const CONTENT_Y: f64 = 68.0;
/// Under the 24-pt two-line question (`h1.q`).
const Q_CONTENT_Y: f64 = 84.0;
const H1_Y: f64 = 12.0;
const TILE: (f64, f64) = (132.0, 96.0);
/// `.row`: card rows; `.row.role` are taller.
const ROW: f64 = 48.0;
const ACT_ROW: f64 = 44.0;
const PERM_ROW: f64 = 64.0;
const MCARD: f64 = 62.0;

pub(super) fn left() -> NSTextAlignment { NSTextAlignment::Left }
pub(super) fn right() -> NSTextAlignment { NSTextAlignment::Right }

fn view<T: objc2::Message>(v: &Retained<T>) -> &NSView
where
    T: AsRef<NSView>,
{
    (**v).as_ref()
}

// ---- page frames ----

/// A page: `view` goes in the window; content is laid on `doc` (flipped, the window's width). Scrolling pages scroll
/// when the window is shorter than their content.
pub(super) struct Frame {
    pub view: Retained<NSView>,
    pub doc: Retained<FlippedView>,
    scroll: Option<Retained<NSScrollView>>,
}

impl Frame {
    pub(super) fn scrolling(mtm: MainThreadMarker) -> Self {
        let frame = rect(0.0, 0.0, W, PAGE_H);
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setBorderType(objc2_app_kit::NSBorderType::NoBorder);
        scroll.setScrollerStyle(objc2_app_kit::NSScrollerStyle::Overlay);
        scroll.contentView().setDrawsBackground(false);
        let doc = FlippedView::new(frame, mtm);
        doc.setAutoresizingMask(Mask::ViewWidthSizable);
        scroll.setDocumentView(Some(&doc));
        scroll.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewHeightSizable);
        let view = Retained::into_super(scroll.clone());
        Self { view, doc, scroll: Some(scroll) }
    }

    pub(super) fn plain(mtm: MainThreadMarker) -> Self {
        let doc = FlippedView::new(rect(0.0, 0.0, W, PAGE_H), mtm);
        doc.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewHeightSizable);
        Self { view: Retained::into_super(doc.clone()), doc, scroll: None }
    }

    /// The scrolled content's height: its last element's bottom and the page's bottom padding.
    pub(super) fn set_height(&self, bottom: f64) {
        if self.scroll.is_none() {
            return;
        }
        let f = self.doc.frame();
        self.doc.setFrame(rect(0.0, 0.0, f.size.width, bottom + 16.0));
    }
}

fn centred(child: &NSView, parent: &NSView, y: f64) {
    let w = parent.frame().size.width;
    let f = child.frame();
    child.setFrameOrigin(NSPoint::new(((w - f.size.width) / 2.0).round(), y));
    child.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
    parent.addSubview(child);
}

/// A card's row label (`.row .lbl`).
fn row_label(parent: &NSView, s: &str, y: f64, h: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let font = regular(14.0);
    text(parent, s, &font, &t2(), 16.0, y + ((h - line_height(&font)) / 2.0).round(), 104.0, left(), mtm)
}

fn check_image(mtm: MainThreadMarker) -> Retained<NSImageView> {
    image_view(rect(0.0, 0.0, 16.0, 16.0), symbol("checkmark.circle.fill", 14.0, 0.0), Some(&hex(GREEN, 1.0)), mtm)
}

/// A provider's mark as a template image: its bundled logo, else the SF Symbol.
pub(super) fn mark_image(mark: &str, size: f64) -> Option<Retained<NSImage>> {
    super::super::menu::provider_image(mark).or_else(|| symbol(mark, size, 0.0))
}

/// Right-aligned run along a row: each visible view placed from `right` leftwards, `gap` apart, centred on the row.
/// Returns the left edge of the run.
fn lay_right(views: &[&NSView], right: f64, h: f64, gap: f64) -> f64 {
    let mut x = right;
    for v in views {
        if v.isHidden() {
            continue;
        }
        let f = v.frame();
        x -= f.size.width;
        v.setFrameOrigin(NSPoint::new(x.round(), ((h - f.size.height) / 2.0).round()));
        v.setAutoresizingMask(Mask::ViewMinXMargin);
        x -= gap;
    }
    x + gap
}

// ---- Welcome ----

pub(super) struct WelcomePage {
    pub frame: Frame,
    pub tile: Retained<AppTile>,
    pub title: Retained<NSTextField>,
}

impl WelcomePage {
    pub(super) fn build(identity: &Identity, mtm: MainThreadMarker) -> Self {
        let frame = Frame::plain(mtm);
        // `.welcome`: the tile and the title centred, 40 pt of padding under them.
        let side = 156.0;
        let title_font = bold(34.0);
        let title_h = line_height(&title_font) + 4.0;
        let group_h = side + 26.0 + title_h;
        let group = FlippedView::new(rect(0.0, ((PAGE_H - 40.0 - group_h) / 2.0).round(), W, group_h), mtm);
        group.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewMinYMargin | Mask::ViewMaxYMargin);
        let tile = AppTile::new(0.0, 0.0, side, (side * 0.225).round(), false, mtm);
        tile.set_icon(&identity.icon);
        let m = tile.margin();
        centred(&tile, &group, -m);
        let title = text(&group, "", &title_font, &t1(), 0.0, side + 26.0, W, NSTextAlignment::Center, mtm);
        title.setAutoresizingMask(Mask::ViewWidthSizable);
        frame.doc.addSubview(&group);
        let page = Self { frame, tile, title };
        page.sync(identity);
        page
    }

    pub(super) fn sync(&self, identity: &Identity) {
        self.tile.set_icon(&identity.icon);
        self.title.setStringValue(&ns(&SetupStep::Welcome.title(&identity.name)));
    }
}

// ---- What should I call you? ----

pub(super) struct NamePage {
    pub frame: Frame,
    pub well: Retained<Plate>,
    pub field: Retained<NSTextField>,
    pub error: Retained<NSTextField>,
}

/// What the name field starts with: the user's name, or nothing while it is the public edition's placeholder "User".
pub(crate) fn user_name_shown(identity: &Identity) -> String {
    if identity.user_name == crate::identity::PUBLIC_USER { String::new() } else { identity.user_name.clone() }
}

impl NamePage {
    pub(super) fn build(identity: &Identity, mtm: MainThreadMarker) -> Self {
        let frame = Frame::plain(mtm);
        // `.page.center`: the title and the field centred, 60 pt of padding under them.
        let group_h = 35.0 + 22.0 + 4.0 + 44.0 + 24.0;
        let group = FlippedView::new(rect(0.0, ((PAGE_H - 60.0 - group_h) / 2.0).round(), W, group_h), mtm);
        group.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewMinYMargin | Mask::ViewMaxYMargin);
        heading(&group, &SetupStep::Name.title(""), 28.0, 0.0, mtm);
        let (well, field) = big_field(&group, rect(((W - 340.0) / 2.0).round(), 61.0, 340.0, 44.0), 20.0, 0.0, "Your name", mtm);
        well.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
        field.setStringValue(&ns(&user_name_shown(identity)));
        let error = text(&group, "", &regular(12.5), &hex(ORANGE, 1.0), 0.0, 61.0 + 44.0 + 8.0, W, NSTextAlignment::Center, mtm);
        error.setAutoresizingMask(Mask::ViewWidthSizable);
        error.setHidden(true);
        frame.doc.addSubview(&group);
        Self { frame, well, field, error }
    }

    pub(super) fn show_error(&self, reason: Option<&str>) {
        self.error.setStringValue(&ns(reason.unwrap_or("")));
        self.error.setHidden(reason.is_none());
    }
}

// ---- Do you have a subscription, or do you want a local model? ----

/// The tiles, in the prototype's order: the subscriptions, then this Mac.
pub(crate) const BRAIN_ORDER: [ProviderChoice; 4] = [ProviderChoice::ChatGpt, ProviderChoice::ClaudeViaT3, ProviderChoice::OpenCodeGo, ProviderChoice::SplashEngine];

/// A provider tile's name: what the user knows it as.
pub(crate) fn tile_title(choice: ProviderChoice) -> &'static str {
    match choice {
        ProviderChoice::ChatGpt => "Codex",
        ProviderChoice::ClaudeViaT3 => "Claude Code",
        ProviderChoice::OpenCodeGo => "OpenCode Go",
        ProviderChoice::SplashEngine => "This Mac",
    }
}

/// The card under the chosen tile: (row label, what it shows, the button when there is something to do).
pub(crate) fn brain_rows(choice: ProviderChoice, status: &ProviderStatus) -> Vec<(&'static str, String, Option<&'static str>)> {
    let mut rows = vec![];
    match choice {
        ProviderChoice::ChatGpt | ProviderChoice::OpenCodeGo => {
            let (value, button) = if let Some(problem) = &status.problem {
                (problem.clone(), Some("Sign In Again…"))
            } else if status.ready {
                ("Signed in".to_string(), None)
            } else if status.checking {
                ("Checking…".to_string(), None)
            } else {
                ("Not signed in".to_string(), Some("Sign In…"))
            };
            rows.push(("Account", value, button));
        }
        ProviderChoice::ClaudeViaT3 => {
            rows.push(("Plan", "Your Claude Code plan, through T3 Code".into(), (!status.ready).then_some("Add")));
        }
        ProviderChoice::SplashEngine => {
            rows.push(("Engine", "Splash, on this Mac".into(), (!status.ready).then_some("Use This Mac")));
            if let Some(model) = &status.model {
                rows.push(("Model", model.clone(), None));
            }
        }
    }
    rows
}

pub(super) struct CardRow {
    pub label: Retained<NSTextField>,
    check: Retained<NSImageView>,
    value: Retained<NSTextField>,
    pub button: Retained<Pad>,
}

pub(super) struct BrainPage {
    pub frame: Frame,
    pub tiles: Vec<(ProviderChoice, Retained<Pad>)>,
    pub band: Retained<Band>,
    pub card: Retained<Plate>,
    pub rows: Vec<CardRow>,
}

impl BrainPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Providers.title(""), 24.0, 4.0, mtm);
        // `.tiles`: the Subscription group, 22 pt, the Local group; each captioned under its tiles.
        let (tw, th) = TILE;
        let groups: [(&str, &[ProviderChoice]); 2] = [("Subscription", &BRAIN_ORDER[..3]), ("Local", &BRAIN_ORDER[3..])];
        let widths: Vec<f64> = groups.iter().map(|(_, g)| g.len() as f64 * tw + (g.len() - 1) as f64 * 10.0).collect();
        let row_w = widths.iter().sum::<f64>() + 22.0;
        let strip = FlippedView::new(rect(0.0, 0.0, row_w, th + 8.0 + 17.0), mtm);
        let mut tiles = vec![];
        let mut x = 0.0;
        for ((caption, group), gw) in groups.iter().zip(&widths) {
            for (i, choice) in group.iter().enumerate() {
                let image = mark_image(choice.mark(), 22.0);
                let tag = ProviderChoice::ALL.iter().position(|c| c == choice).unwrap_or(0) as isize;
                let look = Look::Tile { image, image_side: 26.0, title: tile_title(*choice).into(), caption: None };
                let tile = Pad::new(rect(x + i as f64 * (tw + 10.0), 0.0, tw, th), look, target, sel!(setupProvider:), tag, mtm);
                strip.addSubview(&tile);
                tiles.push((*choice, tile));
            }
            let cap = text(&strip, caption, &regular(12.5), &t3(), x, th + 8.0, *gw, NSTextAlignment::Center, mtm);
            let _ = cap;
            x += gw + 22.0;
        }
        centred(&strip, doc, Q_CONTENT_Y);
        // `.card`, 18 pt under the captions: the chosen provider's rows.
        let card_y = Q_CONTENT_Y + th + 8.0 + 17.0 + 18.0;
        let card = Plate::card(rect(0.0, 0.0, CARD, 8.0 + 2.0 * ROW), mtm);
        let mut rows = vec![];
        for i in 0..2 {
            let y = 4.0 + i as f64 * ROW;
            let label = row_label(&card, "", y, ROW, mtm);
            let check = check_image(mtm);
            check.setFrameOrigin(NSPoint::new(132.0, y + (ROW - 16.0) / 2.0));
            card.addSubview(&check);
            let font = regular(14.0);
            let value = text(&card, "", &font, &t1(), 132.0, y + ((ROW - line_height(&font)) / 2.0).round(), CARD - 132.0 - 150.0, left(), mtm);
            value.setAutoresizingMask(Mask::ViewWidthSizable);
            let button = Pad::button("Sign In…", false, true, 0.0, target, sel!(setupProvider:), mtm);
            button.setFrameOrigin(NSPoint::new(CARD - 16.0 - button.frame().size.width, y + (ROW - 28.0) / 2.0));
            button.setAutoresizingMask(Mask::ViewMinXMargin);
            card.addSubview(&button);
            rows.push(CardRow { label, check, value, button });
        }
        let band = Band::wrap(doc, &card, card_y, CARD, SIDE, mtm);
        band.setHidden(true);
        frame.set_height(card_y + 8.0 + 2.0 * ROW);
        Self { frame, tiles, band, card, rows }
    }

    pub(super) fn sync(&self, chosen: Option<ProviderChoice>, statuses: &[(ProviderChoice, ProviderStatus)]) {
        let status = |c: ProviderChoice| statuses.iter().find(|(s, _)| *s == c).map(|(_, s)| s.clone()).unwrap_or_default();
        for (choice, tile) in &self.tiles {
            tile.set_selected(Some(*choice) == chosen);
            let s = status(*choice);
            let caption = s.ready.then(|| (match choice { ProviderChoice::SplashEngine => "In use", ProviderChoice::ClaudeViaT3 => "Added", _ => "Signed in" }.to_string(), hex(GREEN, 1.0)));
            tile.set_look(Look::Tile { image: mark_image(choice.mark(), 22.0), image_side: 26.0, title: tile_title(*choice).into(), caption });
        }
        let Some(choice) = chosen else {
            self.band.setHidden(true);
            return;
        };
        self.band.setHidden(false);
        let s = status(choice);
        let rows = brain_rows(choice, &s);
        let width = self.band.child_width();
        for (i, row) in self.rows.iter().enumerate() {
            let entry = rows.get(i);
            for v in [view(&row.label), view(&row.value), view(&row.check), view(&row.button)] {
                v.setHidden(entry.is_none());
            }
            let Some((label, value, button)) = entry else { continue };
            row.label.setStringValue(&ns(label));
            row.value.setStringValue(&ns(value));
            // The green check before a value that says it is done.
            let done = s.ready && i == 0;
            row.check.setHidden(!done);
            let vf = row.value.frame();
            let vx = if done { 156.0 } else { 132.0 };
            row.value.setFrame(rect(vx, vf.origin.y, width - vx - 160.0, vf.size.height));
            row.value.setTextColor(Some(&*if s.problem.is_some() && i == 0 { hex(ORANGE, 1.0) } else if s.ready || i > 0 { t1() } else { t2() }));
            match button {
                Some(title) => {
                    row.button.setHidden(false);
                    row.button.retitle(title, 0.0);
                    row.button.setTag(ProviderChoice::ALL.iter().position(|c| *c == choice).unwrap_or(0) as isize);
                    let f = row.button.frame();
                    row.button.setFrameOrigin(NSPoint::new(width - 16.0 - f.size.width, f.origin.y));
                }
                None => row.button.setHidden(true),
            }
        }
        let n = rows.len().max(1) as f64;
        self.card.set_lines((1..rows.len()).map(|i| 4.0 + i as f64 * ROW).collect(), 16.0);
        self.band.set_height(8.0 + n * ROW);
    }
}

// ---- How should I listen? ----

/// Parakeet as Heimdall bundles it (`Resources/models/parakeet-ultra`): 603 MB on disk, measured 4 Nov 2026 in the
/// installed app; encoder INT8, decoder and joint FP16 (lab/setup-wizard/NOTES.md, 28 Oct 19:20).
pub(crate) const PARAKEET_SIZE: &str = "603 MB";
pub(crate) const PARAKEET_DTYPE: &str = "INT8 + FP16";
/// Phonon S as bundled (`models/phonon-31m-en`, `model.q8.gguf`): 71 MB, measured 4 Nov 2026.
pub(crate) const PHONON_SIZE: &str = "71 MB";
pub(crate) const PHONON_DTYPE: &str = "Q8";

/// A model card's right side: the precision chip and the size (`.bigsz`).
fn size_figures(card: &NSView, suggested: bool, dtype: &str, size: &str, right_x: f64, mtm: MainThreadMarker) -> (Retained<NSTextField>, Retained<Plate>, Retained<NSTextField>) {
    let size_font = tabular(17.0, unsafe { objc2_app_kit::NSFontWeightSemibold });
    let size_label = text(card, size, &size_font, &NSColor::whiteColor(), 0.0, 0.0, 0.0, right(), mtm);
    let chip_font = semibold(12.0);
    let chip_w = (super::super::util::text_width(dtype, &chip_font) + 14.0).ceil();
    let chip = Plate::new(rect(0.0, 0.0, chip_w, 22.0), 6.0, mtm);
    chip.set_fill(Some(fill2()));
    let chip_text = text(&chip, dtype, &chip_font, &t1(), 0.0, ((22.0 - line_height(&chip_font)) / 2.0).round(), chip_w, NSTextAlignment::Center, mtm);
    let _ = chip_text;
    card.addSubview(&chip);
    let label = text(card, "Suggested", &semibold(12.5), &hex(YELLOW, 1.0), 0.0, 0.0, 0.0, right(), mtm);
    label.setHidden(!suggested);
    lay_right(&[view(&size_label), view(&chip), view(&label)], right_x, MCARD, 8.0);
    (size_label, chip, chip_text)
}

/// A model card (`.mcard`): its icon, its name and line, and whatever its right side holds.
fn model_card(symbol_name: &str, name: &str, line: &str, mtm: MainThreadMarker) -> (Retained<Plate>, Retained<NSTextField>) {
    let card = Plate::new(rect(0.0, 0.0, CARD, MCARD), 12.0, mtm);
    card.set_selected(false);
    model_icon(&card, symbol_name, 14.0, 11.0, 40.0, mtm);
    let has_line = !line.is_empty();
    let name_font = semibold(14.5);
    let name_y = if has_line { 13.0 } else { ((MCARD - line_height(&name_font)) / 2.0).round() };
    text(&card, name, &name_font, &t1(), 68.0, name_y, 0.0, left(), mtm);
    let line = text(&card, line, &regular(12.5), &t2(), 68.0, 32.0, 360.0, left(), mtm);
    line.setHidden(!has_line);
    (card, line)
}

pub(super) struct ListenPage {
    pub frame: Frame,
    pub seg: Retained<NSSegmentedControl>,
    pub ram: Retained<NSTextField>,
    pub disk: Retained<NSTextField>,
}

/// The decider's precisions as the segmented control shows them: smallest first (the app lists them largest first).
pub(crate) fn decider_segments(decider: &DeciderStatus) -> Vec<&DeciderOption> {
    let mut options: Vec<&DeciderOption> = decider.options.iter().collect();
    options.sort_by_key(|o| o.disk_bytes);
    options
}

impl ListenPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Decider.title(""), 28.0, H1_Y, mtm);
        let tile = Pad::new(rect(0.0, 0.0, TILE.0, TILE.1), Look::Tile { image: symbol("laptopcomputer", 22.0, 0.0), image_side: 26.0, title: "This Mac".into(), caption: None }, target, sel!(setupNothing:), 0, mtm);
        tile.set_selected(true);
        centred(&tile, doc, CONTENT_Y);
        // `.card-area`, 16 pt under the tiles: Parakeet (chosen), then the decider, 8 pt apart.
        let y = CONTENT_Y + TILE.1 + 16.0;
        let (parakeet, _) = model_card("waveform", "Parakeet", "On this Mac. Vella can share it.", mtm);
        parakeet.set_selected(true);
        size_figures(&parakeet, true, PARAKEET_DTYPE, PARAKEET_SIZE, CARD - 14.0, mtm);
        Band::wrap(doc, &parakeet, y, CARD, SIDE, mtm);
        let (decider, _) = model_card("target", "Decider", "", mtm);
        let labels = ["4-bit", "8-bit", "16-bit"].map(ns);
        let refs: Vec<&objc2_foundation::NSString> = labels.iter().map(|s| &**s).collect();
        let seg = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(&objc2_foundation::NSArray::from_slice(&refs), NSSegmentSwitchTracking::SelectOne, Some(target), Some(sel!(setupDecider:)), mtm)
        };
        seg.sizeToFit();
        decider.addSubview(&seg);
        let disk = text(&decider, "0.0 GB", &tabular(17.0, unsafe { objc2_app_kit::NSFontWeightSemibold }), &NSColor::whiteColor(), 0.0, 0.0, 0.0, right(), mtm);
        let ram = text(&decider, "0.0 GB RAM", &tabular(13.0, 0.0), &t3(), 0.0, 0.0, 0.0, right(), mtm);
        Band::wrap(doc, &decider, y + MCARD + 8.0, CARD, SIDE, mtm);
        frame.set_height(y + 2.0 * MCARD + 8.0);
        let page = Self { frame, seg, ram, disk };
        page.lay();
        page
    }

    fn lay(&self) {
        let Some(card) = (unsafe { self.seg.superview() }) else { return };
        lay_right(&[view(&self.disk), view(&self.ram), view(&self.seg)], card.frame().size.width - 14.0, MCARD, 12.0);
    }

    pub(super) fn sync(&self, decider: &DeciderStatus, checked: &str) {
        let options = decider_segments(decider);
        self.seg.setSegmentCount(options.len().max(1) as isize);
        for (i, o) in options.iter().enumerate() {
            let i = i as isize;
            self.seg.setLabel_forSegment(&ns(&o.label), i);
            self.seg.setEnabled_forSegment(o.present, i);
            let tip = if o.present { format!("{} RAM while deciding · {} on disk", size_text(o.memory_bytes), size_text(o.disk_bytes)) } else { format!("{} is not on this Mac", o.label) };
            self.seg.setToolTip_forSegment(Some(&ns(&tip)), i);
        }
        self.seg.setHidden(options.is_empty());
        self.seg.sizeToFit();
        let on = options.iter().position(|o| o.key == checked);
        self.seg.setSelectedSegment(on.map_or(-1, |i| i as isize));
        let pick = on.map(|i| options[i]);
        retext(&self.disk, &pick.map_or(String::new(), |o| size_text(o.disk_bytes)), true);
        retext(&self.ram, &pick.map_or(String::new(), |o| format!("{} RAM", size_text(o.memory_bytes))), true);
        self.lay();
    }
}

// ---- How should I sound? ----

/// An avatar colour per voice: the prototype's palette, the same for a name every time.
fn voice_color(name: &str) -> Retained<NSColor> {
    const COLORS: [u32; 8] = [0x34A853, 0x8E8E2E, 0x9B59B6, 0xC0392B, 0x2E86C1, 0xD35400, 0x16A085, 0x7D3C98];
    let n = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    hex(COLORS[n as usize % COLORS.len()], 1.0)
}

const VOICE_COLUMNS: usize = 5;
const VOICE: (f64, f64) = (0.0, 42.0);

pub(super) struct VoicePage {
    pub frame: Frame,
    pub line: Retained<NSTextField>,
    pub grid: Retained<FlippedView>,
    pub grid_band: Retained<Band>,
    pub tiles: Vec<(String, Retained<Pad>)>,
    target: Retained<AnyObject>,
}

/// The voices the grid offers: the app's everyday list (25 today), else the menu's presets (an older app).
pub(crate) fn voice_list(voice: &SpeechVoiceState) -> &[crate::speech_voice::VoicePreset] {
    if voice.everyday.is_empty() { &voice.presets } else { &voice.everyday }
}

/// The preset the grid shows chosen: Phonon S's, only while Phonon S is the voice.
pub(crate) fn voice_chosen(voice: &SpeechVoiceState) -> Option<&str> {
    (!voice.api && !voice.own && voice.local == crate::speech_voice::LocalVoice::Phonon).then_some(voice.preset.as_str())
}

impl VoicePage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Voice.title(""), 28.0, H1_Y, mtm);
        let (card, line) = model_card("speaker.wave.2.fill", "Phonon S", "Small and fast, on this Mac.", mtm);
        card.set_selected(true);
        size_figures(&card, true, PHONON_DTYPE, PHONON_SIZE, CARD - 14.0, mtm);
        Band::wrap(doc, &card, CONTENT_Y, CARD, SIDE, mtm);
        let grid = FlippedView::new(rect(0.0, 0.0, CARD, VOICE.1), mtm);
        let grid_band = Band::wrap(doc, &grid, CONTENT_Y + MCARD + 12.0, CARD, SIDE, mtm);
        frame.set_height(CONTENT_Y + MCARD + 12.0 + VOICE.1);
        let target = unsafe { Retained::retain(target as *const AnyObject as *mut AnyObject) }.expect("target");
        Self { frame, line, grid, grid_band, tiles: vec![], target }
    }

    pub(super) fn sync(&mut self, voice: &SpeechVoiceState, mtm: MainThreadMarker) {
        let list = voice_list(voice);
        let names: Vec<String> = list.iter().map(|p| p.name.clone()).collect();
        if names != self.tiles.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>() {
            for (_, t) in self.tiles.drain(..) {
                t.removeFromSuperview();
            }
            for (i, p) in list.iter().enumerate() {
                let tile = Pad::new(rect(0.0, 0.0, 10.0, VOICE.1), Look::Voice { color: voice_color(&p.name), name: p.name.clone() }, &self.target, sel!(setupVoice:), i as isize, mtm);
                tile.setToolTip(Some(&ns(&p.desc)));
                tile.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
                self.grid.addSubview(&tile);
                self.tiles.push((p.name.clone(), tile));
            }
            let rows = names.len().div_ceil(VOICE_COLUMNS).max(1);
            self.grid_band.set_height(rows as f64 * VOICE.1 + (rows - 1) as f64 * 7.0);
            self.frame.set_height(CONTENT_Y + MCARD + 12.0 + self.grid_band.frame().size.height);
        }
        self.lay();
        let chosen = voice_chosen(voice);
        for (name, tile) in &self.tiles {
            tile.set_selected(Some(name.as_str()) == chosen);
        }
        let n = list.len();
        self.line.setStringValue(&ns(&if n > 0 { format!("Small and fast, on this Mac. {n} voices.") } else { "Small and fast, on this Mac.".into() }));
        self.line.sizeToFit();
    }

    /// `.vgrid`: five columns, 7 pt apart.
    pub(super) fn lay(&self) {
        let w = self.grid.frame().size.width;
        let cw = (w - (VOICE_COLUMNS - 1) as f64 * 7.0) / VOICE_COLUMNS as f64;
        for (i, (_, tile)) in self.tiles.iter().enumerate() {
            let (r, c) = (i / VOICE_COLUMNS, i % VOICE_COLUMNS);
            tile.setFrame(rect((c as f64 * (cw + 7.0)).round(), r as f64 * (VOICE.1 + 7.0), cw.floor(), VOICE.1));
        }
    }
}

// ---- Allow Heimdall ----

fn permission_look(permission: Permission) -> (&'static str, u32) {
    match permission {
        Permission::Microphone => ("mic.fill", 0xFF9F0A),
        Permission::SystemAudio => ("waveform", 0xFF375F),
        Permission::ScreenRecording => ("rectangle.dashed.badge.record", 0xBF5AF2),
        Permission::Accessibility => ("accessibility", 0x0A84FF),
    }
}

/// Why each permission is needed: the row's tooltip.
fn reason(permission: Permission) -> &'static str {
    match permission {
        Permission::Microphone => "Hears you through every microphone.",
        Permission::SystemAudio => "Hears other apps, such as Discord and Chrome, without recording the screen.",
        Permission::ScreenRecording => "Takes the screenshot when you say “look at this”.",
        Permission::Accessibility => "Reads Discord's speaker names and who is speaking.",
    }
}

/// A permission's state as its row says it: the words, and whether they are green.
pub(crate) fn permission_text(state: PermissionState) -> (&'static str, bool) {
    match state {
        PermissionState::Granted => ("Allowed", true),
        PermissionState::Denied => ("Denied", false),
        PermissionState::NotDetermined => ("Not asked", false),
        PermissionState::Unconfirmed => ("Ready", false),
    }
}

pub(super) struct PermRow {
    pub state: Retained<NSTextField>,
    pub check: Retained<NSImageView>,
    pub open: Retained<Pad>,
}

pub(super) struct AllowPage {
    pub frame: Frame,
    pub title: Retained<NSTextField>,
    pub rows: Vec<PermRow>,
    pub allow: Retained<Pad>,
}

const PERM_CARD: f64 = 620.0;

impl AllowPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        let title = heading(doc, "", 28.0, H1_Y, mtm);
        let n = Permission::ALL.len() as f64;
        let card = Plate::card(rect(0.0, 0.0, PERM_CARD, 8.0 + n * PERM_ROW), mtm);
        let mut rows = vec![];
        for (i, permission) in Permission::ALL.iter().enumerate() {
            let y = 4.0 + i as f64 * PERM_ROW;
            let row = FlippedView::new(rect(0.0, y, PERM_CARD, PERM_ROW), mtm);
            row.setAutoresizingMask(Mask::ViewWidthSizable);
            row.setToolTip(Some(&ns(reason(*permission))));
            let (name, color) = permission_look(*permission);
            let sym = Plate::new(rect(16.0, (PERM_ROW - 36.0) / 2.0, 36.0, 36.0), 9.0, mtm);
            sym.set_fill(Some(hex(color, 1.0)));
            let glyph = image_view(rect(8.0, 8.0, 20.0, 20.0), symbol(name, 16.0, unsafe { objc2_app_kit::NSFontWeightSemibold }), Some(&NSColor::whiteColor()), mtm);
            sym.addSubview(&glyph);
            row.addSubview(&sym);
            let font = medium(15.0);
            let title = text(&row, permission.title(), &font, &t1(), 64.0, ((PERM_ROW - line_height(&font)) / 2.0).round(), 300.0, left(), mtm);
            title.setAutoresizingMask(Mask::ViewWidthSizable);
            let state = text(&row, "Not asked", &regular(13.5), &t3(), 0.0, 0.0, 0.0, right(), mtm);
            let check = check_image(mtm);
            row.addSubview(&check);
            let open = Pad::button("Open Settings", false, true, 0.0, target, sel!(setupOpenSettings:), mtm);
            open.setTag(i as isize);
            row.addSubview(&open);
            card.addSubview(&row);
            rows.push(PermRow { state, check, open });
        }
        card.set_lines((1..Permission::ALL.len()).map(|i| 4.0 + i as f64 * PERM_ROW).collect(), 64.0);
        Band::wrap(doc, &card, CONTENT_Y, PERM_CARD, SIDE, mtm);
        let allow = Pad::button("Allow All", true, false, 110.0, target, sel!(setupAllow:), mtm);
        centred(&allow, doc, CONTENT_Y + 8.0 + n * PERM_ROW + 18.0);
        frame.set_height(CONTENT_Y + 8.0 + n * PERM_ROW + 18.0 + 32.0);
        Self { frame, title, rows, allow }
    }

    pub(super) fn sync(&self, permissions: &[(Permission, PermissionState)], all_granted: bool, requesting: bool) {
        for (row, permission) in self.rows.iter().zip(Permission::ALL) {
            let state = permissions.iter().find(|(p, _)| *p == permission).map(|e| e.1).unwrap_or(PermissionState::NotDetermined);
            let (words, green) = permission_text(state);
            retext(&row.state, words, true);
            row.state.setTextColor(Some(&*if green { t1() } else if state == PermissionState::Denied { hex(ORANGE, 1.0) } else { t3() }));
            // System Audio has no status call: say why it is not "Allowed" yet where the user looks for it.
            let tip = (state == PermissionState::Unconfirmed).then_some("macOS has no status for this one. It shows Allowed once sound arrives from an app in App Audio.");
            row.state.setToolTip(tip.map(ns).as_deref());
            row.check.setHidden(!green);
            row.open.setHidden(state.satisfied());
            let w = unsafe { row.state.superview() }.map_or(PERM_CARD, |v| v.frame().size.width);
            lay_right(&[view(&row.open), view(&row.state), view(&row.check)], w - 18.0, PERM_ROW, 8.0);
        }
        self.allow.setHidden(all_granted);
        self.allow.setEnabled(!requesting);
    }
}

// ---- Home folder ----

pub(super) struct HomePage {
    pub frame: Frame,
    pub band: Retained<Band>,
    pub card: Retained<Plate>,
    pub icon: Retained<NSImageView>,
    pub name: Retained<NSTextField>,
    pub files: Retained<NSTextField>,
    pub pending: Retained<FlippedView>,
    pub pending_line: Retained<NSTextField>,
    pub error: Retained<NSTextField>,
}

const HOME_ROW: f64 = 64.0;

impl HomePage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Home.title(""), 28.0, H1_Y, mtm);
        let card = Plate::card(rect(0.0, 0.0, CARD, 8.0 + HOME_ROW), mtm);
        let icon = image_view(rect(16.0, 4.0 + (HOME_ROW - 40.0) / 2.0, 40.0, 40.0), None, None, mtm);
        card.addSubview(&icon);
        let choose = Pad::button("Choose…", false, true, 0.0, target, sel!(setupChooseHome:), mtm);
        choose.setFrameOrigin(NSPoint::new(CARD - 16.0 - choose.frame().size.width, 4.0 + (HOME_ROW - 28.0) / 2.0));
        choose.setAutoresizingMask(Mask::ViewMinXMargin);
        card.addSubview(&choose);
        let text_w = CARD - 68.0 - choose.frame().size.width - 32.0;
        let name = text(&card, "", &medium(15.0), &t1(), 68.0, 4.0 + 13.0, text_w, left(), mtm);
        name.setAutoresizingMask(Mask::ViewWidthSizable);
        let files = text(&card, "", &regular(12.5), &t2(), 68.0, 4.0 + 35.0, text_w, left(), mtm);
        files.setAutoresizingMask(Mask::ViewWidthSizable);
        // A chosen folder that means moving the files: the size, the restart and the confirm.
        let pending = FlippedView::new(rect(0.0, 4.0 + HOME_ROW, CARD, HOME_ROW), mtm);
        pending.setAutoresizingMask(Mask::ViewWidthSizable);
        pending.addSubview(&image_view(rect(24.0, (HOME_ROW - 24.0) / 2.0, 24.0, 24.0), symbol("arrow.right.circle.fill", 20.0, 0.0), Some(&blue()), mtm));
        let confirm = Pad::button("Move and Restart", true, true, 0.0, target, sel!(setupConfirmMove:), mtm);
        confirm.setFrameOrigin(NSPoint::new(CARD - 16.0 - confirm.frame().size.width, (HOME_ROW - 28.0) / 2.0));
        confirm.setAutoresizingMask(Mask::ViewMinXMargin);
        pending.addSubview(&confirm);
        let line_w = CARD - 68.0 - confirm.frame().size.width - 32.0;
        let pending_line = text(&pending, "", &medium(14.0), &t1(), 68.0, 13.0, line_w, left(), mtm);
        pending_line.setAutoresizingMask(Mask::ViewWidthSizable);
        let restart = text(&pending, "Heimdall restarts to move them.", &regular(12.5), &t2(), 68.0, 35.0, line_w, left(), mtm);
        restart.setAutoresizingMask(Mask::ViewWidthSizable);
        pending.setHidden(true);
        card.addSubview(&pending);
        let error = text(&card, "", &regular(12.5), &hex(ORANGE, 1.0), 16.0, 0.0, CARD - 32.0, left(), mtm);
        error.setAutoresizingMask(Mask::ViewWidthSizable);
        error.setHidden(true);
        let band = Band::wrap(doc, &card, CONTENT_Y, CARD, SIDE, mtm);
        frame.set_height(CONTENT_Y + 8.0 + 2.0 * HOME_ROW + 34.0);
        Self { frame, band, card, icon, name, files, pending, pending_line, error }
    }

    pub(super) fn sync(&self, home: &HomeState) {
        // A folder that is not there yet (a sample in renders) still reads as a folder.
        let icon = if home.home.is_dir() { Some(app_icon(&home.home)) } else { NSImage::imageNamed(unsafe { objc2_app_kit::NSImageNameFolder }) };
        self.icon.setImage(icon.as_deref());
        // The folder's name, as Finder shows it; the full path on hover.
        let name = home.home.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| tilde(&home.home));
        self.name.setStringValue(&ns(&name));
        self.name.setToolTip(Some(&ns(&home.home.to_string_lossy())));
        self.files.setStringValue(&ns(&format!("My files are in {}", tilde(&home.data))));
        let mut h = 8.0 + HOME_ROW;
        let mut lines = vec![];
        match &home.pending {
            Some(m) => {
                self.pending_line.setStringValue(&ns(&format!("Move {} to {}", size_text(m.bytes), tilde(&m.to))));
                self.pending.setHidden(false);
                lines.push(4.0 + HOME_ROW);
                h += HOME_ROW;
            }
            None => self.pending.setHidden(true),
        }
        self.error.setStringValue(&ns(home.error.as_deref().unwrap_or("")));
        self.error.setHidden(home.error.is_none());
        if home.error.is_some() {
            let f = self.error.frame();
            self.error.setFrameOrigin(NSPoint::new(f.origin.x, h - 4.0 + ((34.0 - f.size.height) / 2.0).round() - 4.0));
            lines.push(h - 4.0);
            h += 30.0;
        }
        self.card.set_lines(lines, 16.0);
        self.band.set_height(h);
    }
}

// ---- the status lists: T3 Code and the coding agents (`.acts`) ----

pub(super) struct ActRow {
    pub row: Retained<FlippedView>,
    pub icon: Retained<NSImageView>,
    pub name: Retained<NSTextField>,
    pub shield: Retained<NSImageView>,
    pub state: Retained<NSTextField>,
    pub check: Retained<NSImageView>,
    pub spinner: Retained<NSProgressIndicator>,
    pub button: Retained<Pad>,
}

const ACTS: f64 = 640.0;

fn act_row(card: &NSView, button: &str, primary: bool, target: &AnyObject, action: objc2::runtime::Sel, mtm: MainThreadMarker) -> ActRow {
    let row = FlippedView::new(rect(0.0, 0.0, ACTS, ACT_ROW), mtm);
    row.setAutoresizingMask(Mask::ViewWidthSizable);
    let icon = image_view(rect(14.0, (ACT_ROW - 30.0) / 2.0, 30.0, 30.0), None, Some(&t1()), mtm);
    row.addSubview(&icon);
    let font = medium(14.5);
    let name = text(&row, "", &font, &t1(), 54.0, ((ACT_ROW - line_height(&font)) / 2.0).round(), 300.0, left(), mtm);
    let shield = image_view(rect(0.0, (ACT_ROW - 16.0) / 2.0, 16.0, 16.0), symbol("xmark.shield.fill", 12.0, 0.0), Some(&hex(ORANGE, 1.0)), mtm);
    shield.setHidden(true);
    row.addSubview(&shield);
    let state = text(&row, "", &regular(13.5), &t2(), 0.0, 0.0, 0.0, right(), mtm);
    let check = check_image(mtm);
    row.addSubview(&check);
    let spinner = spinner(rect(0.0, 0.0, 16.0, 16.0), mtm);
    row.addSubview(&spinner);
    let button = Pad::button(button, primary, true, 0.0, target, action, mtm);
    row.addSubview(&button);
    card.addSubview(&row);
    ActRow { row, icon, name, shield, state, check, spinner, button }
}

impl ActRow {
    /// Places the right side (button, state words, check or spinner) and gives the name what is left.
    fn lay(&self, name: &str, dim: Option<&str>) {
        let w = self.row.frame().size.width;
        let left_edge = lay_right(&[view(&self.button), view(&self.state), view(&self.check), view(&self.spinner)], w - 14.0, ACT_ROW, 8.0);
        // `.name .dim`: the version after the name, quieter.
        let s = match dim {
            Some(d) => {
                let full = format!("{name}  {d}");
                let base = super::super::util::attributed(&full, &medium(14.5), &t1());
                let attr = objc2_foundation::NSMutableAttributedString::initWithAttributedString(objc2_foundation::NSMutableAttributedString::alloc(), &base);
                let start = name.encode_utf16().count() + 2;
                let range = objc2_foundation::NSRange::new(start, d.encode_utf16().count());
                let (dim, font) = (t3(), regular(14.5));
                unsafe {
                    attr.addAttribute_value_range(objc2_app_kit::NSForegroundColorAttributeName, AsRef::<AnyObject>::as_ref(&*dim), range);
                    attr.addAttribute_value_range(objc2_app_kit::NSFontAttributeName, AsRef::<AnyObject>::as_ref(&*font), range);
                }
                self.name.setAttributedStringValue(&attr);
                full
            }
            None => {
                self.name.setStringValue(&ns(name));
                name.to_string()
            }
        };
        let _ = s;
        let x = if self.icon.isHidden() && self.icon.image().is_none() { 16.0 } else { 54.0 };
        let f = self.name.frame();
        let natural = self.name.attributedStringValue().size().width.ceil() + 4.0;
        let room = (left_edge - 10.0 - x).max(40.0);
        self.name.setFrame(rect(x, f.origin.y, natural.min(room), f.size.height));
        let nf = self.name.frame();
        self.shield.setFrameOrigin(NSPoint::new(nf.origin.x + nf.size.width + 4.0, ((ACT_ROW - 16.0) / 2.0).round()));
    }

    fn set_state(&self, words: Option<(&str, Retained<NSColor>)>, done: bool, working: bool, button: Option<&str>) {
        match words {
            Some((w, color)) => {
                self.state.setHidden(false);
                retext(&self.state, w, true);
                self.state.setTextColor(Some(&color));
            }
            None => self.state.setHidden(true),
        }
        self.check.setHidden(!done);
        self.spinner.setHidden(!working);
        if working { unsafe { self.spinner.startAnimation(None) } } else { unsafe { self.spinner.stopAnimation(None) } }
        match button {
            Some(title) => {
                self.button.setHidden(false);
                self.button.retitle(title, 0.0);
            }
            None => self.button.setHidden(true),
        }
    }
}

/// Lays visible rows top-down in a card; returns the card's height.
fn stack_rows(card: &Plate, rows: &[&ActRow]) -> f64 {
    let mut y = 4.0;
    let mut lines = vec![];
    for (i, r) in rows.iter().filter(|r| !r.row.isHidden()).enumerate() {
        if i > 0 {
            lines.push(y);
        }
        let f = r.row.frame();
        r.row.setFrame(rect(0.0, y, f.size.width, ACT_ROW));
        y += ACT_ROW;
    }
    card.set_lines(lines, 54.0);
    y + 4.0
}

pub(super) struct T3Page {
    pub frame: Frame,
    pub band: Retained<Band>,
    pub card: Retained<Plate>,
    pub found: ActRow,
    pub install: ActRow,
    pub connect: ActRow,
}

/// T3 Code's connect row: dimmed until T3 Code is here (the prototype's pending rows), then Connect, then done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectRow {
    Pending,
    Ask,
    Done,
}

/// The T3 Code list's rows as words: (found line, its version, its state), whether the install row shows and what it
/// says, whether the connect row shows and whether it is done.
pub(crate) struct T3Rows {
    pub found: String,
    pub version: Option<String>,
    pub found_ok: bool,
    pub found_warning: Option<&'static str>,
    pub install: Option<(Option<String>, Option<String>, Option<&'static str>)>,
    pub connect: ConnectRow,
}

pub(crate) fn t3_rows(t3: &T3Setup) -> T3Rows {
    let ready = t3.ready();
    let (found, version, found_ok, found_warning) = match &t3.found {
        Some(app) => (format!("Found {}", app.name), Some(app.version.clone()), app.supports_v2, (!app.supports_v2).then_some("Needs 0.1.0 or later")),
        None => ("T3 Code isn’t installed yet".into(), None, false, None),
    };
    // (working stage, failure, button)
    let install = (!ready).then(|| match &t3.install {
        T3Install::Working(stage) => (Some(stage.clone()), None, None),
        T3Install::Failed(why) => (None, Some(why.clone()), Some("Download Page")),
        T3Install::Idle => (None, None, Some("Install")),
    });
    T3Rows { found, version, found_ok, found_warning, install, connect: if !ready { ConnectRow::Pending } else if t3.connect_allowed { ConnectRow::Done } else { ConnectRow::Ask } }
}

impl T3Page {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::T3.title(""), 28.0, H1_Y, mtm);
        let card = Plate::card(rect(0.0, 0.0, ACTS, 8.0 + 3.0 * ACT_ROW), mtm);
        let found = act_row(&card, "Install", false, target, sel!(setupT3:), mtm);
        let install = act_row(&card, "Install", false, target, sel!(setupT3:), mtm);
        install.icon.setImage(symbol("arrow.down.circle", 20.0, 0.0).as_deref());
        install.icon.setContentTintColor(Some(&t2()));
        let connect = act_row(&card, "Connect", true, target, sel!(setupT3:), mtm);
        connect.icon.setImage(symbol("point.3.connected.trianglepath.dotted", 20.0, 0.0).as_deref());
        connect.icon.setContentTintColor(Some(&t2()));
        let band = Band::wrap(doc, &card, CONTENT_Y, ACTS, SIDE, mtm);
        frame.set_height(CONTENT_Y + 8.0 + 3.0 * ACT_ROW);
        Self { frame, band, card, found, install, connect }
    }

    pub(super) fn sync(&self, state: &T3Setup) {
        let t3 = state;
        let rows = t3_rows(t3);
        match &t3.found {
            Some(app) => {
                self.found.icon.setImage(Some(&app_icon(&app.path)));
                self.found.icon.setContentTintColor(None);
            }
            None => {
                self.found.icon.setImage(symbol("app.dashed", 22.0, 0.0).as_deref());
                self.found.icon.setContentTintColor(Some(&kit_t3()));
            }
        }
        let warning = rows.found_warning.map(|w| (w, hex(ORANGE, 1.0)));
        self.found.set_state(warning, rows.found_ok, false, None);
        self.found.name.setTextColor(Some(&*if t3.found.is_some() { t1() } else { t2() }));
        self.found.lay(&rows.found, rows.version.as_deref());
        self.install.row.setHidden(rows.install.is_none());
        if rows.install.is_none() {
            // Hidden: nothing of it animates.
            self.install.set_state(None, false, false, None);
        }
        if let Some((working, failed, button)) = &rows.install {
            let words = working.as_deref().map(|w| (w, t2())).or(failed.as_deref().map(|f| (f, hex(ORANGE, 1.0))));
            self.install.set_state(words, false, working.is_some(), *button);
            self.install.lay("Install T3 Code", None);
        }
        // `.acts .row.pending`: the step after the install shows, dimmed, until it can happen.
        let done = rows.connect == ConnectRow::Done;
        self.connect.row.setAlphaValue(if rows.connect == ConnectRow::Pending { 0.4 } else { 1.0 });
        self.connect.set_state(done.then(|| ("Connected", t1())), done, false, (rows.connect == ConnectRow::Ask).then_some("Connect"));
        self.connect.lay("Connect to T3 Code", None);
        let h = stack_rows(&self.card, &[&self.found, &self.install, &self.connect]);
        self.band.set_height(h);
    }
}

/// The most harnesses `harness::ensure` finds: Claude Code, T3 Code, Codex, OpenCode, Grok.
pub(super) const HARNESS_SLOTS: usize = 5;
pub(super) const DENY_LINES: usize = 4;
const DENY_ROW: f64 = 28.0;

/// A harness's mark: the provider's own where Heimdall ships it, else a symbol.
fn harness_mark(name: &str) -> &'static str {
    match name {
        "Claude Code" => "anthropic",
        "Codex" => "codex",
        "OpenCode" => "opencode",
        "T3 Code" => "point.3.connected.trianglepath.dotted",
        _ => "terminal",
    }
}

pub(super) struct HarnessPage {
    pub frame: Frame,
    pub band: Retained<Band>,
    pub card: Retained<Plate>,
    pub rows: Vec<ActRow>,
    pub none: ActRow,
    pub deny_band: Retained<Band>,
    pub deny: Vec<(Retained<NSImageView>, Retained<NSTextField>)>,
}

impl HarnessPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Harnesses.title(""), 28.0, H1_Y, mtm);
        let card = Plate::card(rect(0.0, 0.0, ACTS, 8.0 + HARNESS_SLOTS as f64 * ACT_ROW), mtm);
        let rows: Vec<ActRow> = (0..HARNESS_SLOTS).map(|_| act_row(&card, "", false, target, sel!(setupNothing:), mtm)).collect();
        let none = act_row(&card, "", false, target, sel!(setupNothing:), mtm);
        none.icon.setImage(None);
        none.icon.setHidden(true);
        none.set_state(None, false, false, None);
        none.name.setTextColor(Some(&t2()));
        none.lay("None on this Mac", None);
        let band = Band::wrap(doc, &card, CONTENT_Y, ACTS, SIDE, mtm);
        // The deny list, in two columns under the list.
        let deny_card = Plate::card(rect(0.0, 0.0, ACTS, 12.0 + 2.0 * DENY_ROW), mtm);
        let mut deny = vec![];
        for i in 0..DENY_LINES {
            let (col, line) = (i / 2, i % 2);
            let x = 16.0 + col as f64 * ACTS / 2.0;
            let y = 6.0 + line as f64 * DENY_ROW;
            let icon = image_view(rect(x, y + (DENY_ROW - 16.0) / 2.0, 16.0, 16.0), symbol("xmark.shield.fill", 12.0, 0.0), Some(&hex(ORANGE, 1.0)), mtm);
            deny_card.addSubview(&icon);
            let font = regular(12.5);
            let label = text(&deny_card, "", &font, &t2(), x + 24.0, y + ((DENY_ROW - line_height(&font)) / 2.0).round(), ACTS / 2.0 - 48.0, left(), mtm);
            if col == 1 {
                icon.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
                label.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
            }
            deny.push((icon, label));
        }
        let deny_band = Band::wrap(doc, &deny_card, CONTENT_Y + 8.0 + HARNESS_SLOTS as f64 * ACT_ROW + 12.0, ACTS, SIDE, mtm);
        frame.set_height(CONTENT_Y + 8.0 + HARNESS_SLOTS as f64 * ACT_ROW + 12.0 + 12.0 + 2.0 * DENY_ROW);
        Self { frame, band, card, rows, none, deny_band, deny }
    }

    pub(super) fn sync(&self, harnesses: &HarnessSetup) {
        for (n, row) in self.rows.iter().enumerate() {
            let entry = harnesses.rows.get(n);
            row.row.setHidden(entry.is_none());
            let Some(h) = entry else { continue };
            row.icon.setImage(mark_image(harness_mark(&h.name), 18.0).as_deref());
            row.icon.setContentTintColor(Some(&t1()));
            row.shield.setHidden(!h.guarded);
            row.set_state(Some(if h.ready { ("Never asks", t1()) } else { ("Asks first", t3()) }), h.ready, false, None);
            row.lay(&h.name, None);
        }
        self.none.row.setHidden(!harnesses.rows.is_empty());
        let mut list: Vec<&ActRow> = self.rows.iter().collect();
        list.push(&self.none);
        let h = stack_rows(&self.card, &list);
        self.band.set_height(h);
        // Shown only when a guarded harness is: the others are not held to it.
        let guarded = harnesses.rows.iter().any(|r| r.guarded);
        self.deny_band.setHidden(!guarded || harnesses.deny.is_empty());
        let f = self.deny_band.frame();
        self.deny_band.setFrameOrigin(NSPoint::new(f.origin.x, CONTENT_Y + h + 12.0));
        for (n, (icon, label)) in self.deny.iter().enumerate() {
            let item = harnesses.deny.get(n);
            icon.setHidden(item.is_none());
            label.setHidden(item.is_none());
            label.setStringValue(&ns(item.map_or("", String::as_str)));
        }
    }
}

// ---- Do you want dictation with Vella? ----

/// What the Vella page offers: the add tile's words, and whether "Not now" is a choice (only while Vella is missing).
pub(crate) fn vella_tiles(vella: &VellaSetup) -> (&'static str, bool) {
    if vella.found.is_some() {
        ("Use it", false)
    } else if vella.brew {
        ("Add Vella", true)
    } else {
        ("Get Vella…", true)
    }
}

pub(super) struct VellaPage {
    pub frame: Frame,
    pub add: Retained<Pad>,
    pub not_now: Retained<Pad>,
    pub icon: Retained<NSImageView>,
    pub icon_plate: Retained<Plate>,
    pub status: Retained<NSTextField>,
    pub spinner: Retained<NSProgressIndicator>,
    /// Under the card: the install's stage beside the spinner, or why it failed.
    pub progress: Retained<NSTextField>,
    pub diagram_icon: Retained<NSImageView>,
    pub diagram_heimdall: Retained<AppTile>,
    pub card: Retained<Plate>,
}

const VELLA_HEAD: f64 = 80.0;
/// Vella 2.1.2 on disk (`du`, 4 Nov 2026, the same units as Parakeet's and Phonon S's sizes).
pub(crate) const VELLA_SIZE: &str = "32 MB";
const VELLA_DIAGRAM: f64 = 74.0;

impl VellaPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Vella.title(""), 28.0, H1_Y, mtm);
        let strip = FlippedView::new(rect(0.0, 0.0, 2.0 * TILE.0 + 10.0, TILE.1), mtm);
        let add = Pad::new(rect(0.0, 0.0, TILE.0, TILE.1), Look::Tile { image: None, image_side: 30.0, title: "Add Vella".into(), caption: None }, target, sel!(setupVella:), 0, mtm);
        let not_now = Pad::new(rect(TILE.0 + 10.0, 0.0, TILE.0, TILE.1), Look::Tile { image: symbol("minus", 18.0, 0.0), image_side: 22.0, title: "Not now".into(), caption: None }, target, sel!(setupVella:), 1, mtm);
        strip.addSubview(&add);
        strip.addSubview(&not_now);
        centred(&strip, doc, CONTENT_Y);
        let card = Plate::card(rect(0.0, 0.0, CARD, VELLA_HEAD + VELLA_DIAGRAM), mtm);
        // `.addon-head`: the app's icon, its name and line, and its state at the right.
        let icon_plate = Plate::new(rect(16.0, 14.0, 52.0, 52.0), 12.0, mtm);
        icon_plate.set_fill(Some(hex(0x141418, 1.0)));
        icon_plate.set_ring(Some((white(0.12), 0.5)));
        let glyph = image_view(rect(12.0, 12.0, 28.0, 28.0), symbol("waveform", 22.0, 0.0), Some(&hex(0xB9A8FF, 1.0)), mtm);
        icon_plate.addSubview(&glyph);
        card.addSubview(&icon_plate);
        let icon = image_view(rect(16.0, 14.0, 52.0, 52.0), None, None, mtm);
        card.addSubview(&icon);
        let head = text(&card, "Dictate into any app, reusing the speech model I already run.", &semibold(15.0), &t1(), 82.0, 20.0, CARD - 82.0 - 170.0, left(), mtm);
        head.setAutoresizingMask(Mask::ViewWidthSizable);
        let line = text(&card, "A dictation app: hold a key and talk into any text field.", &regular(13.0), &t2(), 82.0, 41.0, CARD - 82.0 - 170.0, left(), mtm);
        line.setAutoresizingMask(Mask::ViewWidthSizable);
        // The right slot: the size, or the version found. An install's progress goes under the card.
        let status = text(&card, "", &regular(13.0), &t3(), 0.0, 22.0, 0.0, right(), mtm);
        status.setAutoresizingMask(Mask::ViewMinXMargin);
        // `.share`: Heimdall, a blue line, "Parakeet, shared", a blue line, Vella.
        let pill_font = medium(12.5);
        let pill_w = (super::super::util::text_width("Parakeet, shared", &pill_font) + 24.0).ceil();
        let diagram_w = 46.0 + 70.0 + pill_w + 70.0 + 46.0;
        let diagram = FlippedView::new(rect(0.0, VELLA_HEAD, diagram_w, VELLA_DIAGRAM - 18.0), mtm);
        let heimdall = AppTile::new(0.0, 5.0, 46.0, 11.0, false, mtm);
        heimdall.set_mark_share(0.6);
        diagram.addSubview(&heimdall);
        let line_at = |x: f64| {
            let l = Plate::new(rect(x, 5.0 + 22.0, 70.0, 2.0), 0.0, mtm);
            l.set_fill(Some(blue()));
            diagram.addSubview(&l);
        };
        line_at(46.0);
        let pill = Plate::new(rect(116.0, 5.0 + 23.0 - 15.0, pill_w, 30.0), 15.0, mtm);
        pill.set_fill(Some(hex(BLUE, 0.16)));
        text(&pill, "Parakeet, shared", &pill_font, &NSColor::whiteColor(), 0.0, ((30.0 - line_height(&pill_font)) / 2.0).round(), pill_w, NSTextAlignment::Center, mtm);
        diagram.addSubview(&pill);
        line_at(116.0 + pill_w);
        let vella_tile = Plate::new(rect(116.0 + pill_w + 70.0, 5.0, 46.0, 46.0), 11.0, mtm);
        vella_tile.set_fill(Some(hex(0x141418, 1.0)));
        vella_tile.set_ring(Some((white(0.12), 0.5)));
        let glyph = image_view(rect(11.0, 11.0, 24.0, 24.0), symbol("waveform", 18.0, 0.0), Some(&hex(0xB9A8FF, 1.0)), mtm);
        vella_tile.addSubview(&glyph);
        diagram.addSubview(&vella_tile);
        let diagram_icon = image_view(rect(116.0 + pill_w + 70.0, 5.0, 46.0, 46.0), None, None, mtm);
        diagram.addSubview(&diagram_icon);
        diagram.setFrameOrigin(NSPoint::new(((CARD - diagram_w) / 2.0).round(), VELLA_HEAD));
        diagram.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
        card.addSubview(&diagram);
        Band::wrap(doc, &card, CONTENT_Y + TILE.1 + 18.0, CARD, SIDE, mtm);
        let progress_y = CONTENT_Y + TILE.1 + 18.0 + VELLA_HEAD + VELLA_DIAGRAM + 14.0;
        let progress_row = FlippedView::new(rect(0.0, 0.0, 10.0, 20.0), mtm);
        let spinner = spinner(rect(0.0, 2.0, 16.0, 16.0), mtm);
        progress_row.addSubview(&spinner);
        let progress = text(&progress_row, "", &regular(13.0), &t2(), 24.0, 1.0, 0.0, left(), mtm);
        centred(&progress_row, doc, progress_y);
        frame.set_height(progress_y + 20.0);
        Self { frame, add, not_now, icon, icon_plate, status, spinner, progress, diagram_icon, diagram_heimdall: heimdall, card }
    }

    /// `add`: Add Vella is the choice (the default; Not now clears it). Installed, "Use it" is the only tile.
    pub(super) fn sync(&self, vella: &VellaSetup, add: bool, identity: &Identity) {
        let (add_title, offers_not_now) = vella_tiles(vella);
        let app = vella.found.as_ref().map(|a| app_icon(&a.path));
        let caption = Some(("Suggested".to_string(), t3()));
        self.add.set_look(Look::Tile { image: app.clone().or_else(|| symbol("waveform", 22.0, 0.0)), image_side: if app.is_some() { 34.0 } else { 24.0 }, title: add_title.into(), caption });
        self.not_now.setHidden(!offers_not_now);
        // One tile when Vella is here: centred alone.
        if let Some(strip) = unsafe { self.add.superview() } {
            let w = if offers_not_now { 2.0 * TILE.0 + 10.0 } else { TILE.0 };
            let f = strip.frame();
            let pw = unsafe { strip.superview() }.map_or(W, |p| p.frame().size.width);
            strip.setFrame(rect(((pw - w) / 2.0).round(), f.origin.y, w, f.size.height));
        }
        let working = matches!(vella.install, T3Install::Working(_));
        self.add.set_selected(vella.found.is_some() || add);
        self.not_now.set_selected(vella.found.is_none() && !add);
        self.icon.setImage(app.as_deref());
        self.icon.setHidden(app.is_none());
        self.icon_plate.setHidden(app.is_some());
        self.diagram_icon.setImage(app.as_deref());
        self.diagram_icon.setHidden(app.is_none());
        self.diagram_heimdall.set_icon(&identity.icon);
        let words = match &vella.found {
            Some(a) => format!("Found Vella {}", a.version),
            None => VELLA_SIZE.into(),
        };
        let w = self.card.frame().size.width;
        retext(&self.status, &words, false);
        let sf = self.status.frame();
        self.status.setFrame(rect(w - 16.0 - sf.size.width, 22.0, sf.size.width, sf.size.height));
        // Under the card: the install's stage with the spinner, or why it failed; nothing at rest.
        let (line, color) = match (&vella.found, &vella.install) {
            (None, T3Install::Working(stage)) => (stage.clone(), t2()),
            (None, T3Install::Failed(why)) => (why.clone(), hex(ORANGE, 1.0)),
            _ => (String::new(), t2()),
        };
        retext(&self.progress, &line, false);
        self.progress.setTextColor(Some(&color));
        self.progress.setHidden(line.is_empty());
        self.spinner.setHidden(!working);
        if working { unsafe { self.spinner.startAnimation(None) } } else { unsafe { self.spinner.stopAnimation(None) } }
        if let Some(row) = unsafe { self.progress.superview() } {
            let pw = (self.progress.frame().size.width.min(W - 2.0 * SIDE - 24.0)).max(0.0);
            let x = if working { 24.0 } else { 0.0 };
            self.progress.setFrame(rect(x, 1.0, pw, self.progress.frame().size.height));
            let rw = x + pw;
            let f = row.frame();
            let parent_w = unsafe { row.superview() }.map_or(W, |p| p.frame().size.width);
            row.setFrame(rect(((parent_w - rw) / 2.0).round(), f.origin.y, rw, 20.0));
        }
    }
}

// ---- Customise me ----

pub(super) struct CustomPage {
    pub frame: Frame,
    pub hero: Retained<AppTile>,
    pub well: Retained<Plate>,
    pub field: Retained<NSTextField>,
    pub error: Retained<NSTextField>,
    pub marks_strip: Retained<FlippedView>,
    pub marks: Vec<(Icon, Retained<Pad>)>,
    pub wake: Retained<NSTextField>,
    /// The wake word, "Send it", and the voice, left to right.
    pub chips: Vec<Retained<Plate>>,
    pub voice: Retained<NSTextField>,
    pub preview: Retained<Preview>,
    target: Retained<AnyObject>,
    pub watch: Retained<FieldWatch>,
}

const MARK: f64 = 40.0;
const MARK_GAP: f64 = 8.0;
const HERO: f64 = 132.0;

/// The greeting the island will say: "Hi Ada. I'm Heimdall.", or without a name the user never gave.
pub(crate) fn greeting(user: &str, name: &str) -> String {
    if user.is_empty() || user == crate::identity::PUBLIC_USER { format!("Hi. I’m {name}.") } else { format!("Hi {user}. I’m {name}.") }
}

fn wake_chip(parent: &NSView, symbol_name: &str, words: &str, mtm: MainThreadMarker) -> (Retained<Plate>, Retained<NSTextField>) {
    let font = medium(14.0);
    let chip = Plate::new(rect(0.0, 0.0, 100.0, 32.0), 16.0, mtm);
    chip.set_fill(Some(fill2()));
    chip.addSubview(&image_view(rect(12.0, 9.0, 14.0, 14.0), symbol(symbol_name, 12.0, 0.0), Some(&white(0.85)), mtm));
    let label = text(&chip, words, &font, &NSColor::whiteColor(), 32.0, ((32.0 - line_height(&font)) / 2.0).round(), 0.0, left(), mtm);
    let w = 32.0 + label.frame().size.width + 14.0;
    chip.setFrame(rect(0.0, 0.0, w.ceil(), 32.0));
    parent.addSubview(&chip);
    (chip, label)
}

impl CustomPage {
    pub(super) fn build(target: &AnyObject, identity: &Identity, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Customise.title(""), 28.0, H1_Y, mtm);
        let mut y = CONTENT_Y;
        let hero = AppTile::new(0.0, 0.0, HERO, 38.0, true, mtm);
        hero.set_mark_share(0.5);
        centred(&hero, doc, y - hero.margin());
        y += HERO + 14.0;
        let (well, field) = big_field(doc, rect(((W - 260.0) / 2.0).round(), y, 260.0, 42.0), 22.0, unsafe { objc2_app_kit::NSFontWeightSemibold }, "Name", mtm);
        well.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
        let watch = FieldWatch::new(mtm);
        // The page keeps `watch` (the delegate is weak).
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&*watch))) };
        let error = text(doc, "", &regular(12.5), &hex(ORANGE, 1.0), 0.0, y + 44.0, W, NSTextAlignment::Center, mtm);
        error.setAutoresizingMask(Mask::ViewWidthSizable);
        error.setHidden(true);
        y += 42.0 + 18.0;
        let marks_strip = FlippedView::new(rect(0.0, 0.0, 10.0, MARK), mtm);
        centred(&marks_strip, doc, y);
        y += MARK + 18.0;
        let wakes = FlippedView::new(rect(0.0, 0.0, 10.0, 32.0), mtm);
        let (chip, wake) = wake_chip(&wakes, "mic.fill", "“Heimdall”", mtm);
        let (send, _) = wake_chip(&wakes, "paperplane.fill", "“Send it”", mtm);
        let (voice_chip, voice) = wake_chip(&wakes, "speaker.wave.2.fill", "Edwin", mtm);
        centred(&wakes, doc, y);
        y += 32.0 + 18.0;
        let preview = Preview::new(rect(0.0, 0.0, 420.0, 74.0), mtm);
        centred(&preview, doc, y);
        frame.set_height(y + 74.0);
        let target = unsafe { Retained::retain(target as *const AnyObject as *mut AnyObject) }.expect("target");
        let mut page = Self { frame, hero, well, field, error, marks_strip, marks: vec![], wake, chips: vec![chip, send, voice_chip], voice, preview, target, watch };
        page.sync(identity, mtm);
        page.field.setStringValue(&ns(&identity.name));
        page
    }

    pub(super) fn sync(&mut self, identity: &Identity, mtm: MainThreadMarker) {
        let icons = super::super::personalise::grid_icons(&identity.icon);
        if icons != self.marks.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>() {
            for (_, p) in self.marks.drain(..) {
                p.removeFromSuperview();
            }
            for (i, icon) in icons.iter().enumerate() {
                let pad = Pad::new(rect(i as f64 * (MARK + MARK_GAP), 0.0, MARK, MARK), Look::Mark(icon.clone()), &self.target, sel!(setupMark:), i as isize, mtm);
                self.marks_strip.addSubview(&pad);
                self.marks.push((icon.clone(), pad));
            }
            let w = icons.len() as f64 * (MARK + MARK_GAP) - MARK_GAP;
            let f = self.marks_strip.frame();
            let pw = unsafe { self.marks_strip.superview() }.map_or(W, |p| p.frame().size.width);
            self.marks_strip.setFrame(rect(((pw - w) / 2.0).round(), f.origin.y, w, MARK));
        }
        for (icon, pad) in &self.marks {
            pad.set_selected(*icon == identity.icon);
        }
        self.hero.set_icon(&identity.icon);
        // The field's text, which may be ahead of the saved name while it is typed.
        let typed = self.field.stringValue().to_string();
        self.preview_name(identity, &typed);
    }

    /// The name as typed so far: the wake word and the island greeting follow it before it is committed.
    pub(super) fn preview_name(&self, identity: &Identity, typed: &str) {
        let name = if typed.trim().is_empty() { identity.name.as_str() } else { typed.trim() };
        self.wake.setStringValue(&ns(&format!("“{name}”")));
        self.lay_chips();
        self.preview.set(&identity.icon, &greeting(&identity.user_name, name));
    }

    pub(super) fn show_error(&self, reason: Option<&str>) {
        self.error.setStringValue(&ns(reason.unwrap_or("")));
        self.error.setHidden(reason.is_none());
    }

    /// The voice chip: the voice as it speaks now (none known yet: the chip hides).
    pub(super) fn set_voice(&self, name: Option<&str>) {
        self.voice.setStringValue(&ns(name.unwrap_or("")));
        self.chips[2].setHidden(name.is_none());
        self.lay_chips();
    }

    /// `.wakes`: the chips sized to their words, 10 pt apart, the row centred.
    fn lay_chips(&self) {
        let mut x = 0.0;
        for chip in self.chips.iter().filter(|c| !c.isHidden()) {
            let label = chip.subviews().iter().filter_map(|v| v.downcast::<NSTextField>().ok()).next();
            let lw = label.as_ref().map_or(0.0, |l| {
                l.sizeToFit();
                l.frame().size.width
            });
            let w = (32.0 + lw + 14.0).ceil();
            chip.setFrame(rect(x, 0.0, w, 32.0));
            x += w + 10.0;
        }
        let w = (x - 10.0).max(0.0);
        if let Some(wakes) = unsafe { self.chips[0].superview() } {
            let f = wakes.frame();
            let pw = unsafe { wakes.superview() }.map_or(W, |p| p.frame().size.width);
            wakes.setFrame(rect(((pw - w) / 2.0).round(), f.origin.y, w, 32.0));
        }
    }
}

/// The voice as the user hears it now: Phonon S's preset, the ElevenLabs or own-engine voice, or the local model.
pub(crate) fn voice_name(voice: &SpeechVoiceState) -> Option<String> {
    use crate::speech_voice::LocalVoice;
    if voice.own {
        return voice.own_engine.selected.as_ref().map(|(_, n)| n.clone());
    }
    if voice.api {
        return voice.elevenlabs.selected.as_ref().map(|(_, n)| n.clone());
    }
    match voice.local {
        LocalVoice::Phonon => (!voice.preset.is_empty()).then(|| voice.preset.clone()),
        other => Some(other.title().to_string()),
    }
}

define_class!(
    /// The name fields' delegate: typing on Customise updates the wake word, the greeting and Start before it is
    /// committed; on both pages the well's ring follows editing.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallSetupFieldWatch"]
    pub struct FieldWatch;

    unsafe impl NSObjectProtocol for FieldWatch {}

    unsafe impl NSControlTextEditingDelegate for FieldWatch {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _note: &NSNotification) {
            super::super::with_state(|s| if let Some(w) = s.setup.as_mut() { w.name_typed() });
        }

        // After the event: by then the field editor is attached or gone, so the rings read it right.
        #[unsafe(method(controlTextDidBeginEditing:))]
        fn control_text_did_begin_editing(&self, _note: &NSNotification) {
            super::super::with_state_after_event(|s| if let Some(w) = s.setup.as_mut() { w.sync_focus() });
        }

        #[unsafe(method(controlTextDidEndEditing:))]
        fn control_text_did_end_editing(&self, _note: &NSNotification) {
            super::super::with_state_after_event(|s| if let Some(w) = s.setup.as_mut() { w.sync_focus() });
        }
    }

    unsafe impl NSTextFieldDelegate for FieldWatch {}
);

impl FieldWatch {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}
