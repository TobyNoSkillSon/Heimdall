//! The live transcript: one paragraph per utterance, `[Source · Speaker]` then the words, bracketed notes in Vella's
//! blue, partial words dimmed until final. Newest at the bottom; it follows unless the user has scrolled up.
//! [chrome] A double-click on an unidentified "[Discord · Speaker 2]" asks who it is in a sheet and names the voice
//! ("double-click an unidentified 'Speaker 2' to name it, which adds it as a known speaker").

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSEvent, NSFont, NSLineBreakMode,
    NSMutableParagraphStyle, NSScrollView, NSTextField, NSTextView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRange, NSSize,
    NSString,
};

use super::util::*;
use crate::model::*;

/// Lines kept in the window; older ones are dropped in one block (the archive keeps everything).
const KEEP: usize = 4000;
const DROP: usize = 500;

pub struct TranscriptWindow {
    pub window: Retained<NSWindow>,
    scroll: Retained<NSScrollView>,
    text: Retained<TranscriptText>,
    empty: Retained<NSTextField>,
    /// The slots and their nameable labels, shared with the text view (it decides a double-click on its own).
    hits: Rc<RefCell<Hits>>,
    index: HashMap<u64, usize>,
    first: usize,
    /// [chrome] The open "Who is Speaker 2?" sheet.
    naming: Option<Naming>,
}

// ---- [chrome] transcript naming ----
/// An unidentified speaker's label in the transcript: where it sits inside its slot (UTF-16 units) and what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerSpan {
    pub start: usize,
    pub len: usize,
    pub line_id: u64,
    pub source: String,
    pub speaker: String,
}

/// "Speaker 2" → 2: the anonymous names the call's diariser gives (1 to 8). "Speaker ?" has no voice to name.
pub fn unidentified_speaker(speaker: &str) -> Option<i32> {
    speaker.strip_prefix("Speaker ")?.parse::<i32>().ok().filter(|n| (1..=8).contains(n))
}

/// A coded voice's label, "Speaker Fri 23 Oct 21h #3" (quiet speaker naming): nameable like "Speaker 2".
pub fn coded_voice(speaker: &str) -> bool {
    speaker.starts_with("Speaker ") && speaker.rsplit_once(" #").is_some_and(|(_, n)| n.parse::<u32>().is_ok())
}

#[derive(Default)]
pub struct Hits {
    /// (slot id, UTF-16 length including its newline), in display order.
    lines: Vec<(u64, usize)>,
    /// Each slot's nameable labels.
    spans: HashMap<u64, Vec<SpeakerSpan>>,
}

impl Hits {
    /// The nameable label under the text's UTF-16 `index`, if any.
    pub fn speaker_at(&self, index: usize) -> Option<SpeakerSpan> {
        let mut start = 0;
        for (id, len) in &self.lines {
            if index < start + len {
                let offset = index - start;
                return self.spans.get(id)?.iter().find(|s| offset >= s.start && offset < s.start + s.len).cloned();
            }
            start += len;
        }
        None
    }
}

struct Naming {
    span: SpeakerSpan,
    alert: Retained<NSAlert>,
    field: Retained<NSTextField>,
}

pub struct TranscriptTextIvars {
    hits: RefCell<Rc<RefCell<Hits>>>,
}

define_class!(
    /// The transcript's text view: a double-click on an unidentified speaker's label opens the naming sheet instead of
    /// selecting a word; every other click is the text view's own.
    #[unsafe(super(NSTextView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallTranscriptText"]
    #[ivars = TranscriptTextIvars]
    pub struct TranscriptText;

    impl TranscriptText {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if event.clickCount() == 2 {
                if let Some(span) = self.speaker_under(event) {
                    super::transcript_name_requested(span);
                    return;
                }
            }
            unsafe { msg_send![super(self), mouseDown: event] }
        }
    }
);

impl TranscriptText {
    fn speaker_under(&self, event: &NSEvent) -> Option<SpeakerSpan> {
        self.speaker_at_window_point(event.locationInWindow())
    }

    /// The nameable label under a point in the window's coordinates (the double-click's hit test).
    /// In the view's own coordinates through its layout manager: the character whose glyph is under the point, only
    /// when the point is on that glyph (a click past a line's end names nothing).
    pub fn speaker_at_window_point(&self, point: NSPoint) -> Option<SpeakerSpan> {
        let local = self.convertPoint_fromView(point, None);
        let origin = self.textContainerOrigin();
        let p = NSPoint::new(local.x - origin.x, local.y - origin.y);
        let (layout, container) = unsafe { (self.layoutManager()?, self.textContainer()?) };
        let glyph = unsafe { layout.glyphIndexForPoint_inTextContainer_fractionOfDistanceThroughGlyph(p, &container, std::ptr::null_mut()) };
        let bounds = layout.boundingRectForGlyphRange_inTextContainer(NSRange::new(glyph, 1), &container);
        let inside = p.x >= bounds.origin.x && p.x <= bounds.origin.x + bounds.size.width && p.y >= bounds.origin.y && p.y <= bounds.origin.y + bounds.size.height;
        if !inside { return None; }
        let index = layout.characterIndexForGlyphAtIndex(glyph);
        self.ivars().hits.borrow().borrow().speaker_at(index)
    }
}

