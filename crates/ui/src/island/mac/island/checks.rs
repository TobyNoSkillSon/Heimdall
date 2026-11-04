//! `heimdall-ui-demo --island <dir>`: the island checked in process, offscreen. Stills of every gallery card's
//! states over the user's desktop crop (`island-<card>-<n>.png`); the windows' properties; clicks through the real event
//! path (synthetic `NSEvent`s sent to the click target's window: AppKit's own dispatch, the island's own `hit`); and
//! the cost at rest, measured. Nothing reaches the screen: the island's windows are never ordered in (the overlay
//! space check orders in one invisible stand-in parked far off every display).

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSMainMenuWindowLevel, NSWindow, NSWindowSharingType};
use objc2_foundation::{MainThreadMarker, NSDate, NSPoint, NSRunLoop};

use super::host::{with_host, Place};
use crate::island::object::gallery;
use crate::island::object::{Event, Point, Target};
use crate::island::ISLAND_WINDOW_TITLE;

/// Checks and their report.
pub(crate) struct Checks {
    pub failed: Vec<String>,
    pub report: String,
}

impl Checks {
    pub(crate) fn new() -> Checks {
        Checks { failed: vec![], report: String::new() }
    }
    pub(crate) fn check(&mut self, ok: bool, what: &str) {
        let line = format!("{} {what}", if ok { "check ok  " } else { "check FAIL" });
        eprintln!("{line}");
        let _ = writeln!(self.report, "{line}");
        if !ok {
            self.failed.push(what.to_string());
        }
    }
    pub(crate) fn note(&mut self, what: &str) {
        eprintln!("      {what}");
        let _ = writeln!(self.report, "      {what}");
    }
}

pub(crate) fn mouse(kind: NSEventType, at: NSPoint, window: isize) -> Retained<NSEvent> {
    NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(kind, at, NSEventModifierFlags::empty(), 0.0, window, None, 0, 1, 1.0)
        .expect("a mouse event")
}

pub(crate) fn key(code: u16, chars: &str, flags: NSEventModifierFlags, window: isize) -> Retained<NSEvent> {
    let s = objc2_foundation::NSString::from_str(chars);
    NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        NSEventType::KeyDown,
        NSPoint::new(0.0, 0.0),
        flags,
        0.0,
        window,
        None,
        &s,
        &s,
        false,
        code,
    )
    .expect("a key event")
}

/// Turns the main run loop for `seconds` (outputs go out on the main queue).
pub(crate) fn spin(seconds: f64) {
    let until = NSDate::dateWithTimeIntervalSinceNow(seconds);
    NSRunLoop::currentRunLoop().runUntilDate(&until);
}

pub(crate) fn spin_until(limit: f64, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed().as_secs_f64() < limit {
        if done() {
            return true;
        }
        spin(0.02);
    }
    done()
}

/// Moves the island's virtual clock on by `dt` seconds, firing what falls due, and paints.
pub(crate) fn advance(dt: f64) {
    let steps = (dt / 0.05).ceil().max(1.0) as usize;
    for _ in 0..steps {
        with_host(|h| {
            let t = h.clock.unwrap_or(0.0) + dt / steps as f64;
            h.clock = Some(t);
            h.apply(Event::Tick);
        });
    }
    spin(0.0);
}

/// A tap (down and up) on the island's `target`, sent to `window` as AppKit would deliver a trackpad tap.
pub(crate) fn tap(window: &NSWindow, target: &Target) -> bool {
    let Some((place, rect)) = with_host(|h| (h.place, h.island.targets().iter().find(|(t, _)| t == target).map(|(_, r)| *r))).and_then(|(p, r)| r.map(|r| (p, r))) else {
        return false;
    };
    let screen = place.to_screen(Point::new(rect.mid_x(), rect.mid_y()));
    let f = window.frame();
    let at = NSPoint::new(screen.x - f.origin.x, screen.y - f.origin.y);
    let n = window.windowNumber();
    window.sendEvent(&mouse(NSEventType::LeftMouseDown, at, n));
    window.sendEvent(&mouse(NSEventType::LeftMouseUp, at, n));
    true
}

/// A press on the island's `target`, dragged `dy` points (screen points: negative is down), released there.
pub(crate) fn drag(window: &NSWindow, target: &Target, dy: f64) -> bool {
    let Some((place, rect)) = with_host(|h| (h.place, h.island.targets().iter().find(|(t, _)| t == target).map(|(_, r)| *r))).and_then(|(p, r)| r.map(|r| (p, r))) else {
        return false;
    };
    let screen = place.to_screen(Point::new(rect.mid_x(), rect.mid_y()));
    let f = window.frame();
    let at = NSPoint::new(screen.x - f.origin.x, screen.y - f.origin.y);
    let n = window.windowNumber();
    window.sendEvent(&mouse(NSEventType::LeftMouseDown, at, n));
    for k in 1..=4 {
        window.sendEvent(&mouse(NSEventType::LeftMouseDragged, NSPoint::new(at.x, at.y + dy * k as f64 / 4.0), n));
    }
    window.sendEvent(&mouse(NSEventType::LeftMouseUp, NSPoint::new(at.x, at.y + dy), n));
    true
}

/// The last reply pulled down from the notch through the click target's real event path ("we should be
/// also able to pull the dynamic island down and that should show the last response"): a tap and a drag down on the
/// notch at rest, the lip band's tap and drag up; a swipe through the host's wheel handling.
fn pull_down(c: &mut Checks) {
    use crate::island::object::{Class, Token};
    let class = || with_host(|h| h.island.class()).unwrap_or(Class::Rest);
    super::apply(Event::Speaking(true));
    advance(0.1);
    super::apply(Event::SpokenText(Some("The build is green.".into())));
    advance(0.6);
    super::apply(Event::SpokenText(Some("Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.".into())));
    advance(1.0);
    let both = with_host(|h| h.island.spoken().len()).unwrap_or(0);
    c.check(both == 2, "while speaking the island holds both sentences, the second added to the first");
    super::apply(Event::Speaking(false));
    advance(2.0);
    let control = with_host(|h| h.panels().1).expect("host");
    c.check(class() == Class::Rest && with_host(|h| h.notch_catcher()).unwrap_or(false), "after the voice the island rests; the notch's click target waits for a pull");
    c.check(with_host(|h| h.idle()).unwrap_or(false), "at rest with a last reply nothing runs (the notch's target draws nothing)");
    c.check(tap(&control, &Target::Token(Token::Reply)), "the notch at rest is the last reply's target");
    advance(1.0);
    c.check(class() == Class::Reply, "a tap on the notch pulls the last reply down");
    c.check(tap(&control, &Target::ReplyBand), "the pulled-down reply's lip band is a target");
    advance(1.0);
    c.check(class() == Class::Rest, "a tap on its lip band pulls it up");
    c.check(drag(&control, &Target::Token(Token::Reply), -20.0), "a drag starts on the notch");
    advance(1.0);
    c.check(class() == Class::Reply, "a drag down from the notch pulls the last reply down");
    c.check(drag(&control, &Target::ReplyBand, 20.0), "a drag starts on the lip band");
    advance(1.0);
    c.check(class() == Class::Rest, "a drag up on its lip band pulls it up");
    with_host(|h| {
        h.wheel(0.0, 4.0);
        h.wheel(0.0, 5.0);
    });
    advance(1.0);
    c.check(class() == Class::Reply, "a two-finger swipe down over the notch pulls the last reply down");
    c.check(tap(&control, &Target::ReplyBand), "and its band pulls it up again");
    advance(1.0);
}

