//! EmbeddingGemma 2: bounded, offline MLX helper and archive-native media loading.
//! The owning Rust process keeps reader locks, gates, clocks and publication authority.
use crate::{audio, records, store, timeline::{self, Chunk, Embedder, Model}};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering}, mpsc},
    time::{Duration, Instant},
};

pub const MODEL_ID: &str = "google/embeddinggemma-2";
pub const MODEL_VERSION: &str = "914f7f89142e33e77833254d9c9b90c3cef7303b:mlx-swift-c53d7d48:bf16:native-v2:input-v1";
pub const DIMENSIONS: usize = 768;
pub fn model() -> Model {Model{id:MODEL_ID.into(),version:MODEL_VERSION.into(),dimensions:DIMENSIONS}}
/// Live chunks wait until the oldest is this old, then embed in one burst (
/// "a 10 or 15 minute buffer and then do it all at once"). Zero: embed whenever pending.
/// Off since the idle benchmark of 4 Nov (measured, M5 Max on AC): a 12-minute buffer
/// cost 1.60 J per item in bursts against 0.89 J trickled, idle noise 1.17 J, so it saved
/// nothing. Semantic search still shows chunks waiting mid-embedding as literal matches.
pub const LIVE_BUFFER: Duration = Duration::ZERO;
/// Extra look-back for those literal matches: a flush takes time to drain.
pub const LIVE_BUFFER_MARGIN: Duration = Duration::from_secs(5 * 60);
/// SQLite contention can occur while preparing media or publishing a result.
/// Do not turn an ordinary concurrent writer into a sticky chunk/model failure.
pub fn database_busy(error:&anyhow::Error) -> bool {
    error.chain().any(|e| matches!(e.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(code,_)) if matches!(code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)))
}
const TIMEOUT: Duration = Duration::from_secs(60);
/// Capture never recorded any sample inside the chunk's audio ranges (native overflow
/// holes, a tap that closed). Once capture has passed the chunk this is permanent, so
/// the indexer retires it instead of retrying. The text keeps the historical wording
/// that `repair_speaker_samples` and the health query match on.
#[derive(Debug, Clone)]
pub struct NoCapturedAudio { pub chunk:String, pub source:String, pub start_ns:u64, pub end_ns:u64 }
impl std::fmt::Display for NoCapturedAudio {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f,"audio chunk {} has no captured samples for {} [{},{})",self.chunk,self.source,self.start_ns,self.end_ns)
    }
}
impl std::error::Error for NoCapturedAudio {}
/// Not attempted because a search query is waiting: the indexer leaves the chunk for its
/// next pass without recording a failure.
#[derive(Debug, Clone)]
pub struct Deferred;
impl std::fmt::Display for Deferred {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>) -> std::fmt::Result {write!(f,"embedding deferred for a waiting query")}
}
impl std::error::Error for Deferred {}
pub fn deferred(error:&anyhow::Error) -> bool {error.chain().any(|e|e.is::<Deferred>())}
/// A fresh worker's self-test and weight load, counted from its spawn. Until it has answered
/// once it is starting: a caller's cancel drops only that request, never the process.
const STARTUP: Duration = Duration::from_secs(240);
pub fn no_captured_audio(error:&anyhow::Error) -> bool {
    error.chain().any(|e|e.is::<NoCapturedAudio>())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag="type",rename_all="snake_case")]
pub enum Part {
    Text { text:String },
    Audio { path:PathBuf },
    Image { path:PathBuf },
    Video { frames:Vec<PathBuf> },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub parts:Vec<Part>,
    pub query:bool,
    pub weight:f32,
}
impl Input {
    pub fn text(text:&str,query:bool) -> Self {
        Self {parts:vec![Part::Text{text:text.to_owned()}],query,weight:1.}
    }
}

