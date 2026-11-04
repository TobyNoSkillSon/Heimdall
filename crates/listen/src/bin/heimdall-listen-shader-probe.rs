use anyhow::Result;
use std::path::PathBuf;
fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("decoder-profile") {
        println!(
            "{}",
            listen::metal::profile_decoder(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/decoder-static"),
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("decoder") {
        println!(
            "{}",
            listen::metal::compare_decoder(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/decoder-static")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("turn-static") {
        let audio = listen::file::read_wav(&root.join("crates/listen/tests/fixtures/en-2.wav"))?;
        println!(
            "{}",
            listen::metal::compare_turn(
                &root.join("models/smart-turn/owner-turn.onnx"),
                &audio,
                &root.join("models/reference/shader-listen/turn-static")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("daemon-vad") {
        use std::io::{self, BufRead, Write};
        let mut model = listen::onnx::Silero::load(&root.join("models/silero/silero_vad.onnx"))?;
        let audio = listen::file::read_wav(&root.join("crates/listen/tests/fixtures/en-2.wav"))?;
        let batched = std::env::var("HEIMDALL_VAD_BATCH").as_deref() == Ok("1");
        println!("{{\"ready\":true}}");
        io::stdout().flush()?;
        for line in io::stdin().lock().lines() {
            let r: serde_json::Value = serde_json::from_str(&line?)?;
            if let Some(s) = r["seconds"].as_f64() {
                std::thread::sleep(std::time::Duration::from_secs_f64(s));
                println!("{{\"idle\":true}}");
            } else {
                let count = r["count"].as_u64().unwrap_or(64);
                let streams = r["streams"].as_u64().unwrap_or(6) as usize;
                anyhow::ensure!((1..=32).contains(&streams), "VAD streams");
                let mut states = vec![listen::onnx::VadState::default(); streams];
                let start = std::time::Instant::now();
                for tick in 0..count {
                    let chunks: Vec<[f32; 512]> = (0..streams)
                        .map(|i| {
                            let off = (tick as usize * 512 + i * 1024) % (audio.len() - 512);
                            audio[off..off + 512].try_into().unwrap()
                        })
                        .collect();
                    if batched {
                        model.probabilities(&chunks, &mut states)?;
                    } else {
                        for (a, s) in chunks.iter().zip(&mut states) {
                            model.probability(a, s)?;
                        }
                    }
                }
                println!(
                    "{}",
                    serde_json::json!({"work_seconds":start.elapsed().as_secs_f64(),"decisions":count,"streams":streams})
                );
            }
            io::stdout().flush()?;
        }
    } else if args.get(1).map(String::as_str) == Some("daemon-turn") {
        use std::io::{self, BufRead, Write};
        let mut model =
            listen::onnx::SmartTurn::load(&root.join("models/smart-turn/owner-turn.onnx"))?;
        let audio = listen::file::read_wav(&root.join("crates/listen/tests/fixtures/en-2.wav"))?;
        println!("{{\"ready\":true}}");
        io::stdout().flush()?;
        for line in io::stdin().lock().lines() {
            let r: serde_json::Value = serde_json::from_str(&line?)?;
            if let Some(s) = r["seconds"].as_f64() {
                std::thread::sleep(std::time::Duration::from_secs_f64(s));
                println!("{{\"idle\":true}}");
            } else {
                let start = std::time::Instant::now();
                let mut p = 0.;
                let count = r["count"].as_u64().unwrap_or(1);
                for _ in 0..count {
                    p = model.probability(&audio)?;
                }
                println!(
                    "{}",
                    serde_json::json!({"work_seconds":start.elapsed().as_secs_f64(),"decisions":count,"probability":p})
                );
            }
            io::stdout().flush()?;
        }
    } else if args.get(1).map(String::as_str) == Some("encoder-batch") {
        println!(
            "{}",
            listen::metal::compare_encoder_batch(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/encoder-en-2")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("encoder-profile") {
        println!(
            "{}",
            listen::metal::profile_encoder(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/encoder-en-2")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("encoder-isolated") {
        println!(
            "{}",
            listen::metal::compare_encoder_isolated(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/encoder-en-2")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("encoder-layers") {
        println!(
            "{}",
            listen::metal::compare_encoder_layers(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/encoder-en-2")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("encoder-finite") {
        let id = args.get(2).map(String::as_str).unwrap_or("mixed");
        let audio =
            listen::file::read_wav(&root.join(format!("crates/listen/tests/fixtures/{id}.wav")))?;
        println!(
            "{}",
            listen::metal::encoder_finite(
                &root.join("models/parakeet-ultra"),
                &audio[..audio.len().min(240000)]
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("encoder") {
        let audio = listen::file::read_wav(&root.join("crates/listen/tests/fixtures/en-2.wav"))?;
        println!(
            "{}",
            listen::metal::compare_encoder(
                &root.join("models/parakeet-ultra"),
                &audio,
                &root.join("models/reference/shader-listen/encoder-en-2")
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("dump") {
        let name = std::env::var("HEIMDALL_METAL_CAPTURE_TENSOR")?;
        println!(
            "{}",
            listen::metal::inspect_encoder(
                &root.join("models/parakeet-ultra"),
                &root.join("models/reference/shader-listen/encoder-en-2"),
                &name
            )?
        );
    } else if args.get(1).map(String::as_str) == Some("turn") {
        let path = root.join("models/smart-turn/owner-turn.onnx");
        let mut cpu = listen::onnx::SmartTurn::load_backend(&path, "cpu")?;
        let mut gpu = listen::onnx::SmartTurn::load_backend(&path, "metal")?;
        let mut rows = vec![];
        for id in ["en-1", "en-2", "pl-1", "pl-2"] {
            let full = listen::file::read_wav(
                &root.join(format!("crates/listen/tests/fixtures/{id}.wav")),
            )?;
            for fraction in [0.25, 0.5, 1.0] {
                let audio = &full[..(full.len() as f64 * fraction) as usize];
                cpu.probability(audio)?;
                gpu.probability(audio)?;
                for arm in ["cpu", "metal", "metal", "cpu"] {
                    let start = std::time::Instant::now();
                    let p = if arm == "cpu" {
                        cpu.probability(audio)?
                    } else {
                        gpu.probability(audio)?
                    };
                    rows.push(serde_json::json!({"id":id,"fraction":fraction,"arm":arm,"seconds":start.elapsed().as_secs_f64(),"probability":p}));
                }
            }
        }
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        let a = listen::file::read_wav(&root.join("crates/listen/tests/fixtures/en-2.wav"))?;
        let mut vad = listen::onnx::Silero::load(&root.join("models/silero/silero_vad.onnx"))?;
        let mut turn =
            listen::onnx::SmartTurn::load(&root.join("models/smart-turn/owner-turn.onnx"))?;
        let mut states = vec![listen::onnx::VadState::default(); 6];
        for state in &mut states {
            vad.probability(&a[..512].try_into()?, state)?;
        }
        turn.probability(&a)?;
        let mut rows = vec![];
        for streams in [1, 3, 6] {
            let start = std::time::Instant::now();
            let steps = 32;
            for i in 0..steps {
                let chunk = a[(i * 512) % (a.len() - 512)..][..512].try_into()?;
                for state in &mut states[..streams] {
                    vad.probability(chunk, state)?;
                }
            }
            rows.push(serde_json::json!({"model":"silero","streams":streams,"seconds_per_tick":start.elapsed().as_secs_f64()/steps as f64}));
        }

        for streams in [1, 3, 6] {
            let mut states = vec![listen::onnx::VadState::default(); streams];
            let mut singles = states.clone();
            let mut max_error = 0f32;
            let start = std::time::Instant::now();
            let mut seconds = 0.;
            for i in 0..32 {
                let chunk: [f32; 512] = a[(i * 512) % (a.len() - 512)..][..512].try_into()?;
                let batch = vec![chunk; streams];
                let t = std::time::Instant::now();
                let p = vad.probabilities(&batch, &mut states)?;
                seconds += t.elapsed().as_secs_f64();
                for j in 0..streams {
                    let q = vad.probability(&chunk, &mut singles[j])?;
                    max_error = max_error.max((p[j] - q).abs());
                }
            }
            rows.push(serde_json::json!({"model":"silero_batch_cpu","streams":streams,"seconds_per_tick":seconds/32.,"max_abs_probability":max_error,"probe_seconds":start.elapsed().as_secs_f64()}));
        }
        let start = std::time::Instant::now();
        let mut probabilities = vec![];
        for _ in 0..4 {
            probabilities.push(turn.probability(&a)?);
        }
        rows.push(serde_json::json!({"model":"tuned_smart_turn","seconds_per_decision":start.elapsed().as_secs_f64()/4.,"probabilities":probabilities}));
        println!("{}", serde_json::to_string(&rows)?);
    }
    Ok(())
}
