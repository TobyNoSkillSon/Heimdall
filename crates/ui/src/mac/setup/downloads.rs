//! Getting ready (public edition): the models downloading, one row each in the prototype's `.acts` list. A row shows
//! its size waiting (dimmed, `.row.pending`), "1.2 GB of 2.2 GB" with a `.prog` bar while it downloads, a green check
//! and its size when it is in place, and its reason with Retry when it failed.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, AllocAnyThread};
use objc2_app_kit::{NSAttributedStringNSStringDrawing as _, NSAutoresizingMaskOptions as Mask, NSImageView, NSProgressIndicator, NSTextField, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::kit::*;
use super::pages::{check_image, lay_right, left, right, Frame, CONTENT_Y};
use super::super::util::{ns, rect, FlippedView};
use super::{size_text, SIDE};
use crate::model::*;

const ROW: f64 = 44.0;
const ACTS: f64 = 640.0;
/// The list's width, for the card under it (`phrase`).
pub(crate) const ACTS_WIDTH: f64 = ACTS;
const H1_Y: f64 = 12.0;
/// `.prog`: 120 × 5.
const PROG: (f64, f64) = (120.0, 5.0);
/// Rows built up front: the models (with a decider precision chosen later), then the voice, listening, the decider,
/// This Mac's model and the agent.
pub(crate) const SLOTS: usize = 12;

/// A row's right side as words: (state words, bar share, check, retry). `None` share: no bar.
pub(crate) fn row_words(r: &ModelDownload) -> (String, Option<f64>, bool, bool) {
    match &r.state {
        DownloadState::Waiting => (size_text(r.total), None, false, false),
        DownloadState::Downloading => (format!("{} of {}", size_text(r.done), size_text(r.total)), Some(if r.total == 0 { 0.0 } else { r.done as f64 / r.total as f64 }), false, false),
        DownloadState::Done => (size_text(r.total), None, true, false),
        DownloadState::Failed(why) => (why.clone(), None, false, true),
    }
}

pub(super) struct Bar {
    pub track: Retained<Plate>,
    pub fill: Retained<Plate>,
}

impl Bar {
    fn new(mtm: MainThreadMarker) -> Self {
        let track = Plate::new(rect(0.0, 0.0, PROG.0, PROG.1), 2.5, mtm);
        track.set_fill(Some(white(0.10)));
        let fill = Plate::new(rect(0.0, 0.0, 0.0, PROG.1), 2.5, mtm);
        fill.set_fill(Some(blue()));
        track.addSubview(&fill);
        Self { track, fill }
    }

    fn set(&self, share: f64) {
        self.fill.setFrame(rect(0.0, 0.0, (PROG.0 * share.clamp(0.0, 1.0)).round(), PROG.1));
    }
}

pub(super) struct DownloadRow {
    pub row: Retained<FlippedView>,
    pub icon: Retained<NSImageView>,
    pub name: Retained<NSTextField>,
    pub state: Retained<NSTextField>,
    pub bar: Bar,
    pub check: Retained<NSImageView>,
    pub spinner: Retained<NSProgressIndicator>,
    /// The row's fix (Retry, Download, Choose…, Sign In…, Allow…); its tag is the row's index (`SetupWindow::fix_at`).
    pub retry: Retained<Pad>,
}

/// Memory search's model, as its download row is titled.
pub(crate) const MEMORY_MODEL: &str = "EmbeddingGemma 2";

/// One row of the list as words, whether it comes from a download or a readiness check.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Line {
    pub title: String,
    pub detail: Option<String>,
    pub symbol: String,
    pub words: String,
    pub share: Option<f64>,
    pub done: bool,
    pub working: bool,
    pub fix: Option<String>,
    pub problem: bool,
    /// `.row.pending`: still waiting its turn, dimmed.
    pub dim: bool,
}

pub(crate) fn download_line(r: &ModelDownload) -> Line {
    let (words, share, done, retry) = row_words(r);
    Line { title: r.title.clone(), detail: r.detail.clone(), symbol: r.symbol.clone(), words, share, done, working: false,
        fix: retry.then(|| "Retry".to_string()), problem: retry, dim: r.state == DownloadState::Waiting }
}