/// Bug note 2026-10-28T19-45-55: the question card's lip through the click target's real event path. A tap on the
/// lip band and a drag up on it tuck the card away with nothing sent to the agent; a tap and a drag down on the notch
/// bring it back; the last reply waits behind it.
fn question_lip(c: &mut Checks, actions: &std::rc::Rc<std::cell::RefCell<Vec<crate::model::UiAction>>>) {
    use crate::agent_notice::{AgentNotice, NoticeAction, NoticeKind};
    use crate::island::object::{Class, Token};
    let class = || with_host(|h| h.island.class()).unwrap_or(Class::Rest);
    let control = with_host(|h| h.panels().1).expect("host");
    let ask = AgentNotice { action: Some(NoticeAction::single("lip1", "Push the fix to main?", &["Yes", "No"], u64::MAX / 4)), ..AgentNotice::line("lip1", "Push the fix to main?", NoticeKind::Info) };
    super::apply(Event::Notice { notice: ask, body: None });
    advance(1.0);
    actions.borrow_mut().clear();
    let tuck = Target::AskTuck("lip1".into());
    c.check(class() == Class::Question, "a question shows its card");
    c.check(tap(&control, &tuck), "the card's lip band is a target");
    advance(1.0);
    c.check(class() == Class::Rest, "a tap on the card's lip tucks it away");
    c.check(tap(&control, &Target::Token(Token::Reply)), "the notch at rest is a target");
    advance(1.0);
    c.check(class() == Class::Question, "a tap on the notch brings the card back, ahead of the last reply");
    c.check(drag(&control, &tuck, 20.0), "a drag starts on the card's lip");
    advance(1.0);
    c.check(class() == Class::Rest, "a drag up on the card's lip tucks it away");
    c.check(drag(&control, &Target::Token(Token::Reply), -20.0), "a drag starts on the notch");
    advance(1.0);
    c.check(class() == Class::Question, "a drag down from the notch brings the card back");
    c.check(!actions.borrow().iter().any(|a| matches!(a, crate::model::UiAction::AgentNotice(_))), "tucking and bringing it back tell the agent nothing");
    super::apply(Event::CloseQuestion { id: "lip1".into(), answer: String::new() });
    advance(2.0);
}

/// The conversation opens while Heimdall thinks (bug note 2026-10-25T21-28-46) by a tap on its lip, and ⌃⌘H's own
/// handler pulls it down with the composer holding the keyboard (type at once); pressed again while he
/// types, it pulls it up and the composer lets the keyboard go. The composer's placeholder is drawn once, by the island
/// (bug note 2026-10-25T23-12-07).
fn open_chat(c: &mut Checks, dir: &Path, mtm: MainThreadMarker) {
    use crate::island::object::{Class, Token};
    let class = || with_host(|h| h.island.class()).unwrap_or(Class::Rest);
    let control = with_host(|h| h.panels().1).expect("host");
    super::apply(Event::Sent);
    super::apply(Event::Activity(Some(crate::island::Activity::Thinking)));
    advance(1.5);
    c.check(class() == Class::Lip, "Heimdall thinks: the thinking lip");
    c.check(tap(&control, &Target::Token(Token::Reply)), "the thinking lip is a target");
    advance(1.0);
    c.check(class() == Class::Reply, "a tap on the thinking lip pulls the conversation down");
    // Bug note 2026-10-26T19-14-21-2: ⌃⌘H closes it however it opened, the keyboard elsewhere.
    crate::mac::shortcuts::run(crate::shortcuts::Shortcut::Chat, mtm);
    advance(1.0);
    c.check(class() != Class::Reply, "⌃⌘H pulls up a conversation a tap opened");
    c.check(tap(&control, &Target::Token(Token::Reply)), "the thinking lip opens it again");
    advance(1.0);
    c.check(class() == Class::Reply, "pulled down again");
    c.check(tap(&control, &Target::ReplyBand), "and its band pulls it up");
    advance(1.0);
    super::apply(Event::Activity(None));
    advance(1.0);
    crate::mac::shortcuts::run(crate::shortcuts::Shortcut::Chat, mtm);
    advance(1.0);
    c.check(class() == Class::Reply, "⌃⌘H pulls the conversation down");
    let field = with_host(|h| h.fields.composer()).flatten();
    let placed = field.as_ref().is_some_and(|f| unsafe { f.superview() }.is_some());
    c.check(placed, "its composer is placed");
    let editing = field.as_ref().is_some_and(|f| f.currentEditor().is_some());
    let responder = control.firstResponder().is_some_and(|r| field.as_ref().and_then(|f| f.currentEditor()).is_some_and(|e| std::ptr::eq(&*r as *const _ as *const u8, &*e as *const _ as *const u8)));
    c.check(editing && responder, "⌃⌘H: the composer is the click target's first responder (User types at once)");
    c.check(field.as_ref().is_some_and(|f| f.placeholderString().is_none_or(|p| p.to_string().is_empty())), "the composer's native field draws no placeholder (the island draws the one)");
    let drawn = with_host(|h| h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().filter(|s| matches!(&*s.content, crate::island::object::Content::Text { lines, .. } if lines.iter().any(|l| *l == crate::island::object::composer_placeholder()))).count()).unwrap_or(0);
    c.check(drawn == 1, &format!("one drawn placeholder ({drawn})"));
    c.check(composite(&dir.join("island-composer-focused-empty.png"), None), "⌃⌘H: the conversation rendered with its focused, empty composer");
    c.check(composite(&dir.join("island-composer-focused-empty-2x.png"), Some(4.0)), "and the composer at 2x");
    // Typing goes through the field editor, as keys do (its change notification is the composer's).
    let typed = "what are we running on the gpu";
    type_in(&field, typed, true);
    advance(0.5);
    let shown = field.as_ref().map(|f| f.stringValue().to_string()).unwrap_or_default();
    c.check(shown == typed && placeholders() == 0, &format!("typed text: the field shows it ({shown:?}) and no placeholder draws ({})", placeholders()));
    c.check(composite(&dir.join("island-composer-typed.png"), None), "the composer with typed text rendered");
    c.check(composite(&dir.join("island-composer-typed-2x.png"), Some(4.0)), "and at 2x");
    // The placeholder's own words typed: they land where the placeholder was drawn (the switch is in place).
    type_in(&field, &crate::island::object::composer_placeholder(), true);
    advance(0.5);
    let typed_box = glyphs(&dir.join("island-composer-same-words-2x.png"));
    type_in(&field, "", true);
    advance(0.5);
    let drawn_box = glyphs(&dir.join("island-composer-placeholder-2x.png"));
    let aligned = matches!((typed_box, drawn_box), (Some(a), Some(b)) if (a.0 - b.0).abs() <= 3 && (a.1 - b.1).abs() <= 3);
    c.check(aligned, &format!("typed words replace the placeholder in place, within 0.75 pt (typed {typed_box:?}, drawn {drawn_box:?}, 2x crop pixels at 4 px per pt)"));
    // Real keys, as AppKit delivers them to the key window: the field shows them, then backspace empties it.
    let n = control.windowNumber();
    for (code, ch) in [(4u16, "h"), (34, "i")] {
        control.sendEvent(&key(code, ch, NSEventModifierFlags::empty(), n));
    }
    advance(0.3);
    let shown = field.as_ref().map(|f| f.stringValue().to_string()).unwrap_or_default();
    c.check(shown == "hi" && placeholders() == 0, &format!("key presses show in the field ({shown:?}) and hide the placeholder"));
    for _ in 0..2 {
        control.sendEvent(&key(51, "\u{7f}", NSEventModifierFlags::empty(), n));
    }
    advance(0.3);
    let shown = field.as_ref().map(|f| f.stringValue().to_string()).unwrap_or_default();
    c.check(shown.is_empty() && placeholders() == 1, &format!("backspaced empty ({shown:?}): the placeholder is back"));
    let long = "what are we running on the gpu that is almost all the time at sixty percent, and can we stop it while I am on a call with Dave";
    type_in(&field, long, true);
    advance(0.5);
    c.check(composite(&dir.join("island-composer-long-2x.png"), Some(4.0)), "a long message in the composer rendered at 2x");
    type_in(&field, "", true);
    advance(0.5);
    c.check(placeholders() == 1, &format!("cleared: the placeholder is back ({})", placeholders()));
    crate::mac::shortcuts::run(crate::shortcuts::Shortcut::Chat, mtm);
    advance(1.0);
    c.check(class() == Class::Rest, "⌃⌘H again while typing pulls the conversation up");
    c.check(field.as_ref().is_some_and(|f| f.currentEditor().is_none()), "and the composer lets the keyboard go");
    // Opened by a tap, the composer is not focused: the empty field with its one placeholder.
    c.check(tap(&control, &Target::Token(Token::Reply)), "the notch opens the conversation again");
    advance(1.0);
    c.check(field.as_ref().is_some_and(|f| f.currentEditor().is_none()) && placeholders() == 1, "opened by a tap: not focused, one placeholder");
    c.check(composite(&dir.join("island-composer-empty.png"), None), "the empty composer rendered");
    c.check(composite(&dir.join("island-composer-empty-2x.png"), Some(4.0)), "and at 2x");
    c.check(tap(&control, &Target::ReplyBand), "its band pulls it up");
    advance(1.0);
}

