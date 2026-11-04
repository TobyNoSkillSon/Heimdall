//! The archive commit off the listen path (bug note 2026-10-27T16-57-38).
//!
//! Capture's intake thread resamples every source for listen and hands the native PCM to the archive. A commit waits
//! for the store's writer (heimdall_archive::live), which waits out another process holding the lock (27 Oct: up to
//! 60 s behind a compaction step). When the intake thread itself
//! committed, listen heard nothing for as long as the lock was held: on 27 Oct the Shure looked silent for 16 s four
//! times, the stall ladder restarted capture (which could not help: the lock was still held), then listening, then
//! relaunched Heimdall. Now the intake thread only queues archive work here, in order, and a committer thread owns
//! the sink: listen keeps hearing every source while a commit waits, and the archive still gets every sample once the
//! lock frees.
use crate::{ArchiveSink, NativeFormat, SourceId, SourceInfo};
use anyhow::Result;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::{
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

enum Op {
    Add(SourceInfo, u64),
    Remove(SourceId, u64),
    Gap(Option<SourceId>, u64, String),
    Append(SourceId, u64, NativeFormat, Vec<f32>, bool),
    Flush,
}

/// An `ArchiveSink` that never waits on the archive: each call queues its work for the committer thread and returns.
/// The committer's first error is returned by the next call (and by `close`), so a failing archive still stops capture
/// as it did when the intake thread committed. A held write lock is not a failure: the work stays queued and buffered
/// and commits once the lock is free.
pub(crate) struct Committer {
    tx: Option<Sender<Op>>,
    join: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Committer {
    pub(crate) fn start(sink: Box<dyn ArchiveSink>) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let error = Arc::new(Mutex::new(None));
        let failed = error.clone();
        let join = thread::Builder::new()
            .name("heimdall-archive-commit".into())
            .spawn(move || commit(sink, rx, failed))?;
        Ok(Self { tx: Some(tx), join: Some(join), error })
    }
    fn send(&self, op: Op) -> Result<()> {
        if let Some(e) = self.error.lock().unwrap().as_ref() {
            anyhow::bail!("{e}");
        }
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(op).is_ok());
        anyhow::ensure!(sent, "archive committer stopped");
        Ok(())
    }
}

fn commit(mut sink: Box<dyn ArchiveSink>, rx: Receiver<Op>, failed: Arc<Mutex<Option<String>>>) {
    let mut behind: Option<std::time::Instant> = None;
    loop {
        // Idle: the sink's own commit clock (about once a second) still commits what it buffered.
        let op = match sink.commit_wait() {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(op) => op,
                Err(RecvTimeoutError::Timeout) => Op::Flush,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(op) => op,
                Err(_) => break,
            },
        };
        let result = match op {
            Op::Add(info, ns) => sink.add_source(&info, ns),
            Op::Remove(id, ns) => sink.remove_source(&id, ns),
            Op::Gap(id, ns, detail) => sink.gap(id.as_ref(), ns, &detail),
            Op::Append(id, ns, format, samples, discontinuity) => sink.append(&id, ns, format, &samples, discontinuity),
            Op::Flush => sink.flush(),
        };
        match result {
            Ok(()) => {
                if let Some(at) = behind.take() {
                    eprintln!("Capture's archive caught up: {:.1} s behind the held lock", at.elapsed().as_secs_f64());
                }
            }
            // The archive's write lock held past the sink's wait: everything stays buffered in order and the next
            // commit tick tries again (31 Oct 22:22:31: this stopped capture, and with it Listening).
            Err(e) if heimdall_archive::embedding::database_busy(&e) => {
                if behind.is_none() {
                    eprintln!("Capture's archive waits for the lock: audio kept in order until it is free");
                    behind = Some(std::time::Instant::now());
                }
            }
            Err(e) => {
                *failed.lock().unwrap() = Some(format!("{e:#}"));
                return;
            }
        }
    }
    // The last commit waits out a held lock for a bounded time (the thread may outlive the capture's stop; Quit
    // settles it).
    let until = std::time::Instant::now() + FINAL_WAIT;
    loop {
        match sink.flush() {
            Ok(()) => return,
            Err(e) if heimdall_archive::embedding::database_busy(&e) && std::time::Instant::now() < until => {
                thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                *failed.lock().unwrap() = Some(format!("{e:#}"));
                return;
            }
        }
    }
}
/// How long a stopping capture's last commit waits for a held archive lock, beyond the sink's own wait.
const FINAL_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

