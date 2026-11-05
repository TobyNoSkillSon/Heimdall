//! The Debug Agent window: the live Pi session as a timeline. A header names the harness and model and says the run
//! state, with Stop (while a turn runs) and Reload (only once the agent failed). Under it the conversation's telemetry strip (context fill,
//! tokens, tool calls and failed edits, latency, compactions, errors; `agent_debug`), and, while a reload waits, its
//! reason with Reload Now. Below, one block per entry, newest at the bottom, grouped into turns: each request
//! (a wake, the user's typed message with Edit, context) heads its turn with how it ended, and what the agent did
//! (thinking, tool calls and results, failed edits, images, replies, failures) sits indented under it. A streaming
//! entry replaces its own block. At the bottom the composer (Send; Interrupt while a turn runs), or, while a wake waits for review, the
//! interrupted wake with its evidence and Completed / Abandon. The content is `agent_view`'s; this file draws it.

use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAttributedStringAttachmentConveniences as _, NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath,
    NSButton, NSColor, NSEventModifierFlags, NSFont, NSImage, NSLineBreakMode, NSMutableParagraphStyle,
    NSScrollView, NSTextAttachment, NSTextDelegate, NSTextField, NSTextView, NSTextViewDelegate, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSMutableAttributedString, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString,
};

use std::cell::RefCell;

use super::util::*;
use crate::agent_debug::{self, Tile};
use crate::agent_view::{self, AgentView, Change, Display, Edit, Row, Tone};
use crate::model::*;

const HEADER: f64 = 52.0;
const PAD: f64 = 16.0;
/// One row of telemetry tiles, and the gap around them.
const TILE_H: f64 = 48.0;
const TILE_GAP: f64 = 8.0;
/// The pending reload's line.
const PENDING_H: f64 = 34.0;
/// What the agent did sits this far in under the request that started it.
const INDENT: f64 = 20.0;
/// The composer's text grows to this many points, then scrolls.
const COMPOSER_MAX: f64 = 120.0;
const COMPOSER_MIN: f64 = 22.0;
const EDIT_STRIP: f64 = 30.0;
/// The largest an attachment thumbnail draws.
const THUMB_W: f64 = 360.0;
const THUMB_H: f64 = 220.0;
/// Attachment images kept decoded.
const IMAGE_CACHE: usize = 48;

/// An attachment image: being read from the archive, ready, or gone.
#[derive(Clone)]
enum Img {
    Loading,
    Ready(Retained<NSImage>),
    Missing,
}

/// Typed text the user is editing to resend in place of an earlier message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editing {
    pub session_entry_id: String,
    pub time: String,
    /// The draft the composer held before Edit; Cancel brings it back.
    pub draft: String,
}

pub struct AgentWindow {
    pub window: Retained<NSWindow>,
    pub delegate: Retained<AgentDelegate>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    pub stop: Retained<NSButton>,
    pub reload: Retained<NSButton>,
    pub strip: Retained<TelemetryStrip>,
    pending_box: Retained<FieldBox>,
    pending_label: Retained<NSTextField>,
    pub reload_now: Retained<NSButton>,
    /// The edge between the strip and the timeline scrolling under it.
    edge: Retained<Hairline>,
    pub scroll: Retained<NSScrollView>,
    pub text: Retained<NSTextView>,
    empty: Retained<NSTextField>,
    /// The drawn length (UTF-16) of each row's block, in `AgentView::rows` order.
    blocks: Vec<usize>,
    composer_box: Retained<FieldBox>,
    composer_scroll: Retained<NSScrollView>,
    pub composer: Retained<NSTextView>,
    placeholder: Retained<NSTextField>,
    pub send: Retained<NSButton>,
    pub interrupt: Retained<NSButton>,
    edit_label: Retained<NSTextField>,
    edit_cancel: Retained<NSButton>,
    review_title: Retained<NSTextField>,
    review_box: Retained<FieldBox>,
    review_scroll: Retained<NSScrollView>,
    review_text: Retained<NSTextView>,
    pub review_completed: Retained<NSButton>,
    pub review_abandon: Retained<NSButton>,
    pub editing: Option<Editing>,
    /// The last message sent: restored to an empty composer if the backend refuses it.
    last_sent: Option<String>,
    /// The review answered for this delivery id, until the backend moves on.
    review_answered: Option<String>,
    review: Option<agent_view::Review>,
    session: Option<AgentSession>,
    state: Option<AgentWindowState>,
    images: HashMap<String, Img>,
    image_order: Vec<String>,
}

