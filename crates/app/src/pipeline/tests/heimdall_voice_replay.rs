//! Opt-in, real audio and real models (no audio is played, no device is opened, the running Heimdall is not touched):
//!
//! 1. `offline_enrolment_of_heimdalls_voices` renders `heimdall_voice::LINES` silently in this test process with
//!    Supertonic and Phonon S (Edwin), takes Jarvis from Drama 3's own offline renders in the fixture
//!    (`jarvis-rendered/`), builds each voice's profile exactly as the app does (`heimdall_voice::profile`, the user's
//!    speed from his settings, read only) and writes `profiles.json` into the fixture folder.
//! 2. `replay_of_24_oct_echo_and_owner_over_it` replays the 24 Oct 13:34:40–13:38:30 microphone (MacBook speakers,
//!    Shure MV7i) through the real `Pipeline` barge-in with the real speaker encoder against those profiles: every
//!    reply's echo alone must never stop Heimdall; the user's own recorded speech added over each echo must.
//!
//! The fixture stays outside the repository (the archive is the user's): `HEIMDALL_BARGE_FIXTURE`, default
//! `~/Library/Application Support/Heimdall-fixtures/barge-1024` (`mic-16k.wav`, `audio.json`, `evidence.jsonl`, from
//! Heimdall's archive read-only). Models: `HEIMDALL_MODELS`, default the installed app's.
//!
//! cargo test -p app --bin heimdall -- --ignored --nocapture offline_enrolment_of_heimdalls_voices
//! cargo test -p app --bin heimdall -- --ignored --nocapture replay_of_24_oct_echo_and_owner_over_it (about 4 min:
//! beyond nextest's 120 s limit)
use super::plumbing_tests::{rig, Speech};
use super::*;
use std::path::PathBuf;

const S: u64 = 1_000_000_000;
/// Mic time 0 sits here on the test's host clock.
const BASE: u64 = 10 * S;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap())
}
fn fixture() -> PathBuf {
    std::env::var_os("HEIMDALL_BARGE_FIXTURE").map(PathBuf::from).unwrap_or_else(|| home().join("Library/Application Support/Heimdall-fixtures/barge-1024"))
}
fn models() -> PathBuf {
    std::env::var_os("HEIMDALL_MODELS").map(PathBuf::from).unwrap_or_else(|| home().join("Applications/Heimdall.app/Contents/Resources/models"))
}
fn encoder() -> speakers::embedding::ReDimNet {
    speakers::embedding::ReDimNet::load(&models().join("speakers/ReDimNet2B6.onnx")).expect("speaker encoder")
}
fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