/// "Who is Speaker 2?" over the source ("Discord"), a name field, Name (Return) and Cancel (Esc).
pub fn naming_alert(span: &SpeakerSpan, target: &AnyObject, mtm: MainThreadMarker) -> (Retained<NSAlert>, Retained<NSTextField>) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&ns(&format!("Who is {}?", span.speaker)));
    alert.setInformativeText(&ns(&span.source));
    alert_icon(&alert, "person.crop.circle.badge.questionmark");
    let confirm = alert.addButtonWithTitle(&ns("Name"));
    let cancel = alert.addButtonWithTitle(&ns("Cancel"));
    cancel.setKeyEquivalent(&ns("\u{1b}"));
    unsafe {
        confirm.setTarget(Some(target));
        confirm.setAction(Some(sel!(transcriptNameConfirm:)));
        cancel.setTarget(Some(target));
        cancel.setAction(Some(sel!(transcriptNameCancel:)));
    }
    let field = NSTextField::textFieldWithString(&ns(""), mtm);
    field.setFrame(rect(0.0, 0.0, 228.0, 24.0));
    field.setFont(Some(&font(13.0)));
    field.setPlaceholderString(Some(&ns("Name")));
    alert.setAccessoryView(Some(&field));
    alert.layout();
    (alert, field)
}

impl TranscriptWindow {
    /// The nameable label under the text's UTF-16 `index` (what a double-click there names).
    pub fn speaker_at(&self, index: usize) -> Option<SpeakerSpan> {
        self.hits.borrow().speaker_at(index)
    }

    /// The text view (render checks).
    pub fn text_view(&self) -> &TranscriptText {
        &self.text
    }