pub(crate) fn ready_line(r: &ReadyRow) -> Line {
    let base = Line { title: r.title.clone(), detail: r.detail.clone(), symbol: r.symbol.clone(), words: String::new(), share: None, done: false,
        working: false, fix: None, problem: false, dim: false };
    match &r.state {
        ReadyState::Ready => Line { done: true, ..base },
        ReadyState::Working { progress: Some((done, total)), .. } => Line {
            words: format!("{} of {}", size_text(*done), size_text(*total)),
            share: Some(if *total == 0 { 0.0 } else { *done as f64 / *total as f64 }),
            ..base
        },
        ReadyState::Working { words, progress: None } => Line { words: words.clone(), working: true, ..base },
        ReadyState::Blocked { words } => Line { words: words.clone(), dim: true, ..base },
        ReadyState::Optional { why, fix } => Line { words: why.clone(), fix: fix.as_ref().map(|f| f.title().to_string()), ..base },
        ReadyState::Problem { why, fix } => Line { words: why.clone(), fix: fix.as_ref().map(|f| f.title().to_string()), problem: true, ..base },
    }
}

/// Getting ready's list, each row with its fix: the models, then the readiness rows. The decider has its own row (its
/// download, then whether it loaded: the app's `decider_row`), so its model row gives way to it.
pub(crate) fn lines(downloads: &ModelDownloads, ready: &SetupReadiness) -> Vec<(Line, Option<ReadyFix>)> {
    let own_decider = ready.rows.iter().any(|r| r.key == "decider");
    // Memory search's row (once its model is in: whether it can run) stands for its download row.
    let own_memory = ready.rows.iter().any(|r| r.key == "memory");
    let retry = || Some(ReadyFix::Action { title: "Retry".into(), action: Box::new(UiAction::RetryModelDownloads) });
    let mut list: Vec<(Line, Option<ReadyFix>)> = downloads.rows.iter().filter(|d| !(own_decider && d.title == "Decider") && !(own_memory && d.title == MEMORY_MODEL))
        .map(|d| (download_line(d), matches!(d.state, DownloadState::Failed(_)).then(retry).flatten())).collect();
    list.extend(ready.rows.iter().map(|r| {
        if let ReadyState::Optional { fix: Some(f), .. } = &r.state { return (ready_line(r), Some(f.clone())); }
        let fix = match &r.state { ReadyState::Problem { fix, .. } => fix.clone(), _ => None };
        (ready_line(r), fix)
    }));
    list
}

pub(super) struct DownloadsPage {
    pub frame: Frame,
    pub band: Retained<Band>,
    pub card: Retained<Plate>,
    pub rows: Vec<DownloadRow>,
}

