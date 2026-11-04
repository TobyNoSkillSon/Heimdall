use speak::{Audio, AudioOutput, SpeechQueue, Synthesizer};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
#[test]
fn polish_frontend_and_chunking() {
    assert_eq!(
        speak::preprocess_text("Zażółć gęślą jaźń", "pl").unwrap(),
        "<pl>Zaz\u{307}o\u{301}łc\u{301} ge\u{328}s\u{301}la\u{328} jaz\u{301}n\u{301}.</pl>"
    );
    assert!(speak::preprocess_text("hi", "invalid").is_err());
    assert_eq!(
        speak::chunk_text("One. Two.", Some(5)),
        vec!["One.", "Two."]
    );
}
struct FakeSynth;
impl Synthesizer for FakeSynth {
    fn synthesize(&mut self, _: &str, _: &str, cancel: &dyn Fn() -> bool) -> anyhow::Result<Audio> {
        for _ in 0..20 {
            if cancel() {
                anyhow::bail!("cancelled")
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(Audio {
            samples: vec![0.1; 441],
            sample_rate: 44100,
            first_audio_ms: 20.,
            synthesis_ms: 20.,
        })
    }
}
struct Sink(Arc<AtomicUsize>);
impl AudioOutput for Sink {
    fn output(&mut self, _: &Audio, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
        if !cancel() {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}
#[test]
fn queue_submission_and_interrupt() {
    let count = Arc::new(AtomicUsize::new(0));
    let queue = SpeechQueue::new(FakeSynth, Sink(count.clone()));
    queue.speak("first", "en").unwrap();
    queue.speak("second", "pl").unwrap();
    queue.interrupt();
    assert!(queue.speak("bad", "de").is_err());
    let fresh=queue.speak("fresh", "en").unwrap();
    loop {
        match queue.events.recv_timeout(Duration::from_secs(2)).unwrap() {
            speak::SpeechEvent::Complete {job,..} if job==fresh=>break,
            speak::SpeechEvent::Failed {job,error} if job==fresh=>panic!("fresh speech failed: {error}"),
            _=>{},
        }
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
/// Offline integration golden. Opt-in assets; never touches an output device.
#[test]
#[ignore = "requires downloaded local Supertonic 3 models"]
fn golden_en_pl_wavs() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let out = root.join("tts-golden");
    std::fs::create_dir_all(&out).unwrap();
    let load = std::time::Instant::now();
    let mut tts =
        speak::Supertonic::load_with_backend(root.join("supertonic-3"), speak::TtsBackend::Cpu)
            .unwrap();
    let load_ms = load.elapsed().as_secs_f64() * 1000.;
    let mut records = Vec::new();
    for (lang, text) in [
        ("en", "Hello User. I am listening."),
        ("pl", "Cześć User. Słucham ciebie."),
    ] {
        let a = tts.render(text, lang).unwrap();
        assert_eq!(a.sample_rate, 44100);
        assert!(a.duration_s() > 1. && a.duration_s() < 10.);
        let rms = (a.samples.iter().map(|x| (*x as f64).powi(2)).sum::<f64>()
            / a.samples.len() as f64)
            .sqrt();
        assert!(rms > 0.005 && rms < 0.5, "invalid RMS {rms}");
        // The M3 style (`speak::VOICE`); M1's were 117748/0.03738431055 and 123271/0.03617893832.
        let (expected_samples, expected_rms) = if lang == "en" {
            (107035, 0.04281097323)
        } else {
            (111364, 0.04972051036)
        };
        assert_eq!(
            a.samples.len(),
            expected_samples,
            "seeded waveform length regression"
        );
        assert!(
            (rms - expected_rms).abs() < 0.00001,
            "seeded waveform RMS regression"
        );
        let wav = out.join(format!("{lang}.wav"));
        a.write_wav(&wav).unwrap();
        let reader = hound::WavReader::open(&wav).unwrap();
        assert_eq!(reader.duration() as usize, a.samples.len());
        records.push(serde_json::json!({"lang":lang,"text":text,"load_ms":load_ms,"first_audio_ms":a.first_audio_ms,"synthesis_ms":a.synthesis_ms,"audio_seconds":a.duration_s(),"rtf":a.real_time_factor(),"rms":rms,"samples":a.samples.len(),"wav":wav}));
    }
    let records = serde_json::to_string_pretty(&records).unwrap();
    std::fs::write(out.join("measurements.json"), &records).unwrap();
    println!("{records}");
}
#[test]
fn offline_sink_retains_every_chunk() {
    let path = std::env::temp_dir().join(format!("heimdall-speech-{}.wav", std::process::id()));
    let mut sink = speak::WavOutput::new(&path);
    let a = Audio {
        samples: vec![0.1; 123],
        sample_rate: 44100,
        first_audio_ms: 0.,
        synthesis_ms: 0.,
    };
    sink.output(&a, &|| false).unwrap();
    sink.output(&a, &|| false).unwrap();
    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.duration(), 246);
    drop(reader);
    drop(sink);
    std::fs::remove_file(path).unwrap();
}
