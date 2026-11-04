//! Listening Off→On, measured with the real models and no capture (no microphone or screen grant is used).
//! `HEIMDALL_MODELS=<models dir> cargo test -p app --release start_latency -- --ignored --nocapture`
use super::*;

struct Usage {
    cpu_s: f64,
    wakeups: u64,
    footprint_mb: f64,
}
fn usage() -> Usage {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4, &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    assert_eq!(ok, 0);
    #[allow(clashing_extern_declarations)]
    extern "C" { fn mach_timebase_info(info: *mut [u32; 2]) -> i32; }
    let mut timebase = [0u32; 2];
    unsafe { mach_timebase_info(&mut timebase) };
    let ticks = (info.ri_user_time + info.ri_system_time) as f64;
    Usage {
        cpu_s: ticks * timebase[0] as f64 / timebase[1] as f64 / 1e9,
        wakeups: info.ri_pkg_idle_wkups + info.ri_interrupt_wkups,
        footprint_mb: info.ri_phys_footprint as f64 / 1048576.,
    }
}
/// CPU, wakeups and memory of this process over `seconds` with nothing fed.
fn idle_window(label: &str, seconds: u64) -> serde_json::Value {
    let (before, t) = (usage(), Instant::now());
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    let (after, wall) = (usage(), t.elapsed().as_secs_f64());
    serde_json::json!({"window":label,"wall_s":wall,"cpu_percent_one_core":(after.cpu_s-before.cpu_s)/wall*100.,
        "wakeups_per_s":(after.wakeups-before.wakeups) as f64/wall,"footprint_mb":after.footprint_mb})
}

#[test]
#[ignore = "needs the real models (HEIMDALL_MODELS) and loads them several times"]
fn start_latency_breakdown_with_real_models() {
    let models = PathBuf::from(std::env::var("HEIMDALL_MODELS").expect("HEIMDALL_MODELS"));
    let t = tempfile::tempdir().unwrap();
    let mut app = Pipeline::new(t.path().join("data"), models.clone(), Settings::default(), None, true).unwrap();
    let s = app.listen_settings.lock().unwrap().clone();
    let mut report = vec![];
    let mut step = |label: &str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        let ms = t.elapsed().as_secs_f64() * 1000.;
        println!("{label}: {ms:.0} ms");
        report.push((label.to_string(), ms));
    };
    // The parts, each on this thread.
    let mut asr = None;
    step("parakeet load", &mut || asr = Some(listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), &s.parakeet_backend).unwrap()));
    step("silero + smart turn load", &mut || drop(ListenEngine::load_with_turn_backend(&models, asr.take().unwrap(), Some(&s.turn_backend)).unwrap()));
    let mut decider:Option<decide::LocalDecider> = None;
    step("decider load", &mut || decider = Some(decide::LocalDecider::load(models.join("decide"),t.path().join("decider" )).unwrap()));
    println!("{}", idle_window("decider loaded, idle", 10));
    drop(decider);
    step("decider load again, same process", &mut || drop(decide::LocalDecider::load(models.join("decide"),t.path().join("decider" )).unwrap()));
    println!("{}", idle_window("nothing loaded", 10));
    // What Listening On waited for before capture started: the worker's factory, end to end, three times.
    let (tx, rx) = crossbeam_channel::bounded(512);
    for round in 1..=3 {
        step(&format!("listen worker ready, round {round}"), &mut || {
            app.enable_live_worker(tx.clone()).unwrap();
            app.live_worker.as_ref().unwrap().wait_ready().unwrap();
        });
        if round == 3 {
            println!("{}", idle_window("listen worker loaded, idle", 10));
        }
        step(&format!("listening off (worker stop), round {round}"), &mut || app.stop_live_worker(&rx).unwrap());
    }
    println!("{}", idle_window("after stop", 10));
    // The path Listening on takes now (capture and the screen start where marked; neither runs here).
    for round in 1..=3 {
        let on = Instant::now();
        app.set_running(true, "lab").unwrap();
        app.listening_starting();
        app.ensure_live_worker(tx.clone()).unwrap();
        let capture_starts = on.elapsed().as_secs_f64() * 1000.;
        app.prepare_decisions().unwrap();
        let decisions = on.elapsed().as_secs_f64() * 1000.;
        app.live_worker.as_ref().unwrap().wait_ready().unwrap();
        let ready = on.elapsed().as_secs_f64() * 1000.;
        println!("now, on round {round}: capture and screen start at {capture_starts:.0} ms; decisions loaded {decisions:.0} ms; listen models ready {ready:.0} ms");
        let off = Instant::now();
        app.close_live_sources(&rx).unwrap();
        app.set_running(false, "lab").unwrap();
        while let Ok(Some(e)) = rx.try_recv() { app.worker_event(e).unwrap(); }
        println!("now, off round {round}: {:.0} ms (models kept)", off.elapsed().as_secs_f64() * 1000.);
    }
    println!("{}", idle_window("off, listen models kept loaded", 10));
    app.stop_live_worker(&rx).unwrap();
}
