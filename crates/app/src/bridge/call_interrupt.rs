//! Who on a call may stop Heimdall's speech while the user listens to a guest (2026: with a guest listened to, any
//! other voice on the call stopped Heimdall; the user chose "only you and the listened-to guest can interrupt me").
//!
//! - **Listening mode off** (no guest has `listen_to` on): unchanged. Any voice on a call source still open
//!   `GUEST_CUT_AFTER` after it began stops Heimdall (`Tools::guest_talked_over`, bug note 2026-11-05T02-32-17).
//! - **Listening mode on**: a call voice stops Heimdall only when the speaker tracker (Nemotron's live tracks, already
//!   running for the call, `speakers_runtime.rs`) puts a listened guest's track in it, or a track with no confident
//!   label yet ("Speaker 2", "Unknown": a new voice needs about 12 s of clean speech before it is named; the pipeline
//!   also passes a coded voice first heard in this call, or one near a listened guest, as unlabelled). Unlabelled
//!   voices interrupt on purpose: Heimdall must never talk over the listened guest because the tracker has not named
//!   them yet. Every other track (another named guest, a coded voice "Speaker Fri 23 Oct 21h #3" that matched no name,
//!   The user's or Heimdall's voice coming back through a guest's open speakers) and sound with no track at all (noise)
//!   leave Heimdall talking; they still keep a reply from starting until the gap (`hold::OwnerVoice::other_voice`).
//! - The user's own microphone is not on this path: his verified voice stops Heimdall as before (pipeline/barge_in.rs).
//!
//! The pipeline judges each open call voice whenever the tracker's live segments change (`judge`, stored with
//! `SpeechPolicy::voice_attributed`); the call-voice hook (bridge/speech.rs) waits `GUEST_CUT_AFTER`, then polls that
//! verdict every `POLL` until it may cut, the voice ends or Heimdall stops. A verdict still pending `PENDING_MAX` after
//! the voice began (the tracker stalled, is reloading or failed) cuts, the safe side.
use speakers::diar::SpeakerSegment;
use std::collections::BTreeSet;
use std::time::Duration;

/// A track must be active this long inside the open voice to count as talking in it: past a frame or two of a
/// previous speaker's tail, or a diarizer blip.
pub(crate) const TRACK_MIN_S: f64 = 0.16;
/// Once the tracker has heard this much audio after the voice began with no track that may interrupt, the voice is
/// judged someone else's (or noise). Later tracks still change the verdict while the voice stays open.
pub(crate) const DECIDE_AFTER_S: f64 = 0.6;
/// A call voice whose verdict is still pending this long after it began cuts Heimdall anyway (the tracker gave
/// nothing in time: reloading after a quiet spell, failed, or behind).
pub(crate) const PENDING_MAX: Duration = Duration::from_millis(2_500);
/// How often the cut waits for a verdict.
pub(crate) const POLL: Duration = Duration::from_millis(40);

/// What the tracker says about the voice open on a call source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CallVoiceWho {
    /// Not enough of the voice diarized yet.
    Pending,
    /// A listened guest (their name), or a voice with no confident label (`UNLABELLED`), or no tracker for the
    /// source (`NO_TRACKER`): it may stop Heimdall.
    MayInterrupt(String),
    /// Someone else (their labels, joined with ", "), or `NOISE` when no track is in the voice.
    Bystander(String),
}
pub(crate) const UNLABELLED: &str = "unlabelled voice";
pub(crate) const NO_TRACKER: &str = "no speaker tracker";
pub(crate) const NOISE: &str = "no speaker track (noise)";