    /// Render checks: the naming state without a sheet on screen (`begin_naming` attaches it to the window).
    pub fn begin_naming_offscreen(&mut self, span: SpeakerSpan, target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSAlert> {
        let (alert, field) = naming_alert(&span, target, mtm);
        self.naming = Some(Naming { span, alert: alert.clone(), field });
        alert
    }

    /// The whole text, as drawn (tests).
    pub fn text(&self) -> String {
        self.storage().string().to_string()
    }

    /// Opens the naming sheet on the window (one at a time).
    pub fn begin_naming(&mut self, span: SpeakerSpan, target: &AnyObject, mtm: MainThreadMarker) {
        if self.naming.is_some() { return; }
        let (alert, field) = naming_alert(&span, target, mtm);
        let sheet = alert.window();
        self.window.beginSheet_completionHandler(&sheet, None);
        sheet.makeFirstResponder(Some(&field));
        self.naming = Some(Naming { span, alert, field });
    }

    /// The sheet's label while it is open.
    pub fn naming_span(&self) -> Option<SpeakerSpan> {
        self.naming.as_ref().map(|n| n.span.clone())
    }

    /// Types `name` into the open sheet's field (tests).
    pub fn set_naming_text(&self, name: &str) {
        if let Some(n) = &self.naming { n.field.setStringValue(&ns(name)); }
    }

    /// Closes the sheet: with Name and a non-empty name, the label and the name to send.
    pub fn finish_naming(&mut self, confirmed: bool) -> Option<(SpeakerSpan, String)> {
        let n = self.naming.take()?;
        let sheet = n.alert.window();
        if sheet.sheetParent().is_some() { self.window.endSheet(&sheet); }
        sheet.orderOut(None);
        let name = n.field.stringValue().to_string().trim().to_string();
        (confirmed && !name.is_empty()).then_some((n.span, name))
    }
}
// ---- end [chrome] transcript naming

impl TranscriptWindow {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, 620.0, 440.0),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns("Transcript"));
        window.setContentMinSize(NSSize::new(360.0, 200.0));
        window.center();
        window.setFrameAutosaveName(&ns("HeimdallTranscript"));

        let content = window.contentView().unwrap();
        let bounds = content.bounds();
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), bounds);
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);

        let hits = Rc::new(RefCell::new(Hits::default()));
        let text = TranscriptText::alloc(mtm).set_ivars(TranscriptTextIvars { hits: RefCell::new(hits.clone()) });
        let text: Retained<TranscriptText> = unsafe { msg_send![super(text), initWithFrame: bounds] };
        text.setEditable(false);
        text.setSelectable(true);
        text.setRichText(true);
        text.setDrawsBackground(false);
        text.setTextContainerInset(NSSize::new(16.0, 14.0));
        text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        text.setVerticallyResizable(true);
        text.setHorizontallyResizable(false);
        text.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        if let Some(container) = unsafe { text.textContainer() } {
            container.setWidthTracksTextView(true);
        }
        scroll.setDocumentView(Some(&text));
        content.addSubview(&scroll);

        // The empty state: one quiet line in the middle until the first words arrive.
        let empty = text_label("Nothing heard yet", &font(13.0), &tertiary(), mtm);
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

        TranscriptWindow { window, scroll, text, empty, hits, index: HashMap::new(), first: 0, naming: None }
    }


    fn storage(&self) -> Retained<objc2_app_kit::NSTextStorage> {
        unsafe { self.text.textStorage() }.expect("text view has storage")
    }

    /// True when the view shows its last line (within a few points), so new lines should keep it there.
    fn at_bottom(&self) -> bool {
        let clip = self.scroll.contentView();
        let visible = clip.bounds();
        let height = self.text.frame().size.height;
        visible.origin.y + visible.size.height >= height - 24.0
    }

    pub fn upsert(&mut self, line: &TranscriptLine) {
        // An utterance with no words and no notes yet (speech started, nothing recognised) draws nothing:
        // never a bare "[Source · Speaker]" header.
        let empty = !line.segments.iter().any(|seg| match seg {
            Segment::Words(w) | Segment::Note(w) => !w.trim().is_empty(),
        });
        if empty && !self.index.contains_key(&line.id) {
            return;
        }
        let (rendered, span) = if empty { (NSAttributedString::new(), None) } else { render_line(line) };
        self.replace_rendered(line.id, &rendered, span.into_iter().collect());
    }
    pub fn upsert_runs(&mut self,id:u64,lines:&[TranscriptLine]) {
        let rendered=NSMutableAttributedString::new();
        let mut spans = vec![];
        for line in lines {
            let (piece, span) = render_line(line);
            let at = rendered.length();
            spans.extend(span.map(|s| SpeakerSpan { start: s.start + at, ..s }));
            rendered.appendAttributedString(&piece);
        }
        self.replace_rendered(id,&rendered,spans);
    }
    fn replace_rendered(&mut self,id:u64,rendered:&NSAttributedString,spans:Vec<SpeakerSpan>) {
        // A logical utterance keeps one storage slot while its paragraphs change.
        let follow = self.at_bottom();
        let storage = self.storage();
        let length = rendered.length();
        let mut hits = self.hits.borrow_mut();
        if spans.is_empty() { hits.spans.remove(&id); } else { hits.spans.insert(id, spans); }
        if let Some(&position) = self.index.get(&id) {
            let slot = position - self.first;
            let after: usize = hits.lines[slot + 1..].iter().map(|l| l.1).sum();
            let old = hits.lines[slot].1;
            let start = storage.length() - after - old;
            storage.replaceCharactersInRange_withAttributedString(NSRange::new(start, old), &rendered);
            hits.lines[slot].1 = length;
        } else {
            storage.appendAttributedString(&rendered);
            self.index.insert(id, self.first + hits.lines.len());
            hits.lines.push((id, length));
            if hits.lines.len() > KEEP {
                drop(hits);
                self.drop_oldest(&storage);
            }
        }
        self.empty.setHidden(true);
        if follow {
            self.text.scrollRangeToVisible(NSRange::new(storage.length(), 0));
        }
    }

    fn drop_oldest(&mut self, storage: &objc2_app_kit::NSTextStorage) {
        let mut hits = self.hits.borrow_mut();
        let dropped: Vec<(u64, usize)> = hits.lines.drain(..DROP).collect();
        let chars: usize = dropped.iter().map(|l| l.1).sum();
        storage.deleteCharactersInRange(NSRange::new(0, chars));
        for (id, _) in dropped {
            self.index.remove(&id);
            hits.spans.remove(&id);
        }
        self.first += DROP;
    }
}