/// Writes the composer's 2x crop to `path` and returns the left and top of the light glyph pixels in it (text over
/// the field's dark fill), in pixels.
fn glyphs(path: &Path) -> Option<(i64, i64)> {
    if !composite(path, Some(4.0)) {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let rep = objc2_app_kit::NSBitmapImageRep::imageRepWithData(&objc2_foundation::NSData::with_bytes(&data))?;
    let (w, h) = (rep.pixelsWide(), rep.pixelsHigh());
    let (mut left, mut top) = (i64::MAX, i64::MAX);
    // Skip the crop's 12 pt margin and the field's rounded ends; the caret is blue, so only grey-white counts.
    for y in 60..(h - 50) {
        for x in 60..(w / 2) {
            let c = rep.colorAtX_y(x, y)?;
            let (r, g, b) = (c.redComponent(), c.greenComponent(), c.blueComponent());
            if r > 0.4 && (r - b).abs() < 0.05 && (r - g).abs() < 0.05 {
                left = left.min(x as i64);
                top = top.min(y as i64);
            }
        }
    }
    (left < i64::MAX).then_some((left, top))
}

/// The drawn placeholders in the island's frame now.
fn placeholders() -> usize {
    let name = crate::island::object::composer_placeholder();
    with_host(|h| h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().filter(|s| matches!(&*s.content, crate::island::object::Content::Text { lines, .. } if lines.iter().any(|l| *l == name))).count()).unwrap_or(0)
}

/// Replaces the composer's text through its field editor (selecting all, then inserting), as typing does.
pub(crate) fn type_in(field: &Option<Retained<objc2_app_kit::NSTextField>>, text: &str, _focused: bool) {
    let Some(editor) = field.as_ref().and_then(|f| f.currentEditor()) else { return };
    unsafe {
        let _: () = objc2::msg_send![&*editor, selectAll: None::<&objc2::runtime::AnyObject>];
        if text.is_empty() {
            let _: () = objc2::msg_send![&*editor, deleteBackward: None::<&objc2::runtime::AnyObject>];
        } else {
            let _: () = objc2::msg_send![&*editor, insertText: &*super::super::util::ns(text)];
        }
    }
    spin(0.0);
}

/// The island as the user sees it: its drawing with the click target's native views (the composer's field, the typed
/// text and the caret) drawn over it in place, at the display's scale. `zoom`: only the composer, at that scale
/// (4 on a 2x display: the field at twice its real size).
pub(crate) fn composite(path: &Path, zoom: Option<f64>) -> bool {
    with_host(|h| {
        let (w, full) = h.tree.size;
        let (tw, th) = h.island.target_size();
        let frame = h.island.frame(h.clock.unwrap_or(0.0));
        let composer = frame.slots.iter().find(|s| s.id.as_ref() == "reply.composer.fill").map(|s| s.rect);
        // The crop: the composer and 12 pt round it, in the canvas (island x 0 is the canvas's centre).
        let (cx, cy, cw, ch, scale) = match (zoom, composer) {
            (Some(z), Some(r)) => (w / 2.0 + r.x - 12.0, r.y - 12.0, r.w + 24.0, r.h + 24.0, z),
            (Some(_), None) => return false,
            (None, _) => (0.0, 0.0, w, full, h.tree.scale),
        };
        let Some(c) = super::raster::Canvas::new(cw, ch, scale) else { return false };
        c.draw(|| {
            objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.36, 0.40, 0.50, 1.0).setFill();
            objc2_app_kit::NSBezierPath::fillRect(super::super::util::rect(0.0, 0.0, cw, ch));
        });
        h.tree.render_into(&c, (-cx, -cy));
        let Some(cv) = h.panels().1.contentView() else { return false };
        let b = cv.bounds();
        let Some(rep) = cv.bitmapImageRepForCachingDisplayInRect(b) else { return false };
        cv.cacheDisplayInRect_toBitmapImageRep(b, &rep);
        c.draw(|| {
            let at = super::super::util::rect(w / 2.0 - tw / 2.0 - cx, -cy, tw, th);
            let _ = unsafe { rep.drawInRect_fromRect_operation_fraction_respectFlipped_hints(at, objc2_foundation::NSRect::ZERO, objc2_app_kit::NSCompositingOperation::SourceOver, 1.0, true, None) };
        });
        c.png().is_some_and(|p| std::fs::write(path, p).is_ok())
    })
    .unwrap_or(false)
}

