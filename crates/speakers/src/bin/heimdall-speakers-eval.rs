//! Agent-operated bounded public-data evaluation. All outputs stay in ignored models/speakers.
use anyhow::{ensure, Result};
use serde::Serialize;
use speakers::{
    codec,
    diar::{Diarizer, SpeakerSegment},
    embedding::{Embedding, ReDimNet, VoiceEncoder},
    identity::{Calibration, EnrollmentStore, Identity, Trial},
    pipeline::clean_intervals,
    scoring::{self, Annotation},
};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Serialize)]
struct Window {
    speaker: String,
    parts: Vec<(usize, usize)>,
}
fn windows(reference: &[Annotation], samples: usize) -> Vec<Window> {
    let names: Vec<_> = reference
        .iter()
        .map(|a| a.speaker.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let segments: Vec<_> = reference
        .iter()
        .map(|a| SpeakerSegment {
            start_time: a.start,
            end_time: a.end,
            speaker: names.iter().position(|n| n == &a.speaker).unwrap() as i32 + 1,
        })
        .collect();
    let mut out = vec![];
    for (index, name) in names.iter().enumerate() {
        let mut parts = vec![];
        let mut collected = 0;
        let target = 3 * 16000;
        for (a, b) in clean_intervals(&segments, index as i32 + 1, 0., samples as f64 / 16000.) {
            let mut start = (a * 16000.).ceil() as usize;
            let end = (b * 16000.).floor() as usize;
            while start < end {
                let n = (end - start).min(target - collected);
                parts.push((start, start + n));
                collected += n;
                start += n;
                if collected == target {
                    out.push(Window {
                        speaker: name.clone(),
                        parts: std::mem::take(&mut parts),
                    });
                    collected = 0;
                }
            }
        }
    }
    out
}
fn window_audio(audio: &[f32], window: &Window) -> Vec<f32> {
    window
        .parts
        .iter()
        .flat_map(|&(a, b)| audio[a..b].iter().copied())
        .collect()
}
fn write_wav(path: &Path, pcm: &[i16]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(path, spec)?;
    for &s in pcm {
        wav.write_sample(s)?;
    }
    wav.finalize()?;
    Ok(())
}
fn rss() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    #[cfg(target_os = "macos")]
    {
        usage.ru_maxrss as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        usage.ru_maxrss as u64 * 1024
    }
}
fn main() -> Result<()> {
    let models = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models/speakers"));
    let mut wav = hound::WavReader::open(models.join("ami-480.wav"))?;
    let pcm: Vec<i16> = wav
        .samples::<i16>()
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        wav.spec().sample_rate == 16000 && wav.spec().channels == 1,
        "invalid corpus WAV"
    );
    let duration = pcm.len() as f64 / 16000.;
    let reference = scoring::parse_rttm(&std::fs::read_to_string(models.join("ES2004a.rttm"))?)?;
    let windows = windows(&reference, pcm.len());
    let mut indexed: BTreeMap<String, Vec<Window>> = BTreeMap::new();
    for w in windows {
        indexed.entry(w.speaker.clone()).or_default().push(w);
    }
    ensure!(
        indexed.len() == 4 && indexed.values().all(|v| v.len() >= 5),
        "need four voices with >=5 disjoint three-second windows"
    );
    std::fs::write(
        models.join("identity-windows.json"),
        serde_json::to_vec_pretty(&indexed)?,
    )?;
    let load = Instant::now();
    let diar = Diarizer::load(&models.join("Nemotron-3-Diarization.q8_0.gguf"), 0)?;
    let mut encoder = ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?;
    let load_seconds = load.elapsed().as_secs_f64();
    let mut conditions = vec![];
    let mut embeds: BTreeMap<i32, BTreeMap<String, Vec<Embedding>>> = BTreeMap::new();
    // Cold graph/warm kernels: a 1-s silent stream once, excluded from steady-state timings.
    let mut warm = diar.stream(16000)?;
    warm.push_f32(&vec![0.; 16000])?;
    warm.finish()?;
    drop(warm);
    for bitrate in [0, 24000, 64000] {
        let mut opus_receipt = serde_json::Value::Null;
        let decoded = if bitrate == 0 {
            pcm.clone()
        } else {
            let replay = codec::roundtrip(&pcm, bitrate)?;
            write_wav(&models.join(format!("ami-{bitrate}.wav")), &replay.decoded)?;
            let bytes = replay.packets.iter().map(Vec::len).sum::<usize>();
            let mut f = std::fs::File::create(models.join(format!("ami-{bitrate}.opuspkt")))?;
            f.write_all(b"HEIMOPUS\x01")?;
            f.write_all(&(pcm.len() as u64).to_le_bytes())?;
            f.write_all(&(replay.lookahead_samples as u32).to_le_bytes())?;
            for packet in &replay.packets {
                f.write_all(&(packet.len() as u16).to_le_bytes())?;
                f.write_all(packet)?;
            }
            opus_receipt = serde_json::json!({"codec":codec::version(),"target_bits_per_second":bitrate,"actual_payload_bits_per_second":bytes as f64*8./duration,"packet_ms":20,"application":"VOIP","lookahead_samples":replay.lookahead_samples,"input_rate_hz":16000,"packet_loss_percent":0});
            replay.decoded
        };
        let audio: Vec<_> = decoded.iter().map(|s| *s as f32 / 32768.).collect();
        let mut stream = diar.stream(16000)?;
        let start = Instant::now();
        let mut fed = 0;
        let mut first_audio = None;
        let mut first_compute = None;
        for chunk in audio.chunks(320) {
            stream.push_f32(chunk)?;
            fed += chunk.len();
            if first_audio.is_none() && stream.frame_count() > 0 {
                first_audio = Some(fed as f64 / 16000.);
                first_compute = Some(start.elapsed().as_secs_f64());
            }
        }
        stream.finish()?;
        let processing_seconds = start.elapsed().as_secs_f64();
        let segments = stream.completed_segments()?;
        let hypothesis: Vec<_> = segments
            .iter()
            .map(|s| Annotation {
                start: s.start_time,
                end: s.end_time,
                speaker: s.speaker.to_string(),
            })
            .collect();
        let der = scoring::score_der(&reference, &hypothesis, 0., duration)?;
        let mut result = serde_json::json!({"bitrate":bitrate,"audio_seconds":duration,"processing_seconds":processing_seconds,"rtf":processing_seconds/duration,"first_result_audio_seconds":first_audio,"first_result_compute_seconds":first_compute,"peak_memory_bytes":rss(),"der":der,"opus":opus_receipt,"segments":segments});
        let started = Instant::now();
        let mut by_name = BTreeMap::new();
        let mut latencies = vec![];
        for (name, windows) in &indexed {
            let mut samples = vec![];
            for window in windows {
                let audio = window_audio(&audio, window);
                let now = Instant::now();
                samples.push(encoder.embed(&audio)?);
                latencies.push(now.elapsed().as_secs_f64());
            }
            by_name.insert(name.clone(), samples);
        }
        result["identity_embedding_seconds"] = serde_json::json!(started.elapsed().as_secs_f64());
        result["embedding_mean_seconds"] =
            serde_json::json!(latencies.iter().sum::<f64>() / latencies.len() as f64);
        result["embedding_count"] = serde_json::json!(latencies.len());
        std::fs::write(
            models.join(format!("condition-{bitrate}.json")),
            serde_json::to_vec_pretty(&result)?,
        )?;
        println!(
            "condition {bitrate}: DER {:.3}%, RTF {:.5}, {} embeddings",
            der.der_percent,
            processing_seconds / duration,
            latencies.len()
        );
        embeds.insert(bitrate, by_name);
        conditions.push(result);
    }
    // Two enrolment windows/person/codec; next two are calibration, all later windows are test.
    // Codec copies of the same source interval are never treated as independent source windows.
    let mut store = EnrollmentStore::default();
    for bitrate in [24000, 64000] {
        for (name, values) in &embeds[&bitrate] {
            for e in &values[..2] {
                store.enroll(name, e.clone())?;
            }
        }
    }
    let mut trials = vec![];
    for bitrate in [24000, 64000] {
        for (name, values) in &embeds[&bitrate] {
            for query in &values[2..4] {
                let same = store.names[name]
                    .embeddings
                    .iter()
                    .map(|e| e.cosine(query))
                    .max_by(f32::total_cmp)
                    .unwrap();
                let different = store
                    .names
                    .iter()
                    .filter(|(n, _)| *n != name)
                    .flat_map(|(_, v)| v.embeddings.iter())
                    .map(|e| e.cosine(query))
                    .max_by(f32::total_cmp)
                    .unwrap();
                trials.push(Trial {
                    score: same,
                    same_speaker: true,
                });
                trials.push(Trial {
                    score: different,
                    same_speaker: false,
                });
            }
        }
    }
    let calibration=Calibration::fit(&trials,"AMI ES2004a SDM 0–480s, only_words manual RTTM; 2 disjoint enrolment + 2 calibration windows per speaker, 3s clean speech/window; Opus 24/64kbps; codec copies share source intervals")?;
    store.calibration = Some(calibration.clone());
    store.save(&models.join("evaluation-enrollments.json"))?;
    std::fs::write(
        models.join("calibration.json"),
        serde_json::to_vec_pretty(&calibration)?,
    )?;
    let mut identity_results = vec![];
    for bitrate in [0, 24000, 64000] {
        let mut correct = 0usize;
        let mut total = 0usize;
        let mut unknown_correct = 0usize;
        let mut per_speaker = BTreeMap::new();
        let mut evidence = vec![];
        for (name, values) in &embeds[&bitrate] {
            let mut outside = store.clone();
            outside.names.remove(name);
            let mut n_correct = 0;
            let mut n = 0;
            for query in &values[4..] {
                let predicted = store.identify(query);
                let unknown = outside.identify(query);
                let right = matches!(&predicted,Identity::Known{name:got,..} if got==name);
                let reject = matches!(unknown, Identity::Unknown { .. });
                correct += usize::from(right);
                unknown_correct += usize::from(reject);
                total += 1;
                n += 1;
                n_correct += usize::from(right);
                evidence.push(serde_json::json!({"reference":name,"predicted":predicted,"leave_one_speaker_out":unknown}));
            }
            per_speaker.insert(
                name.clone(),
                serde_json::json!({"correct":n_correct,"total":n}),
            );
        }
        identity_results.push(serde_json::json!({"bitrate":bitrate,"known_correct":correct,"known_total":total,"known_accuracy_percent":correct as f64*100./total as f64,"unknown_correct":unknown_correct,"unknown_total":total,"unknown_rejection_percent":unknown_correct as f64*100./total as f64,"per_speaker":per_speaker,"trials":evidence}));
    }
    let receipt = serde_json::json!({"date":"2026-10-11","hardware":"Apple M5 Max, busy shared Mac","dataset":"AMI ES2004a Array1-01, SDM, first 480 seconds, four speakers","reference":"pyannote/AMI-diarization-setup only_words/rttms/test/ES2004a.rttm; manual word annotation","collar_seconds":0,"overlap_scored":true,"uem":"explicit [0,480) crop","model":"Nemotron 3 Q8_0 + ReDimNet2-B6 vb2+vox2 LM ONNX FP32 CPU","geometry":{"chunk":3,"right_context":1,"left_context":0,"fifo":264,"speaker_cache":264,"update_period":222,"native_frame_seconds":diar.seconds_per_frame},"load_seconds":load_seconds,"conditions":conditions,"calibration":calibration,"calibration_trials":trials,"identity":identity_results,"peak_memory_bytes":rss(),"published_baselines":{"nemotron_ami_sdm_der_percent":12.95,"nemotron_dihard3_der_percent":13.55,"nemotron_input_buffer_seconds":0.32,"redimnet_clean_voxceleb_eer_percent":[0.23,0.35,0.67],"note":"Publisher Nemotron uses full benchmark, forced-alignment labels, uncompressed input, bf16 NeMo; our crop/manual words/Opus/Q8 native path differs. Identity accuracy and EER are different metrics; no published AMI/Opus identification accuracy was found."}});
    std::fs::write(
        models.join("evaluation.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("calibration threshold {:.6}; identity {}",calibration.threshold,serde_json::to_string(&identity_results.iter().map(|x|serde_json::json!({"bitrate":x["bitrate"],"accuracy_percent":x["known_accuracy_percent"],"unknown_rejection_percent":x["unknown_rejection_percent"]})).collect::<Vec<_>>())?);
    Ok(())
}