/// `[Discord · Dave] words [note] words` + newline, and an unidentified speaker's label span (from the line's start).
fn render_line(line: &TranscriptLine) -> (Retained<NSAttributedString>, Option<SpeakerSpan>) {
    let out = NSMutableAttributedString::new();
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setLineSpacing(2.0);
    paragraph.setParagraphSpacing(9.0);
    paragraph.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    // Partial words: dimmed and italic until the recogniser settles them.
    let body = if line.is_final { font(13.0) } else { italic(&font(13.0)) };
    let words_color = if line.is_final { label() } else { tertiary() };
    let mut span = None;
    if let Some(l) = &line.label {
        let text = format!("[{} · {}] ", l.source, crate::identity::shown_speaker(&l.speaker));
        let label_font = medium(13.0);
        // Wrapped lines start under the words, so the labels stay a column.
        paragraph.setHeadIndent(text_width(&text, &label_font).ceil());
        append(&out, &text, &label_font, &secondary(), &paragraph);
        if unidentified_speaker(&l.speaker).is_some() || coded_voice(&l.speaker) {
            span = Some(SpeakerSpan { start: 0, len: text.trim_end().encode_utf16().count(), line_id: line.id, source: l.source.clone(), speaker: l.speaker.clone() });
        }
    }
    let mut first = true;
    for segment in &line.segments {
        let gap = if first { "" } else { " " };
        first = false;
        match segment {
            Segment::Words(words) if words.trim().is_empty() => { first = gap.is_empty(); }
            Segment::Words(words) => append(&out, &format!("{gap}{words}"), &body, &words_color, &paragraph),
            Segment::Note(note) => append(&out, &format!("{gap}[{note}]"), &body, &note_blue(), &paragraph),
        }
    }
    append(&out, "\n", &body, &words_color, &paragraph);
    (Retained::into_super(out), span)
}

fn italic(font: &NSFont) -> Retained<NSFont> {
    let descriptor = font.fontDescriptor().fontDescriptorWithSymbolicTraits(
        objc2_app_kit::NSFontDescriptorSymbolicTraits::TraitItalic,
    );
    NSFont::fontWithDescriptor_size(&descriptor, font.pointSize()).unwrap_or_else(|| font.retain())
}

fn append(out: &NSMutableAttributedString, text: &str, font: &NSFont, color: &NSColor, paragraph: &NSMutableParagraphStyle) {
    unsafe {
        let keys: [&NSString; 3] = [
            objc2_app_kit::NSFontAttributeName,
            objc2_app_kit::NSForegroundColorAttributeName,
            objc2_app_kit::NSParagraphStyleAttributeName,
        ];
        let values: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), paragraph.as_ref()];
        let attrs = NSDictionary::from_slices(&keys, &values);
        let piece = NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &ns(text), Some(&attrs));
        out.appendAttributedString(&piece);
    }
}

#[cfg(test)]
mod tests {
    // [chrome] night 1022 stage 3: which labels a double-click names, without AppKit.
    use super::*;
    fn span(start: usize, line_id: u64, speaker: &str) -> SpeakerSpan {
        SpeakerSpan { start, len: 10, line_id, source: "Discord".into(), speaker: speaker.into() }
    }
    #[test]
    fn only_the_diarisers_anonymous_names_are_nameable() {
        assert_eq!(unidentified_speaker("Speaker 2"), Some(2));
        assert_eq!(unidentified_speaker("Speaker 8"), Some(8));
        for name in ["Speaker ?", "Speaker 0", "Speaker 9", "Speaker", "Dave", "speaker 2", "Speaker 2b", "User"] {
            assert_eq!(unidentified_speaker(name), None, "{name}");
        }
    }
    #[test]
    fn coded_voice_labels_are_nameable_too() {
        assert!(coded_voice("Speaker Fri 23 Oct 21h #3"));
        for name in ["Speaker 2", "Speaker ?", "Dave", "Speaker #", "Speaker Fri #x", "Dave #2"] {
            assert!(!coded_voice(name), "{name}");
        }
    }
    #[test]
    fn a_hit_finds_the_label_in_its_slot_and_nothing_outside_labels() {
        let mut hits = Hits::default();
        hits.lines = vec![(1, 30), (2, 25), (3, 40)];
        hits.spans.insert(2, vec![span(0, 2, "Speaker 2")]);
        hits.spans.insert(3, vec![span(0, 3, "Speaker 3"), span(20, 3, "Speaker 2")]);
        assert_eq!(hits.speaker_at(5), None, "slot 1 has no nameable label");
        assert_eq!(hits.speaker_at(30).map(|s| s.line_id), Some(2), "first unit of slot 2");
        assert_eq!(hits.speaker_at(39).map(|s| s.speaker), Some("Speaker 2".into()), "last unit of the label");
        assert_eq!(hits.speaker_at(40), None, "the words after it");
        assert_eq!(hits.speaker_at(55).map(|s| s.speaker), Some("Speaker 3".into()));
        assert_eq!(hits.speaker_at(75).map(|s| s.speaker), Some("Speaker 2".into()), "a later run in the same slot");
        assert_eq!(hits.speaker_at(70), None, "between runs");
        assert_eq!(hits.speaker_at(95), None, "past the end");
    }
}