/// A note is as wide as its content (bug note 2026-10-19T18-46-34): a short text is drawn as wide as it is; a table
/// as wide as its columns need, wrapping cells only past the display's maximum (bug note 2026-10-20T23-12-49).
fn notes_fit(c: &mut Checks) {
    use crate::agent_notice::Detail;
    let (scale, room) = with_host(|h| (h.place.display.scale, crate::island::object::note_max_w(&h.place.display))).unwrap_or((2.0, 600.0));
    let short = super::raster::note(&Detail::Text("Saved.".into()), false, room, scale).map(|d| d.w);
    c.check(short.is_some_and(|w| w < 120.0), &format!("a short text note is drawn as wide as its text ({short:?} of {room})"));
    let long = super::raster::note(&Detail::Text("The build is green, the audio fix from Dave is in, and the old build stays as the rollback until tomorrow.".into()), false, room, scale).map(|d| d.w);
    c.check(long.is_some_and(|w| w > 300.0 && w <= 420.0), &format!("a long text note wraps at the reply's width ({long:?})"));
    let small = super::raster::note(&Detail::Table { headers: vec!["Run".into(), "Time".into()], rows: vec![vec!["a".into(), "1".into()]] }, false, room, scale).map(|d| d.w);
    c.check(small.is_some_and(|w| w < 160.0), &format!("a small table is as wide as its columns ({small:?})"));
    let jobs = super::raster::note(&jobs_table(), false, room, scale).map(|d| (d.w, d.h));
    c.check(jobs.is_some_and(|(w, h)| w > 344.0 && w <= room && h <= 22.0 + 3.0 * 26.0 + 1.0), &format!("the jobs table shows its four columns whole on one line each ({jobs:?} of {room})"));
    let narrow = super::raster::note(&jobs_table(), false, 300.0, scale).map(|d| (d.w, d.h));
    c.check(narrow.is_some_and(|(w, h)| w <= 300.5 && h > 22.0 + 3.0 * 26.0), &format!("squeezed, its cells wrap instead of cutting ({narrow:?})"));
}

fn jobs_table() -> crate::agent_notice::Detail {
    crate::island::object::gallery::jobs_note().detail.expect("a table")
}

/// Stills: each card's settled states over the desktop crop.
fn stills(c: &mut Checks, dir: &Path, mtm: MainThreadMarker) {
    let notch = super::super::gallery::notch_display(mtm);
    let plain = super::super::gallery::plain_display(notch.wallpaper.clone());
    let bg_notch = super::super::gallery::render_background(&notch, true).ok();
    let bg_plain = super::super::gallery::render_background(&plain, true).ok();
    let mut written = 0;
    for card in gallery::all() {
        let d = if card.scenario.display.has_notch() { &notch } else { &plain };
        let Some((bg, _)) = (if d.key == "notch" { bg_notch.as_ref() } else { bg_plain.as_ref() }) else { continue };
        // The moment before each next step (settled), and the end.
        let mut times: Vec<f64> = card.scenario.steps.iter().filter(|(_, st)| !matches!(st, crate::island::object::sim::Step::Ev(Event::SpeechLevel(_)))).map(|(t, _)| *t).collect();
        times.push(card.end);
        times.sort_by(|a, b| a.total_cmp(b));
        let wanted: Vec<f64> = times.windows(2).map(|w| (w[1] - 0.02).max(w[0] + 0.01)).collect();
        let fps = 60.0;
        let wanted_k: Vec<usize> = wanted.iter().map(|t| (t * fps).floor() as usize).collect();
        let (x0, _) = super::super::gallery::crop(d);
        let window_x = d.center_x - super::host::WINDOW_W / 2.0 - x0;
        let mut n = 0;
        let mut last_hash = 0u64;
        let result = super::super::gallery::play(&card, d, fps, |k, f, canvas, _| {
            if !wanted_k.contains(&k) || !(f.visible || f.glow > 0.0) {
                return;
            }
            let Some(out) = super::raster::Canvas::new(bg.pw as f64 / bg.scale, bg.ph as f64 / bg.scale, bg.scale) else { return };
            {
                let img = bg.image().expect("background image");
                let w = bg.pw as f64 / bg.scale;
                let h = bg.ph as f64 / bg.scale;
                objc2_core_graphics::CGContext::save_g_state(Some(&out.ctx));
                objc2_core_graphics::CGContext::translate_ctm(Some(&out.ctx), 0.0, h);
                objc2_core_graphics::CGContext::scale_ctm(Some(&out.ctx), 1.0, -1.0);
                objc2_core_graphics::CGContext::draw_image(Some(&out.ctx), objc2_core_foundation::CGRect::new(objc2_core_foundation::CGPoint::new(0.0, 0.0), objc2_core_foundation::CGSize::new(w, h)), Some(&img));
                let frame = canvas.image().expect("frame image");
                let (fw, fh) = (canvas.pw as f64 / canvas.scale, canvas.ph as f64 / canvas.scale);
                objc2_core_graphics::CGContext::draw_image(Some(&out.ctx), objc2_core_foundation::CGRect::new(objc2_core_foundation::CGPoint::new(window_x, h - fh), objc2_core_foundation::CGSize::new(fw, fh)), Some(&frame));
                objc2_core_graphics::CGContext::restore_g_state(Some(&out.ctx));
            }
            let Some(png) = out.png() else { return };
            let h = {
                use std::hash::{Hash, Hasher};
                let mut s = std::collections::hash_map::DefaultHasher::new();
                png.hash(&mut s);
                s.finish()
            };
            if h == last_hash {
                return;
            }
            last_hash = h;
            if std::fs::write(dir.join(format!("island-{}-{n}.png", card.name)), png).is_ok() {
                n += 1;
            }
        });
        if let Err(e) = result {
            c.check(false, &format!("{}: rendered ({e})", card.name));
        }
        written += n;
    }
    c.check(written >= gallery::all().len(), &format!("stills of every card's states ({written} PNGs)"));
}