/// Caller owns `destination` for exactly the lifetime of the helper request/benchmark.
/// Audio clips preserve every disjoint captured interval, never fill missing callbacks.
pub fn prepare_chunk(root:&Path,c:&Chunk,destination:&Path) -> Result<Vec<Input>> {
    prepare_chunk_while(root,c,destination,&mut ||true)
}
fn prepare_chunk_while(root:&Path,c:&Chunk,destination:&Path,more:&mut dyn FnMut()->bool) -> Result<Vec<Input>> {
    ensure!(more(),"embedding media preparation cancelled");
    crate::private_dir(destination)?;
    let db=store::open(root)?;
    let mut input=Input {parts:vec![],query:false,weight:1.};
    if !c.text.is_empty() {input.parts.push(Part::Text{text:c.text.clone()});}
    if c.role=="command" {
        // Candidate/merged/abandoned records can have no transcript. Embed their
        // actual structured command evidence, not an invented spoken quote.
        let fields=json!({"phase":c.details["phase"],"phrase":c.details["candidate"]["phrase_guess"],"approved":c.details["approved"],"intent":c.details["intent"],"selection":c.details["selection"]});
        input.parts.push(Part::Text{text:format!("\n{fields}")});
    }
    let _reader=crate::compact::reader_lock(root)?;
    let mut images=BTreeSet::new();
    let mut video_frames=BTreeSet::new();
    let mut spans:BTreeMap<String,Vec<(u64,u64)>>=BTreeMap::new();
    match c.kind.as_str() {
        "text"=>{},
        "speech_audio"|"other_audio"=> {
            let ranges=if c.kind=="other_audio" {timeline::sound_ranges(&db,c)?}else{vec![(c.start_ns,c.end_ns)]};
            spans.insert(c.source.clone(),ranges);
        },
        "screen"=> {
            for l in &c.links {
                if l.relation=="image" {images.insert(l.target.clone());}
                if l.relation=="video_frame" {video_frames.insert(l.target.clone());}
            }
            ensure!(!images.is_empty() || !video_frames.is_empty(),"screen state has no representative frame");
            if let Some(title)=c.details["window_title"].as_str().filter(|s|!s.is_empty()) {
                input.parts.push(Part::Text{text:format!("\n{title}")});
            }
        },
        "moment"=> {
            // Members include overlapping utterance/turn projections. Union per source so
            // overlap isn't counted twice. Clip each member in its own host/wall geometry.
            for l in c.links.iter().filter(|l|l.relation=="member") {
                ensure!(more(),"embedding media preparation cancelled");
                // A member retracted since the moment was loaded: the same transaction
                // rewrote the moment, whose new content is selected again.
                let Some(mut child)=timeline::find(&db,&l.target)? else {continue;};
                // The model-free projection links text members to the window. Their
                // paired speech chunk owns the waveform of that same capture span.
                if child.kind=="text" && matches!(child.role.as_str(),"utterance"|"turn") {
                    let paired:Option<String>=db.query_row("SELECT c.id FROM main.timeline_chunks c JOIN main.chunk_links l ON l.chunk_id=c.id WHERE l.relation='text_chunk' AND l.target=?1 AND c.kind='speech_audio'",[&child.id],|r|r.get(0)).optional()?;
                    if let Some(id)=paired {child=timeline::load(&db,&id)?;}
                }
                if child.kind=="screen" {
                    for l in &child.links {
                        if l.relation=="image" {images.insert(l.target.clone());}
                        if l.relation=="video_frame" {video_frames.insert(l.target.clone());}
                    }
                    ensure!(child.links.iter().any(|l|matches!(l.relation.as_str(),"image"|"video_frame")),"screen member has no representative frame");
                }
                if matches!(child.kind.as_str(),"speech_audio"|"other_audio") {
                    let a=c.start_wall_ns.max(child.start_wall_ns);
                    let b=c.end_wall_ns.min(child.end_wall_ns);
                    if b<=a {continue;}
                    let start=child.start_ns+a-child.start_wall_ns;
                    let end=child.start_ns+b-child.start_wall_ns;
                    let ranges=if child.kind=="other_audio" {timeline::sound_ranges(&db,&child)?}else{vec![(start,end)]};
                    for (x,y) in ranges {
                        if x.max(start)<y.min(end) {spans.entry(child.source.clone()).or_default().push((x.max(start),y.min(end)));}
                    }
                }
            }
            // The chunk's quote/context remain verbatim. No generated summaries.
            for context in c.details["context"].as_array().into_iter().flatten() {
                if let Some(title)=context["window_title"].as_str().filter(|s|!s.is_empty()) {
                    input.parts.push(Part::Text{text:format!("\n{title}")});
                }
            }
        },
        _=>anyhow::bail!("unsupported chunk modality {}",c.kind),
    }
    for (i,id) in images.iter().enumerate() {
        ensure!(more(),"embedding media preparation cancelled");
        let (_,bytes)=records::image_bytes(&db,root,id)?;
        let path=destination.join(format!("image-{i}.png"));
        // The image decoder detects native PNG/JPEG/WebP bytes regardless of suffix.
        std::fs::write(&path,bytes)?;
        input.parts.push(Part::Image{path});
    }
    for (i,reference) in video_frames.iter().enumerate() {
        ensure!(more(),"embedding media preparation cancelled");
        let reference:Value=serde_json::from_str(reference).context("video frame reference")?;
        let segment=reference["segment"].as_i64().context("video segment reference")?;
        let host=reference["host_ns"].as_u64().context("video frame clock")?;
        let path=destination.join(format!("video-frame-{i}.png"));
        crate::video::representative_png(root,c.session,segment,host,&path,more)?;
        input.parts.push(Part::Image{path});
    }
    let mut audio_index=0;
    if !spans.is_empty() {
        let name:String=db.query_row("SELECT name FROM main.sessions WHERE id=?1",[c.session],|r|r.get(0))?;
        let mut cache=audio::ObjectCache::default();
        let mut missing=None;
        for (source,ranges) in spans {
            let ranges=union_ranges(ranges);
            for (start,end) in ranges {
                let view=crate::View::session(&root.join(&name))?;
                view.capture_span_while(&source,start,end,more)?;
                let mut clip=c.clone();clip.source=source.clone();clip.kind="speech_audio".into();clip.start_ns=start;clip.end_ns=end;
                let refs=timeline::audio_refs_from_view(&view,&clip)?;
                // Capture flushes project one span per source from first to last
                // callback, bridging overflow holes; speech subtraction can leave a range
                // wholly inside such a hole. Embed the audio that exists.
                if refs.is_empty() {
                    missing.get_or_insert(NoCapturedAudio{chunk:c.id.clone(),source:source.clone(),start_ns:start,end_ns:end});
                    continue;
                }
                let mut at=0;
                let mut samples=vec![];
                let mut format=(16000,1);
                let mut segment=Value::Null;
                for reference in refs {
                    ensure!(more(),"embedding media preparation cancelled");
                    let host=reference["host_ns"].as_u64().context("audio reference clock")?;
                    let pcm=audio::read_block_cached(&view.db,reference["block_id"].as_i64().context("block id")?,&mut cache)?;
                    let frames=pcm.samples.len()/pcm.channels as usize;
                    let a=start.max(host).max(at);
                    let b=end.min(host+frames as u64*1_000_000_000/pcm.rate as u64);
                    if b<=a {continue;}
                    let first=((a-host) as u128*pcm.rate as u128/1_000_000_000) as usize;
                    let last=(((b-host) as u128*pcm.rate as u128/1_000_000_000) as usize).min(frames);
                    if first>=last {continue;}
                    // A gap/discontinuity ends this part. Do not concatenate across missing
                    // time, pad silence, or pretend the waveform is continuous.
                    if at!=0 && (host>at+1_000_000 || format!=(pcm.rate,pcm.channels) || segment!=reference["segment"] || reference["discontinuity"]==true) {
                        write_audio(destination,&mut input.parts,&mut samples,&mut audio_index,format)?;
                    }
                    format=(pcm.rate,pcm.channels);segment=reference["segment"].clone();
                    samples.extend_from_slice(&pcm.samples[first*pcm.channels as usize..last*pcm.channels as usize]);at=b;
                    // A legacy rolled final may span many minutes. Bound host PCM
                    // before resampling; the worker packs all parts into context windows.
                    if samples.len()>=pcm.rate as usize*pcm.channels as usize*120 {
                        write_audio(destination,&mut input.parts,&mut samples,&mut audio_index,format)?;
                    }
                }
                write_audio(destination,&mut input.parts,&mut samples,&mut audio_index,format)?;
            }
        }
        if audio_index==0 {if let Some(missing)=missing {return Err(missing.into());}}
        ensure!(audio_index>0,"audio input had no decodable interval");
    }
    ensure!(!input.parts.is_empty(),"chunk has no supported embedding content");
    Ok(vec![input])
}
fn union_ranges(mut ranges:Vec<(u64,u64)>) -> Vec<(u64,u64)> {
    ranges.sort_unstable();let mut out:Vec<(u64,u64)>=vec![];
    for (a,b) in ranges {if a>=b {continue;}if let Some(last)=out.last_mut().filter(|last|a<=last.1) {last.1=last.1.max(b);}else{out.push((a,b));}}
    out
}
fn write_audio(dir:&Path,parts:&mut Vec<Part>,samples:&mut Vec<f32>,i:&mut usize,format:(u32,u16)) -> Result<()> {
    if samples.is_empty() {return Ok(());}
    // A single callback/reference can itself exceed the reader's 120 s flush
    // threshold. Bound each serialized part after resampling, preserving the tail.
    // Native/Python workers pack these parts into <=6000-token windows (25/s).
    let mono=audio::mono_16k(samples,format.0,format.1)?;
    for chunk in mono.chunks(16000*120) {
        let path=dir.join(format!("audio-{i}.f32"));
        std::fs::write(&path,audio::pcm_bytes(chunk))?;
        parts.push(Part::Audio{path});*i+=1;
    }
    samples.clear();Ok(())
}

