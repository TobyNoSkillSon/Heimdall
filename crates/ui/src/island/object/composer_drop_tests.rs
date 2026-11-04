//! A drop on the open pull-down (30 Oct 03:00, the user: "dropping should attach to the message being typed (or send it,
//! like a notch drop)"): `Island::drop_route`, `Event::ComposerAttach`, the composer's thumbnails and their remove
//! badges, and the send that carries them.

use std::path::PathBuf;

use super::{Approx, Event, Island, Output, Target};
use crate::island::drop::{DropItem, DropKind, Route};
use crate::island::object::{Class, Content, Display};

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn shot(name: &str) -> DropItem {
    DropItem { kind: DropKind::Image, path: Some(PathBuf::from(format!("/drops/{name}.png"))), url: None, text: None }
}

fn link() -> DropItem {
    DropItem { kind: DropKind::Link, path: None, url: Some("https://example.com/spec".into()), text: None }
}

fn thumbs(i: &Island, t: f64) -> Vec<super::Rect> {
    i.frame(t).slots.iter().filter(|s| !s.id.contains('~') && matches!(&*s.content, Content::Thumb { .. })).map(|s| s.rect).collect()
}

fn field(i: &Island, t: f64) -> super::Rect {
    i.frame(t).slots.iter().find(|s| matches!(&*s.content, Content::Field { owner: Target::Composer, .. })).map(|s| s.rect).expect("the composer")
}

fn open(i: &mut Island) {
    i.apply(Event::OpenChat, 0.0);
    i.apply(Event::Tick, 1.0);
    assert_eq!(i.class(), Class::Reply);
}

#[test]
fn a_drop_on_the_pull_down_goes_into_the_composer_only_while_he_writes() {
    let mut i = island();
    assert_eq!(i.drop_route(true), Route::Agent, "no pull-down: a notch drop");
    open(&mut i);
    assert_eq!(i.drop_route(false), Route::Agent, "an empty composer without the keyboard: sent like a notch drop");
    assert_eq!(i.drop_route(true), Route::Composer, "the keyboard in it");
    i.apply(Event::ComposerEmpty(false), 1.1);
    assert_eq!(i.drop_route(false), Route::Composer, "text typed");
    i.apply(Event::Click(Target::ReplyBand), 1.2);
    i.apply(Event::Tick, 2.5);
    assert_eq!(i.drop_route(false), Route::Agent, "pulled up: the notch's again");
}

#[test]
fn an_attached_drop_shows_in_the_composer_and_rides_with_the_typed_message() {
    let mut i = island();
    open(&mut i);
    let before = (i.frame(1.0).shape.h, field(&i, 1.0));
    i.apply(Event::ComposerEmpty(false), 1.1);
    i.apply(Event::ComposerAttach(vec![shot("a"), link()]), 1.2);
    // The same drop again rides once.
    i.apply(Event::ComposerAttach(vec![shot("a")]), 1.3);
    for t in [1.5, 2.0, 2.5] {
        i.apply(Event::Tick, t);
    }
    let t = thumbs(&i, 2.5);
    assert_eq!(t.len(), 2, "one thumbnail per item");
    let f = field(&i, 2.5);
    assert!(t.iter().all(|r| r.y + r.h <= f.y && r.x >= f.x - 16.0), "the thumbnails sit in the composer above its line: {t:?} over {f:?}");
    assert!(i.frame(2.5).shape.h > before.0 + 40.0 && f.y > before.1.y + 40.0, "the composer grows for them");
    assert!(i.frame(2.5).shape.h >= f.y + f.h + 8.0, "the field stays inside the sheet");
    assert!(i.composer_attached().len() == 2 && i.take_outputs().is_empty(), "nothing goes before he sends");
    // Return sends his words with the drop, once.
    i.apply(Event::ComposerSent("  what is this?  ".into()), 3.0);
    assert_eq!(i.take_outputs(), vec![Output::TypedDrop { text: "what is this?".into(), items: vec![shot("a"), link()] }]);
    i.apply(Event::Tick, 4.0);
    assert!(thumbs(&i, 4.0).is_empty() && i.composer_attached().is_empty(), "sent: the composer is empty again");
    assert_eq!(i.frame(4.0).shape.h, before.0, "and as tall as before");
    i.apply(Event::ComposerSent("and again".into()), 4.1);
    assert_eq!(i.take_outputs(), vec![Output::Typed("and again".into())], "a later message is plain typing");
}

#[test]
fn an_attachment_s_badge_removes_it_and_return_sends_a_drop_with_no_words() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::ComposerAttach(vec![shot("a"), shot("b")]), 1.1);
    i.apply(Event::Tick, 2.5);
    let badge = i.targets().iter().find(|(t, _)| *t == Target::ComposerDetach(0)).map(|(_, r)| *r).expect("each thumbnail has its remove badge");
    // The badge is a click on the island (not the field under it).
    let at = super::Point::new(badge.x + badge.w / 2.0, badge.y + badge.h / 2.0);
    assert_eq!(i.hit(2.5, at), Some(Target::ComposerDetach(0)));
    i.apply(Event::Click(Target::ComposerDetach(0)), 2.6);
    i.apply(Event::Tick, 3.5);
    assert_eq!(i.composer_attached(), &[shot("b")]);
    assert_eq!(thumbs(&i, 3.5).len(), 1);
    // Return in an empty composer still sends what was dropped.
    i.apply(Event::ComposerSent(String::new()), 3.6);
    assert_eq!(i.take_outputs(), vec![Output::TypedDrop { text: String::new(), items: vec![shot("b")] }]);
    // And with nothing in it, nothing.
    i.apply(Event::ComposerSent(String::new()), 3.7);
    assert!(i.take_outputs().is_empty());
}

#[test]
fn more_attachments_than_the_row_holds_end_in_a_count() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::ComposerAttach((0..20).map(|k| shot(&k.to_string())).collect()), 1.1);
    i.apply(Event::Tick, 2.5);
    let f = i.frame(2.5);
    let shown = thumbs(&i, 2.5).len();
    let more = f.slots.iter().find_map(|s| match &*s.content {
        Content::Text { lines, .. } if lines.first().is_some_and(|l| l.starts_with('+')) => Some(lines[0].clone()),
        _ => None,
    });
    assert!(shown >= 3 && shown < 20, "{shown}");
    assert_eq!(more, Some(format!("+{}", 20 - shown)));
    let right = thumbs(&i, 2.5).iter().map(|r| r.x + r.w).fold(f64::MIN, f64::max);
    assert!(right <= f.shape.w / 2.0 - 16.0, "the row stays inside the composer: {right} in {}", f.shape.w);
}

/// Review of 30 Oct: a composer something else covers (drawing mode, the reader, a question) takes no drop; it goes
/// to the agent as a notch drop, never into a field he cannot see.
#[test]
fn a_composer_hidden_under_drawing_mode_takes_no_drop() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::ComposerEmpty(false), 1.1);
    assert_eq!(i.drop_route(true), Route::Composer);
    i.apply(Event::DrawOpen, 1.2);
    for t in [1.5, 2.0, 2.5] {
        i.apply(Event::Tick, t);
    }
    assert_ne!(i.class(), Class::Reply, "drawing mode shows instead");
    assert_eq!(i.drop_route(true), Route::Agent);
}
