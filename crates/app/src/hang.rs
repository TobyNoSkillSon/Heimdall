//! Hang evidence: when the UI thread or the live loop stalls past `HANG`, every thread's stack is sampled in-process
//! and written to `<logs>/hangs/`, with a one-line summary in heimdall.log.
//!
//! 24 Oct 02:30 the live loop waited 56 s on the archive lock while clicks queued; the only stack we got was the crash
//! report of the forced Quit a minute later. The watchdog (watchdog.rs) acts on a freeze after 60 s; this records the
//! evidence after 2 s and changes nothing else. Sampling suspends one thread at a time for microseconds, reads its
//! frame-pointer chain with kernel calls only (no allocation or lock while it is suspended), and symbolizes after
//! resuming it.

// libc's mach bindings are deprecated in favour of the mach2 crate, which Heimdall does not depend on.
#![allow(deprecated)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::watchdog::Beats;

/// A UI thread or live loop silent this long is a hang worth a sample.
pub const HANG: Duration = Duration::from_secs(2);
/// The live loop's settings tick (live_loop.rs): a hang is `HANG` past it.
const LIVE_TICK: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(250);
/// One sample per thread per this, however often it stalls.
const QUIET: Duration = Duration::from_secs(10 * 60);
/// Sample files kept.
const KEEP: usize = 20;
const FRAMES: usize = 64;

/// Watches both threads for the life of the process. `post` runs a block on the UI thread.
pub fn spawn(beats: Arc<Beats>, post: impl Fn(Box<dyn FnOnce() + Send>) + Send + 'static, dir: PathBuf) -> std::io::Result<()> {
    std::thread::Builder::new().name("heimdall-hang-sampler".into()).spawn(move || {
        let epoch = Instant::now();
        let ms = move || epoch.elapsed().as_millis() as u64 + 1;
        let answered = Arc::new(AtomicU64::new(0));
        let mut sent = 0u64;
        let mut sent_at = 0u64;
        let mut last_sample: [Option<Instant>; 2] = [None, None];
        let mut in_hang = [false, false];
        loop {
            std::thread::sleep(POLL);
            if beats.is_quitting() {
                // Quit has its own bound; nothing here should run while the process tears down.
                return;
            }
            // UI: one ping in flight at a time; its age is how long the main thread has not run a block.
            if answered.load(Ordering::Acquire) == sent {
                sent += 1;
                sent_at = ms();
                let (answered, n) = (answered.clone(), sent);
                post(Box::new(move || answered.store(n, Ordering::Release)));
            }
            // Before the UI thread answers its first ping it is still launching, not hung.
            let ui = (answered.load(Ordering::Acquire) > 0).then(|| Duration::from_millis(ms().saturating_sub(sent_at)));
            let live = beats.live_silence().map(|d| d.saturating_sub(LIVE_TICK));
            for (i, (stalled, which)) in [(ui, Which::Ui), (live, Which::Live)].into_iter().enumerate() {
                let hung = stalled.is_some_and(|d| d > HANG);
                if hung && !in_hang[i] && last_sample[i].is_none_or(|t| t.elapsed() >= QUIET) {
                    last_sample[i] = Some(Instant::now());
                    record(&dir, which, stalled.unwrap_or_default());
                }
                in_hang[i] = hung;
            }
        }
    })?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Which {
    Ui,
    Live,
}

fn record(dir: &Path, which: Which, stalled: Duration) {
    let (label, name) = match which {
        Which::Ui => ("the menu (main thread)", None),
        Which::Live => ("the live loop", Some("heimdall-app")),
    };
    let threads = sample_all();
    let focus = threads.iter().find(|t| match name { Some(n) => t.name == n, None => t.main });
    // A few more samples of the stalled thread show whether it is stuck in one place or busy.
    let mut again = vec![];
    if let Some(f) = focus {
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(frames) = sample_thread(f.port) { again.push(frames); }
        }
    }
    let mut text = format!("Heimdall hang: {label} silent {:.1} s\n\n", stalled.as_secs_f64());
    if let Some(f) = focus {
        text.push_str(&format!("Stalled thread \"{}\":\n{}\n", f.name, symbolize(&f.frames)));
        for (i, frames) in again.iter().enumerate() {
            let same = frames == &f.frames;
            text.push_str(&format!("Again after {} ms{}\n", (i + 1) * 200, if same { ": same stack" } else { ":" }));
            if !same { text.push_str(&symbolize(frames)); }
        }
        text.push('\n');
    }
    for t in &threads {
        text.push_str(&format!("Thread {} \"{}\"{}:\n{}\n", t.port, t.name, if t.main { " (main)" } else { "" }, symbolize(&t.frames)));
    }
    let summary = focus.map(|f| top(&f.frames)).unwrap_or_else(|| "stalled thread not found".into());
    match write(dir, which, &text) {
        Ok(path) => eprintln!("Hang: {label} silent {:.1} s; at {summary}; stacks in {}", stalled.as_secs_f64(), path.display()),
        Err(e) => eprintln!("Hang: {label} silent {:.1} s; at {summary}; stacks not saved: {e}", stalled.as_secs_f64()),
    }
}

