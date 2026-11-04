//! CPU helper ownership, separate from capture and the live inference owner.
use anyhow::Result;
use heimdall_night::correction::{Client as Helper, HelperLost, Input, Output};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Instant,
};
enum Work {
    Job(String, Input, Instant),
    Unload,
}
/// A job whose helper is lost gets one more try on a fresh helper.
const ATTEMPTS: u32 = 2;
/// Consecutive helper losses wait 1, 2, 4 and 8 s before the next start; the fifth
/// pauses five minutes. One success clears it. Mirrors the live speaker helper.
#[derive(Default)]
struct Recovery {
    losses: u32,
    retry_at: Option<Instant>,
}
impl Recovery {
    fn wait(&mut self, now: Instant) -> Option<std::time::Duration> {
        let wait = self.retry_at.map(|at| at.saturating_duration_since(now)).filter(|d| !d.is_zero());
        if wait.is_none() {
            self.retry_at = None;
            if self.losses >= 5 { self.losses = 0; }
        }
        wait
    }
    fn lost(&mut self, now: Instant) -> std::time::Duration {
        self.losses += 1;
        let delay = std::time::Duration::from_secs(if self.losses >= 5 { 300 } else { 1 << (self.losses - 1) });
        self.retry_at = Some(now + delay);
        delay
    }
    fn succeeded(&mut self) {
        *self = Self::default();
    }
}
pub struct Runtime {
    tx: crossbeam_channel::Sender<Work>,
    pid: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}
impl Runtime {
    pub fn start(
        models: PathBuf,
        events: crossbeam_channel::Sender<crate::pipeline::Internal>,
        session: PathBuf,
        output: PathBuf,
        floor: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::bounded(2);
        let pid = Arc::new(AtomicU32::new(0));
        let child_pid = pid.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new().name("heimdall-speech-split".into()).spawn(move || {
            let _=heimdall_night::gate::background_thread();
            let mut helper:Option<Helper>=None;
            let mut recovery=Recovery::default();
            while let Ok(work)=rx.recv() {
                if stopped.load(Ordering::Acquire) {break;}
                match work {
                    Work::Unload=>{helper.take();child_pid.store(0,Ordering::Release);},
                    Work::Job(key,mut input,admitted)=> {
                        let mut attempts=0;
                        let result=loop {
                            // Back off before starting a helper after a loss; queued jobs wait here.
                            while let Some(wait)=recovery.wait(Instant::now()) {
                                if stopped.load(Ordering::Acquire) {break;}
                                std::thread::sleep(wait.min(std::time::Duration::from_millis(250)));
                            }
                            attempts+=1;
                            let result=(|| -> Result<Output> {
                                heimdall_archive::private_dir(&output)?;
                                heimdall_night::storage::require_at(&output,4*1024*1024,floor.load(Ordering::Acquire))?;
                                if input.samples.is_empty() {
                                    let (samples,start)=heimdall_night::intake::call_region(&session,&input.region)?;
                                    input.samples=samples;input.start_ns=start;
                                }
                                if helper.is_none() {helper=Some(Helper::start(&std::env::current_exe()?,&models)?);child_pid.store(helper.as_ref().unwrap().pid(),Ordering::Release);}
                                anyhow::ensure!(!stopped.load(Ordering::Acquire),"correction owner stopped");
                                let result=helper.as_mut().unwrap().run(&input)?;
                                let mut refs=vec![];
                                for (slot,track) in result.tracks.iter().enumerate() {
                                    heimdall_night::storage::require_at(&output,track.samples.len() as u64*4+1024,floor.load(Ordering::Acquire))?;
                                    let (audio,hash)=heimdall_night::job::save_audio(&output,&track.samples)?;
                                    refs.push(serde_json::json!({"slot":slot,"audio":audio,"audio_sha256":hash,"identity":track.identity,"start_ns":input.start_ns}));
                                }
                                heimdall_night::job::phase(&output,&input.region,"speech_split_cpu_output",serde_json::json!({"job":key,"tracks":refs,"cpu_seconds":result.cpu_seconds,"separator":heimdall_night::separator::MODEL_ID,"identity_model":speakers::embedding::MODEL_ID}))?;
                                Ok(result)
                            })();
                            match result {
                                Ok(output) => {recovery.succeeded();break Ok(output);}
                                Err(error) if error.downcast_ref::<HelperLost>().is_some() => {
                                    helper.take();child_pid.store(0,Ordering::Release);
                                    let delay=recovery.lost(Instant::now());
                                    if attempts<ATTEMPTS && !stopped.load(Ordering::Acquire) {continue;}
                                    break Err(format!("{error:#} (tried {attempts} times; next helper start in {} s)",delay.as_secs()));
                                }
                                // The request's own failure; the helper stays loaded.
                                Err(error) => break Err(format!("{error:#}")),
                            }
                        };
                        if events.send(crate::pipeline::Internal::Inference(Box::new(move |p|p.correction_ready(key,input,result,admitted)))).is_err() {break;}
                    }
                }
            }
            drop(helper);child_pid.store(0,Ordering::Release);
        })?;
        Ok(Self { tx, pid, stop })
    }
    pub fn submit(&self, key: String, input: Input, at: Instant) -> Result<()> {
        self.tx
            .try_send(Work::Job(key, input, at))
            .map_err(|e| anyhow::anyhow!("correction queue unavailable: {e}"))
    }
    pub(crate) fn helper_pid(&self) -> u32 {
        self.pid.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub(crate) fn held_queue() -> (Self, impl Fn() -> bool) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        assert!(tx.try_send(Work::Unload).is_ok());
        (
            Self {
                tx,
                pid: Arc::new(AtomicU32::new(0)),
                stop: Arc::new(AtomicBool::new(false)),
            },
            move || rx.try_recv().is_ok(),
        )
    }
    pub fn unload(&self) {
        let _ = self.tx.try_send(Work::Unload);
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid != 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn helper_losses_back_off_pause_then_start_over_and_success_clears() {
        let mut r = Recovery::default();
        let mut now = Instant::now();
        assert_eq!(r.wait(now), None);
        for seconds in [1, 2, 4, 8, 300] {
            assert_eq!(r.lost(now), Duration::from_secs(seconds));
            assert_eq!(r.wait(now), Some(Duration::from_secs(seconds)));
            assert!(r.wait(now + Duration::from_secs(seconds) - Duration::from_millis(1)).is_some());
            now += Duration::from_secs(seconds);
        }
        assert_eq!(r.wait(now), None, "the five-minute pause ends");
        assert_eq!(r.lost(now), Duration::from_secs(1), "after the pause the ladder starts over");
        r.succeeded();
        assert_eq!(r.wait(now), None);
        assert_eq!(r.lost(now), Duration::from_secs(1));
    }
}