/// The island's windows: never key, unshared by default, titled for the capture filters, at the right levels, click-through
/// except the click target, ordered out at rest with nothing running.
fn windows(c: &mut Checks) {
    let Some((panel, control, idle)) = with_host(|h| {
        let (p, ctl) = h.panels();
        (p, ctl, h.idle())
    }) else {
        c.check(false, "the island is installed");
        return;
    };
    c.check(panel.title().to_string() == ISLAND_WINDOW_TITLE, "the island window carries the capture filters' title");
    c.check(panel.sharingType() == NSWindowSharingType::None, "the island window is unshared by default (Show Islands on Streams off)");
    c.check(panel.ignoresMouseEvents(), "the island window is click-through");
    c.check(panel.level() == NSMainMenuWindowLevel + 3, "the island sits above the menu bar, below menus");
    c.check(!panel.canBecomeKeyWindow(), "the island never takes the keyboard");
    c.check(!control.ignoresMouseEvents() && control.level() == NSMainMenuWindowLevel + 4, "the click target takes clicks, above the island");
    c.check(panel.alphaValue() == 0.0 && control.alphaValue() == 0.0, "checks: the island's windows are invisible");
    c.check(idle, "at rest: no display link, no timer, windows out");
    // The hosted view and its root layer: y runs down from the notch (else the island hangs upside down from the
    // window's bottom, with the click target still at the notch).
    let (view_flipped, root_flipped) = with_host(|h| h.flipped()).unwrap_or((false, false));
    c.check(view_flipped, "the island's hosting view is flipped (isFlipped), so the window's top is the notch");
    c.check(root_flipped, "the island's root layer is geometry-flipped at runtime (AppKit keeps it so while hosted)");
}

/// Waking the symbol renderer before a motion leaves the tree as it was: no layer added, the mask and the root's
/// visibility restored.
fn wake(c: &mut Checks) {
    let Some((ran, same)) = with_host(|h| {
        let tree = &mut h.tree;
        let (hidden, layers, path) = (tree.root.isHidden(), tree.content_layers(), tree.mask_path());
        let ran = tree.wake_renderer();
        let same_path = objc2_core_graphics::CGPath::equal_to_path(path.as_deref(), tree.mask_path().as_deref()) || (path.is_none() && tree.mask_path().is_none());
        (ran, tree.root.isHidden() == hidden && tree.content_layers() == layers && same_path)
    }) else {
        c.check(false, "the island is installed");
        return;
    };
    c.check(ran && same, "waking the symbol renderer leaves the island's tree as it was");
}

/// Clicks through the click target's window: an answer, a token, a note's band.
fn clicks(c: &mut Checks, actions: &std::rc::Rc<std::cell::RefCell<Vec<crate::model::UiAction>>>) {
    use crate::agent_notice::{AgentNotice, AgentNoticeAction, NoticeAction, NoticeKind};
    let ask = AgentNotice { action: Some(NoticeAction::single("q1", "Install the new build?", &["Yes", "No"], u64::MAX / 4)), ..AgentNotice::line("q1", "Install the new build?", NoticeKind::Info) };
    super::apply(Event::Notice { notice: ask, body: None });
    advance(1.0);
    let control = with_host(|h| h.panels().1).expect("host");
    let answered = tap(&control, &Target::AskPick { id: "q1".into(), question: 0, option: 0 });
    c.check(answered, "the question's Yes is a target");
    spin_until(1.0, || actions.borrow().iter().any(|a| matches!(a, crate::model::UiAction::AgentNotice(AgentNoticeAction::Answer { answers, .. }) if answers.first().is_some_and(|x| x.labels == ["Yes"]))));
    c.check(
        actions.borrow().iter().any(|a| matches!(a, crate::model::UiAction::AgentNotice(AgentNoticeAction::Answer { request_id, answers, .. }) if request_id == "q1" && answers.first().is_some_and(|x| x.labels == ["Yes"]))),
        "a tap on Yes answers question q1 through the click target",
    );
    advance(2.0);
    c.check(with_host(|h| h.island.class() == crate::island::object::Class::Rest).unwrap_or(false), "the answered question retracts by itself");
    // An error reads with its red x and retracts to rest by itself, like every line (no token left behind).
    super::apply(Event::Notice { notice: AgentNotice::line("e1", "Build failed", NoticeKind::Error), body: None });
    advance(1.0);
    c.check(with_host(|h| h.island.class() == crate::island::object::Class::Stack).unwrap_or(false), "an error shows");
    advance(4.0);
    c.check(
        with_host(|h| h.island.class() == crate::island::object::Class::Rest && h.island.targets().is_empty()).unwrap_or(false),
        "the error clears itself and leaves no token",
    );
    // An agent's note (bug note 2026-10-20T18-41-17): it opens the last turn's pull-down; the pointer over it,
    // through the host's pointer path, holds it; once the pointer leaves it pulls itself up and nothing stays beside
    // the notch (bug note 2026-10-20T22-19-52): the notch alone opens it again.
    use crate::island::object::Class;
    let mut note = AgentNotice::line("d1", "Ornith 122B sources", NoticeKind::Info);
    note.detail = Some(crate::agent_notice::Detail::Text("One source.\nAnother source.".into()));
    super::apply(Event::Notice { notice: note, body: Some((344.0, 80.0)) });
    advance(1.0);
    let inside = with_host(|h| {
        let (_, h_) = h.island.target_size();
        h.place.to_screen(crate::island::object::Point::new(0.0, h_ / 2.0))
    })
    .expect("host");
    super::host::hover_at(Some(inside));
    advance(crate::island::object::NOTE_UNATTENDED + 5.0);
    c.check(with_host(|h| h.island.class() == Class::Reply).unwrap_or(false), "the pointer over an agent's note holds it open");
    super::host::hover_at(None);
    advance(crate::island::object::NOTE_UNATTENDED - 1.0);
    c.check(with_host(|h| h.island.class() == Class::Reply).unwrap_or(false), "after the pointer leaves it reads its full time");
    advance(1.5);
    c.check(
        with_host(|h| h.island.class() == Class::Rest && h.island.targets().iter().all(|(t, _)| *t == Target::Token(crate::island::object::Token::Reply))).unwrap_or(false),
        "then it pulls itself up, leaving only the notch's own target",
    );
    // Its close X, through the click target's real event path: gone for good.
    c.check(tap(&control, &Target::Token(crate::island::object::Token::Reply)), "the notch opens the last turn again");
    advance(1.0);
    c.check(tap(&control, &Target::NoteClose("d1".into())), "a tap on the note's X");
    advance(1.0);
    c.check(with_host(|h| h.island.note_ids().is_empty() && h.island.class() == Class::Rest).unwrap_or(false), "closes the note for good (nothing else was in that turn)");
    super::apply(Event::ClearNotices);
    advance(1.0);
}