#[test]
#[ignore = "loads Supertonic, Phonon S and the speaker encoder; writes the local fixture's profiles.json"]
fn offline_enrolment_of_heimdalls_voices() {
    use speak::Synthesizer;
    let models = models();
    let speed = std::fs::read(home().join("Library/Application Support/Heimdall/settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["voice_speed_percent"].as_f64())
        .unwrap_or(100.) as f32 / 100.;
    let mut lines: BTreeMap<String, Vec<Vec<f32>>> = BTreeMap::new();
    let to16 = |a: speak::Audio| crate::audio::mono_16k(&a.samples, a.sample_rate, 1).unwrap();
    let started = Instant::now();
    {
        let mut supertonic = speak::Supertonic::load_with_backend(models.join("supertonic-3"), speak::TtsBackend::Cpu).expect("Supertonic");
        lines.insert("supertonic".into(), heimdall_voice::LINES.iter().map(|t| to16(supertonic.synthesize(t, "en", &|| false).unwrap())).collect());
    }
    eprintln!("supertonic rendered in {:.1} s", started.elapsed().as_secs_f64());
    let started = Instant::now();
    {
        let mut phonon = speak::phonon::Phonon::load(&models.join(speak::phonon::MODEL_DIR)).expect("Phonon S");
        phonon.set_voice(speak::phonon::DEFAULT_PRESET).unwrap();
        let rendered = heimdall_voice::LINES
            .iter()
            .map(|t| to16(speak::voice::stream_sentence(&mut phonon, t, &speak::drama3::normalize(t), 1.0, &|| false, &mut |_| Ok(())).unwrap()))
            .collect();
        lines.insert(format!("phonon-{}", speak::phonon::DEFAULT_PRESET), rendered);
    }
    eprintln!("phonon rendered in {:.1} s", started.elapsed().as_secs_f64());
    // Jarvis: Drama 3's own offline renders (22 Oct, `jarvis-rendered/*.wav` in the fixture). A second Drama 3 here
    // would need about 27 GB beside the running Heimdall's, and its memory guard would then unload Heimdall's Jarvis.
    let rendered: Vec<Vec<f32>> = {
        let mut paths: Vec<_> = std::fs::read_dir(fixture().join("jarvis-rendered")).expect("jarvis-rendered").map(|e| e.unwrap().path()).collect();
        paths.sort();
        paths.iter().map(|p| hound::WavReader::open(p).unwrap().samples::<i16>().map(|s| s.unwrap() as f32 / 32768.).collect()).collect()
    };
    assert!(rendered.len() >= 5, "Jarvis renders");
    lines.insert("drama3".into(), rendered);
    let mut encoder = encoder();
    let mut profiles = BTreeMap::new();
    let out = fixture().join("enrolment");
    std::fs::create_dir_all(&out).unwrap();
    for (key, rendered) in &lines {
        for (i, l) in rendered.iter().enumerate() {
            let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
            let mut w = hound::WavWriter::create(out.join(format!("{key}-{i}.wav")), spec).unwrap();
            l.iter().for_each(|s| w.write_sample((s.clamp(-1., 1.) * 32767.) as i16).unwrap());
            w.finalize().unwrap();
        }
        let started = Instant::now();
        let p = heimdall_voice::profile(rendered, speed, &mut |x| encoder.embed_short(x), 0).unwrap();
        eprintln!("{key}: {} lines, {:.1} s of speech, {} samples, embedded in {:.1} s", p.lines,
            rendered.iter().map(|l| l.len()).sum::<usize>() as f64 / 16000., p.embeddings.len(), started.elapsed().as_secs_f64());
        profiles.insert(key.clone(), p);
    }
    std::fs::write(fixture().join("profiles.json"), serde_json::to_vec_pretty(&profiles).unwrap()).unwrap();
}

/// Each run of Heimdall's speech in the fixture, in mic seconds: (Heimdall's own tap start, end).
fn replies(evidence: &[serde_json::Value], mic0: u64) -> Vec<(f64, f64)> {
    let mut out = vec![];
    let mut start = None;
    for r in evidence {
        let p = &r["payload"];
        if r["kind"] != "listen_event" || !p["source"].as_str().unwrap_or("").starts_with("process:") {
            continue;
        }
        let Some(host) = p["host_time_ns"].as_u64() else { continue };
        let t = (host as f64 - mic0 as f64) / 1e9;
        match p["event"].as_str() {
            Some("speech_start") => start = Some(t),
            Some("speech_end") => {
                if let Some(s) = start.take().filter(|s| *s >= 0.) {
                    out.push((s, t));
                }
            }
            _ => {}
        }
    }
    out
}
/// The microphone's own speech start and end events in mic seconds.
fn mic_events(evidence: &[serde_json::Value], mic0: u64) -> Vec<(f64, bool)> {
    evidence
        .iter()
        .filter(|r| r["kind"] == "listen_event" && r["payload"]["source"].as_str().unwrap_or("").starts_with("mic:"))
        .filter_map(|r| {
            let p = &r["payload"];
            let start = match p["event"].as_str()? { "speech_start" => true, "speech_end" => false, _ => return None };
            Some(((p["host_time_ns"].as_u64()? as f64 - mic0 as f64) / 1e9, start))
        })
        .collect()
}

struct Run {
    stopped_at: Option<u64>,
    cosines: Vec<f32>,
    words_checks: usize,
}
/// One reply through the real barge-in: `mic` (16 kHz, mic time 0 at index 0) from `from` to `to` seconds, the
/// microphone's speech events as archived, Heimdall speaking throughout, the encoder real.
fn replay(mic: &[f32], from: f64, to: f64, events: &[(f64, bool)], profiles: &heimdall_voice::State) -> Run {
    let (_t, mut a, source) = rig(false);
    a.tools.set_agent_enabled(true);
    a.hooks.speakers = true;
    let speech = Speech::install(&a);
    speech.say(&a, "holdlong the reply that was playing");
    let ns = |t: f64| BASE + (t * 1e9) as u64;
    let index = |t: f64| ((t * 16000.) as usize).min(mic.len());
    a.audio.insert(source.clone(), AudioRing { start: ns(from - 2.), samples: mic[index(from - 2.)..index(from)].to_vec().into() });
    let cosines = Arc::new(Mutex::new(vec![]));
    let seen = cosines.clone();
    let mut model = encoder();
    let profiles = heimdall_voice::State::new(profiles.profiles.clone(), None, None);
    a.hooks.voice_fn = Some(Box::new(move |x: &[f32]| {
        let c = profiles.similarity(&model.embed_short(x).ok()?)?.1;
        seen.lock().unwrap().push(c);
        Some(c)
    }));
    // Speech already going on when the window opens counts from its start.
    if events.iter().filter(|(t, _)| *t < from).next_back().is_some_and(|(_, start)| *start) {
        a.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: ns(from) }).unwrap();
    }
    let mut next = events.iter().filter(|(t, _)| *t >= from && *t < to).peekable();
    let mut t = from;
    let mut stopped_at = None;
    while t < to {
        while let Some((at, start)) = next.next_if(|(at, _)| *at <= t) {
            let event = if *start { ListenEvent::SpeechStart { source: source.clone(), host_time_ns: ns(*at) } } else { ListenEvent::SpeechEnd { source: source.clone(), host_time_ns: ns(*at) } };
            a.listen(event).unwrap();
        }
        let at = a.audio[&source].end();
        let chunk = &mic[index(t)..index(t + 0.02)];
        a.audio.get_mut(&source).unwrap().push(at, chunk);
        a.barge_audio(&source).unwrap();
        if let Some(stop) = a.index.evidence.records().unwrap().iter().find(|r| r.kind == "barge_in" && r.payload["outcome"] == "stopped") {
            stopped_at = stop.payload["window"]["end_ns"].as_u64();
            break;
        }
        t += 0.02;
    }
    let words_checks = a.index.evidence.records().unwrap().iter().filter(|r| r.kind == "barge_in" && r.payload["side_transcript"].is_object()).count();
    if stopped_at.is_some() {
        speech.cancelled("holdlong the reply that was playing");
    } else {
        a.tools.interrupt();
    }
    let cosines = cosines.lock().unwrap().clone();
    Run { stopped_at, cosines, words_checks }
}

