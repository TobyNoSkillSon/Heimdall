//! Permission-free resident VAD/turn probe; JSONL input, no audio devices or app state.
use anyhow::{ensure, Result};
use listen::{
    features::WhisperFeatures,
    onnx::{Silero, SmartTurn, VadState},
};
use ort::{
    ep::{self, ExecutionProvider},
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

mod support {
    pub mod process_cost;
}
use support::process_cost::{gpu, usage};
fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("vad");
    if mode == "providers" {
        println!(
            "{}",
            json!({"coreml":ep::CoreML::default().is_available()?,"cpu":ep::CPU::default().is_available()?})
        );
        return Ok(());
    }
    let path = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("models/smart-turn/owner-turn.onnx"));
    let backend = args.get(3).map(String::as_str).unwrap_or("cpu");
    let accelerated = matches!(backend, "coreml-gpu" | "coreml-ane");
    // Directly converted ML Program (.mlmodelc): mlmodel-cpu|gpu|all|ane.
    let mut mlmodel = match backend.strip_prefix("mlmodel-") {
        Some(u) => Some(listen::onnx::CoreMlTurn::load(
            &path,
            match u {
                "cpu" => 0,
                "gpu" => 1,
                "all" => 2,
                "ane" => 3,
                _ => anyhow::bail!("unknown Core ML units"),
            },
        )?),
        None => None,
    };
    let cpu_threads = backend
        .strip_prefix("cpu-")
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(1);
    ensure!((1..=4).contains(&cpu_threads), "CPU thread bounds");
    let direct_cpu = mode == "turn" && cpu_threads > 1;
    let specialized =
        mode == "vad" && path.file_name().and_then(|x| x.to_str()) == Some("silero-16k.onnx");
    let mut ep_session = if accelerated || specialized || direct_cpu {
        if accelerated {
            ensure!(
                ep::CoreML::default().is_available()?,
                "CoreML EP unavailable in linked runtime"
            );
        }
        let unit = if backend == "coreml-gpu" {
            ep::coreml::ComputeUnits::CPUAndGPU
        } else {
            ep::coreml::ComputeUnits::CPUAndNeuralEngine
        };
        let model = if mode == "vad" && !specialized {
            root.join("models/silero/silero_vad.onnx")
        } else {
            path.clone()
        };
        let mut builder = Session::builder()?;
        macro_rules! option {
            ($call:expr) => {
                $call.map_err(|e| anyhow::anyhow!(e.to_string()))?
            };
        }
        builder = option!(builder.with_intra_threads(cpu_threads));
        builder = option!(builder.with_inter_threads(1));
        builder = option!(builder.with_no_environment_execution_providers());
        builder = option!(builder.with_intra_op_spinning(false));
        builder = option!(builder.with_inter_op_spinning(false));
        builder = option!(builder.with_parallel_execution(false));
        builder = option!(builder.with_optimization_level(GraphOptimizationLevel::Level3));
        if accelerated {
            builder = option!(builder.with_execution_providers([ep::CoreML::default()
                .with_compute_units(unit)
                .with_model_format(ep::coreml::ModelFormat::MLProgram)
                .with_subgraphs(true)
                .with_profile_compute_plan(
                    std::env::var("HEIMDALL_SMALL_PROBE_PLAN").as_deref() == Ok("1")
                )
                .build()
                .error_on_failure()]));
        }
        Some(builder.commit_from_file(model)?)
    } else {
        None
    };
    let mut vad = if mode == "vad" && !accelerated && !specialized {
        Some(Silero::load(&root.join("models/silero/silero_vad.onnx"))?)
    } else {
        None
    };
    let mut turn = if mode == "turn" && !accelerated && !direct_cpu && mlmodel.is_none() {
        Some(if backend == "default" {
            SmartTurn::load(&path)?
        } else {
            SmartTurn::load_backend(&path, backend)?
        })
    } else {
        None
    };
    let frontend = WhisperFeatures::default();
    println!("{}", json!({"ready":true,"mode":mode,"backend":backend}));
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let r: Value = serde_json::from_str(&line?)?;
        let count = r["count"].as_u64().unwrap_or(1) as usize;
        ensure!((1..=200000).contains(&count), "count bounds");
        let audio_path = r["path"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("crates/listen/tests/fixtures/en-2.wav"));
        // Raw f32 PCM segments ([path, overlap_frames]) are read in place, unconverted.
        let full = match r["segments"].as_array() {
            Some(segments) => {
                let mut pcm = Vec::new();
                for s in segments {
                    let bytes = std::fs::read(s[0].as_str().unwrap_or_default())?;
                    ensure!(bytes.len() % 4 == 0, "truncated PCM");
                    let skip = s[1].as_u64().unwrap_or(0) as usize;
                    pcm.extend(
                        bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .skip(skip)
                            .map(|b| f32::from_le_bytes(*b)),
                    );
                }
                pcm
            }
            None => listen::file::read_wav(&audio_path)?,
        };
        // VAD parity over a whole recording with carried state: native vs ONNX Runtime.
        if mode == "vad-parity" {
            let model = root.join("models/silero/silero_vad.onnx");
            let (mut native, mut reference) = (
                Silero::load(&model)?,
                listen::onnx::OrtSilero::load(&model)?,
            );
            let (mut a, mut b) = (VadState::default(), VadState::default());
            let (mut max_abs, mut flips, mut frames, mut onsets) = (0_f32, 0, 0, [0, 0]);
            let mut speech = [false, false];
            for block in full.as_chunks::<512>().0 {
                let p = [
                    native.probability(block, &mut a)?,
                    reference.probability(block, &mut b)?,
                ];
                max_abs = max_abs.max((p[0] - p[1]).abs());
                flips += [0.35_f32, 0.4]
                    .iter()
                    .filter(|t| (p[0] >= **t) != (p[1] >= **t))
                    .count();
                for i in 0..2 {
                    // The engine's hysteresis: start at 0.4, continue at 0.35.
                    let next = p[i] >= if speech[i] { 0.35 } else { 0.4 };
                    onsets[i] += usize::from(next && !speech[i]);
                    speech[i] = next;
                }
                frames += 1;
            }
            println!(
                "{}",
                json!({"frames":frames,"max_abs":max_abs,"flips":flips,"onsets_native":onsets[0],"onsets_onnx":onsets[1]})
            );
            io::stdout().flush()?;
            continue;
        }
        // Quality-gate mode: one turn probability per end sample, zero-padded past EOF
        // (the held-out set places pauses after the recorded speech).
        if let Some(ends) = r["ends"].as_array() {
            let mut probabilities = Vec::with_capacity(ends.len());
            for end in ends {
                let end = end.as_u64().unwrap_or(0) as usize;
                let mut audio = full[..end.min(full.len())].to_vec();
                audio.resize(end, 0.0);
                probabilities.push(if let Some(m) = &mut mlmodel {
                    m.probability(&frontend.extract(&audio))?
                } else {
                    turn.as_mut().expect("turn backend").probability(&audio)?
                });
            }
            println!("{}", json!({ "probabilities": probabilities }));
            io::stdout().flush()?;
            continue;
        }
        let fraction = r["fraction"].as_f64().unwrap_or(1.);
        ensure!(fraction > 0. && fraction <= 1., "fraction bounds");
        let audio = &full[..(full.len() as f64 * fraction) as usize];
        ensure!(audio.len() >= 512, "audio too short");
        let mut state = VadState::default();
        let mut hidden = vec![0_f32; 256];
        let mut context = [0_f32; 64];
        let u0 = usage()?;
        let start = Instant::now();
        let mut times = Vec::with_capacity(count);
        let mut probabilities = Vec::new();
        let mut features = Vec::new();
        // Optional per-call GPU joules: equal idle windows before and after the call,
        // so system background GPU power cancels (the call must finish inside it).
        let window = r["gpu_window_ms"].as_u64().map(Duration::from_millis);
        let mut gpu_call_nj = Vec::new();
        for i in 0..count {
            ensure!(start.elapsed().as_secs() < 540, "benchmark run ceiling");
            let before_call = window.map(|w| {
                let a = gpu::energy_nj();
                std::thread::sleep(w);
                (a, gpu::energy_nj())
            });
            let tick = Instant::now();
            match mode {
                "vad" => {
                    let off = i * 512 % (audio.len() / 512 * 512);
                    let block: &[f32; 512] = audio[off..off + 512].try_into()?;
                    let p = if let Some(s) = &mut ep_session {
                        let mut input = context.to_vec();
                        input.extend_from_slice(block);
                        let out = if specialized {
                            s.run(ort::inputs!["input"=>Tensor::from_array(([1,576],input))?,"hidden"=>Tensor::from_array(([2,1,128],hidden.clone()))?])?
                        } else {
                            s.run(ort::inputs!["input"=>Tensor::from_array(([1,576],input))?,"state"=>Tensor::from_array(([2,1,128],hidden.clone()))?,"sr"=>Tensor::from_array((Vec::<usize>::new(),vec![16000_i64]))?])?
                        };
                        hidden.copy_from_slice(out["stateN"].try_extract_tensor::<f32>()?.1);
                        context.copy_from_slice(&block[448..]);
                        out["output"].try_extract_tensor::<f32>()?.1[0]
                    } else {
                        vad.as_mut().unwrap().probability(block, &mut state)?
                    };
                    probabilities.push(p);
                }
                "turn" => {
                    let p = if let Some(m) = &mut mlmodel {
                        m.probability(&frontend.extract(audio))?
                    } else if let Some(s) = &mut ep_session {
                        let features = frontend.extract(audio);
                        let out = s.run(ort::inputs!["input_features"=>Tensor::from_array(([1,80,800],features))?])?;
                        out[0].try_extract_tensor::<f32>()?.1[0]
                    } else {
                        turn.as_mut().unwrap().probability(audio)?
                    };
                    probabilities.push(p);
                }
                "features" => features = frontend.extract(audio),
                "noop" => probabilities.push(0.0), // paced wake-up floor, no model

                _ => anyhow::bail!("unknown mode"),
            }
            times.push(tick.elapsed().as_secs_f64());
            if let (Some(w), Some((Some(a), Some(b)))) = (window, before_call) {
                if let Some(wait) = (tick + w).checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                if let Some(c) = gpu::energy_nj() {
                    gpu_call_nj.push((c - b) as i64 - (b - a) as i64);
                }
            }
            if r["paced"].as_bool().unwrap_or(false) {
                let period = r["period_ms"].as_u64().unwrap_or(32);
                let deadline = start + Duration::from_millis((i as u64 + 1) * period);
                if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        }
        let u1 = usage()?;
        if let Some(p) = r["features_out"].as_str() {
            std::fs::write(
                p,
                features
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>(),
            )?;
        }
        println!(
            "{}",
            json!({"seconds":start.elapsed().as_secs_f64(),"times_s":times,"gpu_call_nj":gpu_call_nj,"probabilities":probabilities,"before":u0,"after":u1})
        );
        io::stdout().flush()?;
    }
    Ok(())
}
