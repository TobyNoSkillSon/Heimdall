//! Offline API/latency probe. Agent-operated only; no microphone or permission prompts.
use anyhow::{Context, Result};
use decide::*;
use serde_json::json;
use std::{fs::File, io::Write, path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = crate_root.parent().unwrap().parent().unwrap();
    let models = root.join("models/decide");
    let args: Vec<_> = std::env::args().collect();
    let op = args.get(1).map(|s| s.as_str()).unwrap_or("probe");
    let audio = AudioClip::from_wav(crate_root.join("tests/fixtures/en-1.wav"))?;
    let input = DecisionInput {
        source: DecisionSource::Microphone,
        is_final: true,
        current: Turn {
            id: "probe".into(),
            role: Role::Owner,
            transcript: "Heimdall, please open the first link now.".into(),
            audio,
        },
        context: vec![],
        pending_transcript: String::new(),
        agent_state: "Waiting for the user's next complete request.".into(),
        media: DecisionMedia::Text,
        questions: vec![
            Question::SendNow,
            Question::FixedCommand,
            Question::PickCandidate(vec![
                Candidate {
                    id: "first".into(),
                    description: "First link: Rust documentation".into(),
                },
                Candidate {
                    id: "second".into(),
                    description: "Second link: a recipe".into(),
                },
            ]),
        ],
    };
    if op == "live-speed-requests" {
        let mut file = File::create(args.get(2).context("live requests output path required")?)?;
        let mut cases = Vec::new();
        for (name, text, question) in [
            ("text-command", "Look at this.", Question::CommandCheck(CommandSelection::LookAtThis)),
            ("text-quotation", "The command look at this is just an example.", Question::CommandCheck(CommandSelection::LookAtThis)),
            ("text-send", "Please fix the menu and keep the current colors.", Question::SendNow),
            ("text-barge", "Wait, no, the other file.", Question::BargeIn),
        ] {
            let mut i = input.clone();
            i.current.transcript = text.into();
            i.pending_transcript = i.current.transcript.clone();
            i.questions = vec![question];
            i.current.audio = AudioClip::new(vec![0.0])?;
            cases.push((name.to_string(), i));
        }
        let mut reply = input.clone();
        reply.questions = vec![Question::ReplyLine(vec![
            Candidate { id: "none".into(), description: "Say nothing.".into() },
            Candidate { id: "sure".into(), description: "Sure.".into() },
        ])];
        reply.current.audio = AudioClip::new(vec![0.0])?;
        cases.push(("text-reply".into(), reply));
        let mut audio = input.clone();
        audio.questions = vec![Question::SendNow];
        audio.media = DecisionMedia::Audio;
        audio.pending_transcript = audio.current.transcript.clone();
        cases.push(("audio-send-11s".into(), audio.clone()));
        let mut microphone = audio.clone();
        microphone.questions.push(Question::FixedCommand);
        cases.push(("mic-two-11s".into(), microphone));
        let mut longest = audio.clone();
        longest.current.audio.samples = audio.current.audio.samples.iter().copied().cycle().take(480000).collect();
        longest.questions.push(Question::FixedCommand);
        cases.push(("mic-two-30s".into(), longest));
        audio.current.audio.samples = audio.current.audio.samples[audio.current.audio.samples.len().saturating_sub(48000)..].to_vec();
        let mut microphone = audio.clone();
        microphone.questions.push(Question::FixedCommand);
        cases.push(("mic-two-3s".into(), microphone));
        cases.push(("audio-send-3s".into(), audio));
        for (name, i) in cases {
            writeln!(file, "{}", json!({"name":name, "input":i, "request":D1::request(&i, 0.0)?}))?;
        }
        for clip in ["en-2", "pl-1", "sigma", "human"] {
            let mut i = input.clone();
            i.media = DecisionMedia::Audio;
            i.current.audio = AudioClip::from_wav(crate_root.join("tests/fixtures").join(format!("{clip}.wav")))?;
            i.current.audio.samples.truncate(480000);
            i.questions = vec![Question::SendNow, Question::FixedCommand];
            writeln!(file, "{}", json!({"name":format!("audio-gate-{clip}"), "timing":false, "input":i, "request":D1::request(&i, 0.0)?}))?;
        }
        return Ok(());
    }
    if op == "speed-requests" {
        let mut file = File::create(args.get(2).context("speed requests output path required")?)?;
        for (name, count, media, tail_samples) in [
            ("text-single", 1, DecisionMedia::Text, None),
            ("audio-single-11s", 1, DecisionMedia::Audio, None),
            ("mic-two-11s", 2, DecisionMedia::Audio, None),
            ("mic-two-3s", 2, DecisionMedia::Audio, Some(48000)),
            ("mic-three-11s", 3, DecisionMedia::Audio, None),
        ] {
            let mut i = input.clone();
            i.pending_transcript = i.current.transcript.clone();
            i.questions.truncate(count);
            i.media = media;
            if let Some(samples) = tail_samples {
                i.current.audio.samples = i.current.audio.samples
                    [i.current.audio.samples.len().saturating_sub(samples)..]
                    .to_vec();
            }
            writeln!(
                file,
                "{}",
                json!({"name": name, "input": i, "request": D1::request(&i, 0.95)?})
            )?;
        }
        return Ok(());
    }
    if op == "requests" {
        let mut file = File::create(args.get(2).context("requests output path required")?)?;
        for (name, transcript, media) in [
            (
                "text-request",
                "Heimdall, please open the first link now.",
                DecisionMedia::Text,
            ),
            (
                "text-conversation",
                "Alex, are you coming to dinner tonight?",
                DecisionMedia::Text,
            ),
            (
                "audio-request",
                "Heimdall, please open the first link now.",
                DecisionMedia::Audio,
            ),
            (
                "audio-conversation",
                "Alex, are you coming to dinner tonight?",
                DecisionMedia::Audio,
            ),
            (
                "image-request",
                "Heimdall, please inspect this screenshot now.",
                DecisionMedia::Image(Image {
                    mime: "image/png".into(),
                    bytes: std::fs::read(crate_root.join("tests/fixtures/screen.png"))?,
                }),
            ),
        ] {
            let mut i = input.clone();
            i.current.transcript = transcript.into();
            i.media = media;
            let mut c = json!({"name":name,"input":i,"addressed_probability":0.95,"request":D1::request(&i,0.95)?});
            match i.media {
                DecisionMedia::Audio => c["audio"] = json!("en-1.wav"),
                DecisionMedia::Image(_) => c["image"] = json!("screen.png"),
                _ => {}
            };
            writeln!(file, "{}", serde_json::to_string(&c)?)?;
        }
        return Ok(());
    }
    let out = root.join("models/decide/reference/probe");
    std::fs::create_dir_all(&out)?;
    let baseline_rss = rss_bytes()?;
    let baseline_idle = idle_cpu();
    let start = Instant::now();
    let mut decider = LocalDecider::load(&models, out.join("archive"))?;
    let load_ms = start.elapsed().as_secs_f64() * 1000.;
    let resident_rss = rss_bytes()?;
    let resident_idle = idle_cpu();
    let mut runs = Vec::new();
    for question_count in [1, 3] {
        for media in [
            DecisionMedia::Text,
            DecisionMedia::Audio,
            DecisionMedia::Image(Image {
                mime: "image/png".into(),
                bytes: std::fs::read(crate_root.join("tests/fixtures/screen.png"))?,
            }),
        ] {
            let mut i = input.clone();
            i.media = media;
            if question_count == 1 {
                i.questions = vec![Question::SendNow];
            }
            let _ = decider.decide(&i)?;
            for n in 0..3 {
                let d = decider.decide(&i)?;
                runs.push(json!({"question_count":question_count,"media":match i.media {DecisionMedia::Text=>"text",DecisionMedia::Audio=>"audio",_=>"image"},"run":n,"decision":d}));
            }
        }
    }
    let warmed_rss = rss_bytes()?;
    let peak_bytes = peak_bytes();
    let receipt = json!({"hardware":"Apple M5 Max, 128 GB, busy machine","baseline_rss_bytes":baseline_rss,"resident_rss_bytes":resident_rss,"warmed_rss_bytes":warmed_rss,"process_peak_rss_bytes":peak_bytes,"unloaded_idle":baseline_idle,"resident_idle":resident_idle,"load_ms":load_ms,"runs":runs});
    println!("{}", serde_json::to_string(&receipt)?);
    std::fs::write(
        out.join("timing.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(())
}

fn rss_bytes() -> Result<u64> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()?;
    Ok(String::from_utf8(out.stdout)?.trim().parse::<u64>()? * 1024)
}
fn usage() -> libc::rusage {
    unsafe {
        let mut usage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    }
}
fn cpu_seconds() -> f64 {
    let r = usage();
    r.ru_utime.tv_sec as f64
        + r.ru_stime.tv_sec as f64
        + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
}
fn peak_bytes() -> u64 {
    usage().ru_maxrss as u64
} // bytes on macOS
fn idle_cpu() -> serde_json::Value {
    let start = Instant::now();
    let cpu = cpu_seconds();
    std::thread::sleep(std::time::Duration::from_secs(3));
    json!({"wall_seconds":start.elapsed().as_secs_f64(),"cpu_seconds":cpu_seconds()-cpu})
}