impl AgentWindow {
    /// `autosave`: remember the frame between launches (off for offscreen renders, which resize it).
    pub fn new(mtm: MainThreadMarker, autosave: bool) -> Self {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, 660.0, 600.0),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns("Debug Agent"));
        window.setContentMinSize(NSSize::new(460.0, 380.0));
        window.center();
        if autosave {
            window.setFrameAutosaveName(&ns("HeimdallAgent"));
        }
        let delegate = AgentDelegate::new(mtm);
        let target: &AnyObject = delegate.as_ref();

        let content = window.contentView().unwrap();
        let bounds = content.bounds();
        let (width, height) = (bounds.size.width, bounds.size.height);

        // Header: harness · model over the state line; Stop and (once failed) Reload on the right.
        let header = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, height - HEADER, width, HEADER));
        header.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin);
        let title = text_label("", &medium(13.0), &label(), mtm);
        title.setFrame(rect(20.0, 26.0, width - 220.0, 18.0));
        title.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        header.addSubview(&title);
        let detail = text_label("", &font(11.0), &secondary(), mtm);
        detail.setFrame(rect(20.0, 9.0, width - 220.0, 15.0));
        detail.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        detail.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        header.addSubview(&detail);
        let reload = push_button("Reload", target, sel!(agentReload:), mtm);
        reload.sizeToFit();
        let rw = reload.frame().size.width.max(76.0);
        reload.setFrame(rect(width - PAD - rw, 14.0, rw, 28.0));
        reload.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        header.addSubview(&reload);
        let stop = push_button("Stop", target, sel!(agentStop:), mtm);
        stop.setFrame(rect(width - PAD - rw - 8.0 - 68.0, 14.0, 68.0, 28.0));
        stop.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        stop.setToolTip(Some(&ns("Stops the turn; actions already taken stay done")));
        header.addSubview(&stop);
        content.addSubview(&header);

        // The telemetry strip, then the pending reload's line (hidden unless a reload waits).
        let strip = TelemetryStrip::new(rect(0.0, 0.0, width, TILE_H), mtm);
        content.addSubview(&strip);
        let pending_box = FieldBox::new(rect(0.0, 0.0, width, PENDING_H), mtm);
        content.addSubview(&pending_box);
        let pending_label = text_label("", &font(12.0), &secondary(), mtm);
        pending_label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        pending_box.addSubview(&pending_label);
        let reload_now = push_button("Reload Now", target, sel!(agentReloadNow:), mtm);
        reload_now.setToolTip(Some(&ns("Stops the turn and reloads the agent on the same conversation")));
        pending_box.addSubview(&reload_now);
        let edge = Hairline::new(rect(0.0, 0.0, width, 1.0), mtm);

        let (scroll, text) = text_area(rect(0.0, 0.0, width, height - HEADER - 6.0), NSSize::new(16.0, 14.0), mtm);
        text.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        content.addSubview(&scroll);
        content.addSubview(&edge);

        let empty = text_label("Nothing sent yet", &font(13.0), &tertiary(), mtm);
        empty.sizeToFit();
        content.addSubview(&empty);

        // The composer: a rounded field that grows with its text, Send (and Interrupt while a turn runs) beside it.
        let composer_box = FieldBox::new(rect(0.0, 0.0, width, 40.0), mtm);
        content.addSubview(&composer_box);
        let (composer_scroll, composer) = text_area(rect(0.0, 0.0, 100.0, COMPOSER_MIN), NSSize::new(0.0, 0.0), mtm);
        composer.setEditable(true);
        composer.setRichText(false);
        composer.setFont(Some(&font(13.0)));
        composer.setTextColor(Some(&label()));
        composer.setAllowsUndo(true);
        composer.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        composer_box.addSubview(&composer_scroll);
        let placeholder = text_label("", &font(13.0), &tertiary(), mtm);
        composer_box.addSubview(&placeholder);
        let send = push_button("Send", target, sel!(agentSend:), mtm);
        content.addSubview(&send);
        let interrupt = push_button("Interrupt", target, sel!(agentInterrupt:), mtm);
        interrupt.setToolTip(Some(&ns("Sends it now, after the current tool step")));
        content.addSubview(&interrupt);

        // Editing a sent message: what resending does, and Cancel.
        let edit_label = text_label("", &font(12.0), &secondary(), mtm);
        edit_label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        content.addSubview(&edit_label);
        let edit_cancel = push_button("Cancel", target, sel!(agentCancelEdit:), mtm);
        content.addSubview(&edit_cancel);

        // Review of an interrupted wake: what it was and what ran, then the two answers.
        let review_title = text_label("", &medium(13.0), &NSColor::systemRedColor(), mtm);
        review_title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        content.addSubview(&review_title);
        let review_box = FieldBox::new(rect(0.0, 0.0, 10.0, 10.0), mtm);
        content.addSubview(&review_box);
        let (review_scroll, review_text) = text_area(rect(0.0, 0.0, 10.0, 10.0), NSSize::new(10.0, 8.0), mtm);
        review_box.addSubview(&review_scroll);
        let review_completed = push_button("Completed", target, sel!(agentReviewCompleted:), mtm);
        review_completed.setToolTip(Some(&ns("The agent finished it. Nothing is repeated.")));
        content.addSubview(&review_completed);
        let review_abandon = push_button("Abandon", target, sel!(agentReviewAbandoned:), mtm);
        review_abandon.setToolTip(Some(&ns("Mark it failed. Nothing is repeated.")));
        content.addSubview(&review_abandon);

        let w = AgentWindow {
            window,
            delegate,
            title,
            detail,
            stop,
            reload,
            strip,
            pending_box,
            pending_label,
            reload_now,
            edge,
            scroll,
            text,
            empty,
            blocks: vec![],
            composer_box,
            composer_scroll,
            composer,
            placeholder,
            send,
            interrupt,
            edit_label,
            edit_cancel,
            review_title,
            review_box,
            review_scroll,
            review_text,
            review_completed,
            review_abandon,
            editing: None,
            last_sent: None,
            review_answered: None,
            review: None,
            session: None,
            state: None,
            images: HashMap::new(),
            image_order: vec![],
        };
        w.layout();
        w
    }

    /// The header for this session: harness · model; the raw session id only on hover.
    pub fn set_session(&mut self, session: &AgentSession) {
        self.session = Some(session.clone());
        let harness = session.harness.title();
        let title = match &session.model {
            Some(model) => format!("{harness} · {}", model_name(model)),
            None => harness.to_string(),
        };
        self.title.setStringValue(&ns(&title));
        let id = session.session_id.as_deref().map(|id| ns(&format!("Session {id}")));
        self.detail.setToolTip(id.as_deref());
    }

    /// Everything from the view: the first frame, or after a history replacement.
    pub fn reset(&mut self, view: &AgentView) {
        self.state = view.state.clone();
        let storage = unsafe { self.text.textStorage() }.expect("text view has storage");
        storage.deleteCharactersInRange(NSRange::new(0, storage.length()));
        self.blocks.clear();
        self.append_from(0, view);
        self.apply_state(view);
        self.refresh_telemetry(view);
        self.scroll_to_end();
    }

    /// The telemetry strip from the view; it redraws only when a number changed.
    pub fn refresh_telemetry(&self, view: &AgentView) {
        let t = agent_debug::telemetry(&view.rows, view.session.as_ref(), &view.ttft, running(view.state.as_ref()));
        let tiles = agent_debug::tiles(&t, &|ms| short_time(ms));
        if self.strip.set_tiles(tiles) {
            self.layout();
        }
    }

    /// The turn headers of every turn a change touched (their outcome and counts follow their rows).
    fn refresh_turns(&mut self, changes: &[Change], view: &AgentView) {
        let mut headers: Vec<usize> = vec![];
        for change in changes {
            let first = match change {
                Change::Reset => return,
                Change::Replace(i) | Change::Append(i) => *i,
            };
            if first >= view.rows.len() {
                continue;
            }
            let header = agent_debug::turn_start(&view.rows, first);
            if header < first && !headers.contains(&header) {
                headers.push(header);
            }
        }
        for i in headers {
            self.replace(i, view);
        }
    }

    /// The oldest entry in view (renders).
    pub fn scroll_to_top(&self) {
        let clip = self.scroll.contentView();
        clip.scrollToPoint(NSPoint::new(0.0, 0.0));
        self.scroll.reflectScrolledClipView(&clip);
    }

    /// The newest entry in view (the first frame, and following the stream while the user is at the bottom).
    fn scroll_to_end(&self) {
        if let (Some(manager), Some(container)) = (unsafe { self.text.layoutManager() }, unsafe { self.text.textContainer() }) {
            manager.ensureLayoutForTextContainer(&container);
        }
        self.text.sizeToFit();
        let clip = self.scroll.contentView();
        let y = (self.text.frame().size.height - clip.bounds().size.height).max(0.0);
        clip.scrollToPoint(NSPoint::new(0.0, y));
        self.scroll.reflectScrolledClipView(&clip);
    }

    /// One change from the view.
    pub fn apply(&mut self, change: &Change, view: &AgentView) {
        self.apply_changes(std::slice::from_ref(change), view);
    }

    /// One update's changes: the view follows the newest text when it was at the bottom before the first of them.
    pub fn apply_changes(&mut self, changes: &[Change], view: &AgentView) {
        let follow = self.at_bottom();
        for change in changes {
            match change {
                Change::Reset => return self.reset(view),
                Change::Replace(i) => self.replace(*i, view),
                Change::Append(start) => {
                    self.append_from(*start, view);
                    self.after_new_rows(*start, view);
                }
            }
        }
        self.refresh_turns(changes, view);
        self.refresh_telemetry(view);
        if view.review().is_some() {
            self.apply_state(view);
        }
        if follow {
            self.scroll_to_end();
        }
    }

    fn append_from(&mut self, start: usize, view: &AgentView) {
        let storage = unsafe { self.text.textStorage() }.expect("text view has storage");
        self.blocks.truncate(start);
        for i in start..view.rows.len() {
            let block = self.row_block(view, i);
            self.blocks.push(block.length());
            storage.appendAttributedString(&block);
        }
        self.empty.setHidden(storage.length() > 0);
    }

    fn replace(&mut self, i: usize, view: &AgentView) {
        let Some(row) = view.rows.get(i) else { return };
        if i >= self.blocks.len() {
            return self.append_from(self.blocks.len(), view);
        }
        let storage = unsafe { self.text.textStorage() }.expect("text view has storage");
        let offset: usize = self.blocks[..i].iter().sum();
        let _ = row;
        let block = self.row_block(view, i);
        storage.replaceCharactersInRange_withAttributedString(NSRange::new(offset, self.blocks[i]), &block);
        self.blocks[i] = block.length();
    }

    /// A new run state: the header, the buttons, the composer or the review, and the Edit links that depend on it.
    pub fn set_state(&mut self, view: &AgentView) {
        let old = self.state.clone();
        self.state = view.state.clone();
        let last_turn = view.rows.len().checked_sub(1).map(|i| agent_debug::turn_start(&view.rows, i));
        for (i, row) in view.rows.iter().enumerate() {
            let turn_changed = Some(i) == last_turn && running(old.as_ref()) != running(view.state.as_ref());
            if turn_changed || agent_view::edit(row, old.as_ref()) != agent_view::edit(row, view.state.as_ref()) {
                self.replace(i, view);
            }
        }
        self.apply_state(view);
        if running(old.as_ref()) != running(view.state.as_ref()) {
            self.refresh_telemetry(view);
        }
    }

    /// New rows: a refused send gives the user his text back; the accepted one is now in the list.
    fn after_new_rows(&mut self, start: usize, view: &AgentView) {
        let Some(sent) = self.last_sent.clone() else { return };
        for row in &view.rows[start.min(view.rows.len())..] {
            let Some(e) = row.live() else { continue };
            if e.kind == AgentLiveKind::Prompt && agent_view::typed(e).as_deref() == Some(sent.as_str()) {
                self.last_sent = None;
                return;
            }
            if e.kind == AgentLiveKind::Failure && e.id.starts_with("message-error-") {
                if self.composer_text().is_empty() {
                    self.set_composer_text(&sent);
                }
                self.last_sent = None;
                return;
            }
        }
    }

    fn apply_state(&mut self, view: &AgentView) {
        let state = view.state.as_ref();
        let (line, tone) = agent_view::state_line(state);
        self.detail.setStringValue(&ns(&line));
        self.detail.setTextColor(Some(&color(tone)));
        let can_stop = state.is_some_and(|s| s.can_stop);
        self.stop.setHidden(!can_stop);
        let can_reload = state.is_some_and(|s| s.can_reload);
        self.reload.setEnabled(can_reload);
        // A waiting reload: its reason on one line with Reload Now; the header's Reload steps aside.
        let pending = state.and_then(|s| s.reload.as_ref());
        self.pending_box.setHidden(pending.is_none());
        // [n23-menu] The header's Reload shows only when the agent failed, where it is the way forward; a healthy
        // agent's Reload did nothing visible ("remove the Reload button").
        let failed = state.is_some_and(|s| s.state == AgentRunState::Failed);
        self.reload.setHidden(pending.is_some() || !failed);
        // Stop takes the right edge while Reload is hidden.
        let edge = self.reload.frame();
        let stop_x = if self.reload.isHidden() { edge.origin.x + edge.size.width - 68.0 } else { edge.origin.x - 8.0 - 68.0 };
        self.stop.setFrameOrigin(objc2_foundation::NSPoint::new(stop_x, self.stop.frame().origin.y));
        if let Some(p) = pending {
            // The header already says it reloads after this turn: the line is why.
            let reason = p.reason.trim().trim_end_matches('.');
            self.pending_label.setStringValue(&ns(reason));
            self.pending_label.setToolTip(Some(&ns(reason)));
            self.reload_now.setEnabled(can_reload);
        }
        // Failed: Reload is the way forward.
        self.reload.setBezelColor(failed.then(NSColor::controlAccentColor).as_deref());
        NSView::setNeedsDisplay(&self.reload, true);
        self.reload.setToolTip(Some(&ns(if can_reload {
            "Restarts the agent on the same conversation"
        } else {
            "Turn Agent on to reload"
        })));
        let review = view.review();
        if review.as_ref().map(|r| &r.id) != self.review_answered.as_ref() {
            self.review_answered = None;
        }
        if review != self.review {
            self.review = review.clone();
            self.fill_review(view);
        }
        if self.editing.is_some() && !matches!(state.map(|s| &s.state), Some(AgentRunState::Ready | AgentRunState::Working | AgentRunState::Compacting | AgentRunState::WaitingEngine | AgentRunState::WaitingProvider)) {
            // Editing ends when the agent turns off or needs review; the edited text stays in the composer.
            self.editing = None;
        }
        self.refresh_composer();
        self.layout();
    }

    fn fill_review(&mut self, view: &AgentView) {
        let storage = unsafe { self.review_text.textStorage() }.expect("text view has storage");
        storage.deleteCharactersInRange(NSRange::new(0, storage.length()));
        let Some(review) = &self.review else { return };
        let when = review.wake.and_then(|i| view.rows.get(i)).map(|r| time_label(r.time_ms()));
        let mut title = match &when {
            Some(t) => format!("Interrupted turn · {t}"),
            None => "Interrupted turn".to_string(),
        };
        if review.more > 0 {
            title.push_str(&format!(" · {} more after it", review.more));
        }
        self.review_title.setStringValue(&ns(&title));
        match review.wake.and_then(|i| view.rows.get(i)) {
            Some(row) => storage.appendAttributedString(&self.block(row, None, None, false, false, false)),
            None => storage.appendAttributedString(&plain("Its messages aren't in this session's history", &tertiary())),
        }
        if review.wake.is_some() && review.evidence.is_empty() {
            storage.appendAttributedString(&plain("No tool call or reply recorded after it", &tertiary()));
        }
        for i in &review.evidence {
            if let Some(row) = view.rows.get(*i) {
                storage.appendAttributedString(&self.block(row, None, None, false, false, false));
            }
        }
        self.review_text.scrollRangeToVisible(NSRange::new(0, 0));
    }

    fn refresh_composer(&self) {
        let c = agent_view::composer(self.state.as_ref());
        let editing = self.editing.is_some();
        let empty = self.composer_text().trim().is_empty();
        self.composer.setEditable(c.can_send || editing);
        self.composer.setTextColor(Some(&*if c.can_send { label() } else { tertiary() }));
        self.placeholder.setStringValue(&ns(c.placeholder));
        self.placeholder.setHidden(!empty || self.review.is_some());
        self.send.setTitle(&ns(if editing { "Resend" } else { "Send" }));
        // Resending forks the conversation, which only works while the agent is idle.
        let ready = self.state.as_ref().is_some_and(|s| s.state == AgentRunState::Ready);
        self.send.setEnabled(c.can_send && !empty && (!editing || ready));
        self.send.setToolTip(Some(&ns(match (editing, c.working) {
            (true, _) if !ready => "Available when the agent is ready",
            (true, _) => "Replaces the message and continues from there",
            (false, true) => "Sends after this turn",
            (false, false) => "Sends to the agent",
        })));
        self.interrupt.setHidden(!(c.working && c.can_send) || editing);
        self.interrupt.setEnabled(!empty);
        if let Some(e) = &self.editing {
            self.edit_label.setStringValue(&ns(&format!(
                "Editing {} · resending doesn't undo what the agent already did",
                e.time
            )));
            self.edit_label.setToolTip(Some(&ns("Editing starts a new branch from that message. Actions taken after it, outside this window, stay done.")));
        }
        let answered = self.review_answered.is_some();
        self.review_completed.setEnabled(!answered);
        self.review_abandon.setEnabled(!answered);
    }

    /// Frames for the current window size, the composer's text height and the mode (compose, edit, review).
    pub fn layout(&self) {
        let Some(content) = self.window.contentView() else { return };
        // Reading the newest entry stays so when the transcript changes size.
        let follow = self.at_bottom();
        let b = content.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let reviewing = self.review.is_some();
        for v in [&*self.composer_box as &NSView, &self.send] {
            v.setHidden(reviewing);
        }
        self.placeholder.setHidden(reviewing || !self.composer_text().is_empty());
        if reviewing {
            self.interrupt.setHidden(true);
        } else {
            self.refresh_interrupt_visibility();
        }
        for v in [&*self.review_title as &NSView, &self.review_box, &self.review_completed, &self.review_abandon] {
            v.setHidden(!reviewing);
        }
        let editing = self.editing.is_some() && !reviewing;
        self.edit_label.setHidden(!editing);
        self.edit_cancel.setHidden(!editing);

        let bottom = if reviewing {
            // Title, the evidence (up to about half the window), the answers.
            let wanted = self.text_height(&self.review_text, w - 2.0 * PAD) + 16.0;
            let evidence = wanted.clamp(60.0, ((h - HEADER - self.strip.height_for(w - 2.0 * PAD)) * 0.5).max(60.0));
            let buttons_y = 12.0;
            let bw = 96.0;
            self.review_completed.setFrame(rect(w - PAD - bw, buttons_y, bw, 28.0));
            self.review_abandon.setFrame(rect(w - PAD - 2.0 * bw - 8.0, buttons_y, bw, 28.0));
            let box_y = buttons_y + 28.0 + 10.0;
            self.review_box.setFrame(rect(PAD, box_y, w - 2.0 * PAD, evidence));
            self.review_scroll.setFrame(rect(1.0, 1.0, w - 2.0 * PAD - 2.0, evidence - 2.0));
            let title_y = box_y + evidence + 8.0;
            self.review_title.setFrame(rect(PAD + 2.0, title_y, w - 2.0 * PAD, 18.0));
            title_y + 18.0 + 10.0
        } else {
            let text_h = self.text_height(&self.composer, w - 2.0 * PAD - 200.0).clamp(COMPOSER_MIN, COMPOSER_MAX);
            let box_h = text_h + 14.0;
            let y = 12.0;
            let send_w = 84.0;
            self.send.setFrame(rect(w - PAD - send_w, y + box_h - 28.0 - (box_h - 28.0) / 2.0, send_w, 28.0));
            let interrupt_w = 92.0;
            let field_right = if self.interrupt.isHidden() { w - PAD - send_w - 8.0 } else { w - PAD - send_w - 8.0 - interrupt_w - 6.0 };
            self.interrupt.setFrame(rect(w - PAD - send_w - 8.0 - interrupt_w, self.send.frame().origin.y, interrupt_w, 28.0));
            self.composer_box.setFrame(rect(PAD, y, field_right - PAD, box_h));
            self.composer_scroll.setFrame(rect(10.0, 7.0, field_right - PAD - 20.0, text_h));
            self.placeholder.setFrame(rect(13.0, 7.0 + text_h - 17.0, field_right - PAD - 26.0, 17.0));
            let mut top = y + box_h + 10.0;
            if editing {
                let cancel_w = 76.0;
                self.edit_cancel.setFrame(rect(w - PAD - cancel_w, top - 2.0, cancel_w, 26.0));
                self.edit_label.setFrame(rect(PAD + 2.0, top + 4.0, w - 3.0 * PAD - cancel_w, 16.0));
                top += EDIT_STRIP;
            }
            top
        };
        // Under the header: the telemetry strip, then the pending reload's line.
        let mut top = h - HEADER;
        let strip_h = self.strip.height_for(w - 2.0 * PAD);
        self.strip.setFrame(rect(PAD, top - strip_h, w - 2.0 * PAD, strip_h));
        top -= strip_h + 10.0;
        if !self.pending_box.isHidden() {
            self.pending_box.setFrame(rect(PAD, top - PENDING_H, w - 2.0 * PAD, PENDING_H));
            let bw = self.reload_now.fittingSize().width.max(96.0);
            self.reload_now.setFrame(rect(w - 2.0 * PAD - bw - 4.0, 3.0, bw, 28.0));
            self.pending_label.setFrame(rect(12.0, 9.0, (w - 2.0 * PAD - bw - 28.0).max(40.0), 16.0));
            top -= PENDING_H + 8.0;
        }
        let body = rect(0.0, bottom, w, (top - bottom).max(40.0));
        self.edge.setFrame(rect(0.0, body.origin.y + body.size.height, w, 1.0));
        self.scroll.setFrame(body);
        let size = self.empty.frame().size;
        self.empty.setFrameOrigin(NSPoint::new(((w - size.width) / 2.0).round(), (bottom + (body.size.height - size.height) / 2.0).round()));
        self.text.setFrameSize(NSSize::new(body.size.width, self.text.frame().size.height.max(body.size.height)));
        if follow {
            self.scroll_to_end();
        }
    }

    fn refresh_interrupt_visibility(&self) {
        let c = agent_view::composer(self.state.as_ref());
        self.interrupt.setHidden(!(c.working && c.can_send) || self.editing.is_some());
    }

    /// The height a text view's text takes at `width` (its insets included), measured on the text itself.
    fn text_height(&self, view: &NSTextView, width: f64) -> f64 {
        let inset = view.textContainerInset();
        let Some(storage) = (unsafe { view.textStorage() }) else { return COMPOSER_MIN };
        let line = view.font().map_or(17.0, |f| (f.ascender() - f.descender() + f.leading()).ceil());
        if storage.length() == 0 {
            return line + 2.0 * inset.height;
        }
        let padding = unsafe { view.textContainer() }.map_or(5.0, |c| c.lineFragmentPadding());
        let size = NSSize::new((width - 2.0 * inset.width - 2.0 * padding).max(40.0), f64::MAX);
        let options = objc2_app_kit::NSStringDrawingOptions::UsesLineFragmentOrigin | objc2_app_kit::NSStringDrawingOptions::UsesFontLeading;
        let used = objc2_app_kit::NSAttributedStringNSExtendedStringDrawing::boundingRectWithSize_options_context(&***storage, size, options, None);
        // A trailing newline still takes a line in the field.
        let trailing = if storage.string().to_string().ends_with('\n') { line } else { 0.0 };
        used.size.height.ceil().max(line) + trailing + 2.0 * inset.height
    }

    pub fn composer_text(&self) -> String {
        self.composer.string().to_string()
    }

    pub fn set_composer_text(&self, text: &str) {
        self.composer.setString(&ns(text));
        self.composer_changed();
    }

    /// Typing: the placeholder, the buttons and the field's height follow the text.
    pub fn composer_changed(&self) {
        self.refresh_composer();
        self.layout();
    }

    /// Send (or Resend while editing): the action, and the composer cleared. `None` when nothing can be sent.
    pub fn send(&mut self, steer: bool) -> Option<UiAction> {
        let c = agent_view::composer(self.state.as_ref());
        let text = self.composer_text();
        if !c.can_send || text.trim().is_empty() {
            return None;
        }
        let ready = self.state.as_ref().is_some_and(|s| s.state == AgentRunState::Ready);
        if self.editing.is_some() && !ready {
            return None;
        }
        let action = match self.editing.take() {
            Some(e) => AgentWindowAction::EditAndResend { session_entry_id: e.session_entry_id, text: text.clone() },
            None => AgentWindowAction::Send { text: text.clone(), steer },
        };
        self.last_sent = Some(text);
        self.set_composer_text("");
        Some(UiAction::AgentWindow(action))
    }

    /// Edit on a typed message: its text in the composer, ready to resend.
    pub fn begin_edit(&mut self, key: &str, view: &AgentView) {
        let Some(row) = view.rows.iter().find(|r| r.key() == key) else { return };
        let Some(Edit::Available { session_entry_id, text }) = agent_view::edit(row, view.state.as_ref()) else { return };
        let draft = match self.editing.take() {
            Some(e) => e.draft,
            None => self.composer_text(),
        };
        self.editing = Some(Editing { session_entry_id, time: time_label(row.time_ms()), draft });
        self.set_composer_text(&text);
        self.window.makeFirstResponder(Some(&self.composer));
    }

    pub fn cancel_edit(&mut self) {
        if let Some(e) = self.editing.take() {
            self.set_composer_text(&e.draft);
        }
    }

    /// Completed or Abandon on the wake under review: the action, once.
    pub fn resolve(&mut self, completed: bool) -> Option<UiAction> {
        let review = self.review.as_ref()?;
        if self.review_answered.is_some() {
            return None;
        }
        self.review_answered = Some(review.id.clone());
        let action = UiAction::AgentWindow(AgentWindowAction::ResolveDelivery { id: review.id.clone(), completed });
        self.refresh_composer();
        Some(action)
    }

    pub fn can_stop(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.can_stop)
    }
    pub fn can_reload(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.can_reload)
    }

    fn at_bottom(&self) -> bool {
        let visible = self.scroll.contentView().bounds();
        visible.origin.y + visible.size.height >= self.text.frame().size.height - 24.0
    }

    /// An attachment's bytes arrived (or did not): every block showing it redraws.
    pub fn image_loaded(&mut self, id: &str, bytes: Option<Vec<u8>>, view: &AgentView) {
        let image = bytes.and_then(|b| {
            let data = objc2_foundation::NSData::with_bytes(&b);
            NSImage::initWithData(NSImage::alloc(), &data)
        });
        self.images.insert(id.into(), image.map_or(Img::Missing, Img::Ready));
        self.image_order.retain(|i| i != id);
        self.image_order.push(id.into());
        while self.image_order.len() > IMAGE_CACHE {
            let old = self.image_order.remove(0);
            self.images.remove(&old);
        }
        let follow = self.at_bottom();
        for (i, row) in view.rows.iter().enumerate() {
            if row.live().is_some_and(|e| e.attachments.iter().any(|a| a.archive_id.as_deref() == Some(id))) {
                self.replace(i, view);
            }
        }
        if follow {
            self.scroll_to_end();
        }
        if self.review.is_some() {
            self.fill_review(view);
        }
    }

    /// The image for an attachment id; starts reading it from the archive the first time.
    fn image(&self, id: &str) -> Img {
        if let Some(img) = self.images.get(id) {
            return img.clone();
        }
        request_image(id);
        Img::Loading
    }

    /// Row `i`'s block in the timeline: a request heads its turn; everything else sits indented under it.
    fn row_block(&self, view: &AgentView, i: usize) -> Retained<NSAttributedString> {
        let row = &view.rows[i];
        let prompt = is_prompt(row);
        let turn = prompt.then(|| agent_debug::turn_at(&view.rows, i, &view.ttft, running(view.state.as_ref())));
        self.block(row, turn.as_ref(), view.state.as_ref(), true, !prompt, i > 0)
    }

    /// One row's block. A turn's request: `14:02:41  Wake  Spoke  6.2 s · 3 tools` over the request's text; an
    /// event: `14:02:44  Search transcript · result` over its text (tool text monospaced) and images, indented.
    /// Long bodies are cut to a few lines by kind. Each block ends with one paragraph separator (U+2029).
    /// `with_edit`: the Edit link (not in the review's copy); `gap`: space above a turn (not the first block).
    fn block(&self, row: &Row, turn: Option<&agent_debug::Turn>, state: Option<&AgentWindowState>, with_edit: bool, indent: bool, gap: bool) -> Retained<NSAttributedString> {
        let out = NSMutableAttributedString::new();
        let Some(display) = agent_view::display(row) else { return Retained::into_super(out) };
        let left = if indent { INDENT } else { 0.0 };
        let head_style = NSMutableParagraphStyle::new();
        head_style.setParagraphSpacing(3.0);
        head_style.setFirstLineHeadIndent(left);
        head_style.setHeadIndent(left);
        if turn.is_some() {
            // A turn's heading stays one line: its facts truncate rather than wrap a lone word.
            head_style.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            if gap {
                head_style.setParagraphSpacingBefore(12.0);
            }
        }
        let body_style = NSMutableParagraphStyle::new();
        body_style.setLineSpacing(2.0);
        body_style.setParagraphSpacing(if indent { 10.0 } else { 12.0 });
        body_style.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        body_style.setFirstLineHeadIndent(left);
        body_style.setHeadIndent(left);
        let Display { heading, heading_tone, body, body_tone, images } = display;
        let live = row.live();
        let kind = live.map(|e| e.kind.clone());
        let tool_text = matches!(kind, Some(AgentLiveKind::ToolCall | AgentLiveKind::ToolResult));
        match turn {
            Some(t) => {
                let h = agent_debug::turn_heading(t);
                let what = match (h.what, live) {
                    ("Context", Some(e)) if !e.sent && !e.queued => "Would send",
                    (crate::identity::USER_HEADING, Some(e)) if e.queued && !e.sent => if cfg!(feature = "public") { "You · queued" } else { "User · queued" },
                    (what, _) => what,
                };
                append(&out, &format!("{}   ", time_label(row.time_ms())), &digits(11.0), &tertiary(), &head_style, None);
                append(&out, what, &semibold(13.0), &label(), &head_style, None);
                if !h.outcome.is_empty() {
                    append(&out, "   ", &font(12.0), &tertiary(), &head_style, None);
                    append(&out, h.outcome, &medium(12.0), &color(h.outcome_tone), &head_style, None);
                }
                if !h.facts.is_empty() {
                    append(&out, &format!("   {}", h.facts), &digits(11.5), &secondary(), &head_style, None);
                }
            }
            None => {
                append(&out, &format!("{}   ", time_label(row.time_ms())), &digits(10.5), &tertiary(), &head_style, None);
                append(&out, &heading, &medium(if indent { 12.0 } else { 12.5 }), &color(heading_tone), &head_style, None);
            }
        }
        if with_edit {
            match agent_view::edit(row, state) {
                Some(Edit::Available { .. }) => {
                    append(&out, "   ", &medium(12.0), &tertiary(), &head_style, None);
                    append(&out, "Edit", &medium(12.0), &note_blue(), &head_style, Some(Extra::Link(format!("heimdall-edit:{}", row.key()))));
                }
                Some(Edit::Unavailable(why)) => {
                    append(&out, "   ", &medium(12.0), &tertiary(), &head_style, None);
                    append(&out, "Edit", &medium(12.0), &tertiary(), &head_style, Some(Extra::Tip(why.to_string())));
                }
                None => {}
            }
        }
        if body.is_empty() && images.is_empty() {
            // A one-line entry: the heading says it all.
            append(&out, "\u{2029}", &medium(12.0), &tertiary(), &body_style, None);
            return Retained::into_super(out);
        }
        append(&out, "\n", &medium(12.0), &tertiary(), &head_style, None);
        let body = agent_debug::clamp_lines(&body, agent_debug::max_lines(row));
        // A newline inside the body must not end the block: use line separators.
        let text = body.replace("\r\n", "\u{2028}").replace('\n', "\u{2028}");
        let body_colour = color(body_tone);
        let body_font = match kind {
            _ if tool_text => mono(11.0),
            Some(AgentLiveKind::Reply) => font(13.0),
            Some(AgentLiveKind::Prompt) if turn.is_some() => font(12.5),
            _ => font(12.5),
        };
        if !text.is_empty() {
            append(&out, &text, &body_font, &body_colour, &body_style, None);
        }
        for (n, attachment) in images.iter().enumerate() {
            if n > 0 || !text.is_empty() {
                append(&out, "\u{2028}", &font(13.0), &body_colour, &body_style, None);
            }
            match attachment.archive_id.as_deref().filter(|_| attachment.media_type.starts_with("image/")) {
                Some(id) => match self.image(id) {
                    Img::Ready(image) => out.appendAttributedString(&thumbnail(&image, &body_style)),
                    Img::Loading => append(&out, "Image", &font(12.0), &tertiary(), &body_style, None),
                    Img::Missing => append(&out, "Image no longer in the archive", &font(12.0), &tertiary(), &body_style, None),
                },
                None => append(&out, "Attachment not in the archive", &font(12.0), &tertiary(), &body_style, None),
            }
        }
        append(&out, "\u{2029}", &font(13.0), &label(), &body_style, None);
        Retained::into_super(out)
    }
}

