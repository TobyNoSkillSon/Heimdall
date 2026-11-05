//! Do you want Arsenal? (public edition): Add Arsenal (or Use it, when it is here) and Not now, then a card with
//! Arsenal's icon, its line, its state at the right, and the eight kinds of model it lends Heimdall, as the prototype
//! draws it (`lab/ui-renders/setup-wizard-prototype/12-arsenal.png`). Continue acts on the choice
//! (`super::leaving_arsenal`); an install's stage or failure shows under the card.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSAutoresizingMaskOptions as Mask, NSImageView, NSProgressIndicator, NSTextAlignment, NSTextField};
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::kit::*;
use super::pages::{centred, left, right, Frame, CONTENT_Y, H1_Y, TILE};
use super::super::util::{rect, FlippedView};
use super::{CARD, SIDE, W};
use crate::model::*;

/// Arsenal 1.0.2 as its cask installs it (`brew info`, 4 Nov 2026: 69.6 MB).
pub(crate) const ARSENAL_SIZE: &str = "70 MB";
/// Its catalogue (Arsenal's README, 4 Nov 2026: "The catalog holds 102 models").
pub(crate) const HEAD: &str = "102 local models I can call as tools. I’m built to use it.";
pub(crate) const LINE: &str = "A Mac app that runs on-device models and lends them to me.";
/// The prototype's eight capabilities (Arsenal's catalogue groups), each with its symbol.
pub(crate) const KINDS: [(&str, &str); 8] = [
    ("waveform", "Transcription"),
    ("speaker.wave.2.fill", "Voices"),
    ("doc.text", "Documents"),
    ("translate", "Translation"),
    ("photo", "Images"),
    ("eye", "Vision"),
    ("music.note", "Sound and music"),
    ("point.3.filled.connected.trianglepath.dotted", "Embeddings"),
];
const HEAD_H: f64 = 84.0;
/// One capability's cell; four across, centred in the card (576 pt: inside the card at the window's minimum width).
const KIND: (f64, f64) = (144.0, 76.0);

/// Arsenal's own icon (assets/addons/SOURCES.md), shown until the installed app's `.icns` can be.
fn arsenal_mark() -> Option<Retained<objc2_app_kit::NSImage>> {
    use objc2::AllocAnyThread;
    let data = objc2_foundation::NSData::with_bytes(include_bytes!("../../../assets/addons/arsenal@2x.png"));
    let image = objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data)?;
    image.setSize(objc2_foundation::NSSize::new(64.0, 64.0));
    Some(image)
}

/// The add tile's words and whether "Not now" is offered: Use it when Arsenal is here; Add Arsenal with Homebrew;
/// Get Arsenal… (its page) without it.
pub(crate) fn arsenal_tiles(a: &ArsenalSetup) -> &'static str {
    if a.found.is_some() {
        "Use it"
    } else if a.brew {
        "Add Arsenal"
    } else {
        "Get Arsenal…"
    }
}

pub(super) struct ArsenalPage {
    pub frame: Frame,
    pub add: Retained<Pad>,
    pub not_now: Retained<Pad>,
    pub icon: Retained<NSImageView>,
    pub status: Retained<NSTextField>,
    pub spinner: Retained<NSProgressIndicator>,
    pub progress: Retained<NSTextField>,
    pub card: Retained<Plate>,
    pub kinds: Vec<Retained<NSTextField>>,
}