#[test]
#[ignore = "needs the local 24 Oct fixture, its profiles.json and the speaker encoder"]
fn replay_of_24_oct_echo_and_owner_over_it() {
    let dir = fixture();
    let profiles: BTreeMap<String, speakers::identity::HeimdallVoice> =
        serde_json::from_slice(&std::fs::read(dir.join("profiles.json")).expect("run offline_enrolment_of_heimdalls_voices first")).unwrap();
    let profiles = heimdall_voice::State::new(profiles, None, None);
    let mut reader = hound::WavReader::open(dir.join("mic-16k.wav")).unwrap();
    let mic: Vec<f32> = reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.).collect();
    let audio: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("audio.json")).unwrap()).unwrap();
    let mic0 = audio["sources"]["mic"]["first_sample_host_ns"].as_u64().unwrap();
    let evidence: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("evidence.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let replies = replies(&evidence, mic0);
    let events = mic_events(&evidence, mic0);
    let long: Vec<_> = replies.iter().copied().filter(|(s, e)| e - s >= 3.).collect();
    assert_eq!(long.len(), 5, "the five long replies of 13:34:46–13:37:55: {replies:?}");
    let mut report = vec![];
    // 1. Echo only: never a stop. The 13:35:33 and 13:36:30 replies are the two that were cut live.
    for (s, e) in &long {
        let run = replay(&mic, s - 0.5, e + 0.3, &events, &profiles);
        let low = run.cosines.iter().copied().fold(f32::MAX, f32::min);
        report.push(format!("echo {s:.1}-{e:.1} s: {} checks, lowest {low:.3}, words checks {}, stopped {}", run.cosines.len(), run.words_checks, run.stopped_at.is_some()));
        eprintln!("{}", report.last().unwrap());
        assert!(run.stopped_at.is_none(), "echo alone stopped Heimdall: {report:#?}");
        // A check every half second from the microphone's first speech onset in the reply.
        let onset = events.iter().find(|(t, start)| *start && *t >= s - 0.5).map_or(*s, |(t, _)| *t);
        assert!(run.cosines.len() as f64 >= (e - onset) / 0.5 - 4., "the checks ran: {report:#?}");
    }
    // 2. The user over the echo: his own recorded lines (mic, Heimdall silent) added 2 s into each reply.
    let owner = [(19.33, 32.33), (60.42, 62.83), (118.02, 147.59)];
    let mut latencies = vec![];
    for (s, e) in &long {
        for (ts, te) in owner {
            let onset = s + 2.0;
            let mut mixed = mic.clone();
            let n = (((te - ts) as f64).min(e - onset) * 16000.) as usize;
            let (a, b) = ((onset * 16000.) as usize, (ts * 16000.) as usize);
            for i in 0..n {
                mixed[a + i] += mic[b + i];
            }
            // His speech starts on the mic (the echo's own onset already armed the barge-in).
            let run = replay(&mixed, s - 0.5, e + 0.3, &events, &profiles);
            // From his first word: the Shure gates the room to silence, so his clip opens with about a second of zeros.
            let first_word = (b..b + n).step_by(320).find(|&i| rms(&mic[i..(i + 320).min(mic.len())]) > 0.02).map_or(0., |i| (i - b) as f64 / 16000.);
            let latency = run.stopped_at.map(|at| (at as f64 - (BASE as f64 + (onset + first_word) * 1e9)) / 1e9);
            report.push(format!("owner {ts:.0} over {s:.1}-{e:.1} s (his RMS {:.3}, echo {:.3}): stop after {latency:?} s, cosines {:?}",
                rms(&mic[b..b + n.min(32000)]), rms(&mic[a..a + n.min(32000)]), run.cosines.iter().map(|c| (c * 100.).round() / 100.).collect::<Vec<_>>()));
            eprintln!("{}", report.last().unwrap());
            latencies.push(latency);
        }
    }
    eprintln!("{}", report.join("\n"));
    let stopped: Vec<f64> = latencies.iter().flatten().copied().collect();
    assert_eq!(stopped.len(), latencies.len(), "User over the echo must stop Heimdall: {report:#?}");
    let mut sorted = stopped.clone();
    sorted.sort_by(f64::total_cmp);
    let summary = format!("stops after his first word (audio time, before about 0.3 s of encoder, transcript and decider): min {:.2} s, median {:.2} s, max {:.2} s",
        sorted[0], sorted[sorted.len() / 2], sorted[sorted.len() - 1]);
    eprintln!("{summary}");
    report.push(summary);
    std::fs::write(dir.join("replay-report.txt"), report.join("\n")).unwrap();
    assert!(stopped.iter().all(|l| *l <= 2.0), "{report:#?}");
}