/// A track label that names nobody yet: absent, "Unknown", "Speaker ?" or the anonymous "Speaker N"
/// (`NamingFlow::label`). A coded voice ("Speaker Fri 23 Oct 21h #3") is labelled: it matched no named person.
pub(crate) fn unlabelled(label: Option<&str>) -> bool {
    let Some(label) = label.map(str::trim) else { return true };
    if label.is_empty() || label.eq_ignore_ascii_case("Unknown") || label == "Speaker ?" {
        return true;
    }
    label.strip_prefix("Speaker ").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The verdict on a call voice that began `onset_s` into the tracker session, given its live `segments` (same clock),
/// the audio heard up to `heard_until_s`, each track's current `label` and the listened guests.
pub(crate) fn judge(
    onset_s: f64,
    heard_until_s: f64,
    segments: &[SpeakerSegment],
    label: impl Fn(i32) -> Option<String>,
    listened: &BTreeSet<String>,
) -> CallVoiceWho {
    let mut in_voice: Vec<i32> = vec![];
    for s in segments {
        if s.end_time - s.start_time.max(onset_s) >= TRACK_MIN_S && !in_voice.contains(&s.speaker) {
            in_voice.push(s.speaker);
        }
    }
    in_voice.sort_unstable();
    let mut unlabelled_seen = false;
    let mut others = vec![];
    for track in in_voice {
        let name = label(track);
        match name.as_deref() {
            Some(n) if listened.iter().any(|l| l.eq_ignore_ascii_case(n.trim())) => return CallVoiceWho::MayInterrupt(n.trim().to_owned()),
            n if unlabelled(n) => unlabelled_seen = true,
            Some(n) => others.push(n.trim().to_owned()),
            None => unreachable!("an absent label is unlabelled"),
        }
    }
    if unlabelled_seen {
        return CallVoiceWho::MayInterrupt(UNLABELLED.into());
    }
    if heard_until_s - onset_s < DECIDE_AFTER_S {
        return CallVoiceWho::Pending;
    }
    others.dedup();
    CallVoiceWho::Bystander(if others.is_empty() { NOISE.into() } else { others.join(", ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(speaker: i32, start_time: f64, end_time: f64) -> SpeakerSegment {
        SpeakerSegment { start_time, end_time, speaker }
    }
    fn names(track: i32) -> Option<String> {
        match track {
            0 => Some("Alex".into()),
            1 => Some("Robin".into()),
            2 => Some("Speaker 2".into()),
            3 => Some("Speaker Fri 13 Nov 11h #4".into()),
            4 => Some("Heimdall".into()),
            _ => None,
        }
    }
    fn alex() -> BTreeSet<String> {
        ["Alex".to_string()].into()
    }

    #[test]
    fn labels_that_name_nobody_yet() {
        for l in [None, Some(""), Some("Unknown"), Some("Speaker ?"), Some("Speaker 3"), Some("Speaker 12")] {
            assert!(unlabelled(l), "{l:?}");
        }
        for l in ["Alex", "Speaker Fri 23 Oct 21h #3", "Speaker x", "Heimdall", "User"] {
            assert!(!unlabelled(Some(l)), "{l}");
        }
    }

    #[test]
    fn the_listened_guest_may_interrupt() {
        let v = judge(10., 10.5, &[seg(0, 10.05, 10.4)], names, &alex());
        assert_eq!(v, CallVoiceWho::MayInterrupt("Alex".into()));
    }

    #[test]
    fn another_named_guest_does_not() {
        assert_eq!(judge(10., 10.4, &[seg(1, 10., 10.4)], names, &alex()), CallVoiceWho::Pending, "not decided before DECIDE_AFTER");
        assert_eq!(judge(10., 10.7, &[seg(1, 10., 10.7)], names, &alex()), CallVoiceWho::Bystander("Robin".into()));
        // A coded voice matched no name; Heimdall's own voice back through a guest's speakers is never a guest.
        assert_eq!(judge(10., 10.7, &[seg(3, 10., 10.7), seg(4, 10.1, 10.6)], names, &alex()),
            CallVoiceWho::Bystander("Speaker Fri 13 Nov 11h #4, Heimdall".into()));
    }

    #[test]
    fn noise_with_no_track_does_not() {
        assert_eq!(judge(10., 10.3, &[], names, &alex()), CallVoiceWho::Pending);
        assert_eq!(judge(10., 10.7, &[], names, &alex()), CallVoiceWho::Bystander(NOISE.into()));
    }

    #[test]
    fn an_unlabelled_voice_may_interrupt() {
        assert_eq!(judge(10., 10.4, &[seg(2, 10., 10.4)], names, &alex()), CallVoiceWho::MayInterrupt(UNLABELLED.into()));
        assert_eq!(judge(10., 10.4, &[seg(7, 10., 10.4)], names, &alex()), CallVoiceWho::MayInterrupt(UNLABELLED.into()), "a track with no label at all");
    }

    #[test]
    fn the_listened_guest_over_someone_else_still_interrupts() {
        let v = judge(10., 12., &[seg(1, 10., 12.), seg(0, 11.2, 12.)], names, &alex());
        assert_eq!(v, CallVoiceWho::MayInterrupt("Alex".into()));
    }

    /// The decision delay on recorded, labelled speech (brief 13 Nov: measure it). `ami-live-timeline.json` holds
    /// the live diarizer's output (the shipped Nemotron 3 Q8, fed 20 ms packets as the app feeds a call source) on
    /// the first minutes of AMI ES2004a, recorded once by `crates/speakers/examples/live_timeline.rs`
    /// (`lab/speaker-aware-barge-in/`), with the forced-alignment reference. No model runs here.
    ///
    /// Each clean turn (a speaker starting after half a second of their own silence, nobody else talking from half a
    /// second before to 1.5 s after, lasting a second or more) is a call voice opening at its reference start. With
    /// every track named as the reference speaker it overlaps most (naming finished): as the listened guest, when
    /// does `judge` first let the voice cut, in supplied-audio seconds after the start; and when someone else is
    /// listened to, does it wrongly let the voice cut within its first 1.5 s. The cut itself comes at the later of
    /// `GUEST_CUT_AFTER` and that verdict, plus up to one `POLL` and the helper's compute per frame (wall time, not
    /// in this replay).
    #[test]
    fn decision_delay_on_recorded_labelled_speech() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("call_interrupt_fixtures/ami-live-timeline.json")).unwrap();
        let seg = |v: &serde_json::Value| SpeakerSegment { speaker: v[0].as_i64().unwrap() as i32, start_time: v[1].as_f64().unwrap(), end_time: v[2].as_f64().unwrap() };
        let frames: Vec<(f64, f64, Vec<SpeakerSegment>)> = fixture["frames"].as_array().unwrap().iter()
            .map(|f| (f["heard_s"].as_f64().unwrap(), f["stable_s"].as_f64().unwrap(), f["segments"].as_array().unwrap().iter().map(seg).collect())).collect();
        let finals: Vec<SpeakerSegment> = fixture["final_segments"].as_array().unwrap().iter().map(seg).collect();
        let reference: Vec<(String, f64, f64)> = fixture["reference"].as_array().unwrap().iter()
            .map(|r| (r[0].as_str().unwrap().to_owned(), r[1].as_f64().unwrap(), r[2].as_f64().unwrap())).collect();
        let people: BTreeSet<String> = reference.iter().map(|r| r.0.clone()).collect();
        // Each track named as the reference speaker it overlaps most.
        let mut names = std::collections::HashMap::<i32, String>::new();
        for track in finals.iter().map(|s| s.speaker).collect::<BTreeSet<_>>() {
            let best = people.iter().map(|p| {
                let o: f64 = finals.iter().filter(|s| s.speaker == track).flat_map(|s| reference.iter().filter(move |r| &r.0 == p)
                    .map(move |r| (s.end_time.min(r.2) - s.start_time.max(r.1)).max(0.))).sum();
                (o, p)
            }).max_by(|a, b| a.0.total_cmp(&b.0));
            if let Some((o, p)) = best.filter(|b| b.0 > 0.) { let _ = o; names.insert(track, p.clone()); }
        }
        // Turns: a person's reference segments with gaps under 0.3 s merged.
        let mut turns: Vec<(String, f64, f64)> = vec![];
        for p in &people {
            let mut mine: Vec<_> = reference.iter().filter(|r| &r.0 == p).collect();
            mine.sort_by(|a, b| a.1.total_cmp(&b.1));
            let mut current: Option<(f64, f64)> = None;
            for r in mine {
                current = match current {
                    Some((a, b)) if r.1 - b < 0.3 => Some((a, b.max(r.2))),
                    Some((a, b)) => { turns.push((p.clone(), a, b)); Some((r.1, r.2)) }
                    None => Some((r.1, r.2)),
                };
            }
            if let Some((a, b)) = current { turns.push((p.clone(), a, b)); }
        }
        let talking = |who: &str, a: f64, b: f64, same: bool| reference.iter().any(|r| (r.0 == who) == same && r.2 > a && r.1 < b);
        let last_heard = frames.last().unwrap().0;
        let clean: Vec<_> = turns.iter().filter(|(p, a, b)| b - a >= 1.0 && *a >= 0.5 && a + 2.0 < last_heard
            && !talking(p, a - 0.5, *a, true) && !talking(p, a - 0.5, a + 1.5, false)).collect();
        assert!(clean.len() >= 10, "{} clean turns", clean.len());
        let mut delays = vec![];
        let (mut missed, mut false_cuts, mut bystander_cases) = (0, 0, 0);
        for (p, a, b) in &clean {
            let label = |t: i32| names.get(&t).cloned();
            let listened: BTreeSet<String> = [p.clone()].into();
            match frames.iter().filter(|f| f.0 > *a && f.0 <= b + 0.5).find(|f| matches!(judge(*a, f.1, &f.2, label, &listened), CallVoiceWho::MayInterrupt(_))) {
                Some(f) => delays.push(f.0 - a),
                None => missed += 1,
            }
            for other in people.iter().filter(|o| *o != p) {
                bystander_cases += 1;
                let listened: BTreeSet<String> = [other.clone()].into();
                if frames.iter().filter(|f| f.0 > *a && f.0 <= a + 1.5).any(|f| matches!(judge(*a, f.1, &f.2, label, &listened), CallVoiceWho::MayInterrupt(_))) {
                    false_cuts += 1;
                }
            }
        }
        delays.sort_by(f64::total_cmp);
        let at = |q: f64| delays[((delays.len() - 1) as f64 * q).round() as usize];
        let cut = |d: f64| d.max(super::super::speech_policy::GUEST_CUT_AFTER.as_secs_f64());
        let added: Vec<f64> = delays.iter().map(|d| cut(*d) - super::super::speech_policy::GUEST_CUT_AFTER.as_secs_f64()).collect();
        let mean_added = added.iter().sum::<f64>() / added.len() as f64;
        eprintln!("decision delay on {} ({} clean turns, {} s): verdict after onset median {:.3} s, p90 {:.3} s, max {:.3} s; \
            cut after onset median {:.3} s (today 0.400 s), added mean {:.3} s; listened turns missed {}/{}; other guests wrongly cutting {}/{}",
            fixture["audio"], clean.len(), fixture["seconds"], at(0.5), at(0.9), delays.last().unwrap(), cut(at(0.5)), mean_added,
            missed, clean.len(), false_cuts, bystander_cases);
        // Guards on this recording, so a change to `judge` that slows or breaks it shows.
        assert!(at(0.5) <= 1.0, "median verdict {:.3} s", at(0.5));
        assert!(missed * 4 <= clean.len(), "missed {missed} of {}", clean.len());
    }

    #[test]
    fn a_previous_speakers_tail_or_a_blip_does_not_count() {
        // Alex's segment ends 0.1 s into Robin's voice; a 0.1 s blip of Alex later.
        let v = judge(10., 11., &[seg(0, 9., 10.1), seg(1, 10., 11.), seg(0, 10.5, 10.6)], names, &alex());
        assert_eq!(v, CallVoiceWho::Bystander("Robin".into()));
    }
}