impl ArsenalPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Arsenal.title(""), 28.0, H1_Y, mtm);
        let strip = FlippedView::new(rect(0.0, 0.0, 2.0 * TILE.0 + 10.0, TILE.1), mtm);
        let add = Pad::new(rect(0.0, 0.0, TILE.0, TILE.1), Look::Tile { image: None, image_side: 30.0, title: "Add Arsenal".into(), caption: None }, target, sel!(setupArsenal:), 0, mtm);
        let not_now = Pad::new(rect(TILE.0 + 10.0, 0.0, TILE.0, TILE.1), Look::Tile { image: symbol("minus", 18.0, 0.0), image_side: 22.0, title: "Not now".into(), caption: None }, target, sel!(setupArsenal:), 1, mtm);
        strip.addSubview(&add);
        strip.addSubview(&not_now);
        centred(&strip, doc, CONTENT_Y);
        let grid_h = 2.0 * KIND.1;
        let card = Plate::card(rect(0.0, 0.0, CARD, HEAD_H + grid_h + 16.0), mtm);
        // `.addon-head`: Arsenal's icon, its line, its state at the right.
        let icon = image_view(rect(16.0, 16.0, 52.0, 52.0), None, None, mtm);
        card.addSubview(&icon);
        let head = text(&card, HEAD, &semibold(15.0), &t1(), 82.0, 22.0, CARD - 82.0 - 170.0, left(), mtm);
        head.setAutoresizingMask(Mask::ViewWidthSizable);
        let line = text(&card, LINE, &regular(13.0), &t2(), 82.0, 43.0, CARD - 82.0 - 170.0, left(), mtm);
        line.setAutoresizingMask(Mask::ViewWidthSizable);
        let status = text(&card, "", &regular(13.0), &t3(), 0.0, 32.0, 0.0, right(), mtm);
        status.setAutoresizingMask(Mask::ViewMinXMargin);
        // The capabilities, four to a row, centred in the card.
        let grid = FlippedView::new(rect(0.0, HEAD_H, 4.0 * KIND.0, grid_h), mtm);
        let font = regular(13.5);
        let mut kinds = vec![];
        for (i, (symbol_name, label)) in KINDS.iter().enumerate() {
            let (col, row) = ((i % 4) as f64, (i / 4) as f64);
            let x = col * KIND.0;
            let y = row * KIND.1;
            grid.addSubview(&image_view(rect(x + (KIND.0 - 26.0) / 2.0, y + 6.0, 26.0, 26.0), symbol(symbol_name, 21.0, 0.0), Some(&t1()), mtm));
            kinds.push(text(&grid, label, &font, &t1(), x, y + 40.0, KIND.0, NSTextAlignment::Center, mtm));
        }
        grid.setFrameOrigin(NSPoint::new(((CARD - 4.0 * KIND.0) / 2.0).round(), HEAD_H));
        grid.setAutoresizingMask(Mask::ViewMinXMargin | Mask::ViewMaxXMargin);
        card.addSubview(&grid);
        let card_y = CONTENT_Y + TILE.1 + 18.0;
        Band::wrap(doc, &card, card_y, CARD, SIDE, mtm);
        let progress_y = card_y + HEAD_H + grid_h + 16.0 + 14.0;
        let progress_row = FlippedView::new(rect(0.0, 0.0, 10.0, 20.0), mtm);
        let spinner = spinner(rect(0.0, 2.0, 16.0, 16.0), mtm);
        progress_row.addSubview(&spinner);
        let progress = text(&progress_row, "", &regular(13.0), &t2(), 24.0, 1.0, 0.0, left(), mtm);
        centred(&progress_row, doc, progress_y);
        frame.set_height(progress_y + 20.0);
        Self { frame, add, not_now, icon, status, spinner, progress, card, kinds }
    }

    /// `add`: Add Arsenal (or Use it) is the choice; Not now clears it.
    pub(super) fn sync(&self, a: &ArsenalSetup, add: bool) {
        let app = a.found.as_ref().map(|f| app_icon(&f.path)).or_else(arsenal_mark);
        let caption = Some(("Suggested".to_string(), t3()));
        self.add.set_look(Look::Tile { image: app.clone(), image_side: 34.0, title: arsenal_tiles(a).into(), caption });
        self.add.set_selected(add);
        self.not_now.set_selected(!add);
        self.icon.setImage(app.as_deref());
        self.icon.setHidden(app.is_none());
        let words = match &a.found {
            Some(f) => format!("Found Arsenal {}", f.version),
            None => ARSENAL_SIZE.into(),
        };
        let w = self.card.frame().size.width;
        retext(&self.status, &words, false);
        let sf = self.status.frame();
        self.status.setFrame(rect(w - 16.0 - sf.size.width, 32.0, sf.size.width, sf.size.height));
        let working = matches!(a.install, T3Install::Working(_));
        let (line, color) = match (&a.found, &a.install) {
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
