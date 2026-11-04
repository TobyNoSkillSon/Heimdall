//! Engine ▸ Add Model ▸ a model: its precisions ( and 18:3x: "When we choose, for example, int4 for
//! the Qwen, we click it and download it, and below that precision there's a trash icon so we can delete it"; moved
//! out of the agent's picker into the Engine section on 30 Oct).
//!
//! A precision row: its name (a click downloads one not on this Mac), then at the trailing edge the RAM it needs when
//! that is over the engine's ceiling (orange), and its download (size first), its progress with a stop, or its trash.
//! The trash asks once: a red Delete pill with the size in its place; the second press deletes. Nothing downloads until
//! The user presses a download or the name.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{NSButton, NSColor, NSMenuItem, NSProgressIndicator, NSTextField};
use objc2_foundation::MainThreadMarker;

use super::util::*;
use crate::{EngineModel, EngineQuant};

/// Where precision radios start: the detail rows' column.
pub(crate) const PRECISION_X: f64 = 32.0;
const TRAIL: f64 = 12.0;
const BUTTON: f64 = 22.0;
/// The widest a row's reason (a failed download, a refused delete) shows before it truncates in the middle.
pub(crate) const REASON_MAX: f64 = 240.0;

/// "17.4 GB" (decimal gigabytes, as Hugging Face lists sizes); whole above 100.
pub(crate) fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1e9;
    if g >= 100.0 { format!("{g:.0} GB") } else { format!("{g:.1} GB") }
}

/// The RAM a precision needs, when it is over the engine's ceiling (the warning), else `None`.
pub(crate) fn over_limit(q: &EngineQuant, limit: Option<u64>) -> Option<u64> {
    let (ram, limit) = (q.ram_bytes?, limit?);
    (ram > limit).then_some(ram)
}

/// What the precision row's trailing edge holds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Trailing {
    /// Not on this Mac: a download, its size first.
    Download(Option<u64>),
    /// Downloading: the fraction done (`None` until the size is known).
    Progress(Option<f64>),
    /// On this Mac: the trash.
    Trash,
    /// The trash pressed once: the red Delete pill.
    ConfirmDelete(Option<u64>),
    /// The download failed: why, then a download again.
    Failed(String, Option<u64>),
    /// On this Mac, and its delete was refused: why, then the trash.
    TrashRefused(String),
}

/// The trailing control for `q` of `model`; `armed`: its trash was pressed once.
pub(crate) fn trailing(model: &EngineModel, q: &EngineQuant, armed: bool) -> Trailing {
    if let Some(d) = model.download.as_ref().filter(|d| d.quant == q.quant) {
        if let Some(e) = &d.error {
            // A refused delete leaves the precision on disk: its reason beside the trash, not a download.
            return if q.downloaded { Trailing::TrashRefused(e.clone()) } else { Trailing::Failed(e.clone(), q.size_bytes) };
        }
        return Trailing::Progress(d.total_bytes.filter(|t| *t > 0).map(|t| d.done_bytes as f64 / t as f64));
    }
    if q.downloaded { if armed { Trailing::ConfirmDelete(q.size_bytes) } else { Trailing::Trash } } else { Trailing::Download(q.size_bytes) }
}

/// The width a precision row needs: radio and title, the warning, the trailing control.
pub(crate) fn row_width(q: &EngineQuant, limit: Option<u64>) -> f64 {
    let font = menu_font();
    let warn = over_limit(q, limit).map_or(0.0, |r| text_width(&gb(r), &font) + 30.0);
    let size = q.size_bytes.map_or(0.0, |b| text_width(&format!("Delete {}", gb(b)), &font) + 28.0);
    PRECISION_X + text_width(&q.quant, &font) + 24.0 + warn + size.max(BUTTON) + TRAIL
}

/// The live controls of one precision row, for in-place progress.
pub(crate) struct PrecisionRow {
    pub item: Retained<NSMenuItem>,
    /// The name: a borderless button whose click downloads a precision not on this Mac (inert otherwise).
    pub title: Retained<NSButton>,
    pub id: String,
    pub quant: String,
    pub progress: Option<Retained<NSProgressIndicator>>,
    pub trailing: Trailing,
}

fn symbol_button(name: &str, tip: &str, action: objc2::runtime::Sel, tag: isize, target: Option<&AnyObject>, mtm: MainThreadMarker) -> Retained<NSButton> {
    let button = unsafe { NSButton::buttonWithTitle_target_action(&ns(""), target, Some(action), mtm) };
    button.setBordered(false);
    if let Some(image) = symbol(name) { button.setImage(Some(&image)); }
    button.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageOnly);
    button.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
    button.setToolTip(Some(&ns(tip)));
    button.setTag(tag);
    button
}

