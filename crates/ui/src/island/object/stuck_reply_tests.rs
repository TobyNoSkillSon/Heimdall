//! Bug note 2026-10-31T23-37-26 ("the notch reply text doesn't disappear"): at 23:34 a spoken reply about two job ads
//! stayed under the notch over the user's page, its waveform flat (five dots), until he quit Heimdall at 23:40. The audio
//! stream had stopped calling back while still reporting silence about 33 times a second, and each report counted as
//! the voice being alive. Only sound is a sign of life now, and a click puts the words away.

use super::*;

fn island() -> Island {
    Island::new(Display::MBP16, Box::new(Approx))
}

/// Time passes as the window host runs it: a tick at every deadline up to `to`, then at `to`.
fn advance(i: &mut Island, to: f64) {
    for _ in 0..10_000 {
        match i.deadline() {
            Some(d) if d <= to => i.apply(Event::Tick, d),
            _ => break,
        }
    }
    i.apply(Event::Tick, to);
}

fn speaking(i: &Island) -> bool {
    matches!(i.scene(), Scene::Speaking { .. })
}

const REPLY: [&str; 4] = [
    "Two ads close tomorrow: interLAN in Poznań and 7Technology in Warsaw.",
    "Tick the ones you want on the page and press Done.",
    "One question: would you register as self-employed?",
    "",
];

fn spoken_reply(i: &mut Island) -> f64 {
    i.apply(Event::Speaking(true), 0.0);
    let mut t = 0.0;
    for s in REPLY.iter().filter(|s| !s.is_empty()) {
        i.apply(Event::SpokenText(Some((*s).into())), t);
        for _ in 0..100 {
            t += 0.03;
            i.apply(Event::SpeechLevel(0.3), t);
        }
    }
    t
}

#[test]
fn a_stream_that_reports_only_silence_does_not_hold_the_words() {
    let mut i = island();
    let mut t = spoken_reply(&mut i);
    advance(&mut i, t);
    assert!(speaking(&i), "the reply shows while it plays: {:?}", i.scene());
    // The device stops: no end, no words, only silent level reports, for three minutes.
    let end = t + 180.0;
    while t < end {
        t += 0.03;
        i.apply(Event::SpeechLevel(0.0), t);
    }
    advance(&mut i, t);
    assert!(!speaking(&i), "3 min of reported silence still shows the words: {:?}", i.scene());
    // A next reply shows normally.
    i.apply(Event::Speaking(true), t + 1.0);
    i.apply(Event::SpokenText(Some("Done.".into())), t + 1.0);
    advance(&mut i, t + 1.5);
    assert!(speaking(&i), "{:?}", i.scene());
}

/// Review 1 Nov: a slow sentence mid-reply (silence reported for 6 s, the stream still alive) keeps the words, and the
/// words that follow show in the same reply.
#[test]
fn a_slow_sentence_mid_reply_keeps_its_words() {
    let mut i = island();
    let mut t = spoken_reply(&mut i);
    for _ in 0..200 {
        t += 0.03;
        i.apply(Event::SpeechLevel(0.0), t);
    }
    advance(&mut i, t);
    assert!(speaking(&i), "6 s of a slow sentence retracted the reply: {:?}", i.scene());
    i.apply(Event::SpokenText(Some("And the last one.".into())), t);
    i.apply(Event::SpeechLevel(0.4), t + 0.03);
    advance(&mut i, t + 0.5);
    assert!(matches!(i.scene(), Scene::Speaking { sentences, .. } if sentences.iter().any(|s| s == "And the last one.")), "{:?}", i.scene());
}

#[test]
fn quiet_moments_inside_a_reply_keep_its_words() {
    let mut i = island();
    let mut t = spoken_reply(&mut i);
    // A pause between sentences (1 s of silence), then sound again: one reply, never retracted.
    for _ in 0..33 {
        t += 0.03;
        i.apply(Event::SpeechLevel(0.0), t);
    }
    advance(&mut i, t);
    assert!(speaking(&i));
    i.apply(Event::SpokenText(Some("And one more thing.".into())), t);
    i.apply(Event::SpeechLevel(0.4), t + 0.03);
    advance(&mut i, t + 0.5);
    assert!(speaking(&i));
}

#[test]
fn a_click_puts_the_spoken_words_away_until_the_next_reply() {
    let mut i = island();
    let t = spoken_reply(&mut i);
    advance(&mut i, t + 0.6);
    assert!(speaking(&i));
    let (target, r) = i.targets().iter().find(|(x, _)| *x == Target::Hush).cloned().unwrap_or_else(|| panic!("no Hush among {:?}", i.targets()));
    assert_eq!(i.hit(t + 0.6, r.center()), Some(target.clone()));
    i.apply(Event::Click(target), t + 0.6);
    // The audio plays on; its words do not come back, its later sentences included.
    i.apply(Event::SpeechLevel(0.3), t + 0.7);
    i.apply(Event::SpokenText(Some("A later sentence of the same reply.".into())), t + 0.8);
    advance(&mut i, t + 1.5);
    assert!(!speaking(&i), "{:?}", i.scene());
    i.apply(Event::Speaking(false), t + 2.0);
    advance(&mut i, t + 10.0);
    assert!(!speaking(&i));
    // The next reply shows again.
    i.apply(Event::Speaking(true), t + 20.0);
    i.apply(Event::SpokenText(Some("Next reply.".into())), t + 20.0);
    advance(&mut i, t + 20.5);
    assert!(speaking(&i), "{:?}", i.scene());
}
