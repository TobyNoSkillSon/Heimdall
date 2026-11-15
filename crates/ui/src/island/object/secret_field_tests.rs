//! Night 1111 [input]: the composer's lock and its secret field (bug note 2026-11-10T16-30-03: "there is no safe way
//! to hand Heimdall a secret"). Locked, the composer is a secure field whose Return goes to the app as
//! `Output::Secret` alone, never as a typed message; it unlocks after one secret and whenever the pull-down closes.

use super::{Approx, Event, Island, Output, Target};
use crate::island::object::{Class, Content, Display, SECRET_EMPTY_PLACEHOLDER, SECRET_PLACEHOLDER};
use crate::model::SecretText;

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

fn open(i: &mut Island) {
    open_at(i, 0.0);
}

fn open_at(i: &mut Island, t: f64) {
    i.apply(Event::OpenChat, t);
    i.apply(Event::Tick, t + 1.0);
    assert_eq!(i.class(), Class::Reply);
}

fn field_owner(i: &Island, t: f64) -> Option<Target> {
    i.frame(t).slots.iter().find_map(|s| match &*s.content {
        Content::Field { owner: o @ (Target::Composer | Target::ComposerSecret), .. } if !s.id.contains('~') => Some(o.clone()),
        _ => None,
    })
}

fn lock(i: &Island, t: f64) -> Option<String> {
    i.frame(t).slots.iter().find(|s| s.id.starts_with("reply.composer.lock")).and_then(|s| match &*s.content {
        Content::Symbol { name, .. } => Some(name.clone()),
        _ => None,
    })
}

fn texts(i: &Island, t: f64) -> Vec<String> {
    i.frame(t).slots.iter().filter_map(|s| match &*s.content {
        Content::Text { lines, .. } => Some(lines.join(" ")),
        _ => None,
    }).collect()
}

#[test]
fn the_lock_turns_the_composer_into_a_secret_field_and_back() {
    let mut i = island();
    open(&mut i);
    assert_eq!(field_owner(&i, 1.0), Some(Target::Composer));
    assert_eq!(lock(&i, 1.0).as_deref(), Some("lock"));
    i.apply(Event::Click(Target::ComposerLock), 1.1);
    i.apply(Event::Tick, 1.5);
    assert_eq!(field_owner(&i, 1.5), Some(Target::ComposerSecret));
    assert_eq!(lock(&i, 1.5).as_deref(), Some("lock.fill"));
    assert!(texts(&i, 1.5).iter().any(|t| t == SECRET_PLACEHOLDER), "its placeholder: {:?}", texts(&i, 1.5));
    i.apply(Event::Click(Target::ComposerLock), 1.6);
    i.apply(Event::Tick, 2.5);
    assert_eq!(field_owner(&i, 2.5), Some(Target::Composer));
    assert!(i.take_outputs().is_empty(), "the lock sends nothing");
}

#[test]
fn the_lock_is_hit_before_the_composer_and_beside_its_field() {
    let mut i = island();
    open(&mut i);
    let f = i.frame(1.0);
    let lock = f.slots.iter().find(|s| s.id.starts_with("reply.composer.lock")).expect("the lock").rect;
    let field = f.slots.iter().find(|s| matches!(&*s.content, Content::Field { .. })).expect("the field").rect;
    assert!(field.x + field.w <= lock.x, "the field ends before the lock: {field:?} {lock:?}");
    let centre = super::Point { x: lock.x + lock.w / 2.0, y: lock.y + lock.h / 2.0 };
    assert_eq!(i.hit(1.0, centre), Some(Target::ComposerLock));
}

#[test]
fn a_secret_goes_to_the_app_alone_and_unlocks_the_composer() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::Click(Target::ComposerLock), 1.1);
    i.apply(Event::ComposerEmpty(false), 1.2);
    i.apply(Event::SecretSent(SecretText::new("sk-lab-123")), 1.3);
    let out = i.take_outputs();
    assert_eq!(out, vec![Output::Secret(SecretText::new("sk-lab-123"))]);
    assert!(!format!("{out:?}").contains("sk-lab"), "never printed: {out:?}");
    i.apply(Event::Tick, 1.5);
    i.apply(Event::Tick, 2.0);
    assert_eq!(field_owner(&i, 2.0), Some(Target::Composer), "unlocked after one secret");
    // An empty Return sends nothing.
    i.apply(Event::Click(Target::ComposerLock), 2.1);
    i.apply(Event::SecretSent(SecretText::new("   ")), 2.2);
    assert!(i.take_outputs().is_empty());
}

