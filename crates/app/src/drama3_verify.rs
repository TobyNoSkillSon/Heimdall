//! Separate CPU-only background decoder, resident only with Drama 3.
use anyhow::Result;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
// Warm native prediction caches on the private owner, outside any take deadline.
pub fn decoder(models: &std::path::Path) -> Result<listen::asr::Parakeet> {
    use listen::Recognizer;
    let mut decoder = listen::asr::Parakeet::load_backend(models, "cpu")?;
    decoder.transcribe(&vec![0.; 8_000], 0)?;
    Ok(decoder)
}
pub fn install(
    service: &Arc<speak::drama3::Service>,
    models: std::path::PathBuf,
) -> Result<Arc<listen::VerificationWorker>> {
    let worker = listen::VerificationWorker::spawn(move || decoder(&models))?;
    let client = worker.client();
    service.set_verifier(Some(Arc::new(move |text, audio, cancel| {
        check(&client, text, audio, cancel)
    })));
    let residency = worker.clone();
    let host = Arc::downgrade(service);
    service.set_residency_hook(Arc::new(move |loaded| {
        residency.configure(loaded);
        if !loaded {
            let retained = residency.retained_bytes();
            if retained > 0 {
                if let Some(host) = host.upgrade() {
                    host.record_verifier_unload(retained);
                }
            }
        }
    }));
    let reservation = worker.clone();
    service.set_verifier_reserve(Arc::new(move || reservation.retained_bytes()));
    Ok(worker)
}

pub fn check(
    client: &listen::VerificationClient,
    text: &str,
    audio: &speak::Audio,
    cancel: &dyn Fn() -> bool,
) -> Result<serde_json::Value> {
    let started = Instant::now();
    let samples = crate::audio::mono_16k(&audio.samples, audio.sample_rate, 1)?;
    let mut evidence = hear(text, &samples, |window| {
        client.transcribe(window, Duration::from_millis(750), cancel).map(|t| (t.transcript.text, t.queue_ms, t.decode_ms))
    })?;
    evidence["total_ms"] = (started.elapsed().as_secs_f64() * 1000.).into();
    Ok(evidence)
}

/// The word check on `samples` heard window by window (`transcribe`: text, queue and decode ms). The recognizer hears
/// at most 15 s at once: a longer take is heard in windows cut at its quietest moment (night 1027: seven long takes
/// were rejected unheard, "at most 15 seconds"). A busy verifier passes the take unjudged only when nothing of it was
/// heard yet: after a window was heard, the busy one stays a failure (review, night 1027).
fn hear(text: &str, samples: &[f32], mut transcribe: impl FnMut(Vec<f32>) -> Result<(String, f64, f64)>) -> Result<serde_json::Value> {
    let (mut heard, mut queue_ms, mut decode_ms) = (Vec::new(), 0., 0.);
    for window in windows(samples) {
        match transcribe(samples[window].to_vec()) {
            Ok((said, queue, decode)) => {
                heard.push(said.trim().to_owned());
                queue_ms += queue;
                decode_ms += decode;
            }
            Err(error) if heard.is_empty() => return unjudged(&error).ok_or(error),
            Err(error) => return Err(error),
        }
    }
    let mut evidence = speak::coverage::check(text, &heard.join(" "));
    evidence["queue_ms"] = queue_ms.into();
    evidence["decode_ms"] = decode_ms.into();
    Ok(evidence)
}

/// The recognizer's longest window (15 s at 16 kHz), and how far back from it a cut looks for a pause.
const WINDOW: usize = 240_000;
const SEEK: usize = 48_000;
/// The shortest last window (1 s): a take just over 15 s cut at its last 20 ms left a window of nothing (review, night
/// 1027).
const TAIL: usize = 16_000;
/// `samples` (16 kHz) as windows the recognizer can hear: one when it fits, else each cut at the quietest 20 ms of the
/// three seconds before the limit, so the cut falls between words, and at least `TAIL` before the end.
fn windows(samples: &[f32]) -> Vec<std::ops::Range<usize>> {
    const FRAME: usize = 320;
    let mut out = Vec::new();
    let mut start = 0;
    while samples.len() - start > WINDOW {
        let limit = start + WINDOW;
        let last = limit.min(samples.len() - TAIL) - FRAME;
        let energy = |at: usize| samples[at..at + FRAME].iter().map(|x| x * x).sum::<f32>();
        let cut = (limit - SEEK..=last).step_by(FRAME).min_by(|a, b| energy(*a).total_cmp(&energy(*b))).unwrap_or(last) + FRAME / 2;
        out.push(start..cut);
        start = cut;
    }
    out.push(start..samples.len());
    out
}