fn is_prompt(row: &Row) -> bool {
    row.live().is_some_and(|e| e.kind == AgentLiveKind::Prompt)
}

/// The agent is mid-turn: the last turn is still running.
fn running(state: Option<&AgentWindowState>) -> bool {
    state.is_some_and(|s| s.can_stop || matches!(s.state, AgentRunState::Working | AgentRunState::Compacting))
}

fn semibold(size: f64) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, unsafe { objc2_app_kit::NSFontWeightSemibold })
}

fn mono(size: f64) -> Retained<NSFont> {
    NSFont::monospacedSystemFontOfSize_weight(size, unsafe { objc2_app_kit::NSFontWeightRegular })
}

/// Local "14:05".
fn short_time(ms: u64) -> String {
    time_label(ms).rsplit_once(':').map(|(hm, _)| hm.to_string()).unwrap_or_default()
}

/// Starts reading an attachment from the archive off the main thread, once per id while it is in flight; the
/// result redraws the blocks showing it.
fn request_image(id: &str) {
    thread_local! {
        static IN_FLIGHT: std::cell::RefCell<std::collections::HashSet<String>> = Default::default();
    }
    if !IN_FLIGHT.with(|f| f.borrow_mut().insert(id.to_string())) {
        return;
    }
    let id = id.to_string();
    let done = |id: String, bytes: Option<Vec<u8>>| {
        dispatch2::DispatchQueue::main().exec_async(move || {
            IN_FLIGHT.with(|f| f.borrow_mut().remove(&id));
            super::with_state(move |s| s.agent_image_loaded(&id, bytes))
        })
    };
    let Some(loader) = crate::attachment_loader() else { return done(id, None) };
    let spawned = std::thread::Builder::new().name("heimdall-agent-image".into()).spawn(move || {
        let bytes = loader(&id);
        done(id, bytes);
    });
    if spawned.is_err() {
        eprintln!("Agent window: could not start an image read");
    }
}