#[test]
fn a_secret_never_arrives_from_an_unlocked_composer() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::SecretSent(SecretText::new("stray")), 1.1);
    assert!(i.take_outputs().is_empty());
}

#[test]
fn closing_the_pull_down_unlocks_the_composer() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::Click(Target::ComposerLock), 1.1);
    i.apply(Event::ComposerEmpty(false), 1.2);
    i.apply(Event::Click(Target::ReplyBand), 1.3);
    i.apply(Event::Tick, 2.5);
    open_at(&mut i, 3.0);
    i.apply(Event::Tick, 4.0);
    assert_eq!(field_owner(&i, 4.0), Some(Target::Composer));
    assert!(texts(&i, 4.0).iter().any(|t| *t == super::composer_placeholder()), "empty again, its placeholder shows");
}

/// Review nit "Lock toggle and typed text" (night 1111): the plain composer keeps what he typed while the secret field
/// shows, so the placeholder must not cover it on the way back; and the secret field shows its own placeholder. Events
/// are a second apart and read 0.6 s later: a change within `MOTION_GAP` of the last motion waits for its slot.
#[test]
fn the_placeholder_follows_the_field_shown_through_the_lock() {
    let mut i = island();
    open(&mut i);
    let placeholder = super::composer_placeholder();
    let step = |i: &mut Island, ev: Event, t: f64| -> Vec<String> {
        i.apply(ev, t);
        i.apply(Event::Tick, t + 0.6);
        texts(i, t + 0.6)
    };
    assert!(!step(&mut i, Event::ComposerEmpty(false), 2.0).contains(&placeholder), "typed text, no placeholder");
    assert!(step(&mut i, Event::Click(Target::ComposerLock), 3.0).iter().any(|t| t == SECRET_PLACEHOLDER), "the secret field starts empty");
    let back = step(&mut i, Event::Click(Target::ComposerLock), 4.0);
    assert_eq!(field_owner(&i, 4.6), Some(Target::Composer));
    assert!(!back.contains(&placeholder), "the typed text waited off screen: no placeholder over it: {back:?}");
    // A secret sent from the lock brings back the composer with its text too.
    step(&mut i, Event::Click(Target::ComposerLock), 5.0);
    step(&mut i, Event::ComposerEmpty(false), 6.0);
    let after = step(&mut i, Event::SecretSent(SecretText::new("sk-lab-1")), 7.0);
    assert_eq!(field_owner(&i, 7.6), Some(Target::Composer));
    assert!(!after.contains(&placeholder), "{after:?}");
    // Once sent, the composer is empty again, and the lock's round trip keeps it empty.
    step(&mut i, Event::ComposerSent("hello".into()), 8.0);
    step(&mut i, Event::Click(Target::ComposerLock), 9.0);
    assert!(step(&mut i, Event::Click(Target::ComposerLock), 10.0).contains(&placeholder));
}

/// Review nit (night 1111): a Return in an empty secret field used to do nothing visible; it stays locked and says so,
/// until he types again.
#[test]
fn an_empty_secret_says_nothing_was_sent() {
    let mut i = island();
    open(&mut i);
    i.apply(Event::Click(Target::ComposerLock), 2.0);
    i.apply(Event::Tick, 2.6);
    i.apply(Event::SecretSent(SecretText::new("")), 3.0);
    i.apply(Event::Tick, 3.6);
    assert!(i.take_outputs().is_empty());
    assert_eq!(field_owner(&i, 3.6), Some(Target::ComposerSecret), "still locked");
    assert!(texts(&i, 3.6).iter().any(|t| t == SECRET_EMPTY_PLACEHOLDER), "{:?}", texts(&i, 3.6));
    i.apply(Event::ComposerEmpty(false), 4.0);
    i.apply(Event::ComposerEmpty(true), 5.0);
    i.apply(Event::Tick, 5.6);
    assert!(texts(&i, 5.6).iter().any(|t| t == SECRET_PLACEHOLDER), "typing clears the note: {:?}", texts(&i, 5.6));
}