/// Same-kind chunks per worker request. Idle benchmark, 4 Nov (measured, M5 Max on AC, per
/// item one at a time → batched): text 5.3 → 2.5 ms and screens 73 → 49 ms gain, but speech
/// 35 → 120 ms and other sounds 28 → 54 ms lose, so audio kinds go one per request.
pub const BATCH_TEXT: usize = 8;
pub const BATCH_OTHER_AUDIO: usize = 1;
pub const BATCH_SPEECH_AUDIO: usize = 1;
pub const BATCH_SCREEN: usize = 4;
pub const BATCH_MOMENT: usize = 1;
/// The worker accepts at most this many items per `op:"batch"` request.
pub const MAX_BATCH: usize = 8;
pub fn batch_size(kind:&str) -> usize {
    match kind {
        "text"=>BATCH_TEXT, "other_audio"=>BATCH_OTHER_AUDIO, "speech_audio"=>BATCH_SPEECH_AUDIO,
        "screen"=>BATCH_SCREEN, "moment"=>BATCH_MOMENT, _=>1,
    }.clamp(1,MAX_BATCH)
}
/// With no indexing work for this long, the resident worker frees its MLX scratch once.
pub const IDLE_TRIM: Duration = Duration::from_secs(120);
/// Consecutive crashes of one worker build before it runs as plain MLX for good.
const SAFE_AFTER_CRASHES: u32 = 2;
const CRASH_FILE: &str = "rust-worker-crashes.json";
const INDEX_LOG_EVERY: Duration = Duration::from_secs(60);

/// Darwin background state (CPU, I/O and GPU throttled) for the worker while it indexes.
/// A failure is logged once; the worker then just runs at normal priority.
pub fn set_background(pid:i32,on:bool) -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if pid<=0 {return false;}
    #[cfg(target_os="macos")]
    {
        // SAFETY: plain syscall on a pid; no memory is passed.
        let r=unsafe {libc::setpriority(libc::PRIO_DARWIN_PROCESS,pid as libc::id_t,if on {libc::PRIO_DARWIN_BG} else {0})};
        if r==0 {return true;}
        if !WARNED.swap(true,Ordering::Relaxed) {
            eprintln!("embedding worker priority unchanged (background={on}): {}",std::io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os="macos"))]
    let _=(on,&WARNED);
    false
}

