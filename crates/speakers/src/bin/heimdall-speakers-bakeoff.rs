//! Agent-run causal replay. Inputs/outputs are private QA; never captures or grants permissions.
use anyhow::{Result, ensure};
use speakers::{
    live::LiveStrategy,
    online::OnlineConfig,
    pipeline::Diarization,
    scoring::{Annotation, parse_rttm, score_der},
};
use std::{path::PathBuf, time::Instant};
fn masks(segments: &[speakers::diar::SpeakerSegment], frames: usize) -> Vec<u8> {
    let mut out = vec![0; frames];
    for s in segments {
        if (1..=8).contains(&s.speaker) {
            let a = (s.start_time * 100.).round().max(0.) as usize;
            let b = (s.end_time * 100.).round().max(0.) as usize;
            for v in &mut out[a.min(frames)..b.min(frames)] {
                *v |= 1 << (s.speaker - 1);
            }
        }
    }
    out
}
#[derive(serde::Deserialize)]
struct Packet {
    at_audio_seconds: f64,
    through_audio_seconds: f64,
    segments: Vec<speakers::diar::SpeakerSegment>,
}
struct Replay {
    packets: Vec<Packet>,
    next: usize,
    samples: usize,
    frames: i64,
    segments: Vec<speakers::diar::SpeakerSegment>,
    finished: bool,
}
impl Diarization for Replay {
    fn sample_rate(&self) -> u32 {
        16000
    }
    fn speakers(&self) -> usize {
        8
    }
    fn seconds_per_frame(&self) -> f64 {
        0.01
    }
    fn frame_count(&self) -> i64 {
        self.frames
    }
    fn push_f32(&mut self, audio: &[f32]) -> Result<()> {
        ensure!(!self.finished, "finished replay");
        self.samples += audio.len();
        while self.next < self.packets.len()
            && self.packets[self.next].at_audio_seconds <= self.samples as f64 / 16000. + 1e-8
        {
            let p = &self.packets[self.next];
            self.frames = (p.through_audio_seconds * 100.).round() as i64;
            self.segments = p.segments.clone();
            self.next += 1;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        ensure!(
            self.next == self.packets.len(),
            "unreleased future replay packet"
        );
        Ok(())
    }
    fn segments(&self) -> Result<Vec<speakers::diar::SpeakerSegment>> {
        Ok(self.segments.clone())
    }
}
fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1e6
}
fn gpu_seconds() -> f64 {
    #[cfg(target_os = "macos")]
    {
        speakers::metal::gpu_seconds()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0.
    }
}
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        a.len() >= 5,
        "models wav rttm output strategy [seconds] [window] [threshold]"
    );
    let models = PathBuf::from(&a[0]);
    let strategy = a[4].as_str();
    let limit = a
        .get(5)
        .map(|x| x.parse::<f64>())
        .transpose()?
        .unwrap_or(f64::MAX);
    let window = a
        .get(6)
        .map(|x| x.parse::<f64>())
        .transpose()?
        .unwrap_or(2.);
    let threshold = a
        .get(7)
        .map(|x| x.parse::<f32>())
        .transpose()?
        .unwrap_or(0.455542);
    let mut wav = hound::WavReader::open(&a[1])?;
    let spec = wav.spec();
    ensure!(
        spec.sample_rate == 16000 && spec.channels == 1 && spec.bits_per_sample == 16,
        "requires mono PCM16 16kHz"
    );
    let audio: Vec<_> = wav
        .samples::<i16>()
        .take((limit * 16000.).min(usize::MAX as f64) as usize)
        .map(|x| x.map(|x| x as f32 / 32768.))
        .collect::<std::result::Result<_, _>>()?;
    let duration = audio.len() as f64 / 16000.;
    let frames = audio.len().div_ceil(160);
    let load = Instant::now();
    let store = if let Some(p) = a.get(8) {
        speakers::identity::EnrollmentStore::load(&PathBuf::from(p))?
    } else {
        speakers::identity::EnrollmentStore::default()
    };
    let stream: Box<dyn Diarization> = if strategy == "replay" {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(
            a.get(9)
                .ok_or_else(|| anyhow::anyhow!("replay requires a packet file"))?,
        )?)?;
        let packets: Vec<Packet> = serde_json::from_value(value["packets"].clone())?;
        ensure!(!packets.is_empty(), "empty causal replay");
        for (i, p) in packets.iter().enumerate() {
            ensure!(
                p.at_audio_seconds.is_finite()
                    && p.through_audio_seconds.is_finite()
                    && p.at_audio_seconds >= p.through_audio_seconds
                    && p.through_audio_seconds >= 0.
                    && p.at_audio_seconds <= duration + 1e-6,
                "invalid causal packet"
            );
            if i > 0 {
                ensure!(
                    p.at_audio_seconds >= packets[i - 1].at_audio_seconds
                        && p.through_audio_seconds >= packets[i - 1].through_audio_seconds,
                    "packet time reversal"
                );
            }
            ensure!(
                p.segments.iter().all(|s| (1..=8).contains(&s.speaker)
                    && s.start_time >= 0.
                    && s.end_time <= p.through_audio_seconds + 1e-6
                    && s.end_time > s.start_time),
                "invalid replay segment"
            );
        }
        Box::new(Replay {
            packets,
            next: 0,
            samples: 0,
            frames: 0,
            segments: vec![],
            finished: false,
        })
    } else {
        LiveStrategy::parse(strategy)?.load_enrolled(
            &models,
            0,
            OnlineConfig {
                window_seconds: window,
                cosine_threshold: threshold,
                ..Default::default()
            },
            Some(&store),
        )?
    };
    let encoder = speakers::embedding::ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?;
    let mut pipeline = speakers::pipeline::SpeakerPipeline::new(
        stream,
        encoder,
        speakers::naming::NamingFlow::new(store),
        "eval".into(),
        1,
        0.,
    )?;
    let load_seconds = load.elapsed().as_secs_f64();
    let gpu_before = gpu_seconds();
    let mut cpu = 0.;
    let mut previous = vec![0_u8; frames];
    let mut changed = vec![0_f64; frames];
    let mut first = None;
    let benchmark = std::env::var_os("HEIMDALL_SPEAKERS_BENCHMARK").is_some();
    let measurement_start_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let start = Instant::now();
    let mut compute = 0.;
    let mut total = 0;
    let mut named_at = std::collections::BTreeMap::new();
    // Feed 20ms chunks, sample revisions at each 0.48s of supplied audio. The
    // tracing/scoring cost is excluded from inference RTF, and explicitly reported.
    for block in audio.chunks(7680) {
        let c = cpu_seconds();
        let t = Instant::now();
        for chunk in block.chunks(320) {
            let events = pipeline.push(chunk)?;
            total += chunk.len();
            for e in events {
                if let speakers::naming::SpeakerEvent::LabelChanged { track, .. } = e {
                    named_at.insert(track.speaker, total as f64 / 16000.);
                }
            }
            if first.is_none() && pipeline.stream_ref().frame_count() > 0 {
                first = Some(total as f64 / 16000.);
            }
        }
        compute += t.elapsed().as_secs_f64();
        cpu += cpu_seconds() - c;
        if benchmark {
            continue;
        }
        let current = masks(&pipeline.segments()?, frames);
        for i in 0..frames {
            if current[i] != previous[i] {
                changed[i] = total as f64 / 16000.;
            }
        }
        previous = current;
    }
    let c = cpu_seconds();
    let t = Instant::now();
    pipeline.finish()?;
    compute += t.elapsed().as_secs_f64();
    cpu += cpu_seconds() - c;
    let measurement_end_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let segments = pipeline.segments()?;
    let current = masks(&segments, frames);
    for i in 0..frames {
        if current[i] != previous[i] {
            changed[i] = duration;
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let reference = parse_rttm(&std::fs::read_to_string(&a[2])?)?;
    let hypothesis: Vec<_> = segments
        .iter()
        .map(|s| Annotation {
            start: s.start_time,
            end: s.end_time,
            speaker: s.speaker.to_string(),
        })
        .collect();
    let der = score_der(&reference, &hypothesis, 0., duration)?;
    let mut latencies: Vec<_> = (0..frames)
        .filter(|i| current[*i] != 0)
        .map(|i| (changed[i] - i as f64 / 100.).max(0.))
        .collect();
    latencies.sort_by(f64::total_cmp);
    let p95 = latencies
        .get((latencies.len() as f64 * 0.95).floor() as usize)
        .copied()
        .filter(|_| !benchmark);
    let mean = if benchmark || latencies.is_empty() {
        None
    } else {
        Some(latencies.iter().sum::<f64>() / latencies.len() as f64)
    };
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let peak_bytes = if cfg!(target_os = "macos") {
        u.ru_maxrss as u64
    } else {
        u.ru_maxrss as u64 * 1024
    };
    let mut track_names = std::collections::BTreeMap::new();
    for speaker in 1..=pipeline.stream_ref().speakers() as i32 {
        let track = speakers::naming::TrackId {
            source: "eval".into(),
            session: 1,
            speaker,
        };
        let name = pipeline.naming.label(&track);
        if name != format!("Speaker {speaker}") {
            track_names.insert(speaker, name);
        }
    }
    let result = serde_json::json!({"build_profile":if cfg!(debug_assertions) { "debug" } else { "release" },"strategy":strategy,"window_seconds":window,"cosine_threshold":threshold,"audio_seconds":duration,"load_seconds":load_seconds,"processing_seconds":compute,"measurement_start_unix":measurement_start_unix,"measurement_end_unix":measurement_end_unix,"benchmark_without_trace":benchmark,"cpu_seconds":cpu,"gpu_seconds":gpu_seconds()-gpu_before,"trace_wall_seconds":wall,"rtf":compute/duration,"peak_memory_bytes":peak_bytes,"first_result_audio_seconds":first,"stable_label_mean_seconds":mean,"stable_label_p95_seconds":p95,"latency_definition":"last sampled change of final active-speaker set minus frame start; 0.48s trace resolution; includes initially missing labels and later revisions, excludes compute; wrong final labels also count as stable","der":der,"segments":segments,"track_names":track_names,"named_at_audio_seconds":named_at,"naming_protocol":"actual Rust SpeakerPipeline and NamingFlow, twelve seconds predicted clean speech, latest six-second query against opposite-session enrollments, final retroactive track names"});
    std::fs::write(&a[3], serde_json::to_vec(&result)?)?;
    println!(
        "{}",
        serde_json::json!({"strategy":strategy,"audio_seconds":duration,"der_percent":der.der_percent,"rtf":compute/duration,"stable_p95_seconds":p95,"peak_mib":peak_bytes as f64/1048576.})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prototype_replay_never_releases_future_labels() {
        let mut replay = Replay {
            packets: vec![Packet {
                at_audio_seconds: 2.,
                through_audio_seconds: 1.,
                segments: vec![speakers::diar::SpeakerSegment {
                    start_time: 0.,
                    end_time: 1.,
                    speaker: 1,
                }],
            }],
            next: 0,
            samples: 0,
            frames: 0,
            segments: vec![],
            finished: false,
        };
        replay.push_f32(&vec![0.; 16000]).unwrap();
        assert_eq!(replay.frame_count(), 0);
        assert!(replay.segments().unwrap().is_empty());
        replay.push_f32(&vec![0.; 16000]).unwrap();
        assert_eq!(replay.frame_count(), 100);
        assert_eq!(replay.segments().unwrap().len(), 1);
        replay.finish().unwrap();
        replay.finish().unwrap();
        assert!(replay.push_f32(&[0.]).is_err());
    }
}