fn write(dir: &Path, which: Which, text: &str) -> std::io::Result<PathBuf> {
    let dir = dir.join("hangs");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
    let path = dir.join(format!("hang-{stamp}-{}.txt", if which == Which::Ui { "menu" } else { "live" }));
    std::fs::write(&path, text)?;
    let mut old: Vec<_> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("hang-"))).collect();
    old.sort();
    for p in old.iter().take(old.len().saturating_sub(KEEP)) { let _ = std::fs::remove_file(p); }
    Ok(path)
}

/// The innermost frames outside the system's waiting primitives, for the log line.
fn top(frames: &[u64]) -> String {
    let named: Vec<String> = frames.iter().enumerate().map(|(i, a)| name(*a, i > 0)).collect();
    let mine: Vec<&String> = named.iter().filter(|n| n.contains("Heimdall")).take(3).collect();
    if mine.is_empty() { named.iter().take(3).cloned().collect::<Vec<_>>().join(" <- ") } else { mine.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" <- ") }
}

struct Sampled {
    port: libc::mach_port_t,
    name: String,
    main: bool,
    frames: Vec<u64>,
}
/// Holds a send right to each sampled thread until dropped, so a re-sample never uses a freed port name.
impl Drop for Sampled {
    fn drop(&mut self) {
        unsafe { mach_port_deallocate(libc::mach_task_self(), self.port); }
    }
}

fn symbolize(frames: &[u64]) -> String {
    frames.iter().enumerate().map(|(i, a)| format!("  {i:>2} {}\n", name(*a, i > 0))).collect()
}

/// `image + offset  symbol + offset`. A return address is looked up one byte back, inside its call.
fn name(addr: u64, ret: bool) -> String {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let look = if ret { addr.saturating_sub(1) } else { addr };
    if unsafe { libc::dladdr(look as *const libc::c_void, &mut info) } == 0 || info.dli_fname.is_null() {
        return format!("{addr:#x}");
    }
    let image = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
    let image = image.rsplit('/').next().unwrap_or(&image).to_string();
    let base = info.dli_fbase as u64;
    let symbol = if info.dli_sname.is_null() { String::new() } else {
        format!("  {} + {}", unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }.to_string_lossy(), addr.saturating_sub(info.dli_saddr as u64))
    };
    format!("{image} + {:#x}{symbol}", addr.saturating_sub(base))
}

#[allow(non_camel_case_types)]
type kern_return_t = libc::c_int;
const ARM_THREAD_STATE64: libc::c_int = 6;
const ARM_THREAD_STATE64_COUNT: u32 = 68;
#[repr(C)]
#[derive(Default)]
struct ArmThreadState64 {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    pad: u32,
}
extern "C" {
    fn thread_suspend(thread: libc::mach_port_t) -> kern_return_t;
    fn thread_resume(thread: libc::mach_port_t) -> kern_return_t;
    fn thread_get_state(thread: libc::mach_port_t, flavor: libc::c_int, state: *mut u32, count: *mut u32) -> kern_return_t;
    fn mach_vm_read_overwrite(task: libc::mach_port_t, address: u64, size: u64, data: u64, outsize: *mut u64) -> kern_return_t;
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> kern_return_t;
    fn pthread_main_thread_np() -> libc::pthread_t;
}