/// An image scaled into the thumbnail box, as an attachment character.
fn thumbnail(image: &NSImage, style: &NSMutableParagraphStyle) -> Retained<NSAttributedString> {
    let size = image.size();
    let scale = (THUMB_W / size.width.max(1.0)).min(THUMB_H / size.height.max(1.0)).min(1.0);
    let attachment = NSTextAttachment::new();
    attachment.setImage(Some(image));
    attachment.setBounds(rect(0.0, 0.0, (size.width * scale).round(), (size.height * scale).round()));
    let s = NSAttributedString::attributedStringWithAttachment(&attachment);
    let m = NSMutableAttributedString::initWithAttributedString(NSMutableAttributedString::alloc(), &s);
    unsafe { m.addAttribute_value_range(objc2_app_kit::NSParagraphStyleAttributeName, style.as_ref(), NSRange::new(0, m.length())) };
    Retained::into_super(m)
}

/// A read-only selectable text view in a scroll view that hides its scroller at rest.
fn text_area(frame: NSRect, inset: NSSize, mtm: MainThreadMarker) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    // TextKit 1: the window measures and replaces blocks through the layout manager.
    let text = NSTextView::textViewUsingTextLayoutManager(false, mtm);
    text.setFrame(rect(0.0, 0.0, frame.size.width, frame.size.height));
    text.setEditable(false);
    text.setSelectable(true);
    text.setRichText(true);
    text.setDrawsBackground(false);
    text.setTextContainerInset(inset);
    text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    text.setVerticallyResizable(true);
    text.setHorizontallyResizable(false);
    text.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
    if let Some(container) = unsafe { text.textContainer() } {
        container.setWidthTracksTextView(true);
    }
    unsafe {
        let keys: [&NSString; 2] = [objc2_app_kit::NSForegroundColorAttributeName, objc2_app_kit::NSCursorAttributeName];
        let cursor = objc2_app_kit::NSCursor::pointingHandCursor();
        let blue = note_blue();
        let values: [&AnyObject; 2] = [blue.as_ref(), cursor.as_ref()];
        text.setLinkTextAttributes(Some(&NSDictionary::from_slices(&keys, &values)));
    }
    scroll.setDocumentView(Some(&text));
    (scroll, text)
}

