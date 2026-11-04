//! Bug note 2026-10-30T18-42-08 and with Listening off he heard "a default voice", not Jarvis.
//! Opt-in (loads the 27 GB Drama 3 sidecar; about 90 s; never plays a sound):
//!
//! `HEIMDALL_JARVIS_LIVE=1 cargo test -p app --bin heimdall jarvis_voice -- --ignored --nocapture`
//!
//! The real speech path (`LazySynth::stream_packets`, the word-check verifier, the sidecar) says one line twice in an
//! isolated data folder, with no capture and no pipeline at all (Listening off): once with Jarvis never claimed, as on
//! 31 Oct before the fix (the residency check ran only after capture events), and once claimed by the residency rule
//! the settings tick now runs (`jarvis_wanted`: Speaking on, Jarvis chosen, no Listening input). Each take is scored with
//! Heimdall's own speaker embedder (ReDimNet2-B6) against Jarvis's reference voice. The threshold 0.55 sits between the
//! closest other voice (0.34) and the weakest Jarvis take (0.76), measured 23 Oct (lab/voice/jarvis-similarity-2026-10-23).
use super::speech::LazySynth;
use crate::speech_voice::{Route, Voices};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU16, Arc, Mutex};

const LINE: &str = "Two ads close tomorrow: interLAN in Poznań and 7Technology in Warsaw. Tick the ones you want on the page and press Done.";
const JARVIS_MIN: f32 = 0.55;

fn models() -> PathBuf {
    std::env::var_os("HEIMDALL_TEST_MODELS").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("Applications/Heimdall.app/Contents/Resources/models"))
}

fn say(synth: &mut LazySynth) -> speak::Audio {
    let mut audio: Vec<f32> = vec![];
    let mut rate = 0;
    let said = speak::Synthesizer::stream_packets(synth, LINE, "en", &|| false, &mut |p| {
        if let speak::SpeechPacket::Audio(a) = p {
            rate = a.sample_rate;
            audio.extend_from_slice(&a.samples);
        }
        Ok(())
    }).expect("the line was said");
    if audio.is_empty() { said } else { speak::Audio { samples: audio, sample_rate: rate, first_audio_ms: 0., synthesis_ms: 0. } }
}

/// Speech only: 20 ms frames within 40 dB of the loudest (as lab/voice/jarvis-similarity-2026-10-23/score.py).
fn voiced(x: &[f32]) -> Vec<f32> {
    let n = 320;
    let rms: Vec<f32> = x.chunks_exact(n).map(|f| (f.iter().map(|v| v * v).sum::<f32>() / n as f32 + 1e-12).sqrt()).collect();
    let peak = rms.iter().cloned().fold(0., f32::max);
    x.chunks_exact(n).zip(&rms).filter(|(_, r)| **r > peak * 0.01).flat_map(|(f, _)| f.iter().copied()).collect()
}

fn similarity(net: &mut speakers::embedding::ReDimNet, reference: &speakers::embedding::Embedding, a: &speak::Audio) -> f32 {
    let pcm = crate::audio::mono_16k(&a.samples, a.sample_rate, 1).unwrap();
    net.embed_short(&voiced(&pcm)).unwrap().cosine(reference)
}

