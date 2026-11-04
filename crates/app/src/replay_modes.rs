//! Offline command-line modes of the same binary: isolated lab data, WAV replay (paced or silent), idle and
//! hot-swap smokes, power and CPU sampling. Never touches the real profile.
use super::*;

pub(super) fn isolated_data(p: Option<PathBuf>, mode: &str) -> Result<PathBuf> {
    let path = p.unwrap_or_else(|| {
        std::env::temp_dir().join("Heimdall-lab")
            .join(format!("{mode}-{}", uuid::Uuid::new_v4()))
    });
    // The profile in use, through any link (a moved personal profile leaves one at its old path).
    let real = settings::default_data()?;
    let real = real.canonicalize().unwrap_or(real);
    ensure!(
        !path.starts_with(&real),
        "offline modes refuse User's real data directory"
    );
    settings::private_dir(&path)?;
    let path = path.canonicalize()?;
    ensure!(
        !path.starts_with(&real),
        "offline modes refuse User's real data directory"
    );
    Ok(path)
}
pub(super) fn wavs(p: &Path) -> Result<Vec<PathBuf>> {
    if p.is_file() {
        ensure!(
            p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")),
            "replay expects WAV files"
        );
        return Ok(vec![p.into()]);
    }
    ensure!(p.is_dir(), "replay input is unavailable");
    let mut out = vec![];
    let mut dirs = vec![p.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(d)? {
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_dir() {
                dirs.push(e.path());
            } else if e.path().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("wav"))
            {
                out.push(e.path());
            }
            ensure!(out.len() + dirs.len() <= 100000, "replay input too large");
        }
    }
    out.sort();
    ensure!(!out.is_empty(), "directory has no WAV files");
    Ok(out)
}
pub(super) fn read_wav(p: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(p)?;
    let f = r.spec();
    let data = if f.sample_format == hound::SampleFormat::Float {
        r.samples::<f32>().collect::<Result<Vec<_>, _>>()?
    } else {
        let scale = 2_f32.powi(f.bits_per_sample as i32 - 1);
        r.samples::<i32>()
            .map(|s| s.map(|v| v as f32 / scale))
            .collect::<Result<Vec<_>, _>>()?
    };
    ensure!(
        data.len() <= 16000 * 60 * 60 * 32,
        "file exceeds bounded replay input"
    );
    ensure!(data.iter().all(|v| v.is_finite()), "nonfinite WAV");
    archive::mono_16k(
        &data,
        capture::NativeFormat {
            sample_rate: f.sample_rate,
            channels: f.channels,
        },
    )
}
pub(super) fn fake_source(source: &str, n: usize, generation: u64) -> capture::SourceInfo {
    let mic = source == "mic" || (source == "mixed" && n.is_multiple_of(3));
    let discord = source == "discord" || (source == "mixed" && n % 3 == 1);
    let kind = if mic {
        capture::SourceKind::InputDevice {
            name: format!("Replay mic {}", n + 1),
            uid: format!("replay-{n}"),
        }
    } else {
        capture::SourceKind::Process {
            name: if discord {
                "Discord replay".into()
            } else {
                "App replay".into()
            },
            bundle_id: discord.then(|| "com.hnc.Discord".into()),
            pid: n as u32,
            birth_us: generation,
            owner: None,
            output_devices: vec![],
        }
    };
    capture::SourceInfo::new(
        kind,
        capture::NativeFormat {
            sample_rate: 16000,
            channels: 1,
        },
        generation,
    )
}
#[allow(clippy::too_many_arguments)]
pub(super) fn feed_realtime(
    app: &mut pipeline::Pipeline,
    raw: &mut dyn capture::ArchiveSink,
    source: &str,
    streams: usize,
    generation: u64,
    origin: u64,
    audio: &[f32],
    listen_rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>,
) -> Result<()> {
    let mut ids = vec![];
    for n in 0..streams {
        let i = fake_source(source, n, generation);
        raw.add_source(&i, origin)?;
        ids.push(i.id.clone());
        app.source(i);
    }
    let (tx, rx) = bounded::<Option<capture::AudioChunk>>(512);
    let producer_ids = ids.clone();
    std::thread::scope(|scope| -> Result<()> {
        let producer = scope.spawn(move || -> Result<()> {
            let started = Instant::now();
            for (frame, chunk) in audio.chunks(320).enumerate() {
                let target = Duration::from_millis(frame as u64 * 20);
                if target > started.elapsed() {
                    std::thread::sleep(target - started.elapsed());
                }
                for id in &producer_ids {
                    let ns = origin + frame as u64 * 20_000_000;
                    raw.append(
                        id,
                        ns,
                        capture::NativeFormat {
                            sample_rate: 16000,
                            channels: 1,
                        },
                        chunk,
                        false,
                    )?;
                    tx.send(Some(capture::AudioChunk {
                        source_id: id.clone(),
                        label: "replay".into(),
                        host_ns: ns,
                        samples: chunk.to_vec(),
                        discontinuity: false,
                    }))?;
                }
            }
            for id in producer_ids {
                raw.remove_source(&id, origin + audio.len() as u64 * 62500)?;
            }
            tx.send(None)?;
            Ok(())
        });
        let consumed = (|| -> Result<()> {
            let never = crossbeam_channel::never();
            let mut done = false;
            while !done || ids.iter().any(|id| app.source_open(&id.0)) {
                let active_rx = if done { &never } else { &rx };
                let speaker_ready = app.speaker_events_rx.clone();
                select! {
                    recv(speaker_ready)->e=>app.speaker_update(e?)?,
                    recv(active_rx)->chunk => match chunk?{Some(c)=>app.capture_event(capture::CaptureEvent::Audio(c))?,None=>{done=true;for id in &ids{app.finish_source(&id.0)?;}}},
                    recv(listen_rx)->event=>match event?{Some(e)=>app.worker_event(e)?,None=>bail!("listen stopped during replay")},
                }
            }
            Ok(())
        })();
        drop(rx);
        let produced = producer
            .join()
            .map_err(|_| anyhow::anyhow!("replay source panicked"))?;
        consumed?;
        produced?;
        Ok(())
    })
}
pub(super) fn silent_replay(
    app: &mut pipeline::Pipeline,
    raw: &mut dyn capture::ArchiveSink,
    source: &str,
    streams: usize,
    generation: u64,
    listen_rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>,
) -> Result<()> {
    feed_realtime(
        app,
        raw,
        source,
        streams,
        generation,
        generation * 1_000_000_000,
        &vec![0_f32; 160000],
        listen_rx,
    )
}
// Optional permission-free resident bracket. Both benchmark arms use the same
// event loop and sampler; no audio is fabricated during loaded idle.
pub(super) fn loaded_idle(app: &mut pipeline::Pipeline, rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>, dir: &Path, label: &str, macmon: Option<&Path>) -> Result<Option<serde_json::Value>> {
    let seconds: u64 = std::env::var("HEIMDALL_REPLAY_LOADED_IDLE_SECONDS").ok().map(|v| v.parse()).transpose()?.unwrap_or(0);
    ensure!(seconds <= 60, "loaded idle bracket exceeds 60 seconds");
    if seconds == 0 { return Ok(None); }
    println!("{}", serde_json::json!({"event":"resource_window_start","label":label}));
    let window = metrics::Window::start(label, dir, macmon, app.speaker_cpu.clone())?;
    let end = crossbeam_channel::after(Duration::from_secs(seconds));
    loop {
        let speaker_ready = app.speaker_events_rx.clone();
        select! {
            recv(end) -> _ => break,
            recv(speaker_ready) -> e => app.speaker_update(e?)?,
            recv(rx) -> e => match e? { Some(e) => app.worker_event(e)?, None => bail!("listen stopped during loaded idle") },
        }
    }
    Ok(Some(window.finish()?))
}
#[allow(clippy::too_many_arguments)]
pub(super) fn replay(
    p: &Path,
    data: &Path,
    models: &Path,
    source: &str,
    u: Option<ui::Ui>,
    realtime: bool,
    streams: usize,
    macmon: Option<&Path>,
) -> Result<()> {
    let paths = wavs(p)?;
    let mut s = Settings::load(&data.join("settings.json"))?;
    s.dry_run = true;
    s.setup_requested = false;
    let start = Instant::now();
    let mut app = pipeline::Pipeline::new(data.into(), models.into(), s, u, true)?;
    let mut raw = app.raw_archive()?;
    let mut seconds = 0.;
    let (listen_tx, listen_rx) = bounded(512);
    let metrics_dir = data.join("metrics");
    let mut receipt = serde_json::json!({"realtime":realtime,"streams":streams,"source_policy":source,"busy_machine":true,"hardware":"Apple M5 Max","date_ms":agent::now_ms()});
    if realtime {
        let load = Instant::now();
        app.preload(listen_tx, source == "discord" || source == "mixed")?;
        receipt["preload_seconds"] = serde_json::json!(load.elapsed().as_secs_f64());
        if let Some(v) = loaded_idle(&mut app, &listen_rx, &metrics_dir, "loaded-idle-before", macmon)? { receipt["loaded_idle_before"] = v; }
        println!("{}", serde_json::json!({"event":"resource_window_start","label":"digital-silence-before"}));
        let bracket =
            metrics::Window::start("idle-before", &metrics_dir, macmon, app.speaker_cpu.clone())?;
        silent_replay(&mut app, raw.as_mut(), source, streams, 100000, &listen_rx)?;
        receipt["idle_before"] = bracket.finish()?;
    }
    let measurement = if realtime {
        println!(
            "{}",
            serde_json::json!({"event":"speech_measurement_start"})
        );
        Some(metrics::Window::start(
            "speech",
            &metrics_dir,
            macmon,
            app.speaker_cpu.clone(),
        )?)
    } else {
        None
    };
    for (file, path) in paths.iter().enumerate() {
        let audio = read_wav(path)?;
        let generation = file as u64 + 1;
        let origin = 1_000_000_000 + (seconds * 1e9) as u64;
        seconds += audio.len() as f64 / 16000.;
        if realtime {
            feed_realtime(
                &mut app,
                raw.as_mut(),
                source,
                streams,
                generation,
                origin,
                &audio,
                &listen_rx,
            )?;
            continue;
        }
        let mut ids = vec![];
        for n in 0..streams {
            let info = fake_source(source, n, generation);
            raw.add_source(&info, origin)?;
            ids.push(info.id.clone());
            app.source(info);
        }
        let clock = Instant::now();
        for (i, c) in audio.chunks(320).enumerate() {
            if realtime {
                let target = Duration::from_millis(i as u64 * 20);
                if target > clock.elapsed() {
                    std::thread::sleep(target - clock.elapsed());
                }
            }
            for id in &ids {
                let ns = origin + i as u64 * 20_000_000;
                raw.append(
                    id,
                    ns,
                    capture::NativeFormat {
                        sample_rate: 16000,
                        channels: 1,
                    },
                    c,
                    false,
                )?;
                app.capture_event(capture::CaptureEvent::Audio(capture::AudioChunk {
                    source_id: id.clone(),
                    label: "replay".into(),
                    host_ns: ns,
                    samples: c.to_vec(),
                    discontinuity: false,
                }))?;
            }
        }
        for id in ids {
            app.finish_source(&id.0)?;
            raw.remove_source(&id, origin + audio.len() as u64 * 62500)?;
        }
    }
    if let Some(w) = measurement {
        receipt["speech"] = w.finish()?;
        println!("{}", serde_json::json!({"event":"resource_window_start","label":"digital-silence-after"}));
        let bracket =
            metrics::Window::start("idle-after", &metrics_dir, macmon, app.speaker_cpu.clone())?;
        silent_replay(&mut app, raw.as_mut(), source, streams, 200000, &listen_rx)?;
        receipt["idle_after"] = bracket.finish()?;
        if let Some(v) = loaded_idle(&mut app, &listen_rx, &metrics_dir, "loaded-idle-after", macmon)? { receipt["loaded_idle_after"] = v; }
        settings::private_write(
            &data.join("metrics.json"),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        println!(
            "{}",
            serde_json::json!({"event":"realtime_metrics","measurements":receipt})
        );
    }
    raw.flush()?;
    drop(raw);
    if realtime {
        app.stop_live_worker(&listen_rx)?;
    }
    app.finish()?;
    println!(
        "{}",
        serde_json::json!({"event":"replay_complete","dry_run":true,"realtime":realtime,"streams":streams,"files":paths.len(),"audio_seconds_per_stream":seconds,"aggregate_audio_seconds":seconds*streams as f64,"wall_seconds":start.elapsed().as_secs_f64(),"final_utterances":app.finals,"decisions":app.decisions,"screenshot_notes":app.notes,"decision_errors":app.errors,"gap_events":app.gaps,"transcript_segments":app.segments,"speaker_prompts":app.speaker_prompts,"coded_voices":app.coded_voices,"session":app.session})
    );
    ensure!(
        app.finals > 0 && app.decisions == app.finals && app.errors == 0 && app.gaps == 0,
        "replay did not complete all local decisions"
    );
    Ok(())
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(super) struct Power {
    pub(super) user: u64,
    pub(super) system: u64,
    pub(super) interrupts: u64,
    pub(super) platform: u64,
    pub(super) timer1: u64,
    pub(super) timer2: u64,
}
extern "C" {
    fn task_info(task: u32, flavor: u32, info: *mut u32, count: *mut u32) -> i32;
    static mach_task_self_: u32;
}
pub(super) fn power() -> Result<Power> {
    let mut p = Power::default();
    let mut count = 12;
    ensure!(
        unsafe {
            task_info(
                mach_task_self_,
                21,
                &mut p as *mut _ as *mut u32,
                &mut count,
            )
        } == 0,
        "task power counters unavailable"
    );
    Ok(p)
}
pub(super) fn cpu() -> f64 {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    r.ru_utime.tv_sec as f64
        + r.ru_utime.tv_usec as f64 / 1e6
        + r.ru_stime.tv_sec as f64
        + r.ru_stime.tv_usec as f64 / 1e6
}
/// Explicit, bounded lab only. Preflight refuses before any stream if the responsible
/// process lacks grants. No provider query, settings/profile mutation or UI input.
pub(super) fn hotswap_replay_smoke(data:PathBuf,models:PathBuf,wav:&Path)->Result<()> {
    let audio=read_wav(wav)?;ensure!(!audio.is_empty(),"empty smoke clip");
    let mut app=pipeline::Pipeline::new(data.clone(),models,Settings::default(),None,true)?;
    let (tx,rx)=bounded(512);app.preload(tx,false)?;let mut raw=app.raw_archive()?;
    let mut ids=vec![];let origin=capture::host_now_ns();
    for source in ["mic","app"]{let info=fake_source(source,0,1);raw.add_source(&info,origin)?;ids.push(info.id.clone());app.source(info);}
    let mut watch=settings::SettingsWatch::default();
    let start=Instant::now();let begin_cpu=cpu();let mut changes=0;
    // Same live worker, paced intake and archive paths; fake native capture and screen frames.
    for tick in 0..1650u64 {
        let at=Duration::from_millis(tick*20);if at>start.elapsed(){std::thread::sleep(at-start.elapsed());}
        let ns=origin+tick*20_000_000;
        if tick%50==0 {app.screen_buffer.lock().unwrap().push(screen::Frame::new(screen::DisplayInfo {id:1,x:0.,y:0.,width:2,height:1},ns,2,1,vec![0;6])?)?;}
        if [100,300,500,700,900,1100].contains(&tick) {
            let mut next=app.settings.clone();next.speak_replies=changes%2==0;
            next.free_space_floor_bytes=(10+changes%2)*1024*1024*1024;
            next.tts_backend=if changes%2==0 {"cpu"}else{"metal"}.into();
            if changes<2 {next.parakeet_backend=if changes==0 {"coreml"}else{"metal"}.into();}
            next.screen.fps=if changes%2==0 {2}else{1};
            next.save(&data.join("settings.json"))?;
            app.apply_settings(watch.read_changed(&data.join("settings.json"))?.context("missing settings revision")?)?;
            app.screen_buffer.lock().unwrap().reconfigure(app.settings.screen.config(&[(2,1)]),ns)?;
            changes+=1;
        }
        let chunk=if tick<1500 {(0..320).map(|i|audio[(tick as usize*320+i)%audio.len()]).collect::<Vec<_>>()}else{vec![0.;320]};
        for id in &ids {
            raw.append(id,ns,capture::NativeFormat {sample_rate:16000,channels:1},&chunk,false)?;
            app.capture_event(capture::CaptureEvent::Audio(capture::AudioChunk {source_id:id.clone(),label:id.0.clone(),host_ns:ns,samples:chunk.clone(),discontinuity:false}))?;
        }
        while let Ok(Some(event))=rx.try_recv(){app.worker_event(event)?;}
    }
    app.stop_live_worker(&rx)?;raw.flush()?;
    for id in &ids {app.finish_source(&id.0)?;}
    let gaps=app.gaps;let errors=app.errors;let session=app.session.clone();app.finish()?;
    let expected_utterances=app.index.evidence.records()?.iter().filter(|r|r.kind=="listen_event"&&r.payload["event"]=="final_utterance").count();
    let indexed=app.index.utterances_between(0,u64::MAX/2)?;
    let indexed_per_stream:Vec<_>=ids.iter().map(|id|indexed.iter().filter(|r|r["source_id"]==id.0).count()).collect();
    let archive=capture::Archive::open_read_only(&session)?;
    let counts:Vec<_>=ids.iter().map(|id|archive.read_blocks(id).map(|blocks|blocks.iter().map(|b|b.samples.len()).sum::<usize>())).collect::<Result<_>>()?;
    let receipt=serde_json::json!({"native_capture":false,"native_screen":false,"busy_machine":true,"wall_seconds":start.elapsed().as_secs_f64(),"cpu_seconds":cpu()-begin_cpu,"streams":2,"changes":changes,"samples_per_stream":counts,"expected_samples_per_stream":1650*320,"utterances_indexed":indexed.len(),"utterances_expected":expected_utterances,"utterances_per_stream":indexed_per_stream,"gaps":gaps,"errors":errors,"pending_reload":app.pending_reload,"session":session});
    settings::private_write(&data.join("hot-swap-smoke.json"),&serde_json::to_vec_pretty(&receipt)?)?;println!("{receipt}");
    ensure!(counts==vec![1650*320;2]&&gaps==0&&errors==0&&app.pending_reload.is_empty(),"replay swap smoke reported loss or failed setting");
    ensure!(expected_utterances>0&&indexed.len()==expected_utterances&&app.finals==expected_utterances&&indexed_per_stream.iter().all(|n|*n>0),"replay final transcript loss: {} indexed / {expected_utterances} expected",indexed.len());
    Ok(())
}
pub(super) struct LabPlayback(pub(super) std::process::Child);
impl Drop for LabPlayback { fn drop(&mut self) { let _=self.0.kill();let _=self.0.wait(); } }
pub(super) fn hotswap_smoke(data: PathBuf, models: PathBuf) -> Result<()> {
    let states=ui::permissions::status();
    ensure!(states.contains(&(ui::Permission::Microphone,ui::PermissionState::Granted)), "lab microphone grant unavailable; no prompt requested");
    ensure!(screen::screen_permission_granted(), "lab screen grant unavailable; no prompt requested");
    let consented=Settings::load(&settings::default_data()?.join("settings.json"))?;
    ensure!(consented.setup_requested,"existing System Audio grant has not been confirmed");
    let s=Settings {setup_requested:true,recorded_apps:vec![settings::AppChoice {key:"/usr/bin/afplay".into(),name:"afplay".into(),path:"/usr/bin/afplay".into()}],..Default::default()};
    let mut app=pipeline::Pipeline::new(data.clone(),models,s,None,false)?;
    let (listen_tx,listen_rx)=bounded(512);app.preload(listen_tx,false)?;
    let (tx,rx)=bounded(512);
    let permissions=LivePermissions {mic:true,system:true,screen:true,..Default::default()};
    let mut capture=Some(CaptureRun::start(&app,tx,permissions)?);
    let mut screens=None;start_screen(&mut app,&mut screens)?;adopt_screen(&mut app,&mut screens,Duration::from_secs(30))?;ensure!(screens.is_some(),"lab screen did not start within 30 s");
    let wav=agent::runtime::checkout_root().join("crates/listen/tests/fixtures/en-2.wav");
    let start=Instant::now();let begin_cpu=cpu();let mut playback: Option<LabPlayback>=None;
    let mut changes=0;let mut samples=std::collections::BTreeMap::<String,usize>::new();
    let mut mic_samples=0;let mut app_samples=0;let mut screen_configs=0;
    while start.elapsed()<Duration::from_secs(30) {
        if playback.as_mut().map(|p|p.0.try_wait()).transpose()?.flatten().is_some()||playback.is_none(){
            playback=Some(LabPlayback(std::process::Command::new("/usr/bin/afplay").args(["-v","0.1"]).arg(&wav).spawn()?));
        }
        if changes<6&&start.elapsed().as_secs()>=2+changes*4 {
            let mut next=app.settings.clone();
            next.speak_replies=changes%2==0;
            next.free_space_floor_bytes=(10+changes%2)*1024*1024*1024;
            next.tts_backend=if changes%2==0 {"cpu"}else{"metal"}.into();
            if changes<2 {next.parakeet_backend=if changes==0 {"coreml"}else{"metal"}.into();}

            next.screen.fps=if changes%2==0 {2}else{1};
            next.screen.resolution=if changes%2==0 {settings::ScreenResolution::P720}else{settings::ScreenResolution::P540};
            app.apply_settings(next)?;
            screens.as_ref().unwrap().reconfigure(app.settings.screen.config(&ui::display_sizes()))?;
            changes+=1;
        }
        while let Ok(Some(e))=listen_rx.try_recv(){app.worker_event(e)?;}
        while let Ok(e)=screens.as_ref().unwrap().events.try_recv(){if let screen::ScreenEvent::Configured {error}=e {ensure!(error.is_none(),"screen swap failed: {error:?}");screen_configs+=1;}}
        if let Ok(e)=rx.recv_timeout(Duration::from_millis(20)) {
            if let capture::CaptureEvent::Audio(c)=&e {
                *samples.entry(c.source_id.0.clone()).or_default()+=c.samples.len();
                if c.source_id.0.starts_with("mic:"){mic_samples+=c.samples.len();}else{app_samples+=c.samples.len();}
            }
            app.capture_event(e)?;
        }
    }
    drop(playback.take());
    capture.take();while let Ok(e)=rx.try_recv(){app.capture_event(e)?;}
    app.stop_live_worker(&listen_rx)?;
    let frames=screens.as_ref().unwrap().buffer.lock().unwrap().frame_count();screens.take();
    let gaps=app.gaps;let errors=app.errors;let session=app.session.clone();app.finish()?;
    let receipt=serde_json::json!({"busy_machine":true,"wall_seconds":start.elapsed().as_secs_f64(),"cpu_seconds":cpu()-begin_cpu,"changes":changes,"screen_configs":screen_configs,"screen_frames":frames,"mic_samples":mic_samples,"app_samples":app_samples,"sources":samples,"gaps":gaps,"errors":errors,"session":session,"pending_reload":app.pending_reload});
    settings::private_write(&data.join("hot-swap-smoke.json"),&serde_json::to_vec_pretty(&receipt)?)?;
    println!("{receipt}");
    ensure!(mic_samples>0&&app_samples>0&&frames>0&&screen_configs==6,"lab streams/configurations did not all deliver");
    ensure!(gaps==0&&errors==0&&app.pending_reload.is_empty(),"hot-swap smoke reported loss or failed setting");
    Ok(())
}
pub(super) fn idle(seconds: u64, p: Option<PathBuf>, models: PathBuf) -> Result<()> {
    ensure!(
        (10..=60).contains(&seconds),
        "idle window must be 10..60 seconds"
    );
    let data = isolated_data(p, "idle")?;
    let app = pipeline::Pipeline::new(data, models, Settings::default(), None, false)?;
    let discovery = capture::Capture::discover_only()?;
    // Same no-capture app state; wait on a channel, without capture or model threads.
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    let begin = power()?;
    let before = cpu();
    let t = Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    let elapsed = t.elapsed().as_secs_f64();
    let used = cpu() - before;
    let after = power()?;
    println!(
        "{}",
        serde_json::json!({"event":"idle_measurement","no_capture":true,"models_loaded":false,"busy_machine":true,"wall_seconds":elapsed,"cpu_seconds":used,"cpu_percent_one_core":used/elapsed*100.,"interrupt_wakeups":after.interrupts-begin.interrupts,"platform_idle_wakeups":after.platform-begin.platform,"interrupt_wakeups_per_second":(after.interrupts-begin.interrupts) as f64/elapsed,"platform_idle_wakeups_per_second":(after.platform-begin.platform) as f64/elapsed,"ui_included":false})
    );
    let _ = tx.send(());
    let _ = waiting.join();
    drop(discovery);
    drop(app);
    Ok(())
}