impl DownloadsPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Models.title(""), 28.0, H1_Y, mtm);
        let card = Plate::card(rect(0.0, 0.0, ACTS, 8.0 + SLOTS as f64 * ROW), mtm);
        let rows = (0..SLOTS)
            .map(|i| {
                let row = FlippedView::new(rect(0.0, 0.0, ACTS, ROW), mtm);
                row.setAutoresizingMask(Mask::ViewWidthSizable);
                let icon = image_view(rect(14.0, (ROW - 30.0) / 2.0, 30.0, 30.0), None, Some(&t2()), mtm);
                row.addSubview(&icon);
                let font = medium(14.5);
                let name = text(&row, "", &font, &t1(), 54.0, ((ROW - line_height(&font)) / 2.0).round(), 300.0, left(), mtm);
                let state = text(&row, "", &tabular(13.5, 0.0), &t2(), 0.0, 0.0, 0.0, right(), mtm);
                let bar = Bar::new(mtm);
                row.addSubview(&bar.track);
                let check = check_image(mtm);
                row.addSubview(&check);
                let spinner = spinner(rect(0.0, 0.0, 16.0, 16.0), mtm);
                row.addSubview(&spinner);
                let retry = Pad::button("Retry", false, true, 0.0, target, sel!(setupReadyFix:), mtm);
                retry.setTag(i as isize);
                row.addSubview(&retry);
                card.addSubview(&row);
                DownloadRow { row, icon, name, state, bar, check, spinner, retry }
            })
            .collect();
        let band = Band::wrap(doc, &card, CONTENT_Y, ACTS, SIDE, mtm);
        frame.set_height(CONTENT_Y + 8.0 + SLOTS as f64 * ROW);
        Self { frame, band, card, rows }
    }

    pub(super) fn sync(&self, downloads: &ModelDownloads, ready: &SetupReadiness) {
        let list: Vec<Line> = lines(downloads, ready).into_iter().map(|(l, _)| l).collect();
        let mut y = 4.0;
        let mut seps = vec![];
        for (i, slot) in self.rows.iter().enumerate() {
            let entry = list.get(i);
            slot.row.setHidden(entry.is_none());
            let Some(r) = entry else {
                unsafe { slot.spinner.stopAnimation(None) };
                continue;
            };
            if i > 0 {
                seps.push(y);
            }
            let f = slot.row.frame();
            slot.row.setFrame(rect(0.0, y, f.size.width, ROW));
            y += ROW;
            slot.icon.setImage(symbol(&r.symbol, 18.0, 0.0).as_deref());
            retext(&slot.state, &r.words, true);
            slot.state.setHidden(r.words.is_empty());
            slot.state.setTextColor(Some(&*if r.problem { hex(ORANGE, 1.0) } else { t2() }));
            slot.bar.track.setHidden(r.share.is_none());
            slot.bar.set(r.share.unwrap_or(0.0));
            slot.check.setHidden(!r.done);
            slot.spinner.setHidden(!r.working);
            if r.working { unsafe { slot.spinner.startAnimation(None) } } else { unsafe { slot.spinner.stopAnimation(None) } }
            slot.retry.setHidden(r.fix.is_none());
            if let Some(title) = &r.fix {
                slot.retry.retitle(title, 0.0);
            }
            slot.row.setAlphaValue(if r.dim { 0.4 } else { 1.0 });
            let w = slot.row.frame().size.width;
            let views: [&NSView; 5] = [&slot.retry, &slot.check, &slot.bar.track, &slot.state, &slot.spinner];
            let edge = lay_right(&views, w - 14.0, ROW, 10.0);
            name_with_detail(&slot.name, &r.title, r.detail.as_deref());
            let natural = slot.name.attributedStringValue().size().width.ceil() + 4.0;
            let nf = slot.name.frame();
            slot.name.setFrame(rect(54.0, nf.origin.y, natural.min((edge - 64.0).max(40.0)), nf.size.height));
            // A long reason gives way to the name.
            let sf = slot.state.frame();
            if sf.origin.x < 54.0 + natural + 10.0 {
                let x = 54.0 + natural.min(220.0) + 10.0;
                let right_edge = sf.origin.x + sf.size.width;
                slot.state.setFrame(rect(x, sf.origin.y, (right_edge - x).max(40.0), sf.size.height));
            }
        }
        self.card.set_lines(seps, 54.0);
        self.band.set_height(y + 4.0);
        let f = self.band.frame();
        self.band.setFrameOrigin(NSPoint::new(f.origin.x, CONTENT_Y));
        self.frame.set_height(CONTENT_Y + y + 4.0);
    }

    /// Where a card under the list goes (its band's top), and the page's height with it.
    pub(super) fn below(&self) -> f64 {
        let f = self.band.frame();
        f.origin.y + f.size.height + 18.0
    }
}