fn wav(path: &Path, a: &speak::Audio) {
    let spec = hound::WavSpec { channels: 1, sample_rate: a.sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for s in &a.samples { w.write_sample(*s).unwrap(); }
    w.finalize().unwrap();
}

#[test]
#[ignore = "loads Jarvis (27 GB); opt-in with HEIMDALL_JARVIS_LIVE=1"]
fn jarvis_voice_with_listening_off_is_really_jarvis() {
    if std::env::var_os("HEIMDALL_JARVIS_LIVE").is_none() { return; }
    let models = models();
    let data = tempfile::tempdir().unwrap();
    let out = std::env::var_os("HEIMDALL_JARVIS_OUT").map(PathBuf::from).unwrap_or_else(|| data.path().join("takes"));
    std::fs::create_dir_all(&out).unwrap();
    let session = data.path().join("archive/session-jarvis-check");
    std::fs::create_dir_all(&session).unwrap();
    let drama3 = speak::drama3::Service::new(models.join("drama3"));
    crate::drama3_verify::install(&drama3, models.join("parakeet-ultra")).unwrap();
    let voices = Arc::new(Voices::new(&models));
    assert_eq!(voices.route(), Route::Jarvis, "User's choice: Jarvis");
    let mut synth = LazySynth {
        drama3: Some(drama3.clone()), session, models: models.join("supertonic-3"), model: Arc::new(Mutex::new(None)), backend: String::new(),
        record: None, speed: Arc::new(AtomicU16::new(100)), sample: None, voices: Some(voices.clone()),
    };
    let mut net = speakers::embedding::ReDimNet::load(&models.join("speakers/ReDimNet2B6.onnx")).unwrap();
    let reference = {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../speak/drama3/engine/voice/jarvis.wav");
        let mut r = hound::WavReader::open(&path).unwrap();
        let spec = r.spec();
        let x: Vec<f32> = if spec.sample_format == hound::SampleFormat::Float { r.samples::<f32>().map(Result::unwrap).collect() }
            else { let s = 2f32.powi(spec.bits_per_sample as i32 - 1); r.samples::<i32>().map(|v| v.unwrap() as f32 / s).collect() };
        let pcm = crate::audio::mono_16k(&x, spec.sample_rate, spec.channels).unwrap();
        net.embed_short(&voiced(&pcm)).unwrap()
    };

    // Before: nothing claims Jarvis (no capture event, so no residency check). The lighter voice says it.
    let before = say(&mut synth);
    wav(&out.join("before-unclaimed.wav"), &before);
    let before_sim = similarity(&mut net, &reference, &before);

    // After: the settings tick's rule, with no Listening input, claims him; the line waits for the load as a reply does.
    drama3.configure(crate::pipeline::jarvis_wanted(true, false, "speaking", voices.route()));
    assert!(drama3.wait_ready(std::time::Duration::from_secs(180), &|| false), "Jarvis loaded: {:?}", drama3.status());
    let after = say(&mut synth);
    wav(&out.join("after-claimed.wav"), &after);
    let after_sim = similarity(&mut net, &reference, &after);
    let switches: Vec<_> = drama3.events().into_iter().filter(|e| e["event"] == "voice_switch").collect();
    drama3.configure(false);

    let report = serde_json::json!({"line": LINE, "before": {"rate": before.sample_rate, "seconds": before.samples.len() as f64 / before.sample_rate as f64, "similarity": before_sim},
        "after": {"rate": after.sample_rate, "seconds": after.samples.len() as f64 / after.sample_rate as f64, "similarity": after_sim}, "switches_after_claim": switches.len(), "threshold": JARVIS_MIN});
    std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("{report:#}");
    assert!(before_sim < JARVIS_MIN, "the unclaimed take should be the lighter voice: {before_sim}");
    assert!(after_sim >= JARVIS_MIN, "with Speaking on and Listening off the line is not Jarvis: {after_sim}");
    assert!(switches.is_empty(), "Jarvis handed sentences to the lighter voice: {switches:?}");
}

const LONG: &str = "Here is where the night got to. The archive now has one writer, so the database lock that kept stopping speech \
should be gone. Jarvis runs on the new Rust engine, which batches the later sentences of a reply while the first one plays. \
If that engine ever fails to start, the old Python worker takes over by itself. Goals replaces your to-do list, and your nine \
open items moved into it. Four goals are seeded: getting a job, shipping Arsenal, shipping Heimdall publicly, and making Jarvis \
efficient. The full test suite ran once, and nothing failed that does not also fail on the previous build. That is everything \
for now.";

/// Install 1 Nov (Rust engine the default): the installed bundle's engine says a long reply as the app streams it, and
/// playback at the user's speed is simulated from each packet's arrival: which engine loaded, first audio, every gap.
/// `HEIMDALL_JARVIS_LIVE=1 HEIMDALL_JARVIS_OUT=<dir> cargo test -p app --bin heimdall jarvis_long -- --ignored --nocapture`
#[test]
#[ignore = "loads Jarvis (27 GB); opt-in with HEIMDALL_JARVIS_LIVE=1"]
fn jarvis_long_reply_on_the_default_engine_plays_without_gaps() {
    if std::env::var_os("HEIMDALL_JARVIS_LIVE").is_none() { return; }
    let models = models();
    let data = tempfile::tempdir().unwrap();
    let out = std::env::var_os("HEIMDALL_JARVIS_OUT").map(PathBuf::from).unwrap_or_else(|| data.path().join("takes"));
    std::fs::create_dir_all(&out).unwrap();
    let percent: u16 = std::env::var("HEIMDALL_JARVIS_SPEED").ok().and_then(|v| v.parse().ok()).unwrap_or(160);
    let drama3 = speak::drama3::Service::new(models.join("drama3"));
    crate::drama3_verify::install(&drama3, models.join("parakeet-ultra")).unwrap();
    drama3.set_pace(Arc::new(AtomicU16::new(percent)));
    drama3.configure(true);
    assert!(drama3.wait_ready(std::time::Duration::from_secs(180), &|| false), "Jarvis loaded: {:?}", drama3.status());
    let mut events = drama3.events();
    let ready = events.iter().rev().find(|e| e["event"] == "ready").cloned().unwrap_or_default();
    let rate = percent as f64 / 100.;
    let asked = std::time::Instant::now();
    let (mut start, mut first_ms, mut queued_s, mut gaps) = (None::<std::time::Instant>, 0., 0f64, Vec::<f64>::new());
    let mut samples: Vec<f32> = vec![];
    let mut sample_rate = 0;
    drama3.stream(LONG, &|| false, &mut |p| {
        if let speak::SpeechPacket::Audio(a) = p {
            let now = std::time::Instant::now();
            match start {
                None => { start = Some(now); first_ms = asked.elapsed().as_secs_f64() * 1000.; }
                Some(s) => {
                    // the listener reaches the end of what was queued at s + queued/rate; arriving later is a gap
                    let due = queued_s / rate;
                    let at = now.duration_since(s).as_secs_f64();
                    if at > due { gaps.push((at - due) * 1000.); start = Some(s + std::time::Duration::from_secs_f64(at - due)); }
                }
            }
            sample_rate = a.sample_rate;
            queued_s += a.samples.len() as f64 / a.sample_rate as f64;
            samples.extend_from_slice(&a.samples);
        }
        Ok(())
    }).expect("the long reply was said");
    events.extend(drama3.events());
    drama3.configure(false);
    let audio = speak::Audio { samples, sample_rate, first_audio_ms: first_ms, synthesis_ms: 0. };
    wav(&out.join("long-reply.wav"), &audio);
    let mut net = speakers::embedding::ReDimNet::load(&models.join("speakers/ReDimNet2B6.onnx")).unwrap();
    let reference = {
        let path = models.join("drama3/engine/voice/jarvis.wav");
        let mut r = hound::WavReader::open(&path).unwrap();
        let spec = r.spec();
        let x: Vec<f32> = if spec.sample_format == hound::SampleFormat::Float { r.samples::<f32>().map(Result::unwrap).collect() }
            else { let s = 2f32.powi(spec.bits_per_sample as i32 - 1); r.samples::<i32>().map(|v| v.unwrap() as f32 / s).collect() };
        net.embed_short(&voiced(&crate::audio::mono_16k(&x, spec.sample_rate, spec.channels).unwrap())).unwrap()
    };
    let sim = similarity(&mut net, &reference, &audio);
    let switches = events.iter().filter(|e| e["event"] == "voice_switch").count();
    let fallback: Vec<_> = events.iter().filter(|e| e["event"] == "engine_fallback" || e["event"] == "engine_strike").cloned().collect();
    let audible: Vec<f64> = gaps.iter().copied().filter(|g| *g >= 20.).collect();
    let report = serde_json::json!({"engine": ready["engine"], "load_ms": ready["load_ms"], "speed_percent": percent,
        "audio_s": queued_s, "first_audio_ms": first_ms, "gaps_ms_over_20": audible, "gap_total_ms": gaps.iter().sum::<f64>(),
        "similarity": sim, "threshold": JARVIS_MIN, "voice_switches": switches, "engine_events": fallback,
        "rejected_takes": events.iter().filter(|e| e["event"] == "sentence_coverage" && e["result"]["accepted"] == false).count(),
        "batches": events.iter().filter(|e| e["event"] == "batch").cloned().collect::<Vec<_>>()});
    std::fs::write(out.join("long-report.json"), serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("{report:#}");
    assert_eq!(ready["engine"], "rust", "the default engine: {ready}");
    assert!(sim >= JARVIS_MIN, "not Jarvis: {sim}");
    assert_eq!(switches, 0, "sentences went to the lighter voice");
    assert!(audible.is_empty(), "gaps: {audible:?}");
    // Install 2 Nov: batching is the default, so a seven-sentence reply has its later sentences batched.
    assert!(events.iter().any(|e| e["event"] == "batch"), "no batch on a long reply");
}
