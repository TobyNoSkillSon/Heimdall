//! Buffered live last-hour indexing and AC/idle backlog, outside the capture/pipeline threads.
use anyhow::Result;
use heimdall_archive::{embedding::Service, timeline::{self, BackfillGate}};
use std::{path::PathBuf, sync::{Arc,Mutex,Condvar}, time::Duration};

pub struct Jobs {
    stop:Arc<(Mutex<bool>,Condvar)>,
    thread:Option<std::thread::JoinHandle<()>>,
    status:Arc<Mutex<Option<String>>>,
}
pub fn backfill_gate() -> BackfillGate {
    heimdall_night::gate::state().map(|s|BackfillGate{on_ac:s.on_ac,idle_seconds:s.idle_seconds})
        .unwrap_or(BackfillGate{on_ac:false,idle_seconds:f64::NAN})
}
impl Jobs {
    pub fn start(root:PathBuf,service:Arc<Service>,floor:Arc<std::sync::atomic::AtomicU64>) -> Result<Self> {
        let stop=Arc::new((Mutex::new(false),Condvar::new()));let state=stop.clone();
        let status=Arc::new(Mutex::new(None));let report=status.clone();
        let thread=std::thread::Builder::new().name("heimdall-embeddings".into()).spawn(move|| {
            let _=heimdall_night::gate::background_thread();
            let model=heimdall_archive::embedding::model();
            let mut buffer=timeline::LiveBuffer::new(heimdall_archive::embedding::LIVE_BUFFER);
            loop {
                let alive=|| !*state.0.lock().unwrap() && crate::archive::free_bytes(&root).is_ok_and(|n|n>floor.load(std::sync::atomic::Ordering::Acquire)+16*1024*1024);
                if !*state.0.lock().unwrap() {
                    let result=(|| -> Result<usize> {
                        // Live chunks wait in the buffer (`LIVE_BUFFER`, zero: none), then flush
                        // group by group until none are pending. Plain SQL, outside the provider lock.
                        let flush=buffer.due(&root,&model,heimdall_archive::wall_ns())?;
                        // Query waiters win the next turn; release the provider after each
                        // group (`limit` 1 = one worker request of up to eight same-kind
                        // chunks), so a query waits for at most one group.
                        let Some(mut provider)=service.try_index()? else {return Ok(0);};
                        if flush {
                            let live=timeline::embed_live(&root,&mut *provider,1,heimdall_archive::wall_ns(),&mut ||alive())?;
                            buffer.embedded(live);
                            if live>0 {return Ok(live);}
                        }
                        // Recheck live work between every pair of backlog groups.
                        let backlog=timeline::embed_batch(&root,&mut *provider,1,&mut || {
                            if alive() {backfill_gate()}else{BackfillGate{on_ac:false,idle_seconds:f64::NAN}}
                        })?;
                        // The model stays resident; after a quiet spell it frees scratch once.
                        if backlog==0 {provider.trim_if_idle(heimdall_archive::embedding::IDLE_TRIM);}
                        Ok(backlog)
                    })();
                    match result {
                        Ok(n)=>{
                            match timeline::embedding_error_status(&root,&model) {
                                Ok(status)=>*report.lock().unwrap()=status,
                                Err(e) if heimdall_archive::embedding::database_busy(&e)=>{},
                                Err(e)=>*report.lock().unwrap()=Some(timeline::embedding_failure_status(&format!("{e:#}"))),
                            }
                            if n>0 {continue;}
                        },
                        // The chunk is still unindexed; the next scheduled pass retries.
                        // The menu shows only plain "Search indexing ..." sentences; the
                        // next successful pass replaces this from the archive's error rows.
                        Err(e) if heimdall_archive::embedding::database_busy(&e)=>{},
                        Err(e)=>{
                            eprintln!("embedding job failed: {e:#}");
                            *report.lock().unwrap()=Some(timeline::embedding_failure_status(&format!("{e:#}")));
                        },
                    }
                }
                let guard=state.0.lock().unwrap();if *guard {break;}
                let (guard,_)=state.1.wait_timeout(guard,Duration::from_secs(30)).unwrap();
                if *guard {break;}
            }
            if let Ok(mut provider)=service.0.lock() {provider.unload();}
        })?;
        Ok(Self{stop,thread:Some(thread),status})
    }
    pub fn status(&self)->Option<String> {self.status.lock().unwrap().clone()}
}
impl Drop for Jobs {
    fn drop(&mut self) {
        *self.stop.0.lock().unwrap()=true;self.stop.1.notify_all();
        if let Some(thread)=self.thread.take() {let _=thread.join();}
    }
}