/// The cost at rest, in real time (the one thing a simulation cannot measure): a live-clock island, offscreen, shows
/// a moment, retracts, then rests 3 s. Nothing may run and the process's CPU over the rest must stay near zero.
fn idle_cost(c: &mut Checks, mtm: MainThreadMarker) {
    let saved = super::host::HOST.with(|h| h.borrow_mut().take());
    let host = super::host::Host::new(mtm, Place::offscreen(), true);
    super::host::HOST.with(|h| *h.borrow_mut() = Some(host));
    super::apply(Event::Sent);
    let settled = spin_until(4.0, || with_host(|h| h.idle()).unwrap_or(false));
    c.check(settled, "a live island returns to rest after a moment (no link, no timer, windows out)");
    let cpu = || {
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
        u.ru_utime.tv_sec as f64 + u.ru_utime.tv_usec as f64 / 1e6 + u.ru_stime.tv_sec as f64 + u.ru_stime.tv_usec as f64 / 1e6
    };
    let (t0, c0) = (Instant::now(), cpu());
    spin(3.0);
    let (secs, used) = (t0.elapsed().as_secs_f64(), cpu() - c0);
    let ticks = with_host(|h| h.stats.ticks).unwrap_or(0);
    let pct = used / secs * 100.0;
    c.note(&format!("at rest: {:.3} % CPU over {secs:.1} s (process total), {ticks} island ticks since the moment", pct));
    c.check(pct < 0.5 && with_host(|h| h.idle()).unwrap_or(false), &format!("at rest the island costs nothing ({pct:.3} % CPU)"));
    super::host::HOST.with(|h| *h.borrow_mut() = saved);
}

