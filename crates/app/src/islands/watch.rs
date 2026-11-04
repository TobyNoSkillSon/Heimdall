//! The one watcher on the islands folder (night 1022): a kqueue on `islands/` and `responses/` (no polling). After
//! each burst of changes it publishes `IslandsEvent::Changed` when the islands differ from the last ones, then
//! consumes every response, acts on it (responses.rs), hands its line to the agent and publishes
//! `IslandsEvent::Response`.

use super::{Host, IslandsEvent};
use anyhow::{bail, Result};
use serde_json::{Map, Value};
use std::os::fd::RawFd;
use std::sync::Arc;
use std::time::Duration;

/// Changes closer together than this are one burst (a temp file plus rename, several islands written at once).
const SETTLE: Duration = Duration::from_millis(40);

type Publish = Arc<dyn Fn(&IslandsEvent) + Send + Sync>;

/// One pass: publish what changed, then consume and act on the responses. `last` is the previous island set.
pub fn step(h: &Host, last: &mut Option<Vec<(String, Map<String, Value>)>>, publish: &dyn Fn(&IslandsEvent)) {
    let all = h.root.all();
    if last.as_ref() != Some(&all) {
        h.remember(&all);
        publish(&IslandsEvent::Changed(all.clone()));
        *last = Some(all);
    }
    for r in h.root.take_responses() {
        if let Some((class, line)) = super::responses::handle(h, &r) {
            h.deliver(class, &line);
        }
        publish(&IslandsEvent::Response(r));
    }
}

pub struct Watcher {
    kq: RawFd,
    thread: Option<std::thread::JoinHandle<()>>,
}

const STOP: usize = 1;

fn kevent(kq: RawFd, changes: &[libc::kevent], out: &mut [libc::kevent], timeout: Option<Duration>) -> i32 {
    let ts = timeout.map(|t| libc::timespec { tv_sec: t.as_secs() as _, tv_nsec: t.subsec_nanos() as _ });
    // SAFETY: plain kevent(2) on buffers we own.
    unsafe {
        libc::kevent(kq, changes.as_ptr(), changes.len() as i32, out.as_mut_ptr(), out.len() as i32,
            ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec))
    }
}

fn vnode(fd: RawFd) -> libc::kevent {
    libc::kevent {
        ident: fd as usize, filter: libc::EVFILT_VNODE, flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_ATTRIB,
        data: 0, udata: std::ptr::null_mut(),
    }
}

fn user(flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent { ident: STOP, filter: libc::EVFILT_USER, flags, fflags, data: 0, udata: std::ptr::null_mut() }
}

impl Watcher {
    /// Watch `h.root` on a thread of its own; responses waiting from before the start are handled at once.
    pub fn spawn(h: Arc<Host>, publish: Publish) -> Result<Watcher> {
        h.root.ensure()?;
        // SAFETY: kqueue(2) and open(2) with O_EVTONLY; the descriptors live as long as the watcher thread.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 { bail!("kqueue failed") }
        let mut dirs = Vec::new();
        for d in [h.root.islands(), h.root.responses()] {
            let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes())?;
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY) };
            if fd < 0 { unsafe { libc::close(kq) }; bail!("cannot watch {}", d.display()) }
            dirs.push(fd);
        }
        let mut changes: Vec<libc::kevent> = dirs.iter().map(|fd| vnode(*fd)).collect();
        changes.push(user(libc::EV_ADD | libc::EV_CLEAR, 0));
        if kevent(kq, &changes, &mut [], None) < 0 {
            unsafe { libc::close(kq) };
            for fd in dirs { unsafe { libc::close(fd) }; }
            bail!("kevent registration failed");
        }
        let thread = std::thread::Builder::new().name("islands-watch".into()).spawn(move || {
            let mut last = None;
            step(&h, &mut last, publish.as_ref());
            let mut out = [user(0, 0); 8];
            'run: loop {
                let n = kevent(kq, &[], &mut out, None);
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue }
                    eprintln!("islands: watcher stopped: {}", std::io::Error::last_os_error());
                    break;
                }
                if out[..n as usize].iter().any(|e| e.filter == libc::EVFILT_USER) { break }
                // Let the burst finish, then read once.
                loop {
                    let n = kevent(kq, &[], &mut out, Some(SETTLE));
                    if n <= 0 { break }
                    if out[..n as usize].iter().any(|e| e.filter == libc::EVFILT_USER) { break 'run }
                }
                step(&h, &mut last, publish.as_ref());
            }
            for fd in dirs { unsafe { libc::close(fd) }; }
        })?;
        Ok(Watcher { kq, thread: Some(thread) })
    }

    pub fn stop(&mut self) {
        let Some(t) = self.thread.take() else { return };
        kevent(self.kq, &[user(0, libc::NOTE_TRIGGER)], &mut [], None);
        let _ = t.join();
        unsafe { libc::close(self.kq) };
    }
}

