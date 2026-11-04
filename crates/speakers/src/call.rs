//! Call-scoped native inference. Models live in an owned helper so idle teardown
//! returns Metal/ORT allocations to the OS, including library-global caches.
use crate::{
    diar::SpeakerSegment,
    embedding::{Embedding, ReDimNet, VoiceEncoder},
    live::LiveStrategy,
    online::OnlineConfig,
    pipeline::Diarization,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    rc::Rc,
    time::{Duration, Instant},
};
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// Quiet on the call source before the helper unloads and the call session ends. A
/// new session starts with anonymous tracks and needs about 12 s of clean speech per
/// voice before names return, so every expiry inside a call left lines "Unknown".
/// On 20 Oct a 15 s expiry restarted the Discord session eight times in 57 min on
/// pauses of 18–33 s; 120 s keeps a call's names through ordinary pauses and still
/// frees the ~0.9 GiB helper two minutes after the call goes quiet.
pub const QUIET_TIMEOUT: Duration = Duration::from_secs(120);
#[derive(Serialize, Deserialize)]
enum Request {
    Start {
        models: PathBuf,
        gpu: i32,
        strategy: LiveStrategy,
        enrollments: crate::identity::EnrollmentStore,
    },
    Push(Vec<f32>),
    Segments,
    Finalized,
    Probabilities(i64),
    Embed(Vec<f32>),
    Finish,
    /// The voice encoder alone (no diarizer): Heimdall's own-voice check during speech through speakers.
    StartEncoder {
        models: PathBuf,
    },
    /// Half a second to six seconds, repeated to the model's six-second input (`ReDimNet::embed_short`).
    EmbedShort(Vec<f32>),
}
#[derive(Serialize, Deserialize)]
enum Reply {
    Ready {
        speakers: usize,
        seconds_per_frame: f64,
    },
    Pushed {
        frames: i64,
        revision: i64,
        stable_frames: i64,
    },
    Segments(Vec<SpeakerSegment>),
    Probabilities(Option<crate::diar::FrameProbabilities>),
    Embedding(Embedding),
    Error(String),
}
/// Status travels with every helper result so process isolation cannot hide fallback.
#[derive(Serialize, Deserialize)]
struct WorkerReply {
    reply: Reply,
    gpu_status: Option<String>,
}
fn write_reply(out: &mut impl Write, reply: &Reply) -> Result<()> {
    #[derive(Serialize)]
    struct Wire<'a> {
        reply: &'a Reply,
        gpu_status: Option<&'static str>,
    }
    write_message(
        out,
        &Wire {
            reply,
            gpu_status: crate::gpu_recovery::gpu_status(),
        },
    )
}
fn write_message(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "speaker request too large");
    out.write_all(&(bytes.len() as u32).to_le_bytes())?;
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(())
}
fn read_message<T: serde::de::DeserializeOwned>(input: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= MAX_MESSAGE, "speaker reply too large");
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
struct Helper {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    gpu_status: Option<String>,
}
impl Helper {
    fn request(&mut self, request: Request) -> Result<Reply> {
        self.request_within(request, Duration::from_secs(60))
    }
    fn request_within(&mut self, request: Request, limit: Duration) -> Result<Reply> {
        write_message(&mut self.input, &request)?;
        // Read every part under the same deadline, including a partial frame.
        let deadline = Instant::now() + limit;
        let mut reader = DeadlineReader {
            input: &mut self.output,
            deadline,
        };
        let response: WorkerReply = read_message(&mut reader)?;
        self.gpu_status = response.gpu_status;
        match response.reply {
            Reply::Error(e) => anyhow::bail!("speaker helper: {e}"),
            r => Ok(r),
        }
    }
}
struct DeadlineReader<'a> {
    input: &'a mut ChildStdout,
    deadline: Instant,
}
impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                let remaining = self.deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                let mut fd = libc::pollfd {
                    fd: self.input.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe {
                    libc::poll(
                        &mut fd,
                        1,
                        remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
                    )
                };
                if result > 0 {
                    break;
                }
                if result == 0 {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        self.input.read(bytes)
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[derive(Clone)]
pub struct CallEncoder {
    helper: Rc<RefCell<Helper>>,
}
impl VoiceEncoder for CallEncoder {
    fn embed(&mut self, audio: &[f32]) -> Result<Embedding> {
        ensure!(
            audio.len() >= crate::embedding::MIN_SAMPLES
                && audio.len() <= crate::embedding::WINDOW_SAMPLES
                && audio.iter().all(|v| v.is_finite()),
            "invalid remote embedding audio"
        );
        match self
            .helper
            .borrow_mut()
            .request(Request::Embed(audio.to_vec()))?
        {
            Reply::Embedding(e) => {
                e.validate()?;
                Ok(e)
            }
            _ => anyhow::bail!("unexpected embedding reply"),
        }
    }
}
pub struct CallStream {
    helper: Rc<RefCell<Helper>>,
    frames: i64,
    revision: i64,
    stable_frames: i64,
    finished: bool,
    speakers: usize,
    seconds_per_frame: f64,
}
impl CallStream {
    pub fn gpu_status(&self) -> Option<String> {
        self.helper.borrow().gpu_status.clone()
    }
    pub fn worker_pid(&self) -> u32 {
        self.helper.borrow().child.id()
    }
}
impl crate::pipeline::Diarization for CallStream {
    fn take_finalized_segments(&self) -> Result<Vec<SpeakerSegment>> {
        match self.helper.borrow_mut().request(Request::Finalized)? {
            Reply::Segments(s) => Ok(s), _ => anyhow::bail!("unexpected finalized reply"),
        }
    }
    fn output_revision(&self) -> i64 { self.revision }
    fn stable_frames(&self) -> i64 { self.stable_frames }
    fn frame_probabilities(&self,since:i64) -> Result<Option<crate::diar::FrameProbabilities>> {
        match self.helper.borrow_mut().request(Request::Probabilities(since))? {
            Reply::Probabilities(p) => Ok(p),
            _ => anyhow::bail!("unexpected probabilities reply"),
        }
    }
    fn sample_rate(&self) -> u32 {
        16000
    }
    fn speakers(&self) -> usize {
        self.speakers
    }
    fn seconds_per_frame(&self) -> f64 {
        self.seconds_per_frame
    }
    fn frame_count(&self) -> i64 {
        self.frames
    }
    fn push_f32(&mut self, audio: &[f32]) -> Result<()> {
        ensure!(
            !self.finished && audio.len() <= 16000 && audio.iter().all(|v| v.is_finite()),
            "invalid remote PCM"
        );
        match self
            .helper
            .borrow_mut()
            .request(Request::Push(audio.to_vec()))?
        {
            Reply::Pushed { frames,revision,stable_frames } => {
                self.revision=revision;self.stable_frames=stable_frames;
                self.frames = frames;
                Ok(())
            }
            _ => anyhow::bail!("unexpected push reply"),
        }
    }
    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        match self.helper.borrow_mut().request(Request::Finish)? {
            Reply::Pushed { frames,revision,stable_frames } => {
                self.revision=revision;self.stable_frames=stable_frames;
                self.frames = frames;
                self.finished = true;
                Ok(())
            }
            _ => anyhow::bail!("unexpected finish reply"),
        }
    }
    fn segments(&self) -> Result<Vec<SpeakerSegment>> {
        match self.helper.borrow_mut().request(Request::Segments)? {
            Reply::Segments(s) => Ok(s),
            _ => anyhow::bail!("unexpected segment reply"),
        }
    }
}
/// Standby owns no models or helper. Activation starts one dedicated inference
/// process; 15 seconds without VAD-confirmed speech expires its whole pipeline.
/// Tick from the host timer even if capture has stopped. A new activation needs a
/// new call/session ID and audio origin; pending naming evidence stays in parent.
pub struct CallScopedSpeakers {
    metal_backend: Option<String>,
    executable: PathBuf,
    models: PathBuf,
    gpu: i32,
    strategy: LiveStrategy,
    pipeline: Option<crate::pipeline::SpeakerPipeline<CallEncoder, CallStream>>,
    naming: Option<crate::naming::NamingFlow>,
    last_activity: Option<Instant>,
}
impl CallScopedSpeakers {
    pub fn gpu_status(&self) -> Option<String> {
        self.pipeline
            .as_ref()
            .and_then(|p| p.stream_ref().gpu_status())
    }
    pub fn new(
        executable: PathBuf,
        models: PathBuf,
        gpu: i32,
        naming: crate::naming::NamingFlow,
    ) -> Self {
        Self {
            metal_backend: None,
            executable,
            models,
            gpu,
            strategy: LiveStrategy::default(),
            pipeline: None,
            naming: Some(naming),
            last_activity: None,
        }
    }
    pub fn activate(
        &mut self,
        source: String,
        session: u64,
        origin: f64,
        now: Instant,
    ) -> Result<()> {
        self.activate_observed(source, session, origin, now, |_| {})
    }
    /// Initialise off intake; account for child CPU from spawn, including loading.
    pub fn activate_observed(
        &mut self,
        source: String,
        session: u64,
        origin: f64,
        now: Instant,
        on_spawn: impl FnOnce(u32),
    ) -> Result<()> {
        ensure!(origin.is_finite(), "invalid call origin");
        ensure!(self.pipeline.is_none(), "call already active");
        let store = &self.naming.as_ref().expect("standby naming").store;
        let (stream, encoder) = start_helper(
            &self.executable,
            &self.models,
            self.gpu,
            self.strategy,
            store,
            on_spawn,
            self.metal_backend.as_deref(),
        )?;
        self.pipeline = Some(crate::pipeline::SpeakerPipeline::new(
            stream,
            encoder,
            self.naming.take().expect("standby naming"),
            source,
            session,
            origin,
        )?);
        self.last_activity = Some(now);
        Ok(())
    }
    /// Change the backend only in standby; active calls keep their timeline/track IDs.
    pub fn set_strategy(&mut self, strategy: LiveStrategy) -> Result<()> {
        ensure!(
            !self.is_active(),
            "stop the active call before changing diarization strategy"
        );
        self.strategy = strategy;
        Ok(())
    }
    /// Applied to the owned helper's environment only; never mutates the parent process.
    pub fn set_metal_backend(&mut self, backend: &str) -> Result<()> {
        ensure!(!self.is_active(), "speaker call must drain before changing backend");
        ensure!(matches!(backend, "default" | "redimnet" | "nemotron" | "all"), "invalid speaker backend");
        self.metal_backend = Some(match backend { "all" => "1", "default" => "0", b => b }.into());
        Ok(())
    }
    pub fn strategy(&self) -> LiveStrategy {
        self.strategy
    }
    pub fn pipeline_mut(
        &mut self,
    ) -> Option<&mut crate::pipeline::SpeakerPipeline<CallEncoder, CallStream>> {
        self.pipeline.as_mut()
    }
    pub fn push(
        &mut self,
        audio: &[f32],
        speech: bool,
        now: Instant,
    ) -> Result<Vec<crate::naming::SpeakerEvent>> {
        if speech {
            self.last_activity = Some(now);
        }
        let result = self
            .pipeline
            .as_mut()
            .context("activate multi-speaker call before push")?
            .push(audio);
        if result.is_err() {
            self.release();
        }
        result
    }
    pub fn expire(&mut self, now: Instant) -> Result<Vec<crate::naming::SpeakerEvent>> {
        if self
            .last_activity
            .is_some_and(|t| now.saturating_duration_since(t) >= QUIET_TIMEOUT)
        {
            self.stop()
        } else {
            Ok(vec![])
        }
    }
    pub fn stop(&mut self) -> Result<Vec<crate::naming::SpeakerEvent>> {
        let result = self.pipeline.as_mut().map(|p| p.finish()).transpose();
        self.release();
        Ok(result?.unwrap_or_default())
    }
    pub fn release(&mut self) {
        if let Some(p) = self.pipeline.take() {
            self.naming = Some(p.into_naming());
        }
        self.last_activity = None;
    }
    pub fn is_active(&self) -> bool {
        self.pipeline.is_some()
    }
    pub fn worker_pid(&self) -> Option<u32> {
        self.pipeline.as_ref().map(|p| p.stream_ref().worker_pid())
    }
    pub fn naming_mut(&mut self) -> &mut crate::naming::NamingFlow {
        if let Some(p) = self.pipeline.as_mut() {
            &mut p.naming
        } else {
            self.naming.as_mut().expect("standby naming")
        }
    }
}
fn start_helper(
    executable: &Path,
    models: &Path,
    gpu: i32,
    strategy: LiveStrategy,
    enrollments: &crate::identity::EnrollmentStore,
    on_spawn: impl FnOnce(u32),
    metal_backend: Option<&str>,
) -> Result<(CallStream, CallEncoder)> {
    let mut command = Command::new(executable);
    if let Some(backend) = metal_backend { command.env("HEIMDALL_SPEAKERS_METAL", backend); }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start bundled speaker helper")?;
    on_spawn(child.id());
    let input = child.stdin.take().expect("piped stdin");
    let output = child.stdout.take().expect("piped stdout");
    let helper = Rc::new(RefCell::new(Helper {
        child,
        input,
        output,
        gpu_status: None,
    }));
    let Reply::Ready {
        speakers,
        seconds_per_frame,
    } = helper.borrow_mut().request(Request::Start {
        models: models.into(),
        gpu,
        strategy,
        enrollments: enrollments.clone(),
    })?
    else {
        anyhow::bail!("unexpected start reply")
    };
    ensure!(
        (2..=8).contains(&speakers) && seconds_per_frame.is_finite() && seconds_per_frame > 0.,
        "invalid backend geometry"
    );
    Ok((
        CallStream {
            helper: helper.clone(),
            frames: 0,
            revision: 0,
            stable_frames: 0,
            finished: false,
            speakers,
            seconds_per_frame,
        },
        CallEncoder { helper },
    ))
}
/// The voice encoder alone in its own helper process (about 50 MB of weights, CPU, one thread), for Heimdall's
/// own-voice check while it speaks through speakers. Dropping it ends the process and frees the model.
pub struct EncoderHelper {
    helper: Helper,
}
impl EncoderHelper {
    pub fn start(executable: &Path, models: &Path, on_spawn: impl FnOnce(u32)) -> Result<Self> {
        let mut child = Command::new(executable)
            // CPU only: the check must not queue behind the GPU's speech and transcription work.
            .env("HEIMDALL_SPEAKERS_METAL", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start bundled speaker helper")?;
        on_spawn(child.id());
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        let mut helper = Helper { child, input, output, gpu_status: None };
        match helper.request_within(Request::StartEncoder { models: models.into() }, Duration::from_secs(30))? {
            Reply::Ready { .. } => Ok(Self { helper }),
            _ => anyhow::bail!("unexpected start reply"),
        }
    }
    pub fn pid(&self) -> u32 {
        self.helper.child.id()
    }
    /// 0.5–6 s of 16 kHz speech, repeated to six seconds. `limit` bounds the wait; a helper that misses it is dropped
    /// by the caller.
    pub fn embed_short(&mut self, audio: &[f32], limit: Duration) -> Result<Embedding> {
        ensure!(
            audio.len() >= crate::embedding::SAMPLE_RATE / 2
                && audio.len() <= crate::embedding::WINDOW_SAMPLES
                && audio.iter().all(|v| v.is_finite()),
            "invalid remote embedding audio"
        );
        match self.helper.request_within(Request::EmbedShort(audio.to_vec()), limit)? {
            Reply::Embedding(e) => {
                e.validate()?;
                Ok(e)
            }
            _ => anyhow::bail!("unexpected embedding reply"),
        }
    }
}
fn run_encoder(models: &Path, input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    let mut encoder = match ReDimNet::load(&models.join("ReDimNet2B6.onnx")) {
        Ok(e) => e,
        Err(e) => {
            write_reply(output, &Reply::Error(format!("{e:#}")))?;
            return Err(e);
        }
    };
    write_reply(output, &Reply::Ready { speakers: 0, seconds_per_frame: 0. })?;
    loop {
        let request: Request = read_message(input)?;
        let reply = match request {
            Request::EmbedShort(a) => encoder.embed_short(&a).map(Reply::Embedding),
            Request::Embed(a) => encoder.embed(&a).map(Reply::Embedding),
            _ => Err(anyhow::anyhow!("the voice encoder helper only embeds")),
        };
        match reply {
            Ok(r) => write_reply(output, &r)?,
            Err(e) => {
                write_reply(output, &Reply::Error(format!("{e:#}")))?;
                return Err(e);
            }
        }
    }
}
/// Only the bundled helper binary calls this. EOF/error exits and releases models.
pub fn run_helper() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let (models, gpu, strategy, enrollments) = match read_message(&mut input)? {
        Request::Start { models, gpu, strategy, enrollments } => (models, gpu, strategy, enrollments),
        Request::StartEncoder { models } => return run_encoder(&models, &mut input, &mut output),
        _ => anyhow::bail!("first speaker request must start"),
    };
    let loaded = (|| -> Result<_> {
        let stream =
            strategy.load_enrolled(&models, gpu, OnlineConfig::default(), Some(&enrollments))?;
        let encoder = ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?;
        Ok((stream, encoder))
    })();
    let (mut stream, mut encoder) = match loaded {
        Ok(v) => v,
        Err(e) => {
            write_reply(&mut output, &Reply::Error(format!("{e:#}")))?;
            return Err(e);
        }
    };
    write_reply(
        &mut output,
        &Reply::Ready {
            speakers: stream.speakers(),
            seconds_per_frame: stream.seconds_per_frame(),
        },
    )?;
    loop {
        let request: Request = read_message(&mut input)?;
        let reply = (|| -> Result<Reply> {
            match request {
                Request::Push(a) => {
                    stream.push_f32(&a)?;
                    Ok(Reply::Pushed {
                        frames: stream.frame_count(),
                        revision: stream.output_revision(),
                        stable_frames: stream.stable_frames(),
                    })
                }
                Request::Probabilities(since) => Ok(Reply::Probabilities(stream.frame_probabilities(since)?)),
                Request::Finalized => Ok(Reply::Segments(stream.take_finalized_segments()?)),
                Request::Segments => Ok(Reply::Segments(stream.segments()?)),
                Request::Embed(a) => Ok(Reply::Embedding(encoder.embed(&a)?)),
                Request::Finish => {
                    stream.finish()?;
                    Ok(Reply::Pushed {
                        frames: stream.frame_count(),
                        revision: stream.output_revision(),
                        stable_frames: stream.stable_frames(),
                    })
                }
                Request::EmbedShort(a) => Ok(Reply::Embedding(encoder.embed_short(&a)?)),
                Request::Start { .. } | Request::StartEncoder { .. } => anyhow::bail!("speaker helper already started"),
            }
        })();
        match reply {
            Ok(r) => write_reply(&mut output, &r)?,
            Err(e) => {
                write_reply(&mut output, &Reply::Error(format!("{e:#}")))?;
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod gpu_status_tests {
    use super::*;
    #[test]
    fn fallback_status_survives_worker_protocol() {
        let status = "Voice running on CPU after a GPU error";
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &WorkerReply {
                reply: Reply::Pushed { frames: 17,revision:9,stable_frames:16 },
                gpu_status: Some(status.into()),
            },
        )
        .unwrap();
        let reply: WorkerReply = read_message(&mut wire.as_slice()).unwrap();
        assert_eq!(reply.gpu_status.as_deref(), Some(status));
        assert!(matches!(reply.reply, Reply::Pushed { frames: 17,revision:9,stable_frames:16 }));
    }
}