/// The overlay space against the real SkyLight (the swipe itself needs a real Space switch and is not checked here): a
/// stand-in window, invisible and parked far off every display, joins a space of its own `Overlay` (not the live one),
/// is found in it, and leaves it on release; the island's windows keep today's collection behaviour, the fallback.
fn overlay_space(c: &mut Checks, mtm: MainThreadMarker) {
    use crate::mac::overlay_space::{real::Real, Overlay, SkyLight};
    use objc2::MainThreadOnly as _;
    let behaviour = with_host(|h| {
        let (p, ctl) = h.panels();
        (p.collectionBehavior(), ctl.collectionBehavior())
    });
    let wanted = super::super::util::overlay_behavior();
    c.check(behaviour == Some((wanted, wanted)), "the island's windows keep canJoinAllSpaces, stationary, full-screen auxiliary (the fallback)");
    let mut probe = Real::load();
    c.check(probe.missing().is_none(), &format!("SkyLight exports every call the overlay space needs (missing: {:?})", probe.missing()));
    let panel = objc2_app_kit::NSPanel::initWithContentRect_styleMask_backing_defer(
        objc2_app_kit::NSPanel::alloc(mtm),
        super::super::util::rect(-60_000.0, 60_000.0, 10.0, 10.0),
        objc2_app_kit::NSWindowStyleMask::Borderless | objc2_app_kit::NSWindowStyleMask::NonactivatingPanel,
        objc2_app_kit::NSBackingStoreType::Buffered,
        false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setAlphaValue(0.0);
    panel.setIgnoresMouseEvents(true);
    panel.setSharingType(NSWindowSharingType::None);
    panel.setCollectionBehavior(wanted);
    panel.orderFrontRegardless();
    let number = panel.windowNumber() as i64;
    let mut overlay = Overlay::new(Real::load());
    overlay.join(number, "check window");
    let lines = overlay.take_lines();
    let joined = lines.iter().any(|l| l.contains("joined by check window (verified)"));
    c.check(joined, &format!("a window joins a level-100 overlay space and is found in it ({})", lines.join(" | ")));
    let space = lines.iter().find_map(|l| l.strip_prefix("island: overlay space ")?.split(' ').next()?.parse::<u64>().ok());
    overlay.release();
    let released = overlay.take_lines();
    let conn = probe.connection();
    let left = space.is_some_and(|s| probe.spaces_of(conn, number).is_some_and(|list| !list.contains(&s)));
    c.check(left, &format!("on release it leaves the space ({})", released.join(" | ")));
    panel.orderOut(None);
}

/// `--island <dir>`.
pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("render_island runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    let actions: std::rc::Rc<std::cell::RefCell<Vec<crate::model::UiAction>>> = Default::default();
    let sink = actions.clone();
    super::set_sink(Box::new(move |a| sink.borrow_mut().push(a)));
    super::install_offscreen(mtm);
    windows(&mut c);
    overlay_space(&mut c, mtm);
    wake(&mut c);
    clicks(&mut c, &actions);
    pull_down(&mut c);
    question_lip(&mut c, &actions);
    open_chat(&mut c, dir, mtm);
    notes_fit(&mut c);
    stills(&mut c, dir, mtm);
    idle_cost(&mut c, mtm);
    std::fs::write(dir.join("island-checks.txt"), &c.report).ok();
    c.failed
}

/// `heimdall-ui-demo --relay-activity <dir>` (bug note 30 Oct 03:00: "In relay mode the island pull-down shows nothing
/// while the relay agent is working"). The pulled-down conversation through one turn, fed what the app sends: for a
/// Pi turn its rows come from Pi's session file, for a relay turn from the relay's live tool calls
/// (`chat_history::relay_call`); `bridge::relay_activity_tests` checks they are the same rows and the same states. Each
/// turn renders mid-run (the working dots under its rows) and ended (the reply, no dots), over the desktop grey at the
/// display's scale. Returns the failures.
pub(crate) fn render_relay_activity(dir: &Path) -> Vec<String> {
    use crate::island::chat::{ActionKind, ChatEntry, ChatKind, Heard, NoteShape};
    use crate::island::object::{Class, Content};
    let mtm = MainThreadMarker::new().expect("render_relay_activity runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    super::set_sink(Box::new(|_| {}));
    const REPLY: &str = "Green: 412 tests passed. A worker is fixing the slow one; I'll tell you when it lands.";
    let row = |k: u64, kind: ChatKind| ChatEntry { at_ms: 1_792_780_000_000 + k * 1000, kind };
    let earlier = vec![
        row(0, ChatKind::Owner("What did Dave push this morning?".into())),
        row(1, ChatKind::Heimdall { text: "The audio fix for the call recorder. The old build stays as the rollback.".into(), heard: Heard::Whole }),
        row(2, ChatKind::Owner("Is the build green?".into())),
    ];
    let mut during = earlier.clone();
    during.push(row(3, ChatKind::Heimdall { text: "On it, checking the build.".into(), heard: Heard::Whole }));
    during.push(row(4, ChatKind::Note { id: "notice-build".into(), title: "Build".into(), shape: NoteShape::Text }));
    during.push(row(5, ChatKind::Action { kind: ActionKind::Started, text: "Fix the slow test".into(), answer: None }));
    let mut ended = during.clone();
    ended.push(row(9, ChatKind::Heimdall { text: REPLY.into(), heard: Heard::Whole }));
    let dots = || with_host(|h| h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().any(|s| s.id.as_ref() == "reply.working" && matches!(&*s.content, Content::Dots))).unwrap_or(false);
    let class = || with_host(|h| h.island.class()).unwrap_or(Class::Rest);
    for turn in ["pi", "relay"] {
        // Each turn on a fresh island, as after a launch: nothing of the other turn shows.
        super::install_offscreen(mtm);
        super::apply(Event::Chat(earlier.clone()));
        super::apply(Event::OpenChat);
        advance(1.0);
        super::apply(Event::Sent);
        advance(0.3);
        super::apply(Event::Activity(Some(crate::island::Activity::Thinking)));
        advance(0.4);
        super::apply(Event::Chat(during.clone()));
        advance(1.5);
        c.check(class() == Class::Reply && dots(), &format!("{turn}: mid-run, the pulled-down conversation shows its rows and the working dots"));
        c.check(composite(&dir.join(format!("{turn}-turn-working.png")), None), &format!("{turn}: rendered mid-run"));
        super::apply(Event::LastReply(REPLY.into()));
        super::apply(Event::Chat(ended.clone()));
        super::apply(Event::Activity(None));
        advance(1.5);
        c.check(class() == Class::Reply && !dots(), &format!("{turn}: ended, the reply shows and the dots are gone"));
        c.check(composite(&dir.join(format!("{turn}-turn-ended.png")), None), &format!("{turn}: rendered ended"));
    }
    std::fs::write(dir.join("relay-activity-checks.txt"), &c.report).ok();
    c.failed
}

/// `heimdall-ui-demo --recovery-countdown <dir>`: the quota recovery card, four choices and its
/// countdown, offscreen at real size: at 0:47 (13 s into its minute), at 0:00 (still open, the controller's to close),
/// and the same card without a countdown, as every other card looks. Returns the failures.
pub(crate) fn render_recovery_countdown(dir: &Path) -> Vec<String> {
    use crate::agent_notice::{AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};
    use crate::island::object::{Class, Content};
    let mtm = MainThreadMarker::new().expect("render_recovery_countdown runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    super::set_sink(Box::new(|_| {}));
    const WALL0: f64 = 1_792_780_000_000.0;
    let options = ["Claude Opus · account 2", "Codex · gpt-6", "Local model · Qwen3.8 27B", "Wait for Claude account 1"];
    let card = |countdown: bool, at: f64| {
        let q = AskQuestion { id: "q1".into(), header: None, text: "Claude account 1 is out of quota. Continue with".into(), multi_select: false, allow_other: false, field: None, options: options.iter().map(|l| AskOption::label(l)).collect() };
        let expires_ms = (WALL0 + (at + 60.0) * 1000.0) as u64;
        AgentNotice { action: Some(NoticeAction::Ask { request_id: "emergency:recover".into(), questions: vec![q], expires_ms, countdown }), ..AgentNotice::line("emergency:recover", "Claude account 1 is out of quota", NoticeKind::Warning) }
    };
    let text = || {
        with_host(|h| {
            h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().find(|s| s.id.as_ref() == "q.countdown").and_then(|s| match &*s.content {
                Content::Text { lines, .. } => lines.first().cloned(),
                _ => None,
            })
        })
        .flatten()
    };
    let class = || with_host(|h| h.island.class()).unwrap_or(Class::Rest);
    for (name, countdown) in [("countdown", true), ("plain", false)] {
        super::install_offscreen(mtm);
        with_host(|h| h.island.set_wall_origin(WALL0));
        let at = with_host(|h| h.clock.unwrap_or(0.0)).unwrap_or(0.0);
        super::apply(Event::Notice { notice: card(countdown, at), body: None });
        advance(13.0);
        c.check(class() == Class::Question, &format!("{name}: the card shows"));
        if countdown {
            c.check(text().as_deref() == Some("0:47"), &format!("{name}: 13 s in, it reads 0:47 ({:?})", text()));
            c.check(composite(&dir.join("recovery-countdown-0-47.png"), None), "rendered at 0:47");
            advance(48.0);
            c.check(class() == Class::Question && text().as_deref() == Some("0:00"), &format!("{name}: out of time, still open at 0:00 ({:?})", text()));
            c.check(composite(&dir.join("recovery-countdown-0-00.png"), None), "rendered at 0:00");
        } else {
            c.check(text().is_none(), "plain: no countdown");
            c.check(composite(&dir.join("recovery-plain.png"), None), "rendered without a countdown");
        }
    }
    std::fs::write(dir.join("recovery-countdown-checks.txt"), &c.report).ok();
    c.failed
}

/// `heimdall-ui-demo --catching-up <dir>` (31 Oct: the archive locked for about four minutes and speech stalled with
/// no sign): Heimdall catching up, offscreen at real size: its arrival (the stack and its words), the count fallen in
/// place, the widest words on one line, the token at rest, a restart over it (the restart wins) and its token back
/// after (checked in the model; its still is the gallery's), and cleared (the island at rest). Returns the failures.
pub(crate) fn render_catching_up(dir: &Path) -> Vec<String> {
    use crate::island::object::{Class, Content, Fx};
    use crate::island::{HealthCue, Part, BUSY_SYMBOL};
    let mtm = MainThreadMarker::new().expect("render_catching_up runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    super::set_sink(Box::new(|_| {}));
    super::install_offscreen(mtm);
    let busy = |label: &str| Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: label.into() });
    let class = || with_host(|h| h.island.class()).unwrap_or(Class::Rest);
    // The slot `id`'s symbol name and effect, or its text lines.
    let symbol = |id: &str| {
        with_host(|h| {
            h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().find(|s| s.id.as_ref() == id).and_then(|s| match &*s.content {
                Content::Symbol { name, .. } => Some((name.clone(), s.fx)),
                _ => None,
            })
        })
        .flatten()
    };
    let words = || {
        with_host(|h| {
            h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().find(|s| s.id.as_ref() == "stack.words").and_then(|s| match &*s.content {
                Content::Text { lines, .. } => Some(lines.clone()),
                _ => None,
            })
        })
        .flatten()
        .unwrap_or_default()
    };
    let busy_icon = |id: &str| symbol(id) == Some((BUSY_SYMBOL.to_string(), Fx::Variable));
    let shot = |c: &mut Checks, name: &str, what: &str| {
        let ok = composite(&dir.join(name), None);
        c.check(ok, &format!("rendered {what} ({name})"));
    };

    super::apply(busy("Catching up · 6,489 notes to save"));
    advance(0.8);
    c.check(class() == Class::Stack && busy_icon("stack.icon"), &format!("arrival: the stack, its icon lit in turn ({:?}, {:?})", class(), symbol("stack.icon")));
    c.check(words() == ["Catching up · 6,489 notes to save"], &format!("arrival: its words on one line ({:?})", words()));
    shot(&mut c, "catching-up-1-arrival.png", "the arrival");

    let since = |id: &str| with_host(|h| h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().find(|s| s.id.as_ref() == id).map(|s| (s.key, s.since))).flatten();
    let icon_before = since("stack.icon");
    advance(1.0);
    super::apply(busy("Catching up · 2,310 notes to save"));
    advance(0.6);
    c.check(words() == ["Catching up · 2,310 notes to save"], &format!("resent: the lower count in place ({:?})", words()));
    c.check(since("stack.icon") == icon_before, "resent: the icon did not arrive again");
    shot(&mut c, "catching-up-2-count-falls.png", "the count fallen");

    // The widest words the app sends: five digits, still one line.
    super::apply(busy("Catching up · 12,345 notes to save"));
    advance(0.6);
    c.check(words() == ["Catching up · 12,345 notes to save"], &format!("five digits on one line ({:?})", words()));
    shot(&mut c, "catching-up-3-five-digits.png", "five digits");

    advance(crate::island::object::BUSY_READ);
    c.check(class() == Class::Ears && busy_icon("ear.l") && symbol("ear.r").is_some_and(|(n, _)| n == "hourglass"), &format!("rests as a token in the ears ({:?}, {:?}, {:?})", class(), symbol("ear.l"), symbol("ear.r")));
    shot(&mut c, "catching-up-4-token.png", "the token at rest");
    let ticks = with_host(|h| h.island.needs_ticks(h.clock.unwrap_or(0.0)));
    c.check(ticks == Some(crate::island::object::FrameNeed::None), &format!("the token asks for no ticks ({ticks:?})"));
    super::apply(busy("Catching up · 940 notes to save"));
    advance(0.6);
    c.check(class() == Class::Ears && busy_icon("ear.l"), "resent at rest: still the token, nothing opens");

    super::apply(Event::Health(HealthCue::Restarting { key: "listen".into(), part: Part::Transcription, label: "Restarting transcription".into() }));
    advance(0.8);
    c.check(class() == Class::Stack && symbol("stack.icon").is_some_and(|(n, _)| n == "arrow.clockwise"), &format!("a restart wins ({:?}, {:?})", class(), symbol("stack.icon")));
    shot(&mut c, "catching-up-5-restart-wins.png", "a restart over it");
    advance(crate::island::object::RESTART_READ);
    c.check(symbol("ear.l").is_some_and(|(n, _)| n == "arrow.clockwise"), "the restart's token holds over it");
    shot(&mut c, "catching-up-6-restart-token.png", "the restart's token over it");
    super::apply(Event::Health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription back".into() }));
    advance(0.8);
    c.check(class() == Class::Ears && busy_icon("ear.l"), &format!("the restart back: catching up's token returns, not opened again ({:?})", class()));
    // No still here: the ear's symbol turns from the restart's arrow into the stack by the system's replace transition,
    // which `renderInContext` never steps (it keeps the arrow); the gallery card `health_catching_up_restart`, drawn
    // by Core Animation's own renderer, shows it.

    super::apply(Event::Health(HealthCue::Recovered { key: "archive".into(), part: Part::Agent, label: "Caught up".into() }));
    advance(1.2);
    let alerting = with_host(|h| h.island.alerting()).unwrap_or(true);
    c.check(class() == Class::Rest && !alerting, &format!("caught up: the island rests, nothing red ({:?})", class()));
    shot(&mut c, "catching-up-7-cleared.png", "cleared");
    std::fs::write(dir.join("catching-up-checks.txt"), &c.report).ok();
    c.failed
}

/// `heimdall-ui-demo --composer <dir>`: only the conversation's composer and ⌃⌘H (`open_chat`), offscreen, in a few
/// seconds: one placeholder, typing through real key events, the field focused, and the renders. Returns the failures.
pub(crate) fn render_composer(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("render_composer runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut c = Checks::new();
    super::set_sink(Box::new(|_| {}));
    super::install_offscreen(mtm);
    // Audit 27 Oct: the first focus of a text field built AppKit's field editor on the opening motion's fourth frame
    // (25-28 ms). The composer is made and its editing readied at install, without the keyboard.
    let ready = with_host(|h| {
        let field = h.fields.composer();
        field.is_some_and(|f| unsafe { f.superview() }.is_none()) && !h.fields.composer_focused() && !h.panels().1.isKeyWindow()
    });
    c.check(ready == Some(true), "the composer's field is made at install, off the view, and nothing has the keyboard");
    super::apply(Event::Speaking(true));
    advance(0.1);
    super::apply(Event::SpokenText(Some("The build is green.".into())));
    advance(0.6);
    super::apply(Event::SpokenText(Some("Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.".into())));
    advance(1.0);
    super::apply(Event::Speaking(false));
    advance(2.0);
    open_chat(&mut c, dir, mtm);
    std::fs::write(dir.join("composer-checks.txt"), &c.report).ok();
    c.failed
}

/// The island as it stands, over a plain desktop grey, to `path` (agent checks' pictures).
pub(crate) fn snapshot(path: &Path) -> bool {
    with_host(|h| {
        let (w, full) = h.tree.size;
        let Some(c) = super::raster::Canvas::new(w, full, h.tree.scale) else { return false };
        c.draw(|| {
            objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.36, 0.40, 0.50, 1.0).setFill();
            objc2_app_kit::NSBezierPath::fillRect(super::super::util::rect(0.0, 0.0, w, full));
        });
        h.tree.render_into(&c, (0.0, 0.0));
        c.png().is_some_and(|p| std::fs::write(path, p).is_ok())
    })
    .unwrap_or(false)
}