pub fn color(tone: Tone) -> Retained<NSColor> {
    match tone {
        Tone::Primary => label(),
        Tone::Secondary => secondary(),
        Tone::Tertiary => tertiary(),
        Tone::Dim => NSColor::colorWithWhite_alpha(1.0, 0.5),
        Tone::Blue => note_blue(),
        Tone::Orange => orange(),
        Tone::Red => NSColor::systemRedColor(),
        Tone::Green => green(),
    }
}

/// Local "14:02:44".
pub fn time_label(ms: u64) -> String {
    if ms == 0 {
        return String::new();
    }
    thread_local! {
        static FORMATTER: Retained<objc2_foundation::NSDateFormatter> = {
            let f = objc2_foundation::NSDateFormatter::new();
            f.setDateFormat(Some(&ns("HH:mm:ss")));
            f
        };
    }
    let date = objc2_foundation::NSDate::dateWithTimeIntervalSince1970(ms as f64 / 1000.0);
    FORMATTER.with(|f| f.stringFromDate(&date).to_string())
}

/// "claude-opus-5-5" → "Opus 5.5"; anything else as given.
pub fn model_name(model: &str) -> String {
    let parts: Vec<&str> = model.split('-').collect();
    if let ["claude", family, major, minor, ..] = parts.as_slice() {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if digits(major) && digits(minor) && minor.len() <= 2 {
            let mut f = family.chars();
            let family: String = f.next().map(|c| c.to_uppercase().chain(f).collect()).unwrap_or_default();
            return format!("{family} {major}.{minor}");
        }
    }
    model.to_string()
}