#[derive(Debug)]
enum Fault { Closed, Protocol(String) }
struct Process {
    child:Child,
    stdin:ChildStdin,
    replies:mpsc::Receiver<Result<Value,Fault>>,
}
impl Drop for Process {fn drop(&mut self) {let _=self.child.kill();let _=self.child.wait();}}
/// How a request ended, which decides whether the process survives.
enum Failure {
    /// The worker answered with an error; it is healthy and stays resident.
    Keep(anyhow::Error),
    /// Cancelled, timed out or spoke nonsense: kill it, but it did not crash.
    Drop(anyhow::Error),
    /// Cancelled while the worker was still starting: it keeps starting, its reply is discarded.
    Abandon(anyhow::Error),
    /// Exited or closed stdout with a request pending.
    Crash(anyhow::Error),
}
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct Crashes { binary:String, crashes:u32, safe:bool }
fn binary_identity(path:&Path) -> String {
    std::fs::metadata(path).map(|m| {
        let mtime=m.modified().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0,|d|d.as_nanos());
        format!("{}:{mtime}",m.len())
    }).unwrap_or_default()
}
#[derive(Default)]
struct IndexStats {
    since:Option<Instant>, items:usize, failed:usize, kinds:BTreeMap<String,usize>, busy:Duration,
    requests:usize, worker_seconds:f64, tokens:u64, windows:u64,
}
impl IndexStats {
    /// The worker's own accounting for one request; absent fields count as zero.
    fn worker(&mut self,stats:&Value) {
        self.requests+=1;self.worker_seconds+=stats["seconds"].as_f64().unwrap_or(0.);
        self.tokens+=stats["tokens"].as_u64().unwrap_or(0);self.windows+=stats["windows"].as_u64().unwrap_or(0);
    }
    fn line(&self) -> String {
        let wall=self.since.map_or(0.,|s|s.elapsed().as_secs_f64());
        let kinds=self.kinds.iter().map(|(k,n)|format!("{k} {n}")).collect::<Vec<_>>().join(", ");
        let busy=self.busy.as_secs_f64();
        format!("embedding index: {} items in {wall:.0}s ({kinds}; {} failed), {:.2} items/s while busy, {:.0} ms/item mean; worker {} requests, {:.1}s, {} tokens, {} windows",
            self.items,self.failed,self.items as f64/busy.max(1e-9),busy*1000./self.items.max(1) as f64,self.requests,self.worker_seconds,self.tokens,self.windows)
    }
}
pub struct MlxEmbedder {
    directory:PathBuf,
    state:Option<PathBuf>,
    process:Option<Process>,
    sequence:u64,
    pid:Arc<AtomicI32>,
    crashes:Crashes,
    /// A request ran since the last trim; `last_used` is when the last one finished.
    dirty:bool,
    last_used:Instant,
    stats:IndexStats,
    /// Search queries waiting for the provider (shared with `Service`).
    waiters:Arc<AtomicUsize>,
    /// The worker has not answered yet since it started.
    fresh:bool,
    /// When the running worker was spawned (start-up allowance).
    spawned:Instant,
    /// Requests cancelled while the worker was starting, oldest first, with their own timeout.
    /// The worker still answers them in order; those replies are discarded.
    abandoned:VecDeque<(u64,Duration)>,
    /// The last request ended with the worker exiting.
    last_crash:bool,
    /// Off while single items are retried after a crashed batch: one poison input must not
    /// count twice toward safe mode.
    count_crashes:bool,
}
impl MlxEmbedder {
    /// Cheap and lazy: model loading happens only on the first admitted job/query.
    pub fn new(model_root:&Path) -> Self {Self::with_state(model_root,None)}
    /// `state` is the worker's private persistent directory (self-test verdicts, crash count).
    pub fn with_state(model_root:&Path,state:Option<PathBuf>) -> Self {
        Self {directory:model_root.join("embeddinggemma2"),state,process:None,sequence:0,pid:Arc::new(AtomicI32::new(0)),
            crashes:Crashes::default(),dirty:false,last_used:Instant::now(),stats:IndexStats::default(),
            waiters:Arc::new(AtomicUsize::new(0)),fresh:false,spawned:Instant::now(),abandoned:VecDeque::new(),
            last_crash:false,count_crashes:true}
    }
    /// The running worker's pid, 0 when none. Shared so a waiting query can lift
    /// the worker out of background state while an index request is in flight.
    pub fn pid_handle(&self) -> Arc<AtomicI32> {self.pid.clone()}
    pub fn pid(&self) -> Option<u32> {self.process.as_ref().map(|p|p.child.id())}
    pub fn running(&self) -> bool {self.process.is_some()}
    pub fn safe_mode(&self) -> bool {self.crashes.safe}
    fn worker(&self) -> PathBuf {self.directory.join("native/heimdall-embedding-worker")}
    fn load_crashes(&mut self,binary:String) {
        if self.crashes.binary==binary {return;}
        let saved=self.state.as_ref().and_then(|s|std::fs::read(s.join(CRASH_FILE)).ok())
            .and_then(|b|serde_json::from_slice::<Crashes>(&b).ok()).filter(|c|c.binary==binary);
        // A new build starts with a clean record; its predecessor's crashes say nothing about it.
        self.crashes=saved.unwrap_or(Crashes{binary,..Default::default()});
    }
    fn save_crashes(&self) {
        let Some(state)=&self.state else {return;};
        let result=(|| -> Result<()> {
            crate::private_dir(state)?;
            let temp=state.join(format!("{CRASH_FILE}.tmp"));
            std::fs::write(&temp,serde_json::to_vec(&self.crashes)?)?;
            std::fs::rename(temp,state.join(CRASH_FILE))?;Ok(())
        })();
        if let Err(e)=result {eprintln!("embedding worker crash record not saved: {e:#}");}
    }
    fn crashed(&mut self) {
        self.crashes.crashes+=1;
        if self.crashes.crashes>=SAFE_AFTER_CRASHES && !self.crashes.safe {
            self.crashes.safe=true;
            eprintln!("EmbeddingGemma 2 worker crashed {} times in a row; it now runs in safe mode (plain MLX) until it is rebuilt",self.crashes.crashes);
        }
        self.save_crashes();
    }
    fn succeeded(&mut self) {
        if self.crashes.crashes>0 {self.crashes.crashes=0;self.save_crashes();}
    }
    fn stop(&mut self) {self.pid.store(0,Ordering::SeqCst);self.process.take();self.abandoned.clear();}
    /// Without blocking, discard the replies to abandoned requests that have arrived.
    fn settle(&mut self) {
        let Some(p)=self.process.as_mut() else {return;};
        while let Some(&(id,_))=self.abandoned.front() {
            match p.replies.try_recv() {
                Err(mpsc::TryRecvError::Empty)=> {
                    // A start nobody waits on any more still has its bound: overdue means hung.
                    let budget=STARTUP+self.abandoned.iter().map(|a|a.1).sum::<Duration>();
                    if self.spawned.elapsed()>=budget {eprintln!("embedding worker start overdue; stopping it");self.stop();}
                    return;
                },
                Ok(Ok(reply)) if reply["id"].as_u64()==Some(id)=>{self.abandoned.pop_front();self.fresh=false;},
                Ok(Err(Fault::Closed))|Err(mpsc::TryRecvError::Disconnected)=>{
                    let signal=self.died_by_signal();
                    self.stop();
                    if signal {self.crashed();}
                    return;
                },
                _=>{self.stop();return;},
            }
        }
    }
    /// A closed pipe is a crash only when the worker died from a signal (Metal abort, trap,
    /// segfault). An orderly exit, such as the start-up memory refusal, is not.
    fn died_by_signal(&mut self) -> bool {
        use std::os::unix::process::ExitStatusExt;
        let Some(p)=self.process.as_mut() else {return false;};
        for _ in 0..100 {
            match p.child.try_wait() {
                Ok(Some(status))=>return status.signal().is_some(),
                Ok(None)=>std::thread::sleep(Duration::from_millis(10)),
                Err(_)=>return false,
            }
        }
        false
    }
    fn start(&mut self) -> Result<()> {
        if self.process.is_some() {return Ok(());}
        let worker=self.worker();
        ensure!(worker.is_file() && self.directory.join("native/mlx.metallib").is_file() && self.directory.join("weights/model.safetensors").is_file(),"EmbeddingGemma 2 runtime assets unavailable at {}",self.directory.display());
        self.load_crashes(binary_identity(&worker));
        let mut command=Command::new(worker);
        command.arg(self.directory.join("weights")).env_remove("HEIMDALL_EMBED_STATE").env_remove("HEIMDALL_EMBED_SAFE")
            // Lab switches never reach a serving helper: no part switches, no mixed precision (serving stays BF16).
            .env_remove("HEIMDALL_EMBED_LAB").env_remove("HEIMDALL_EMBED_QUANT");
        if let Some(state)=&self.state {
            match crate::private_dir(state) {
                Ok(())=>{command.env("HEIMDALL_EMBED_STATE",state);},
                Err(e)=>eprintln!("embedding worker state directory unavailable: {e:#}"),
            }
        }
        if self.crashes.safe {command.env("HEIMDALL_EMBED_SAFE","1");}
        let mut child=command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().context("start offline embedding worker")?;
        let stdin=child.stdin.take().context("worker stdin")?;
        let stdout=child.stdout.take().context("worker stdout")?;
        let (tx,replies)=mpsc::sync_channel(1);
        std::thread::Builder::new().name("embedding-replies".into()).spawn(move|| {
            let mut reader=BufReader::new(stdout);
            loop {
                let mut line=String::new();
                // A batch of eight 768d JSON vectors is ~130 KiB. Bound even a corrupt/noisy worker.
                let result=match reader.by_ref().take(4*1024*1024).read_line(&mut line) {
                    Err(_)|Ok(0)=>Err(Fault::Closed),
                    Ok(_) if !line.ends_with('\n')=>Err(Fault::Protocol("embedding worker reply truncated".into())),
                    Ok(_)=>serde_json::from_str(&line).map_err(|e|Fault::Protocol(format!("embedding worker reply: {e}"))),
                };
                let failed=result.is_err();if tx.send(result).is_err() || failed {break;}
            }
        })?;
        self.pid.store(child.id() as i32,Ordering::SeqCst);
        self.fresh=true;self.spawned=Instant::now();self.abandoned.clear();
        self.process=Some(Process{child,stdin,replies});Ok(())
    }
    /// One request/reply. Only crashes, timeouts, protocol faults and cancellations of a started
    /// worker end the process; a reply carrying `error` leaves the worker resident. A cancel
    /// before the worker's first answer leaves it starting: killing it would throw its self-test
    /// and weight load away, and the next start would repeat them.
    fn exchange(&mut self,mut request:Value,timeout:Duration,background:bool,more:&mut (impl FnMut()->bool + ?Sized)) -> Result<Value> {
        ensure!(more(),"embedding cancelled");self.start()?;self.sequence+=1;
        let id=self.sequence;request["id"]=json!(id);
        // A fresh worker first runs its self-test and loads weights: the rest of its start-up
        // allowance, normal priority. Abandoned requests ahead of this one are answered first.
        let fresh=self.fresh;
        let base=timeout;
        let ahead:Duration=self.abandoned.iter().map(|a|a.1).sum();
        let timeout=timeout+ahead+if fresh {STARTUP.saturating_sub(self.spawned.elapsed())} else {Duration::ZERO};
        let background=background && !fresh && self.waiters.load(Ordering::SeqCst)==0;
        let result=(|| -> Result<Value,Failure> {
            let p=self.process.as_mut().ok_or_else(||Failure::Drop(anyhow::anyhow!("embedding worker")))?;
            set_background(p.child.id() as i32,background);
            let written=serde_json::to_writer(&mut p.stdin,&request).map_err(std::io::Error::from)
                .and_then(|_|p.stdin.write_all(b"\n")).and_then(|_|p.stdin.flush());
            // A broken pipe means the worker already exited.
            if let Err(e)=written {return Err(Failure::Crash(anyhow::anyhow!("embedding worker disconnected: {e}")));}
            let start=Instant::now();
            let reply=loop {
                if !more() {
                    let error=anyhow::anyhow!("embedding cancelled");
                    // Still starting, and not yet overdue (a hung start is still ended).
                    let starting=self.fresh && self.spawned.elapsed()<STARTUP+TIMEOUT;
                    return Err(if starting {Failure::Abandon(error)} else {Failure::Drop(error)});
                }
                if start.elapsed()>=timeout {return Err(Failure::Drop(anyhow::anyhow!("embedding worker timed out")));}
                match p.replies.recv_timeout(Duration::from_millis(20)) {
                    // The answer to a request abandoned during start-up: the worker is up now.
                    Ok(Ok(r)) if self.abandoned.front().is_some_and(|a|r["id"].as_u64()==Some(a.0))=>{
                        self.abandoned.pop_front();self.fresh=false;
                    },
                    Ok(Ok(r))=>break r,
                    Ok(Err(Fault::Protocol(e)))=>return Err(Failure::Drop(anyhow::anyhow!(e))),
                    Ok(Err(Fault::Closed))|Err(mpsc::RecvTimeoutError::Disconnected)=>
                        return Err(Failure::Crash(anyhow::anyhow!("embedding worker disconnected without a reply"))),
                    Err(mpsc::RecvTimeoutError::Timeout)=>{},
                }
            };
            if reply["id"].as_u64()!=Some(id) {return Err(Failure::Drop(anyhow::anyhow!("embedding response identity mismatch")));}
            if let Some(error)=reply["error"].as_str() {return Err(Failure::Keep(anyhow::anyhow!("EmbeddingGemma 2: {error}")));}
            Ok(reply)
        })();
        self.last_used=Instant::now();
        if request["op"]!="trim" {self.dirty=true;}
        self.last_crash=false;
        match result {
            Ok(reply)=>{self.fresh=false;self.succeeded();Ok(reply)},
            Err(Failure::Keep(e))=>{self.fresh=false;Err(e)},
            Err(Failure::Drop(e))=>{self.stop();Err(e)},
            Err(Failure::Abandon(e))=>{self.abandoned.push_back((id,base));Err(e)},
            Err(Failure::Crash(e))=>{
                let signal=self.died_by_signal();
                self.stop();self.last_crash=true;
                if signal && self.count_crashes {self.crashed();}
                Err(e)
            },
        }
    }
    fn identity(&mut self,reply:&Value) -> Result<()> {
        if reply["model_id"]==MODEL_ID && reply["model_version"]==MODEL_VERSION {return Ok(());}
        self.stop();anyhow::bail!("embedding worker model identity mismatch")
    }
    fn single(&mut self,inputs:&[Input],background:bool,more:&mut (impl FnMut()->bool + ?Sized)) -> Result<Vec<f32>> {
        let reply=self.exchange(json!({"inputs":inputs,"dimensions":DIMENSIONS}),TIMEOUT,background,more)?;
        self.identity(&reply)?;
        if background {self.stats.worker(&reply["stats"]);}
        let vector=vector(&reply["vector"]);
        if vector.is_err() {self.stop();}
        vector
    }
    pub fn inputs(&mut self,inputs:&[Input],more:&mut (impl FnMut()->bool + ?Sized)) -> Result<Vec<f32>> {
        self.single(inputs,false,more)
    }
    /// One `op:"batch"` request for several prepared chunks, one result per item in order.
    /// A per-item error (here or from the worker) fails only that item. If the whole request
    /// fails, every item is retried alone so one poison chunk cannot sink its neighbours.
    pub fn embed_inputs_many(&mut self,items:Vec<Result<Vec<Input>>>,more:&mut (impl FnMut()->bool + ?Sized)) -> Vec<Result<Vec<f32>>> {
        let (mut out,mut ready,mut inputs)=(Vec::with_capacity(items.len()),vec![],vec![]);
        for (n,item) in items.into_iter().enumerate() {
            match item {
                Ok(i)=>{ready.push(n);inputs.push(i);out.push(Err(anyhow::anyhow!("embedding not attempted")));},
                Err(e)=>out.push(Err(e)),
            }
        }
        let batch=if inputs.len()<2 {None} else {
            let timeout=TIMEOUT+Duration::from_secs(15)*(inputs.len() as u32-1);
            Some(self.exchange(json!({"op":"batch","items":inputs,"dimensions":DIMENSIONS}),timeout,true,more)
                .and_then(|reply| {
                    self.identity(&reply)?;
                    let vectors=reply["vectors"].as_array().filter(|v|v.len()==inputs.len());
                    let Some(vectors)=vectors else {self.stop();anyhow::bail!("embedding batch reply has the wrong number of vectors");};
                    self.stats.worker(&reply["stats"]);
                    Ok(vectors.iter().enumerate().map(|(i,v)| {
                        if let Some(error)=reply["errors"][i].as_str() {anyhow::bail!("EmbeddingGemma 2: {error}");}
                        vector(v)
                    }).collect::<Vec<_>>())
                }))
        };
        match batch {
            Some(Ok(vectors))=>{for (&n,v) in ready.iter().zip(vectors) {out[n]=v;}},
            // Paused for memory: every item would be refused the same way. Retried next pass.
            Some(Err(error)) if format!("{error:#}").contains("embedding paused") => {
                for &n in &ready {out[n]=Err(anyhow::anyhow!("{error:#}"));}
            },
            Some(Err(error))=> {
                if more() {eprintln!("embedding batch of {} failed ({error:#}); embedding each item alone",ready.len());}
                // After a crashed batch the culprit is one input: its single retry does not count again.
                self.count_crashes=!self.last_crash;
                for (&n,i) in ready.iter().zip(&inputs) {
                    out[n]=if !more() {Err(anyhow::anyhow!("embedding cancelled"))}
                        else if self.waiters.load(Ordering::SeqCst)>0 {Err(anyhow::Error::new(Deferred))}
                        else {self.single(i,true,more)};
                }
                self.count_crashes=true;
            },
            None=>for (&n,i) in ready.iter().zip(&inputs) {out[n]=self.single(i,true,more);},
        }
        out
    }
    pub fn query(&mut self,text:&str,more:&mut impl FnMut()->bool) -> Result<timeline::VectorSearch> {
        ensure!(!text.is_empty() && text.len()<=8192,"semantic query bound");
        let values=self.inputs(&[Input::text(text,true)],more)?;
        Ok(timeline::VectorSearch {model_id:MODEL_ID.into(),model_version:MODEL_VERSION.into(),values})
    }
    /// Ask a resident worker to free its MLX scratch once `idle` has passed since its last
    /// request. Sent once per stretch of use, never repeatedly. Weights stay loaded.
    pub fn trim_if_idle(&mut self,idle:Duration) -> bool {
        // A worker still answering requests abandoned during its start-up is not idle yet.
        self.settle();
        if self.process.is_none() || !self.abandoned.is_empty() || !self.dirty || self.last_used.elapsed()<idle {return false;}
        self.dirty=false;
        match self.exchange(json!({"op":"trim"}),Duration::from_secs(10),true,&mut ||true) {
            Ok(_)=>true,
            Err(e)=>{eprintln!("embedding worker trim failed: {e:#}");false},
        }
    }
    pub fn unload(&mut self) {self.stop();}
    fn account(&mut self,chunks:&[Chunk],results:&[Result<Vec<f32>>],started:Instant) {
        let s=&mut self.stats;
        s.since.get_or_insert(started);s.busy+=started.elapsed();
        for (c,r) in chunks.iter().zip(results) {
            if r.is_ok() {s.items+=1;*s.kinds.entry(c.kind.clone()).or_default()+=1;} else {s.failed+=1;}
        }
        if s.since.is_some_and(|t|t.elapsed()>=INDEX_LOG_EVERY) {
            eprintln!("{}",s.line());
            self.stats=IndexStats::default();
        }
    }
}
fn vector(value:&Value) -> Result<Vec<f32>> {
    let vector:Vec<f32>=serde_json::from_value(value.clone()).context("embedding response vector")?;
    ensure!(vector.len()==DIMENSIONS,"embedding response geometry");
    timeline::cosine(&vector,&vector)?;Ok(vector)
}
impl Embedder for MlxEmbedder {
    fn model(&self) -> Model {model()}
    fn embed(&mut self,root:&Path,c:&Chunk) -> Result<Vec<f32>> {
        self.embed_while(root,c,&mut ||true)
    }
    fn embed_while(&mut self,root:&Path,c:&Chunk,more:&mut dyn FnMut()->bool)->Result<Vec<f32>> {
        let started=Instant::now();
        let result=(|| {
            let temp=tempfile::tempdir()?;
            let inputs=prepare_chunk_while(root,c,temp.path(),more)?;
            self.single(&inputs,true,more)
        })();
        let results=vec![result];self.account(std::slice::from_ref(c),&results,started);
        results.into_iter().next().unwrap()
    }
    fn batch_size(&self,kind:&str) -> usize {batch_size(kind)}
    fn embed_many_while(&mut self,root:&Path,chunks:&[Chunk],more:&mut dyn FnMut()->bool) -> Vec<Result<Vec<f32>>> {
        let started=Instant::now();
        // Each chunk owns its media directory until the request returns.
        let mut temps=vec![];
        let items=chunks.iter().map(|c| {
            let temp=tempfile::tempdir()?;
            let inputs=prepare_chunk_while(root,c,temp.path(),more);
            temps.push(temp);inputs
        }).collect();
        let results=self.embed_inputs_many(items,more);
        drop(temps);
        self.account(chunks,&results,started);
        results
    }
}
/// One provider shared by automatic indexing and explicit semantic queries; no duplicate
/// model residency. Queries can cancel while queued or while the helper is executing.
/// The third field is the worker pid, so a waiting query can end its background state.
pub struct Service(pub Mutex<MlxEmbedder>, Arc<AtomicUsize>, Arc<AtomicI32>);

