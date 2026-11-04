//! Agent-operated lab probe. Independent live and verification owners. Never opens capture, UI or the installed profile. Playback is
//! opt-in per smoke request; ordinary calibration remains silent.
#[path = "../audio.rs"]
mod audio;
#[path = "../drama3_verify.rs"]
mod drama3_verify;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
struct Silent;
impl listen::SpeechDetector for Silent {
    fn probability(&mut self, _: &[f32; 512], _: &mut listen::onnx::VadState) -> Result<f32> {
        Ok(0.)
    }
}
impl listen::TurnDetector for Silent {
    fn probability(&mut self, _: &[f32]) -> Result<f32> {
        Ok(0.)
    }
}
fn wav(path: &Path) -> Result<speak::Audio> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    ensure!(spec.channels == 1, "mono lab WAV required");
    let samples = if spec.sample_format == hound::SampleFormat::Float {
        reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?
    } else {
        let scale = 2_f32.powi(spec.bits_per_sample as i32 - 1);
        reader
            .samples::<i32>()
            .map(|x| x.map(|x| x as f32 / scale))
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(speak::Audio {
        samples,
        sample_rate: spec.sample_rate,
        first_audio_ms: 0.,
        synthesis_ms: 0.,
    })
}
fn footprint() -> Result<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    ensure!(
        unsafe {
            libc::proc_pid_rusage(
                std::process::id() as i32,
                libc::RUSAGE_INFO_V2,
                info.as_mut_ptr().cast(),
            )
        } == 0,
        "physical footprint unavailable"
    );
    Ok(unsafe { info.assume_init() }.ri_phys_footprint)
}
fn main() -> Result<()> {
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let only = std::env::args().any(|x| x == "--verification-only");
    let assets = std::env::args()
        .skip(1)
        .find(|x| x != "--verification-only")
        .map(PathBuf::from);
    let service = assets.map(speak::drama3::Service::new);
    if let Some(service) = &service {
        service.configure(true);
        let at = Instant::now();
        while !service.ready() {
            ensure!(
                at.elapsed() < Duration::from_secs(48),
                "Drama load deadline: {:?}",
                service.status()
            );
            ensure!(
                !service.events().iter().any(|x| x["event"] == "failure"),
                "Drama unavailable: {:?}",
                service.status()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let worker = if only {
        None
    } else {
        Some(listen::ListenWorker::spawn(
            Default::default(),
            move || {
                let asr =
                    listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), "metal")?;
                listen::ListenEngine::new(asr, Silent, Silent, Default::default())
            },
        )?)
    };
    if let Some(worker) = &worker {
        worker.wait_ready()?;
        // Warm live decoding before taking the extra-decoder baseline.
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../listen/tests/fixtures/en-2.wav");
        worker.transcribe_side(99, 0, listen::file::read_wav(&fixture)?)?;
        loop {
            match worker.recv_event() {
                Some(listen::WorkerEvent::SideTranscript { result, .. }) => {
                    result.map_err(anyhow::Error::msg)?;
                    break;
                }
                Some(_) => {}
                None => anyhow::bail!("live worker stopped"),
            }
        }
    }
    // Let completed Metal warmup temporaries settle before measuring a copy.
    let mut previous = footprint()?;
    let mut stable = 0;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        let current = footprint()?;
        if current.abs_diff(previous) < 4 * 1024 * 1024 {
            stable += 1;
        } else {
            stable = 0;
        }
        previous = current;
        if stable >= 3 {
            break;
        }
    }
    let baseline = footprint()?;
    println!(
        "{}",
        json!({"phase":"live_ready","baseline_bytes":baseline})
    );
    io::stdout().flush()?;
    let verification = if let Some(service) = &service {
        drama3_verify::install(
            service,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/parakeet-ultra"),
        )?
    } else {
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/parakeet-ultra");
        let verifier = listen::VerificationWorker::spawn(move || drama3_verify::decoder(&model))?;
        verifier.configure(true);
        verifier
    };
    verification.wait_ready(Duration::from_secs(30))?;
    let client = verification.client();
    println!(
        "{}",
        json!({"ready":true,"backend":"cpu","live_backend":if only {"none"} else {"metal"},"asr_instances":if only {1} else {2},"baseline_bytes":baseline,"loaded_bytes":footprint()?,"extra_bytes":footprint()?.saturating_sub(baseline)})
    );
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let started = Instant::now();
        let mut response=(||->Result<Value>{
            if let Some(text)=request["speak"].as_str(){
                let service=service.as_ref().context("pass prepared assets for speech smoke")?;
                let mut first=None;let mut until:f64=0.;let mut gaps=Vec::new();let mut chunks=0;
                // `paced`: the app's real backpressure without a speaker. The same eight-chunk bounded channel as
                // SpeechQueue, drained at real-time speed by a silent consumer.
                let paced=request["paced"]==true;
                let (tx,rx)=std::sync::mpsc::sync_channel(if paced {8} else {1<<16});
                let epoch=std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let generation=epoch.clone();
                let play=request["play"]==true;
                let player=if play {Some(std::thread::spawn(move || {
                    use speak::AudioOutput;
                    speak::CoreAudioOutput.output_stream(speak::PlaybackStream::new(rx,generation),&||false,speak::Cancellation::new(||false))
                }))}else if paced {Some(std::thread::spawn(move || -> Result<()> {
                    for chunk in rx.iter() {
                        let chunk:speak::PlaybackChunk=chunk;
                        if chunk.generation!=generation.load(std::sync::atomic::Ordering::Acquire) {continue;}
                        std::thread::sleep(Duration::from_secs_f64(chunk.audio.duration_s()));
                    }
                    Ok(())
                }))}else{None};
                let result=service.stream(text,&||false,&mut |packet| {
                    if let speak::SpeechPacket::Audio(a)=packet {
                        let now=started.elapsed().as_secs_f64();first.get_or_insert(now);
                        if chunks>0 && now>until {gaps.push((now-until)*1000.);}
                        until=until.max(now)+a.samples.len() as f64/a.sample_rate as f64;chunks+=1;
                        if play || paced {tx.send(speak::PlaybackChunk{audio:a,generation:epoch.load(std::sync::atomic::Ordering::Acquire),label:None})?;}
                    }else if let speak::SpeechPacket::RestartFirst=packet {until=started.elapsed().as_secs_f64();epoch.fetch_add(1,std::sync::atomic::Ordering::AcqRel);}
                    Ok(())
                });
                drop(tx);
                if let Some(player)=player {player.join().map_err(|_|anyhow::anyhow!("playback worker panicked"))??;}
                let a=match result {
                    Ok(a)=>a,
                    Err(error)=>match error.downcast::<speak::drama3::SpeechFailure>() {
                        Ok(f)=>return Ok(json!({"fallback":true,"remaining":f.remaining,"played_text":f.played_text,"reason":f.to_string(),"checks":service.events(),"wall_ms":started.elapsed().as_secs_f64()*1000.})),
                        Err(error)=>return Err(error),
                    },
                };
                // `rates`: the delivered Jarvis audio time-stretched as playback does it (WSOLA), each take written
                // to `save_dir` and checked by the same recognizer; returns seconds and the word check per rate.
                let mut stretched=vec![];
                if let Some(rates)=request["rates"].as_array() {
                    let dir=PathBuf::from(request["save_dir"].as_str().context("save_dir")?);
                    std::fs::create_dir_all(&dir)?;
                    for rate in rates {
                        let rate=rate.as_f64().context("rate")? as f32;
                        let started=Instant::now();
                        let samples=speak::stretch::stretch(&a.samples,rate);
                        let stretch_ms=started.elapsed().as_secs_f64()*1000.;
                        let take=speak::Audio{samples,sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.};
                        let path=dir.join(format!("{}-{rate:.2}.wav",request["name"].as_str().unwrap_or("take")));
                        take.write_wav(&path)?;
                        let check=drama3_verify::check(&client,text,&take,&||false)?;
                        stretched.push(json!({"rate":rate,"seconds":take.duration_s(),"stretch_ms":stretch_ms,"accepted":check["accepted"],"transcript":check["transcript"],"wav":path}));
                    }
                }
                return Ok(json!({"fallback":false,"stretched":stretched,"device_played":play,"first_ms":first.map(|x|1000.*x),"seconds":a.samples.len() as f64/44100.,"materialization_gaps_ms":gaps,"checks":service.events(),"wall_ms":started.elapsed().as_secs_f64()*1000.}));
            }
            if let Some(path)=request["live_wav"].as_str(){
                let a=wav(Path::new(path))?;let samples=audio::mono_16k(&a.samples,a.sample_rate,1)?;
                let worker=worker.as_ref().context("live owner disabled")?;
                worker.transcribe_side(100,0,samples)?;
                loop{match worker.recv_event(){Some(listen::WorkerEvent::SideTranscript{result,decode_ms,..})=>return Ok(json!({"live_text":result.map_err(anyhow::Error::msg)?.text,"decode_ms":decode_ms})),Some(_)=>{},None=>anyhow::bail!("listening worker stopped")}}
            }
            // `supertonic_rates`: the lighter voice at each speed, as the app plays it.
            if let Some(rates)=request["supertonic_rates"].as_array(){
                let text=request["text"].as_str().context("text")?;
                let mut model=speak::Supertonic::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/supertonic-3"))?;
                let mut out=vec![];
                for rate in rates {
                    let rate=rate.as_f64().context("rate")? as f32;
                    // Natural-speed render, then the same WSOLA as Jarvis (what the app plays).
                    let mut take=model.render(text,"en")?;
                    take.samples=speak::stretch::stretch(&take.samples,rate);
                    let check=drama3_verify::check(&client,text,&take,&||false)?;
                    out.push(json!({"rate":rate,"seconds":take.duration_s(),"accepted":check["accepted"],"transcript":check["transcript"]}));
                }
                return Ok(json!({"supertonic":out}));
            }
            if let Some(actual)=request["transcript"].as_str(){return Ok(speak::coverage::check(request["text"].as_str().context("text")?,actual));}
            if let Some(path)=request["timed_wav"].as_str(){let a=wav(Path::new(path))?;let samples=audio::mono_16k(&a.samples,a.sample_rate,1)?;return Ok(serde_json::to_value(client.transcribe(samples,Duration::from_millis(750),&||false)?.transcript)?);}
            let a=wav(Path::new(request["wav"].as_str().context("wav")?))?;
            drama3_verify::check(&client,request["text"].as_str().context("text")?,&a,&||false)
        })().unwrap_or_else(|e|json!({"error":format!("{e:#}")}));
        response["footprint_bytes"] = footprint()?.into();
        response["extra_bytes"] = footprint()?.saturating_sub(baseline).into();
        println!("{response}");
        io::stdout().flush()?;
    }
    if let Some(service) = service {
        service.stop();
    }
    verification.configure(false);
    ensure!(
        verification.wait_unloaded(Duration::from_secs(3)),
        "verification decoder failed to unload"
    );
    println!(
        "{}",
        json!({"phase":"unloaded","footprint_bytes":footprint()?})
    );
    io::stdout().flush()?;
    Ok(())
}
