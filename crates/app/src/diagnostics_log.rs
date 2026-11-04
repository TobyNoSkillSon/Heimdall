//! A blocking pipe reader sleeps when there is no output. Rotation is checked per chunk,
//! so a burst cannot grow a log beyond the configured bound. No UI or archive dependency.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
const LIMIT: u64 = 2 * 1024 * 1024;
/// Old copies kept beside `heimdall.log`: 2 MB holds about a day on a busy one, and three copies held only three days
/// when a week was needed to count voice drops (4 Nov). Ten keep about a week and a half, at most 22 MB.
const COPIES: usize = 10;
fn open_log(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}
/// `2026-10-19 18:48:32.123 `: local time at the start of every line, whoever wrote it (panics, helpers, any crate).
fn local_time() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&seconds, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} ",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis()
    )
}
struct Sink {
    dir: PathBuf,
    file: File,
    len: u64,
    /// The next byte starts a line (it gets the time).
    line_start: bool,
    clock: fn() -> String,
    /// The last whole line written, when it repeats ("repeated identical log lines must collapse
    /// everywhere … one line with a count").
    last_line: Option<Vec<u8>>,
    /// Copies of `last_line` held back since `since`, written as one count line.
    repeats: u64,
    since: std::time::Instant,
}
/// Identical lines within this window are counted, not written; a repeat after it starts a new window.
const REPEAT_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
impl Sink {
    fn open(dir: &Path) -> anyhow::Result<Self> {
        heimdall_archive::private_dir(dir)?;
        let path = dir.join("heimdall.log");
        let file = open_log(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            dir: dir.into(),
            file,
            len,
            line_start: len == 0 || last_byte(&path) == Some(b'\n'),
            clock: local_time,
            last_line: None,
            repeats: 0,
            since: std::time::Instant::now(),
        })
    }
    /// Puts the time before each line that starts in `data`; a line split across reads gets it once. A whole line
    /// identical to the one before, within `REPEAT_WINDOW`, is held back and counted; the count is written as one line
    /// when a different line comes or the window ends.
    fn stamp(&mut self, data: &[u8]) -> Vec<u8> {
        let time = (self.clock)();
        let mut out = Vec::with_capacity(data.len() + time.len() * 2);
        let mut rest = data;
        while !rest.is_empty() {
            let end = rest.iter().position(|&b| b == b'\n').map(|i| i + 1);
            let (piece, whole) = match end { Some(n) => (&rest[..n], true), None => (rest, false) };
            rest = &rest[piece.len()..];
            if self.line_start && whole && piece != b"\n" {
                let now = std::time::Instant::now();
                if self.last_line.as_deref() == Some(piece) && now.duration_since(self.since) < REPEAT_WINDOW {
                    self.repeats += 1;
                    continue;
                }
                self.flush_repeats(&mut out, &time);
                if self.last_line.as_deref() != Some(piece) {
                    self.last_line = Some(piece.to_vec());
                }
                self.since = now;
                out.extend_from_slice(time.as_bytes());
                out.extend_from_slice(piece);
                continue;
            }
            // A blank line or a line split across reads: written as before, and never counted.
            self.flush_repeats(&mut out, &time);
            self.last_line = None;
            if self.line_start && piece != b"\n" {
                out.extend_from_slice(time.as_bytes());
            }
            out.extend_from_slice(piece);
            self.line_start = whole;
        }
        out
    }
    fn flush_repeats(&mut self, out: &mut Vec<u8>, time: &str) {
        if self.repeats > 0 {
            let seconds = self.since.elapsed().as_secs_f64().max(0.);
            out.extend_from_slice(format!("{time}(the line above repeated {} more times in {:.0} s)\n", self.repeats, seconds.max(1.)).as_bytes());
            self.repeats = 0;
        }
    }
    fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let stamped = self.stamp(data);
        self.write_raw(&stamped)
    }
    fn write_raw(&mut self, mut data: &[u8]) -> anyhow::Result<()> {
        while !data.is_empty() {
            if self.len >= LIMIT {
                self.file.sync_data()?;
                for n in (1..=COPIES).rev() {
                    let dest = self.dir.join(format!("heimdall.log.{n}"));
                    let source = if n == 1 {
                        self.dir.join("heimdall.log")
                    } else {
                        self.dir.join(format!("heimdall.log.{}", n - 1))
                    };
                    match std::fs::rename(source, dest) {
                        Ok(()) => (),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e.into()),
                    }
                }
                self.file = open_log(&self.dir.join("heimdall.log"))?;
                self.len = 0;
            }
            let n = data.len().min((LIMIT - self.len) as usize);
            self.file.write_all(&data[..n])?;
            self.len += n as u64;
            data = &data[n..];
        }
        // Diagnostics are sparse; persist each read, including archive errors and panic output.
        self.file.sync_data()?;
        Ok(())
    }
}
fn last_byte(path: &Path) -> Option<u8> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::End(-1)).ok()?;
    let mut byte = [0];
    file.read_exact(&mut byte).ok()?;
    Some(byte[0])
}
// The saved stderr may itself be a full pipe or a stalled filesystem. Isolate
// it from the capture reader: at most one fallback worker and eight queued 8 KiB chunks
// can be retained. Saturation drops best-effort output while capture keeps
// draining. Do not change O_NONBLOCK on dup(2): that would change the original
// open-file description shared with the launcher and inherited helpers too.
fn fallback(stderr: File, error: String) -> std::io::Result<std::sync::mpsc::SyncSender<Vec<u8>>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    std::thread::Builder::new()
        .name("heimdall-log-fallback".into())
        .spawn(move || {
            let mut stderr = stderr;
            // One failure record, before any best-effort captured output.
            if writeln!(stderr, "Heimdall durable log failed: {error}").is_err() {
                return;
            }
            for bytes in rx {
                if stderr.write_all(&bytes).is_err() {
                    break;
                }
            }
        })?;
    Ok(tx)
}
static DRAIN: OnceLock<Mutex<std::sync::mpsc::Receiver<()>>> = OnceLock::new();
extern "C" fn finish() {
    unsafe {
        libc::close(1);
        libc::close(2);
    }
    // Children can inherit these descriptors. Never wait for their lifetime at exit.
    if let Some(done) = DRAIN.get() {
        if let Ok(done) = done.lock() {
            let _ = done.recv_timeout(std::time::Duration::from_millis(250));
        }
    }
}
pub(super) fn capture(dir: &Path) -> anyhow::Result<()> {
    if DRAIN.get().is_some() {
        return Ok(());
    }
    let mut sink = Sink::open(dir)?;
    let mut fds = [0; 2];
    anyhow::ensure!(
        unsafe { libc::pipe(fds.as_mut_ptr()) } == 0,
        "diagnostic pipe: {}",
        std::io::Error::last_os_error()
    );
    let mut read = unsafe { File::from_raw_fd(fds[0]) };
    let write = unsafe { File::from_raw_fd(fds[1]) };
    // Helpers inherit stdout/stderr, not our private reader or its extra write fd.
    unsafe {
        libc::fcntl(read.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let original = unsafe { libc::dup(2) };
    anyhow::ensure!(original >= 0, "save stderr");
    let mut original_stderr = Some(unsafe { File::from_raw_fd(original) });
    unsafe {
        libc::fcntl(original, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let (done, drained) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("heimdall-log".into())
        .spawn(move || {
            let mut buf = [0; 8192];
            let mut failed = false;
            let mut forwarding = None;
            loop {
                match read.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !failed {
                            if let Err(e) = sink.write(&buf[..n]) {
                                failed = true;
                                forwarding =
                                    fallback(original_stderr.take().unwrap(), format!("{e:#}"))
                                        .ok();
                            }
                        }
                        if let Some(tx) = &forwarding {
                            let _ = tx.try_send(buf[..n].to_vec());
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // Held-back repeats are counted in the log before it closes.
            if !failed {
                let _ = sink.write(b"");
                let time = (sink.clock)();
                let mut out = vec![];
                sink.flush_repeats(&mut out, &time);
                let _ = sink.write_raw(&out);
            }
            let _ = done.send(());
        })?;
    DRAIN
        .set(Mutex::new(drained))
        .map_err(|_| anyhow::anyhow!("diagnostic reader already initialized"))?;
    anyhow::ensure!(
        unsafe { libc::dup2(write.as_raw_fd(), 1) } >= 0,
        "capture stdout"
    );
    anyhow::ensure!(
        unsafe { libc::dup2(write.as_raw_fd(), 2) } >= 0,
        "capture stderr"
    );
    unsafe {
        libc::atexit(finish);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_sink_child_fixture() {
        let Some(root) = std::env::var_os("HEIMDALL_FAILED_SINK_ROOT") else {
            return;
        };
        if std::env::var_os("HEIMDALL_FILL_ORIGINAL_STDERR").is_some() {
            // Saturate the parent's unread pipe before capture duplicates it.
            unsafe {
                let flags = libc::fcntl(2, libc::F_GETFL);
                assert_eq!(libc::fcntl(2, libc::F_SETFL, flags | libc::O_NONBLOCK), 0);
                let bytes = [b'x'; 4096];
                while libc::write(2, bytes.as_ptr().cast(), bytes.len()) > 0 {}
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                assert_eq!(libc::fcntl(2, libc::F_SETFL, flags), 0);
            }
        }
        let root = PathBuf::from(root);
        // Rotation must fail even on a writable test filesystem.
        let log = open_log(&root.join("heimdall.log")).unwrap();
        log.set_len(LIMIT).unwrap();
        std::fs::write(root.join(format!("heimdall.log.{}", COPIES - 1)), b"prior rotation").unwrap();
        std::fs::create_dir(root.join(format!("heimdall.log.{COPIES}"))).unwrap();
        capture(&root).unwrap();
        let bytes = [b'z'; 8192];
        for _ in 0..256 {
            std::io::stdout().write_all(&bytes).unwrap();
        }
        // Reaching this point proves the drain kept consuming well beyond pipe capacity.
        std::fs::write(root.join("drained"), b"yes").unwrap();
    }
    #[test]
    fn failed_sink_keeps_draining_with_an_unread_original_stderr_pipe() {
        for full in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "diagnostics::log::tests::failed_sink_child_fixture",
                    "--nocapture",
                ])
                .env("HEIMDALL_FAILED_SINK_ROOT", root.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped());
            if full {
                command.env("HEIMDALL_FILL_ORIGINAL_STDERR", "1");
            }
            let mut child = command.spawn().unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let timed_out = loop {
                if child.try_wait().unwrap().is_some() {
                    break false;
                }
                if std::time::Instant::now() >= until {
                    child.kill().unwrap();
                    break true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            let output = child.wait_with_output().unwrap();
            assert!(!timed_out, "sink failure blocked capture with full={full}");
            assert!(output.status.success(), "{:?}", output.status);
            assert!(root.path().join("drained").exists());
            if !full {
                assert_eq!(
                    String::from_utf8_lossy(&output.stderr)
                        .matches("Heimdall durable log failed:")
                        .count(),
                    1
                );
            }
        }
    }
    #[test]
    fn every_line_starts_with_the_local_time_once() {
        let d = tempfile::tempdir().unwrap();
        let mut sink = Sink::open(d.path()).unwrap();
        sink.clock = || "T ".into();
        // A line split across two pipe reads, then two lines in one read, then a partial line.
        sink.write(b"voice: Jarvis st").unwrap();
        sink.write(b"opped\n\nhealth: drama3 unavailable\nthread 'main' panicked").unwrap();
        sink.write(b" at x\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("heimdall.log")).unwrap(),
            "T voice: Jarvis stopped\n\nT health: drama3 unavailable\nT thread 'main' panicked at x\n"
        );
        // A reopened log continues where the last line ended.
        let mut sink = Sink::open(d.path()).unwrap();
        assert!(sink.line_start);
        sink.write(b"x").unwrap();
        let mut sink = Sink::open(d.path()).unwrap();
        assert!(!sink.line_start);
        sink.clock = || "T ".into();
        sink.write(b"y\nz").unwrap();
        assert!(std::fs::read_to_string(d.path().join("heimdall.log")).unwrap().ends_with("xy\nT z"));
        let real = local_time();
        assert_eq!(real.len(), "2026-10-19 18:48:32.123 ".len(), "{real}");
        assert_eq!(&real[4..5], "-");
    }
    /// "repeated identical log lines must collapse everywhere … one line with a count" (31 Oct 22:21:
    /// one failure logged 1,000+ times in 90 s).
    #[test]
    fn identical_lines_collapse_into_one_count_line() {
        let d = tempfile::tempdir().unwrap();
        let mut sink = Sink::open(d.path()).unwrap();
        sink.clock = || "T ".into();
        sink.write(b"Listen failed: evidence worker failed\n").unwrap();
        for _ in 0..411 {
            sink.write(b"Listen failed: evidence worker failed\n").unwrap();
        }
        // Several in one read count the same way.
        sink.write(b"Listen failed: evidence worker failed\nListen failed: evidence worker failed\n").unwrap();
        sink.write(b"agent: Ready\nagent: Ready\n\nagent: Ready\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("heimdall.log")).unwrap(),
            "T Listen failed: evidence worker failed\nT (the line above repeated 413 more times in 1 s)\n\
             T agent: Ready\nT (the line above repeated 1 more times in 1 s)\n\nT agent: Ready\n"
        );
        // A line split across reads is written whole and never counted.
        sink.write(b"agent: Re").unwrap();
        sink.write(b"ady\n").unwrap();
        assert!(std::fs::read_to_string(d.path().join("heimdall.log")).unwrap().ends_with("T agent: Ready\nT agent: Ready\n"));
    }
    #[test]
    fn burst_is_bounded_and_newest_tail_survives_rotation() {
        let d = tempfile::tempdir().unwrap();
        let mut sink = Sink::open(d.path()).unwrap();
        sink.clock = || "T ".into();
        sink.write(&vec![b'a'; (LIMIT as usize) * 5 + 17]).unwrap();
        sink.write(b"tail").unwrap();
        // The burst is one line: its time comes first, then 5 full files of it rotate out.
        assert_eq!(
            std::fs::read(d.path().join("heimdall.log")).unwrap().len(),
            2 + 17 + 4
        );
        assert!(std::fs::read(d.path().join("heimdall.log"))
            .unwrap()
            .ends_with(b"tail"));
        let files: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(files.len(), 1 + 5.min(COPIES));
        for f in files {
            assert!(f.unwrap().metadata().unwrap().len() <= LIMIT);
        }
    }
}