/// The verifier was too busy to hear the take (its queue was full, or it did not start in time): nothing was judged, so
/// the take passes and its evidence says so. Any other verifier failure stays a failure.
pub fn unjudged(error: &anyhow::Error) -> Option<serde_json::Value> {
    let reason = error.to_string();
    matches!(reason.as_str(), "verification deadline" | "verification queue full").then(|| {
        serde_json::json!({"accepted": true, "unjudged": reason})
    })
}

#[cfg(test)]
mod tests {
    use super::{hear, unjudged, windows, TAIL, WINDOW};

    #[test]
    fn a_take_just_over_fifteen_seconds_leaves_no_sliver_window() {
        let voiced = |n: usize| (0..n).map(|k| ((k as f32) * 0.05).sin() * 0.3).collect::<Vec<f32>>();
        // Silent just before 15 s: the quietest frame of the old seek, which left a last window of 10-20 ms.
        for extra in [1, 320, 5_000, 20_000] {
            let mut take = voiced(WINDOW + extra);
            take[WINDOW - 640..WINDOW].iter_mut().for_each(|x| *x = 0.);
            let w = windows(&take);
            assert!(w.iter().all(|r| r.len() >= TAIL && r.len() <= WINDOW), "{extra}: {w:?}");
            assert_eq!(w.last().unwrap().end, take.len());
        }
    }

    #[test]
    fn a_busy_verifier_after_a_heard_window_is_a_failure() {
        let take = vec![0.1f32; WINDOW * 2];
        let mut n = 0;
        let r = hear("One two three.", &take, |_| {
            n += 1;
            if n == 1 { Ok(("One two".into(), 1., 1.)) } else { Err(anyhow::anyhow!("verification queue full")) }
        });
        assert!(r.is_err(), "{r:?}");
        // Busy on the first window: nothing was heard, so the take passes unjudged as before.
        let r = hear("One two three.", &take, |_| Err(anyhow::anyhow!("verification deadline"))).unwrap();
        assert_eq!((r["accepted"].as_bool(), r["unjudged"].as_str()), (Some(true), Some("verification deadline")));
        let mut parts = ["One two", "three."].into_iter();
        let r = hear("One two three.", &take[..WINDOW + 20_000], |_| Ok((parts.next().unwrap_or("").into(), 1., 2.))).unwrap();
        assert_eq!((r["accepted"].as_bool(), r["decode_ms"].as_f64()), (Some(true), Some(4.)), "two windows, joined");
    }

    #[test]
    fn a_take_over_fifteen_seconds_is_heard_in_windows_cut_at_a_pause() {
        let voiced = |n: usize| (0..n).map(|k| ((k as f32) * 0.05).sin() * 0.3).collect::<Vec<f32>>();
        assert_eq!(windows(&voiced(WINDOW)), [0..WINDOW], "15 s fits one window");
        // 20 s with a pause at 13.5-13.7 s: the cut falls in it.
        let mut take = voiced(320_000);
        take[216_000..219_200].iter_mut().for_each(|x| *x = 0.);
        let w = windows(&take);
        assert_eq!(w.len(), 2);
        assert!((216_000..219_200).contains(&w[0].end), "{w:?}");
        assert_eq!((w[0].start, w[1].start, w[1].end), (0, w[0].end, 320_000));
        // 40 s of speech without a pause: every window fits and together they cover it.
        let w = windows(&voiced(640_000));
        assert!(w.iter().all(|r| r.len() <= WINDOW) && w.windows(2).all(|p| p[0].end == p[1].start) && w.last().unwrap().end == 640_000, "{w:?}");
    }

    #[test]
    fn busy_verifier_passes_the_take_and_says_it_was_not_judged() {
        for reason in ["verification deadline", "verification queue full"] {
            let evidence = unjudged(&anyhow::anyhow!(reason)).expect(reason);
            assert_eq!(evidence["accepted"], true, "{reason}");
            assert_eq!(evidence["unjudged"], reason);
        }
    }

    #[test]
    fn other_verifier_failures_stay_failures() {
        for reason in ["verification worker stopped", "live recognizer unavailable", "verification queue full: retry", ""] {
            assert!(unjudged(&anyhow::anyhow!(reason)).is_none(), "{reason:?}");
        }
    }
}