/// `.name .dim`: the precision after the name, quieter.
fn name_with_detail(field: &NSTextField, name: &str, detail: Option<&str>) {
    let Some(d) = detail else {
        field.setStringValue(&ns(name));
        return;
    };
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
    field.setAttributedStringValue(&attr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_download_rows_say_size_progress_done_or_why() {
        let row = |state, done| ModelDownload { title: "Decider".into(), detail: Some("4-bit".into()), symbol: "target".into(), done, total: 2_200_000_000, state };
        assert_eq!(row_words(&row(DownloadState::Waiting, 0)), ("2.2 GB".into(), None, false, false));
        assert_eq!(row_words(&row(DownloadState::Downloading, 1_200_000_000)).0, "1.2 GB of 2.2 GB");
        assert!(row_words(&row(DownloadState::Downloading, 1_100_000_000)).1.is_some_and(|s| (s - 0.5).abs() < 1e-9));
        assert_eq!(row_words(&row(DownloadState::Done, 2_200_000_000)), ("2.2 GB".into(), None, true, false));
        let failed = row_words(&row(DownloadState::Failed("No connection".into()), 9));
        assert_eq!((failed.0.as_str(), failed.3), ("No connection", true));
    }
}

#[cfg(test)]
mod lines_tests {
    use super::*;

    /// One Decider row: its download, then, once it is in place, whether it loaded; the fixes follow the list drawn.
    #[test]
    fn wizard_the_decider_has_its_own_row_and_each_rows_fix_is_its_own() {
        let d = |title: &str, state| ModelDownload { title: title.into(), state, total: 1, done: 1, ..Default::default() };
        let r = |key: &str, state| ReadyRow { key: key.into(), title: key.into(), detail: None, symbol: String::new(), state };
        let choose = ReadyFix::Step { title: "Choose…".into(), step: SetupStep::Providers };
        let ready = SetupReadiness { rows: vec![
            r("decider", ReadyState::Working { words: "Loading…".into(), progress: None }),
            r("agent", ReadyState::Problem { why: "No agent chosen yet".into(), fix: Some(choose.clone()) }),
        ] };
        let downloads = ModelDownloads { rows: vec![d("Parakeet", DownloadState::Failed("x".into())), d("Decider", DownloadState::Done)] };
        let l = lines(&downloads, &ready);
        assert_eq!(l.len(), 3, "the decider's own row (download, then loaded) stands for its model row");
        assert!(l[1].0.working && l[1].0.title == "decider");
        // A row waiting on another row is still: dimmed, no spinner (VM run 5 Nov).
        let blocked = ready_line(&r("listening", ReadyState::Blocked { words: "Waits for the microphone".into() }));
        assert!(!blocked.working && blocked.dim && blocked.words == "Waits for the microphone" && !blocked.problem);
        assert_eq!(l[0].1, Some(ReadyFix::Action { title: "Retry".into(), action: Box::new(UiAction::RetryModelDownloads) }));
        assert_eq!(l[2].1, Some(choose));
        assert!(!super::super::start_allowed(&downloads, &ready));
        let green = SetupReadiness { rows: vec![r("agent", ReadyState::Ready)] };
        let done = ModelDownloads { rows: vec![d("Parakeet", DownloadState::Done)] };
        assert!(super::super::start_allowed(&done, &green));
        assert!(!super::super::start_allowed(&done, &SetupReadiness::default()), "nothing reported yet is not ready");
        // Review 5 Nov (merge): in the public edition an empty model list is downloads not begun, never done.
        assert_eq!(ModelDownloads::default().finished(), !cfg!(feature = "public"));
        if cfg!(feature = "public") { assert!(!super::super::start_allowed(&ModelDownloads::default(), &green), "Start waits for the models"); }
        // Memory search that can't run here (VM run 5 Nov 05:43): its own row in place of its download, its reason in
        // grey with no spinner or fix, and Start doesn't wait for it.
        let memory = ReadyState::Optional { why: "Needs 8 GB free memory".into(), fix: None };
        let with_memory = SetupReadiness { rows: vec![r("agent", ReadyState::Ready), r("memory", memory)] };
        let both = ModelDownloads { rows: vec![d("Parakeet", DownloadState::Done), d(MEMORY_MODEL, DownloadState::Done)] };
        let l = lines(&both, &with_memory);
        assert_eq!(l.iter().map(|(l, _)| l.title.as_str()).collect::<Vec<_>>(), ["Parakeet", "agent", "memory"]);
        assert!(!l[2].0.working && !l[2].0.done && !l[2].0.problem && !l[2].0.dim && l[2].0.words == "Needs 8 GB free memory" && l[2].1.is_none());
        assert!(super::super::start_allowed(&both, &with_memory), "memory search is not needed to start");
        // Closing an unfinished Setup asks to quit, in these exact words.
        assert_eq!(super::super::QUIT_ALERT.0, "Quit Heimdall?");
        assert_eq!(super::super::QUIT_ALERT.2, ["Quit", "Cancel"]);
    }
}
