//! Lab only: Heimdall's listen model and Vella's transcription socket, nothing else (no capture, UI, archive or
//! agent), in an isolated data directory. For measuring Vella-through-Heimdall latency and memory without touching
//! the running Heimdall.
//!
//! cargo run --release -p app --bin heimdall-vella-asr-lab -- --data-dir /tmp/vella-asr-lab \
//!     [--models DIR] [--no-announce] [--minutes N] [--probe WAV] [--exit-after-probe]
//!
//! Prints the socket path and the process's resident memory once the model is ready. With announcing on (the default)
//! a running Vella hears `dev.heimdall.asr` ready for this socket and borrows it; `gone` is posted at exit.
#[path = "../vella_asr.rs"]
mod vella_asr;
use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    time::{Duration, Instant},
};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}
/// Resident and physical-footprint bytes of this process (footprint is what Activity Monitor calls Memory).
fn memory() -> (u64, u64) {
    let pid = std::process::id() as libc::c_int;
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let resident = if unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, &mut task as *mut _ as *mut libc::c_void, size) } == size { task.pti_resident_size } else { 0 };
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let footprint = if unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, &mut usage as *mut _ as *mut libc::rusage_info_t) } == 0 { usage.ri_phys_footprint } else { 0 };
    (resident, footprint)
}
fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}
/// One request through the socket, as Vella sends it; prints the reply and the client-side wall time.
fn probe(socket: &Path, wav: &Path) -> Result<()> {
    let audio = listen::file::read_wav(wav)?;
    let mut s = UnixStream::connect(socket)?;
    s.write_all(b"{\"v\":1,\"op\":\"status\"}\n")?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    println!("status: {}", line.trim());
    let sent = Instant::now();
    let mut s = UnixStream::connect(socket)?;
    s.write_all(format!("{{\"v\":1,\"op\":\"transcribe\",\"model\":\"{}\",\"rate\":16000,\"samples\":{},\"format\":\"f32le\"}}\n", vella_asr::MODEL, audio.len()).as_bytes())?;
    let bytes: Vec<u8> = audio.iter().flat_map(|v| v.to_le_bytes()).collect();
    s.write_all(&bytes)?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    println!("probe {} ({:.1} s audio): client wall {:.0} ms", wav.display(), audio.len() as f64 / 16000., sent.elapsed().as_secs_f64() * 1000.);
    println!("reply: {}", line.trim());
    Ok(())
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut data, mut models, mut announce, mut minutes, mut wav, mut exit_after) = (None, PathBuf::from("models"), true, 60u64, None, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" => data = Some(PathBuf::from(args.next().context("--data-dir needs a directory")?)),
            "--models" => models = PathBuf::from(args.next().context("--models needs a directory")?),
            "--no-announce" => announce = false,
            "--minutes" => minutes = args.next().context("--minutes needs a number")?.parse()?,
            "--probe" => wav = Some(PathBuf::from(args.next().context("--probe needs a WAV")?)),
            "--exit-after-probe" => exit_after = true,
            _ => bail!("usage: heimdall-vella-asr-lab --data-dir DIR [--models DIR] [--no-announce] [--minutes N] [--probe WAV] [--exit-after-probe]"),
        }
    }
    let data = data.context("--data-dir is required: an isolated directory, never Heimdall's own")?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME missing")?);
    ensure!(data != home.join("Library/Application Support/Heimdall"), "refusing Heimdall's real data directory");
    // A hard lifetime: a forgotten lab server never outlives a day.
    ensure!((1..=24 * 60).contains(&minutes), "--minutes must be 1..1440");
    std::fs::create_dir_all(&data)?;
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let (r0, f0) = memory();
    println!("before load: resident {:.2} GiB, footprint {:.2} GiB", gib(r0), gib(f0));
    // Like the app: the service lives on a non-main thread (the app's live loop runs on "heimdall-app"), and the main
    // thread only runs its run loop, as AppKit does there, which delivers Vella's dev.vella.asr.query.
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let service = std::thread::Builder::new().name("heimdall-app".into()).spawn(move || -> Result<()> {
        let load = Instant::now();
        let worker = Arc::new(vella_asr::spawn_listen_worker(&models, "metal", "metal")?);
        let (mut asr, error) = vella_asr::VellaAsr::start(&data, announce);
        if let Some(e) = error { return Err(e); }
        worker.wait_ready()?;
        asr.sync(Some(worker.clone()));
        let (r, f) = memory();
        println!("ready in {:.1} s", load.elapsed().as_secs_f64());
        println!("socket: {}", asr.socket().display());
        println!("resident {:.2} GiB, footprint {:.2} GiB (model + engine: {:.2} GiB footprint)", gib(r), gib(f), gib(f.saturating_sub(f0)));
        println!("announce: {}", if announce { "dev.heimdall.asr ready posted" } else { "off" });
        let _ = std::io::stdout().flush();
        if let Some(wav) = &wav {
            if let Err(e) = probe(asr.socket(), wav) { eprintln!("probe failed: {e:#}"); }
            let (r, f) = memory();
            println!("after probe: resident {:.2} GiB, footprint {:.2} GiB", gib(r), gib(f));
            let _ = std::io::stdout().flush();
        }
        while !STOP.load(Ordering::Acquire) && Instant::now() < deadline && !(exit_after && wav.is_some()) {
            std::thread::sleep(Duration::from_millis(500));
            asr.sync(Some(worker.clone()));
        }
        drop(asr);
        println!("stopped: gone posted, socket removed");
        Ok(())
    })?;
    while !service.is_finished() {
        let tick = Instant::now();
        unsafe {
            let date = objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.5);
            objc2_foundation::NSRunLoop::mainRunLoop().runMode_beforeDate(objc2_foundation::NSDefaultRunLoopMode, &date);
        }
        std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
    }
    service.join().map_err(|_| anyhow::anyhow!("service thread panicked"))??;
    Ok(())
}