enum Extra {
    Link(String),
    Tip(String),
}

fn append(out: &NSMutableAttributedString, text: &str, font: &NSFont, color: &NSColor, paragraph: &NSMutableParagraphStyle, extra: Option<Extra>) {
    unsafe {
        let mut keys: Vec<&NSString> = vec![
            objc2_app_kit::NSFontAttributeName,
            objc2_app_kit::NSForegroundColorAttributeName,
            objc2_app_kit::NSParagraphStyleAttributeName,
        ];
        let extra_value = extra.as_ref().map(|e| match e {
            Extra::Link(s) | Extra::Tip(s) => ns(s),
        });
        let mut values: Vec<&AnyObject> = vec![font.as_ref(), color.as_ref(), paragraph.as_ref()];
        if let (Some(e), Some(v)) = (&extra, &extra_value) {
            keys.push(match e {
                Extra::Link(_) => objc2_app_kit::NSLinkAttributeName,
                Extra::Tip(_) => objc2_app_kit::NSToolTipAttributeName,
            });
            values.push(v.as_ref());
        }
        let attrs = NSDictionary::from_slices(&keys, &values);
        let piece = NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &ns(text), Some(&attrs));
        out.appendAttributedString(&piece);
    }
}

fn plain(text: &str, colour: &NSColor) -> Retained<NSAttributedString> {
    let out = NSMutableAttributedString::new();
    let style = NSMutableParagraphStyle::new();
    style.setParagraphSpacing(10.0);
    append(&out, text, &font(13.0), colour, &style, None);
    append(&out, "\u{2029}", &font(13.0), colour, &style, None);
    Retained::into_super(out)
}