impl Drop for Watcher {
    fn drop(&mut self) { self.stop() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::islands::{files::Root, Class};
    use serde_json::json;
    use std::sync::mpsc;

    fn wait_for(rx: &mpsc::Receiver<IslandsEvent>, want: impl Fn(&IslandsEvent) -> bool) -> IslandsEvent {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let e = rx.recv_timeout(left).expect("no matching islands event within 5 s");
            if want(&e) { return e }
        }
    }

    #[test]
    fn publishes_changes_and_consumed_responses_from_a_temp_folder() {
        let d = tempfile::tempdir().unwrap();
        let h = Arc::new(Host::new(Root(d.path().to_path_buf()), None));
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = lines.clone();
        h.set_deliver(Arc::new(move |c: Class, t: &str| l.lock().unwrap().push((c, t.to_string()))));
        // A click from before the start is handled at once.
        h.root.ensure().unwrap();
        std::fs::write(h.root.responses().join("notes-100-1-1.json"), r#"{"island":"notes","kind":"open","value":"/tmp/x"}"#).unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let mut w = Watcher::spawn(h.clone(), Arc::new(move |e: &IslandsEvent| { let _ = tx.lock().unwrap().send(e.clone()); })).unwrap();
        wait_for(&rx, |e| matches!(e, IslandsEvent::Response(r) if r.get("kind").is_some_and(|k| k == "open")));
        h.root.put("tests", json!({"title": "Tests"}).as_object().unwrap()).unwrap();
        let e = wait_for(&rx, |e| matches!(e, IslandsEvent::Changed(all) if all.iter().any(|(id, _)| id == "tests")));
        let IslandsEvent::Changed(all) = e else { unreachable!() };
        assert_eq!(all[0].1["title"], "Tests");
        std::fs::write(h.root.responses().join(".tmp"), "{}").unwrap();
        std::fs::rename(h.root.responses().join(".tmp"), h.root.responses().join("tests-200-1-2.json")).unwrap();
        std::fs::write(h.root.responses().join(".tmp2"), r#"{"island":"tests","kind":"button","action":"go","value":"Go"}"#).unwrap();
        std::fs::rename(h.root.responses().join(".tmp2"), h.root.responses().join("tests-300-1-3.json")).unwrap();
        wait_for(&rx, |e| matches!(e, IslandsEvent::Response(r) if r.get("kind").is_some_and(|k| k == "button")));
        w.stop();
        assert!(std::fs::read_dir(h.root.responses()).unwrap().flatten().all(|e| e.file_name().to_string_lossy().starts_with('.')));
        let lines = lines.lock().unwrap();
        assert!(lines.contains(&(Class::Context, "User opened /tmp/x from the island 'notes'.".into())), "{lines:?}");
        assert!(lines.contains(&(Class::Answer, "User clicked 'Go' on the island 'Tests' (action go).".into())), "{lines:?}");
    }

    #[test]
    fn step_publishes_changed_only_when_the_islands_differ() {
        let d = tempfile::tempdir().unwrap();
        let h = Host::new(Root(d.path().to_path_buf()), None);
        let count = std::cell::Cell::new(0);
        let publish = |e: &IslandsEvent| if matches!(e, IslandsEvent::Changed(_)) { count.set(count.get() + 1) };
        let mut last = None;
        step(&h, &mut last, &publish);
        step(&h, &mut last, &publish);
        assert_eq!(count.get(), 1);
        h.root.put("a", json!({"title": "A"}).as_object().unwrap()).unwrap();
        step(&h, &mut last, &publish);
        assert_eq!(count.get(), 2);
    }
}