/// Every thread but this one, suspended one at a time.
fn sample_all() -> Vec<Sampled> {
    let mut out = vec![];
    unsafe {
        let task = libc::mach_task_self();
        let me = libc::mach_thread_self();
        let main = libc::pthread_mach_thread_np(pthread_main_thread_np());
        let mut list: libc::thread_act_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        if libc::task_threads(task, &mut list, &mut count) != 0 || list.is_null() {
            mach_port_deallocate(task, me);
            return out;
        }
        let ports: Vec<libc::mach_port_t> = std::slice::from_raw_parts(list, count as usize).to_vec();
        libc::vm_deallocate(task, list as libc::vm_address_t, count as usize * std::mem::size_of::<libc::mach_port_t>());
        out.reserve(ports.len());
        for port in ports {
            let mut kept = false;
            if port != me {
                let mut name = [0 as libc::c_char; 64];
                let pthread = libc::pthread_from_mach_thread_np(port);
                if pthread != 0 as libc::pthread_t { libc::pthread_getname_np(pthread, name.as_mut_ptr(), name.len()); }
                let name = std::ffi::CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
                // CoreAudio's real-time I/O threads are never the stall and must not miss a deadline for it.
                if !name.starts_with("com.apple.audio.") {
                    if let Some(frames) = sample_thread(port) {
                        out.push(Sampled { port, name, main: port == main, frames });
                        kept = true;
                    }
                }
            }
            if !kept { mach_port_deallocate(task, port); }
        }
        mach_port_deallocate(task, me);
    }
    out
}

#[cfg(not(target_arch = "aarch64"))]
fn sample_thread(_: libc::mach_port_t) -> Option<Vec<u64>> {
    None
}
/// One thread's pc, lr and frame-pointer chain. While it is suspended only kernel calls run, into a stack buffer.
#[cfg(target_arch = "aarch64")]
fn sample_thread(port: libc::mach_port_t) -> Option<Vec<u64>> {
    let mut frames = [0u64; FRAMES];
    let mut n = 0;
    unsafe {
        let task = libc::mach_task_self();
        if thread_suspend(port) != 0 { return None; }
        let mut state = ArmThreadState64::default();
        let mut count = ARM_THREAD_STATE64_COUNT;
        let ok = thread_get_state(port, ARM_THREAD_STATE64, &mut state as *mut _ as *mut u32, &mut count) == 0;
        if ok {
            const MASK: u64 = 0x0000_7fff_ffff_ffff;
            frames[0] = state.pc & MASK;
            frames[1] = state.lr & MASK;
            n = 2;
            let mut fp = state.fp & MASK;
            while n < FRAMES && fp != 0 && fp % 8 == 0 {
                let mut pair = [0u64; 2];
                let mut got = 0u64;
                if mach_vm_read_overwrite(task, fp, 16, pair.as_mut_ptr() as u64, &mut got) != 0 || got != 16 { break; }
                let (next, ret) = (pair[0] & MASK, pair[1] & MASK);
                if ret == 0 { break; }
                // A leaf that saved no frame of its own: lr already holds this return address.
                if !(n == 2 && ret == frames[1]) { frames[n] = ret; n += 1; }
                if next <= fp { break; }
                fp = next;
            }
        }
        thread_resume(port);
        if !ok { return None; }
    }
    Some(frames[..n].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn parked_here(ready: std::sync::mpsc::Sender<libc::mach_port_t>, stop: std::sync::mpsc::Receiver<()>) {
        ready.send(unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }).unwrap();
        let _ = stop.recv();
    }

    #[test]
    fn a_blocked_thread_is_sampled_through_its_callers() {
        let (ready, port) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel();
        let t = std::thread::Builder::new().name("hang-test-parked".into()).spawn(move || parked_here(ready, stopped)).unwrap();
        let port = port.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let all = sample_all();
        let parked = all.iter().find(|s| s.name == "hang-test-parked").expect("the parked thread is listed by name");
        assert!(parked.frames.len() > 3, "{:?}", parked.frames);
        let text = symbolize(&parked.frames);
        assert!(text.contains("parked_here"), "the sampled chain reaches the blocking function:\n{text}");
        assert_eq!(sample_thread(port).map(|f| f.len() > 3), Some(true));
        assert!(all.iter().any(|s| s.main), "the main thread is marked");
        stop.send(()).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn a_stall_writes_one_sample_file_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..(KEEP + 2) {
            write(dir.path(), Which::Live, "x").unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        // Same-second names overwrite each other; never more than KEEP stay.
        assert!(std::fs::read_dir(dir.path().join("hangs")).unwrap().count() <= KEEP);
        record(dir.path(), Which::Ui, Duration::from_millis(2500));
        let files: Vec<_> = std::fs::read_dir(dir.path().join("hangs")).unwrap().filter_map(|e| e.ok()).collect();
        let menu = files.iter().find(|e| e.file_name().to_string_lossy().ends_with("-menu.txt")).expect("menu hang file");
        let text = std::fs::read_to_string(menu.path()).unwrap();
        assert!(text.starts_with("Heimdall hang: the menu (main thread) silent 2.5 s"));
        assert!(text.contains("Thread "), "every thread listed");
    }
}