define_class!(
    /// A rounded field background (the composer, the review's evidence): the text-field fill with a hairline edge.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallFieldBox"]
    pub struct FieldBox;

    impl FieldBox {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                rect(b.origin.x + 0.5, b.origin.y + 0.5, b.size.width - 1.0, b.size.height - 1.0),
                8.0,
                8.0,
            );
            NSColor::colorWithWhite_alpha(1.0, 0.05).setFill();
            path.fill();
            NSColor::separatorColor().setStroke();
            path.setLineWidth(1.0);
            path.stroke();
        }
    }
);

impl FieldBox {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

pub struct StripIvars {
    tiles: RefCell<Vec<Tile>>,
}

define_class!(
    /// The telemetry strip: one rounded cell per number (`agent_debug::tiles`), the value over its caption, a thin
    /// meter under the context's value. Six across when wide, three or two per row when narrow. Draws only when a
    /// number changes.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallTelemetryStrip"]
    #[ivars = StripIvars]
    pub struct TelemetryStrip;

    impl TelemetryStrip {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl TelemetryStrip {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(StripIvars { tiles: RefCell::new(vec![]) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn tiles(&self) -> Vec<Tile> {
        self.ivars().tiles.borrow().clone()
    }

    /// New numbers; true when the strip's height may have changed (the caller lays out again).
    fn set_tiles(&self, tiles: Vec<Tile>) -> bool {
        let mut current = self.ivars().tiles.borrow_mut();
        if *current == tiles {
            return false;
        }
        let relayout = current.len() != tiles.len();
        *current = tiles;
        drop(current);
        NSView::setNeedsDisplay(self, true);
        relayout
    }

    fn columns(&self, width: f64) -> usize {
        let n = self.ivars().tiles.borrow().len().max(1);
        [6usize, 3, 2].into_iter().find(|&c| c <= n && width >= c as f64 * 150.0 - TILE_GAP).unwrap_or(2).min(n)
    }

    pub fn height_for(&self, width: f64) -> f64 {
        let n = self.ivars().tiles.borrow().len();
        if n == 0 {
            return 0.0;
        }
        let rows = n.div_ceil(self.columns(width)) as f64;
        rows * TILE_H + (rows - 1.0) * TILE_GAP
    }

    fn draw(&self) {
        let tiles = self.ivars().tiles.borrow();
        let b = self.bounds();
        let cols = self.columns(b.size.width);
        let w = ((b.size.width - (cols as f64 - 1.0) * TILE_GAP) / cols as f64).floor();
        let truncating = NSMutableParagraphStyle::new();
        truncating.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let options = objc2_app_kit::NSStringDrawingOptions::UsesLineFragmentOrigin | objc2_app_kit::NSStringDrawingOptions::TruncatesLastVisibleLine;
        for (i, tile) in tiles.iter().enumerate() {
            let (col, row) = ((i % cols) as f64, (i / cols) as f64);
            let x = col * (w + TILE_GAP);
            let y = row * (TILE_H + TILE_GAP);
            let cell = rect(x + 0.5, y + 0.5, w - 1.0, TILE_H - 1.0);
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(cell, 9.0, 9.0);
            NSColor::colorWithWhite_alpha(1.0, 0.045).setFill();
            path.fill();
            NSColor::colorWithWhite_alpha(1.0, 0.06).setStroke();
            path.setLineWidth(1.0);
            path.stroke();
            let draw = |text: &str, font: &NSFont, colour: &NSColor, frame: NSRect| {
                let s = NSMutableAttributedString::new();
                append(&s, text, font, colour, &truncating, None);
                objc2_app_kit::NSAttributedStringNSExtendedStringDrawing::drawWithRect_options_context(&**s, frame, options, None);
            };
            let value_colour = if tile.tone == Tone::Tertiary { tertiary() } else { color(tile.tone) };
            draw(&tile.value, &value_font(), &value_colour, rect(x + 12.0, y + 7.0, w - 24.0, 20.0));
            draw(&tile.caption, &font(10.5), &color(tile.caption_tone), rect(x + 12.0, y + 27.0, w - 24.0, 14.0));
            if let Some(fill) = tile.fill {
                // The meter: a hairline track along the cell's foot, filled to the share used.
                let (mx, my, mw) = (x + 12.0, y + TILE_H - 6.0, w - 24.0);
                NSColor::colorWithWhite_alpha(1.0, 0.10).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(mx, my, mw, 2.0), 1.0, 1.0).fill();
                let ink = if tile.tone == Tone::Orange { orange() } else { NSColor::controlAccentColor() };
                ink.setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(mx, my, (mw * fill).max(2.0), 2.0), 1.0, 1.0).fill();
            }
        }
    }
}

fn value_font() -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(15.0, unsafe { objc2_app_kit::NSFontWeightMedium })
}

define_class!(
    /// The Agent window's buttons' target and its text views' delegate. Every call goes through the main-thread
    /// state, as the menu's controller does.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallAgentDelegate"]
    pub struct AgentDelegate;

    unsafe impl NSObjectProtocol for AgentDelegate {}

    unsafe impl NSTextDelegate for AgentDelegate {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, _notification: &NSNotification) {
            super::with_state(|s| {
                if let Some(w) = &s.agent_window {
                    w.composer_changed();
                }
            });
        }
    }

    unsafe impl NSTextViewDelegate for AgentDelegate {
        #[unsafe(method(textView:clickedOnLink:atIndex:))]
        fn clicked_on_link(&self, _view: &NSTextView, link: &AnyObject, _index: usize) -> bool {
            link_clicked(link)
        }

        /// Return sends; Shift- or Option-Return starts a new line.
        #[unsafe(method(textView:doCommandBySelector:))]
        fn do_command(&self, view: &NSTextView, command: Sel) -> bool {
            composer_command(view, command, self.mtm())
        }
    }

    impl AgentDelegate {
        #[unsafe(method(agentSend:))]
        fn send(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_send(false));
        }
        #[unsafe(method(agentInterrupt:))]
        fn interrupt(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_send(true));
        }
        #[unsafe(method(agentStop:))]
        fn stop(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_stop());
        }
        #[unsafe(method(agentReload:))]
        fn reload(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_reload());
        }
        #[unsafe(method(agentReloadNow:))]
        fn reload_now(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_reload_now());
        }
        #[unsafe(method(agentCancelEdit:))]
        fn cancel_edit(&self, _sender: &AnyObject) {
            super::with_state(|s| {
                if let Some(w) = &mut s.agent_window {
                    w.cancel_edit();
                }
            });
        }
        #[unsafe(method(agentReviewCompleted:))]
        fn review_completed(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_resolve(true));
        }
        #[unsafe(method(agentReviewAbandoned:))]
        fn review_abandoned(&self, _sender: &AnyObject) {
            super::with_state(|s| s.agent_resolve(false));
        }
        #[unsafe(method(agentWindowResized:))]
        fn resized(&self, _notification: &NSNotification) {
            super::with_state(|s| {
                if let Some(w) = &s.agent_window {
                    w.layout();
                }
            });
        }
    }
);