fn label(text: &str, colour: &NSColor, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&ns(text), mtm);
    field.setFont(Some(&menu_font()));
    field.setTextColor(Some(colour));
    field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
    field
}

/// One precision row of `model` at `width`; `tag` indexes the state's precision list.
#[allow(clippy::too_many_arguments)]
pub(crate) fn precision_row(model: &EngineModel, q: &EngineQuant, armed: bool, limit: Option<u64>, width: f64, tag: isize, target: Option<&AnyObject>, mtm: MainThreadMarker) -> PrecisionRow {
    let trailing = trailing(model, q, armed);
    // The whole row presses its name (`ControlRow`): a download for one not here, nothing otherwise.
    let downloads = matches!(trailing, Trailing::Download(_) | Trailing::Failed(..));
    let action = downloads.then_some(sel!(downloadPrecision:));
    let item = menu_item(&q.quant, None, action, "", target, mtm);
    item.setTag(tag);
    let host = super::controls::ControlRow::new(rect(0.0, 0.0, width, 28.0), mtm);
    let radio = unsafe { NSButton::buttonWithTitle_target_action(&ns(&q.quant), target, action, mtm) };
    radio.setBordered(false);
    radio.setFont(Some(&menu_font()));
    radio.setAlignment(objc2_app_kit::NSTextAlignment::Left);
    radio.setTag(tag);
    host.addSubview(&radio);
    item.setView(Some(&host));
    let host: Retained<objc2_app_kit::NSView> = Retained::into_super(host);
    let mut right = width - TRAIL;
    let mut progress = None;
    // Lays `view` right to left from `right`.
    fn place(host: &objc2_app_kit::NSView, right: &mut f64, view: &objc2_app_kit::NSView, w: f64, h: f64) {
        *right -= w;
        view.setFrame(rect(*right, (28.0 - h) / 2.0, w, h));
        view.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin);
        host.addSubview(view);
        *right -= 6.0;
    }
    match &trailing {
        Trailing::Download(size) | Trailing::Failed(_, size) => {
            // The size first, then the arrow: what pressing it costs.
            let b = symbol_button("arrow.down.circle", "Download", sel!(downloadPrecision:), tag, target, mtm);
            b.setTitle(&ns(&size.map(gb).unwrap_or_default()));
            b.setFont(Some(&menu_font()));
            b.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageTrailing);
            let w = b.intrinsicContentSize().width.ceil() + 4.0;
            place(&host, &mut right, &b, w, BUTTON);
            if let Trailing::Failed(why, _) = &trailing {
                let f = label(why, &NSColor::systemOrangeColor(), mtm);
                f.setToolTip(Some(&ns(why)));
                let w = text_width(why, &menu_font()).min(REASON_MAX).ceil() + 4.0;
                place(&host, &mut right, &f, w, 17.0);
            }
        }
        Trailing::Progress(done) => {
            let stop = symbol_button("xmark.circle.fill", "Stop Download", sel!(cancelPrecisionDownload:), tag, target, mtm);
            place(&host, &mut right, &stop, BUTTON, BUTTON);
            let bar = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), rect(0.0, 0.0, 72.0, 6.0));
            bar.setStyle(objc2_app_kit::NSProgressIndicatorStyle::Bar);
            bar.setControlSize(objc2_app_kit::NSControlSize::Small);
            bar.setIndeterminate(done.is_none());
            bar.setMinValue(0.0);
            bar.setMaxValue(1.0);
            bar.setDoubleValue(done.unwrap_or(0.0));
            place(&host, &mut right, &bar, 72.0, 12.0);
            progress = Some(bar);
        }
        Trailing::Trash | Trailing::TrashRefused(_) => {
            // Its size on disk, then the trash: laid like a download row's size and arrow.
            let b = symbol_button("trash", "Delete", sel!(trashPrecision:), tag, target, mtm);
            b.setTitle(&ns(&q.size_bytes.map(gb).unwrap_or_default()));
            b.setFont(Some(&menu_font()));
            b.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageTrailing);
            let w = b.intrinsicContentSize().width.ceil() + 4.0;
            place(&host, &mut right, &b, w.max(BUTTON), BUTTON);
            if let Trailing::TrashRefused(why) = &trailing {
                let f = label(why, &NSColor::systemOrangeColor(), mtm);
                f.setToolTip(Some(&ns(why)));
                let w = text_width(why, &menu_font()).min(REASON_MAX).ceil() + 4.0;
                place(&host, &mut right, &f, w, 17.0);
            }
        }
        Trailing::ConfirmDelete(size) => {
            // Asks once, in place: the red pill names what goes.
            let title = size.map_or_else(|| "Delete".to_string(), |b| format!("Delete {}", gb(b)));
            let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(&title), target, Some(sel!(deletePrecision:)), mtm) };
            b.setBezelStyle(objc2_app_kit::NSBezelStyle::Push);
            b.setControlSize(objc2_app_kit::NSControlSize::Small);
            b.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(objc2_app_kit::NSFont::smallSystemFontSize())));
            b.setBezelColor(Some(&NSColor::systemRedColor()));
            b.setTag(tag);
            let w = b.intrinsicContentSize().width.ceil() + 4.0;
            place(&host, &mut right, &b, w, BUTTON);
        }
    }
    if let Some(ram) = over_limit(q, limit) {
        // Over the engine's memory: the RAM it needs, in orange beside a triangle.
        let text = gb(ram);
        let f = label(&text, &NSColor::systemOrangeColor(), mtm);
        let tip = format!("Needs {text} of memory; the engine has {}", limit.map(gb).unwrap_or_default());
        f.setToolTip(Some(&ns(&tip)));
        let w = text_width(&text, &menu_font()).ceil() + 4.0;
        place(&host, &mut right, &f, w, 17.0);
        if let Some(image) = symbol("exclamationmark.triangle.fill") {
            let view = objc2_app_kit::NSImageView::imageViewWithImage(&image, mtm);
            view.setContentTintColor(Some(&NSColor::systemOrangeColor()));
            view.setToolTip(Some(&ns(&tip)));
            right += 2.0;
            place(&host, &mut right, &view, 16.0, 14.0);
        }
    }
    // The name stops short of the trailing controls.
    radio.setFrame(rect(PRECISION_X - 12.0, 2.0, (right - PRECISION_X + 12.0).max(60.0), 24.0));
    radio.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::empty());
    PrecisionRow { item, title: radio, id: model.id.clone(), quant: q.quant.clone(), progress, trailing }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineDownload;

    fn model() -> EngineModel {
        EngineModel { id: "qwen3.8-27b".into(), label: "Qwen3.8 27B".into(), quant: "UD-Q4_K_XL".into(), ..Default::default() }
    }

    #[test]
    fn a_precision_offers_download_size_first_then_trash_once_on_disk_and_asks_before_deleting() {
        let q = EngineQuant { quant: "UD-Q4_K_XL".into(), size_bytes: Some(17_378_626_464), downloaded: false, ..Default::default() };
        assert_eq!(trailing(&model(), &q, false), Trailing::Download(Some(17_378_626_464)));
        assert_eq!(gb(17_378_626_464), "17.4 GB");
        let on_disk = EngineQuant { downloaded: true, ..q.clone() };
        assert_eq!(trailing(&model(), &on_disk, false), Trailing::Trash);
        assert_eq!(trailing(&model(), &on_disk, true), Trailing::ConfirmDelete(Some(17_378_626_464)));
        let mut downloading = model();
        downloading.download = Some(EngineDownload { quant: "UD-Q4_K_XL".into(), done_bytes: 5, total_bytes: Some(10), error: None });
        assert_eq!(trailing(&downloading, &q, false), Trailing::Progress(Some(0.5)));
        downloading.download.as_mut().unwrap().error = Some("Hugging Face unreachable".into());
        assert_eq!(trailing(&downloading, &q, false), Trailing::Failed("Hugging Face unreachable".into(), Some(17_378_626_464)));
        // A refused delete keeps the trash beside its reason.
        downloading.download.as_mut().unwrap().error = Some("In use by the engine".into());
        assert_eq!(trailing(&downloading, &on_disk, false), Trailing::TrashRefused("In use by the engine".into()));
    }

    #[test]
    fn only_a_precision_over_the_engine_ceiling_warns_with_its_ram() {
        let limit = Some(48u64 << 30);
        let small = EngineQuant { ram_bytes: Some(30 << 30), ..Default::default() };
        let giant = EngineQuant { ram_bytes: Some(86 << 30), ..Default::default() };
        assert_eq!(over_limit(&small, limit), None);
        assert_eq!(over_limit(&giant, limit), Some(86 << 30));
        assert_eq!(over_limit(&giant, None), None, "no ceiling known: no warning");
        assert_eq!(gb(244_000_000_000), "244 GB");
    }
}