impl ArchiveSink for Committer {
    fn add_source(&mut self, info: &SourceInfo, ns: u64) -> Result<()> {
        self.send(Op::Add(info.clone(), ns))
    }
    fn remove_source(&mut self, id: &SourceId, ns: u64) -> Result<()> {
        self.send(Op::Remove(id.clone(), ns))
    }
    fn gap(&mut self, id: Option<&SourceId>, ns: u64, detail: &str) -> Result<()> {
        self.send(Op::Gap(id.cloned(), ns, detail.into()))
    }
    fn append(&mut self, id: &SourceId, ns: u64, format: NativeFormat, samples: &[f32], discontinuity: bool) -> Result<()> {
        self.send(Op::Append(id.clone(), ns, format, samples.to_vec(), discontinuity))
    }
    fn flush(&mut self) -> Result<()> {
        self.send(Op::Flush)
    }
    /// The committer keeps the commit clock.
    fn commit_wait(&self) -> Option<std::time::Duration> {
        None
    }
    /// Waits for everything queued to commit (a capture stop: `Pipeline::close` bounds that wait for the live loop).
    fn close(&mut self) -> Result<()> {
        drop(self.tx.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        match self.error.lock().unwrap().as_ref() {
            Some(e) => anyhow::bail!("{e}"),
            None => Ok(()),
        }
    }
}

impl Drop for Committer {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceInfo, SourceKind};
    /// A sink whose flushes find the lock held `busy` times, then commit; it counts what it was given.
    struct Locked {
        busy: Arc<Mutex<usize>>,
        appended: Arc<Mutex<usize>>,
        flushed: Arc<Mutex<usize>>,
    }
    fn locked() -> anyhow::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), Some("database is locked".into())).into()
    }
    impl ArchiveSink for Locked {
        fn add_source(&mut self, _: &SourceInfo, _: u64) -> Result<()> { Ok(()) }
        fn remove_source(&mut self, _: &SourceId, _: u64) -> Result<()> { Ok(()) }
        fn gap(&mut self, _: Option<&SourceId>, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn append(&mut self, _: &SourceId, _: u64, _: NativeFormat, _: &[f32], _: bool) -> Result<()> {
            *self.appended.lock().unwrap() += 1;
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            let mut busy = self.busy.lock().unwrap();
            if *busy > 0 {
                *busy -= 1;
                return Err(locked());
            }
            *self.flushed.lock().unwrap() += 1;
            Ok(())
        }
        fn commit_wait(&self) -> Option<std::time::Duration> { None }
    }
    /// 31 Oct 22:22:31: "archive pipeline stopped: database is locked" ended capture, and Listening with it.
    #[test]
    fn a_held_lock_never_stops_capture_and_the_audio_commits_after_it() {
        let (busy, appended, flushed) = (Arc::new(Mutex::new(3)), Arc::new(Mutex::new(0)), Arc::new(Mutex::new(0)));
        let mut c = Committer::start(Box::new(Locked { busy: busy.clone(), appended: appended.clone(), flushed: flushed.clone() })).unwrap();
        let format = NativeFormat { sample_rate: 16000, channels: 1 };
        let id = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "held".into() }, format, 1).id;
        for i in 0..4 {
            c.append(&id, i, format, &[0.1; 16], false).unwrap();
            c.flush().unwrap();
        }
        c.close().unwrap();
        assert_eq!(*appended.lock().unwrap(), 4);
        assert_eq!(*busy.lock().unwrap(), 0, "three flushes found the lock held");
        assert!(*flushed.lock().unwrap() >= 1, "and a later one committed");
    }
}