/// A cancelled/failed query must release its priority just like a successful one.
struct QueryWaiter<'a>(&'a AtomicUsize);
impl<'a> QueryWaiter<'a> {
    fn new(waiters:&'a AtomicUsize)->Self {
        waiters.fetch_add(1,Ordering::SeqCst);
        Self(waiters)
    }
}
impl Drop for QueryWaiter<'_> {
    fn drop(&mut self) {self.0.fetch_sub(1,Ordering::SeqCst);}
}
impl Service {
    pub fn new(models:&Path)->Self {Self::with_state(models,None)}
    pub fn with_state(models:&Path,state:Option<PathBuf>)->Self {
        let provider=MlxEmbedder::with_state(models,state);let pid=provider.pid_handle();
        let waiters=provider.waiters.clone();
        Self(Mutex::new(provider),waiters,pid)
    }
    /// Automatic indexing yields to explicit queries, including ones that arrived
    /// while the previous group was in flight. Hold this guard for ONE group only.
    pub fn try_index(&self)->Result<Option<MutexGuard<'_,MlxEmbedder>>> {
        if self.1.load(Ordering::SeqCst)>0 {return Ok(None);}
        match self.0.try_lock() {
            Ok(p) if self.1.load(Ordering::SeqCst)==0=>Ok(Some(p)),
            Ok(_)=>Ok(None),
            Err(std::sync::TryLockError::WouldBlock)=>Ok(None),
            Err(e)=>anyhow::bail!("embedding service lock: {e}"),
        }
    }
    pub fn query(&self,text:&str,more:&mut impl FnMut()->bool)->Result<timeline::VectorSearch> {
        ensure!(more(),"embedding query cancelled");
        let _waiter=QueryWaiter::new(&self.1);
        // The index request in flight finishes at normal priority so this query waits less.
        let pid=self.2.load(Ordering::SeqCst);
        if pid>0 {set_background(pid,false);}
        loop {
            ensure!(more(),"embedding query cancelled");
            match self.0.try_lock() {
                Ok(mut p)=>return p.query(text,more),
                Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(Duration::from_millis(20)),
                Err(e)=>anyhow::bail!("embedding service lock: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_single_audio_block_splits_without_losing_samples_or_tail() -> Result<()> {
        let t=tempfile::tempdir()?;
        let expected:Vec<f32>=(0..16000*245+37).map(|i|(i%997) as f32/997.).collect();
        let mut samples=expected.clone();let mut parts=vec![];let mut i=0;
        write_audio(t.path(),&mut parts,&mut samples,&mut i,(16000,1))?;
        assert!(samples.is_empty());assert_eq!(parts.len(),3);
        let mut got=vec![];
        for part in parts {
            let Part::Audio{path}=part else {panic!()};
            let bytes=std::fs::read(path)?;
            assert!(bytes.len()/4<=16000*120);
            assert!((bytes.len()/4).div_ceil(640)+2<=6000);
            got.extend(bytes.chunks_exact(4).map(|b|f32::from_le_bytes(b.try_into().unwrap())));
        }
        assert_eq!(got,expected);Ok(())
    }
    #[cfg(target_os="macos")]
    #[test]
    fn worker_background_state_is_set_and_cleared() {
        let mut child=Command::new("sleep").arg("5").spawn().unwrap();
        let pid=child.id() as i32;
        // getpriority(PRIO_DARWIN_PROCESS) reports 1 only for the caller itself (measured on
        // macOS 27: another pid always reads 0), so read the scheduler priority instead:
        // 31 normal, 4 in Darwin background state.
        let read=|| String::from_utf8(Command::new("ps").args(["-o","pri=","-p",&pid.to_string()]).output().unwrap().stdout).unwrap().trim().parse::<i32>().unwrap();
        let normal=read();
        assert!(set_background(pid,true));
        assert!(read()<normal,"background priority {} vs {normal}",read());
        assert!(set_background(pid,false));
        assert_eq!(read(),normal);
        let _=child.kill();let _=child.wait();
        assert!(!set_background(0,true));
    }
    #[test]
    fn batch_sizes_stay_within_the_worker_bound() {
        for kind in ["text","other_audio","speech_audio","screen","moment","unknown"] {
            assert!((1..=MAX_BATCH).contains(&batch_size(kind)));
        }
        assert_eq!(batch_size("moment"),1);
        // Batched audio was slower per item (4 Nov idle benchmark): one per request.
        assert_eq!(batch_size("speech_audio"),1);
        assert_eq!(batch_size("other_audio"),1);
        assert!(batch_size("text")>1 && batch_size("screen")>1);
    }
    /// The live buffer is off (4 Nov benchmark: no energy saved): live chunks embed whenever pending.
    #[test]
    fn live_buffer_is_off() {
        assert_eq!(LIVE_BUFFER,Duration::ZERO);
    }
    #[test]
    fn intervals_union_overlap_without_filling_gaps() {
        assert_eq!(union_ranges(vec![(4,7),(1,3),(2,4),(9,10)]),vec![(1,7),(9,10)]);
    }
    #[test]
    fn missing_assets_fail_without_network_or_archive_writes() {
        let t=tempfile::tempdir().unwrap();let mut p=MlxEmbedder::new(t.path());
        assert!(p.query("test",&mut ||true).unwrap_err().to_string().contains("unavailable"));
        assert!(std::fs::read_dir(t.path()).unwrap().next().is_none());
    }
    #[test]
    fn queued_query_wins_over_next_index_chunk_and_clears_priority_on_error() {
        let t=tempfile::tempdir().unwrap();
        let service=std::sync::Arc::new(Service::new(t.path()));
        // Model a chunk already in flight. The query may queue, never preempt it.
        let indexing=service.try_index().unwrap().unwrap();
        let (tx,rx)=mpsc::channel();
        let (release,continue_query)=mpsc::channel();
        let query_service=service.clone();
        let query=std::thread::spawn(move|| {
            let mut checks=0;
            query_service.query("test",&mut || {
                checks+=1;
                if checks==2 {
                    tx.send(()).unwrap();
                    continue_query.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                true
            })
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(service.1.load(Ordering::SeqCst),1);
        drop(indexing);
        assert!(service.try_index().unwrap().is_none());
        release.send(()).unwrap();
        assert!(query.join().unwrap().unwrap_err().to_string().contains("unavailable"));
        assert_eq!(service.1.load(Ordering::SeqCst),0);
        assert!(service.try_index().unwrap().is_some());
    }
    #[test]
    fn cancelled_waiter_does_not_starve_indexing() {
        let t=tempfile::tempdir().unwrap();let service=Service::new(t.path());
        let indexing=service.try_index().unwrap().unwrap();
        let mut checks=0;
        let error=service.query("test",&mut || {checks+=1;checks<2}).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(service.1.load(Ordering::SeqCst),0);
        drop(indexing);
        assert!(service.try_index().unwrap().is_some());
        assert!(std::fs::read_dir(t.path()).unwrap().next().is_none());
    }
    #[test]
    fn overlapping_waiters_keep_priority_until_the_last_one_leaves() {
        let t=tempfile::tempdir().unwrap();let service=Service::new(t.path());
        let first=QueryWaiter::new(&service.1);let second=QueryWaiter::new(&service.1);
        drop(first);assert!(service.try_index().unwrap().is_none());
        drop(second);assert!(service.try_index().unwrap().is_some());
    }
}