/// An Edit link in the transcript: editing starts on the next turn of the state.
fn link_clicked(link: &AnyObject) -> bool {
    let link = if let Some(s) = link.downcast_ref::<NSString>() {
        s.to_string()
    } else if let Some(url) = link.downcast_ref::<objc2_foundation::NSURL>() {
        url.absoluteString().map(|s| s.to_string()).unwrap_or_default()
    } else {
        return false;
    };
    let Some(key) = link.strip_prefix("heimdall-edit:").map(str::to_owned) else { return false };
    super::with_state(move |s| s.agent_edit(&key));
    true
}

/// Return in the composer sends; Shift- or Option-Return is a new line (AppKit's own handling).
fn composer_command(view: &NSTextView, command: Sel, mtm: MainThreadMarker) -> bool {
    if command != sel!(insertNewline:) {
        return false;
    }
    let newline = objc2_app_kit::NSApplication::sharedApplication(mtm)
        .currentEvent()
        .is_some_and(|e| e.modifierFlags().intersects(NSEventModifierFlags::Shift | NSEventModifierFlags::Option));
    if newline {
        return false;
    }
    let composer = view.retain();
    super::with_state(move |s| {
        if s.agent_window.as_ref().is_some_and(|w| std::ptr::eq(&*w.composer, &*composer)) {
            s.agent_send(false);
        }
    });
    true
}

impl AgentDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Lays the window out again whenever it resizes.
pub fn observe_resize(w: &AgentWindow) {
    let center = objc2_foundation::NSNotificationCenter::defaultCenter();
    unsafe {
        center.addObserver_selector_name_object(
            &w.delegate,
            sel!(agentWindowResized:),
            Some(objc2_app_kit::NSWindowDidResizeNotification),
            Some(w.window.as_ref()),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_strings_drop_plumbing() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("gpt-6.1-sol"), "gpt-6.1-sol");
    }
}
