//! Heimdall knows its own voice: "only when it can hear itself … Do you correctly see
//! yourself as yourself?" and "Whenever I switch to a different voice, before we use it, generate a couple of lines in
//! the background with no audio, run them through speaker recognition, and associate those voices with yourself."
//!
//! - **Enrolment.** Each voice that can speak now (the chosen one, and Supertonic, the lighter voice that takes a
//!   sentence Jarvis cannot say) renders `LINES` silently, never played, at low priority while Heimdall is silent and
//!   The user is not talking, with Speaking on. Nothing is loaded or reloaded into the app's voices for it: Jarvis and
//!   Phonon S only while already loaded, Supertonic from a private instance when it is not resident. Each line is
//!   embedded by the speaker encoder as rendered, at the user's speed, and through a simulated
//!   speaker-to-microphone path (`room_path`: speaker band, a small room, the echo's level and the room's noise), and
//!   kept in `speakers.json` (`EnrollmentStore::heimdall`, keyed by the voice's sample key). A voice is enrolled
//!   once; a new voice, or a new `RECIPE`, enrols again.
//! - **The check.** While Heimdall speaks through speakers, the barge-in embeds the microphone's last
//!   `WINDOW_NS` every `STEP_NS` and compares it with those profiles (`judge`): Heimdall's voice and anything
//!   unsure keep it talking; only a confident other voice goes on to the words and the decider (pipeline/barge_in.rs).
//! - **Measured** on the 24 Oct 13:34–13:38 archive (MacBook speakers, Shure MV7i; replies played back from the
//!   app's own tap as the profile, leave-one-reply-out): 1.5 s echo windows scored 0.53–0.87 against Heimdall's
//!   voice, the user's own speech −0.02–0.26. `HEIMDALL_MIN` and `OTHER_MAX` sit in that gap. With profiles enrolled
//!   from silent renders (`tests/heimdall_voice_replay.rs`) the five replies' echo windows scored 0.525 or more.
//!
//! - **The user's voice** (decision 31, 28 Oct 20:33): barge-in stops Heimdall only on speech that speaker ID
//!   confidently matches to the user. His profile (`UserVoice`, `user-voice.json` in Heimdall's data folder) is learned
//!   from his own primary-microphone turns that woke Heimdall outside calls while Heimdall was silent: each turn's
//!   middle six seconds, embedded by the same encoder, at most `USER_MAX_SAMPLES`, newest kept. It is used once it holds
//!   `USER_MIN_SAMPLES`; a turn that matches fewer than half of them (not him) is not added.
//!
//! The encoder runs alone in the bundled speaker helper (`speakers::call::EncoderHelper`, CPU, about 50 MB of
//! weights), started on first use and ended after `IDLE` without work.
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use speakers::{embedding::Embedding, identity::HeimdallVoice};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// How much of the microphone one check reads: the newest audio, so the user starting over the echo fills it quickly.
pub(crate) const WINDOW_NS: u64 = 1_500_000_000;
/// One check every half second while the speech goes on.
pub(crate) const STEP_NS: u64 = 500_000_000;
/// At or above: Heimdall's own voice.
pub(crate) const HEIMDALL_MIN: f32 = 0.45;
/// Below: confidently another voice (the user or a real person in the room). Between the two: unsure, never a cut.
pub(crate) const OTHER_MAX: f32 = 0.40;
/// At or above, against the user's profile: his voice (`user_verdict`). Measured 29 Oct on real archive audio (1.5 s
/// windows louder than RMS 0.01, profile from 12 of his other wakes; `lab/barge-voice/user-scores`): his own speech
/// p5 0.60 / median 0.77; Heimdall's echo at most 0.31; ten Discord guests p95 0.39; media p95 0.41.
pub(crate) const USER_MIN: f32 = 0.60;
/// His score must also beat Heimdall's by this much: his words over Heimdall's echo, never the echo alone.
pub(crate) const USER_MARGIN: f32 = 0.10;
/// Windows in a row that must be his before Heimdall stops. With `USER_MIN` and two windows the measurement had no
/// echo stop, 0.7% of guest utterances and 6.7% of media clips, and a median 1.04 s from his first word (one window:
/// 0.76 s, but guests 4%).
pub(crate) const USER_WINDOWS: u32 = 2;
/// A window quieter than this is not speech and is never scored: near-silent windows on his microphone score like him
/// (about 0.7) against a profile that carries that microphone's noise floor. 0.01 was the measurement's speech gate.
pub(crate) const SPEECH_RMS: f32 = 0.0095;
/// Root mean square of a window.
pub(crate) fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}
/// The user's profile is used from this many samples on.
pub(crate) const USER_MIN_SAMPLES: usize = 3;
/// At most this many samples are kept; the oldest leaves first.
pub(crate) const USER_MAX_SAMPLES: usize = 16;
/// A new sample joins only if it matches at least half of those kept this well (the speaker model's calibrated
/// same-speaker threshold for 6-second windows, `speakers.json`), so one stray sample can never admit another voice.
pub(crate) const USER_JOIN: f32 = 0.455;
/// After this many rejected samples in a row that all match each other, the profile starts again from them (his mic or
/// voice changed); misfits that do not agree with each other (guests, the room) never replace it.
pub(crate) const USER_RESTART: usize = 5;
pub(crate) const USER_FILE: &str = "user-voice.json";
pub(crate) const USER_RECIPE: &str = "user-voice-v1;wakes;middle-6s";

/// The user's own voice, learned from his wakes (module docs).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct UserVoice {
    pub(crate) embeddings: Vec<Embedding>,
    #[serde(default)]
    pub(crate) recipe: String,
    #[serde(default)]
    pub(crate) updated_ms: u64,
    /// Recent samples that did not fit (`USER_RESTART`).
    #[serde(default)]
    pub(crate) rejected: Vec<Embedding>,
}
impl UserVoice {
    pub(crate) fn load(path: &std::path::Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Self>(&b).ok()).filter(|v| v.recipe == USER_RECIPE).unwrap_or_default()
    }
    pub(crate) fn save(&self, path: &std::path::Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
    pub(crate) fn ready(&self) -> bool {
        self.embeddings.len() >= USER_MIN_SAMPLES
    }
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn similarity(&self, query: &Embedding) -> Option<f32> {
        self.ready().then(|| self.embeddings.iter().map(|e| e.cosine(query)).fold(f32::NEG_INFINITY, f32::max))
    }
    /// Adds one sample of his voice. Returns whether it was kept. A sample joins when it matches at least half of the
    /// kept ones (the first starts the profile; while it is not ready, a misfit starts it again from the newest, so the
    /// first three agree with each other).
    pub(crate) fn add(&mut self, e: Embedding, now_ms: u64) -> bool {
        self.recipe = USER_RECIPE.into();
        let matching = self.embeddings.iter().filter(|k| k.cosine(&e) >= USER_JOIN).count();
        let fits = self.embeddings.is_empty() || 2 * matching >= self.embeddings.len();
        if !fits && !self.ready() {
            self.embeddings.clear();
        } else if !fits {
            return self.restart_with(e, now_ms);
        }
        self.rejected.clear();
        self.embeddings.push(e);
        if self.embeddings.len() > USER_MAX_SAMPLES {
            self.embeddings.remove(0);
        }
        self.updated_ms = now_ms;
        true
    }
}
impl UserVoice {
    /// A sample whose voice did not match his print (a line d1 would have sent, held as another voice's): it only
    /// counts toward `USER_RESTART`, never joins. Returns whether the profile started again from the misfits.
    pub(crate) fn misfit(&mut self, e: Embedding, now_ms: u64) -> bool {
        if !self.ready() {
            return false;
        }
        self.restart_with(e, now_ms)
    }
    fn restart_with(&mut self, e: Embedding, now_ms: u64) -> bool {
        self.rejected.push(e);
        if self.rejected.len() > USER_RESTART {
            self.rejected.remove(0);
        }
        let agree = self.rejected.len() == USER_RESTART
            && self.rejected.iter().all(|a| self.rejected.iter().all(|b| a.cosine(b) >= USER_JOIN));
        if !agree {
            return false;
        }
        self.embeddings = std::mem::take(&mut self.rejected);
        self.updated_ms = now_ms;
        true
    }
}
/// One barge-in window's scores: its best cosine against the user's profile (None until it is ready) and against
/// Heimdall's own voices (None when none is enrolled).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Scores {
    pub(crate) user: Option<f32>,
    pub(crate) heimdall: Option<(String, f32)>,
}
/// Who one window is, for the barge-in: `User` stops Heimdall; nothing else ever does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Who {
    User,
    Heimdall,
    /// Confidently not Heimdall, and not confidently the user (another person, or his voice too faint to tell).
    Other,
    Unsure,
    /// No user profile yet and no voice of Heimdall's to compare with.
    NoProfile,
}
/// The verdict on one window. With the user's profile: his voice at `USER_MIN` or more, beating Heimdall's score by
/// `USER_MARGIN`, and the window not Heimdall's own voice. Before his profile is ready (a fresh install, before his
/// first three wakes): a window confidently not Heimdall's voice counts as his, so he can always stop it.
pub(crate) fn user_verdict(s: &Scores) -> Who {
    let heimdall = s.heimdall.as_ref().map(|(_, c)| *c).filter(|c| c.is_finite());
    let own = heimdall.is_some_and(|c| judge(c) == Speaker::Heimdall);
    match s.user.filter(|c| c.is_finite()) {
        Some(_) if own => Who::Heimdall,
        Some(u) if u >= USER_MIN && heimdall.is_none_or(|h| u - h >= USER_MARGIN) => Who::User,
        Some(_) if heimdall.is_some_and(|h| judge(h) == Speaker::Other) => Who::Other,
        Some(_) => Who::Unsure,
        None if s.user.is_some() => Who::Unsure,
        None => match heimdall.map(judge) {
            Some(Speaker::Heimdall) => Who::Heimdall,
            Some(Speaker::Other) => Who::User,
            Some(Speaker::Unsure) => Who::Unsure,
            None => Who::NoProfile,
        },
    }
}

/// Who said one whole utterance on the user's microphone: "the mic should label speech as the user
/// only when it actually matches my voice signature", and Heimdall's own voice "shouldn't be sent to you".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MicVoice {
    /// His voice print: his line, as always.
    Owner,
    /// Heimdall's own voice (its echo through speakers): archived as Heimdall's, never the decider's or the agent's.
    Heimdall,
    /// Someone else on his microphone, or his voice too unlike his print to tell: context only, never a wake.
    Unknown,
    /// No verdict could be had (no profile yet, the encoder down or late, no audio): his microphone is his, as before.
    Unverified,
}
/// At or above against his profile, on a whole utterance: his voice (`mic_verdict`). Measured 30 Oct
/// (`lab/self-voice/REPORT.md`, whole utterances, his print from his wakes): 1,496 of his Shure lines from five days,
/// 80.7% at or above with the margin; 510 Discord guests' lines, 0.2%; 31 echo utterances, none.
pub(crate) const MIC_USER_MIN: f32 = 0.55;
/// His score must beat Heimdall's by this much.
pub(crate) const MIC_USER_MARGIN: f32 = 0.10;
/// At or above against Heimdall's voices, and above his score: Heimdall's own voice (echo 0.61-0.81 tonight; none of
/// his 1,496 lines or the guests' 510 reached it).
pub(crate) const MIC_HEIMDALL_MIN: f32 = 0.50;
/// Below, on a reliable utterance, against his print: confidently not his voice (guests p95 0.39, measured 29 Oct).
pub(crate) const MIC_OTHER_MAX: f32 = 0.40;
/// A reliable utterance has this much speech (word time) at `SPEECH_RMS` or louder: below it his own lines miss his
/// print often (half of those under 0.8 s, a fifth up to 3 s), so a short or quiet line is never called someone else's.
pub(crate) const MIC_RELIABLE_NS: u64 = 1_500_000_000;
/// Less audio than this is not embedded at all.
pub(crate) const MIC_MIN_SAMPLES: usize = 8_000;
/// The verdict on one whole utterance's scores. Heimdall's voice wins over an unsure score of his; his own needs his
/// print at `MIC_USER_MIN` and the margin. Anything else is someone else's only when that is safe to say: a reliable
/// utterance clearly unlike his print, or any line not his while Heimdall was audible through speakers (its echo).
/// Otherwise no verdict: his microphone stays his. With the measured scores: 0.7% of his lines are called someone
/// else's, and 15% of guests' lines heard on his microphone stay his (mostly short ones).
pub(crate) fn mic_verdict(s: &Scores, reliable: bool, heimdall_audible: bool) -> MicVoice {
    let heimdall = s.heimdall.as_ref().map(|(_, c)| *c).filter(|c| c.is_finite());
    let user = s.user.filter(|c| c.is_finite());
    if heimdall.is_some_and(|h| h >= MIC_HEIMDALL_MIN && user.is_none_or(|u| h > u)) {
        return MicVoice::Heimdall;
    }
    match user {
        Some(u) if u >= MIC_USER_MIN && heimdall.is_none_or(|h| u - h >= MIC_USER_MARGIN) => MicVoice::Owner,
        Some(_) if heimdall_audible => MicVoice::Unknown,
        Some(u) if reliable && u < MIC_OTHER_MAX => MicVoice::Unknown,
        // No print of his yet (a fresh install, before three wakes), or too little to tell.
        _ => MicVoice::Unverified,
    }
}

/// What a profile is made of; a profile with another recipe is enrolled again.
pub(crate) const RECIPE: &str = "heimdall-voice-v1;7-lines;natural+speed;clean+room-v1";
/// The recipe a voice's profile must carry: `RECIPE` and what the voice is made of, so a new model behind the same
/// voice (Drama 3's weights changed on 29 Oct under the same Jarvis key) enrols it again.
pub(crate) fn recipe_for(route: &crate::speech_voice::Route) -> String {
    match route {
        crate::speech_voice::Route::Jarvis => format!("{RECIPE};model={}", drama3_model().trim()),
        _ => RECIPE.into(),
    }
}
/// The Drama 3 weights the engine loads (crates/speak/drama3/engine/model-path.txt; empty where that engine is absent).
#[cfg(not(feature = "public"))]
fn drama3_model() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../speak/drama3/engine/model-path.txt")).unwrap_or_default()
}
/// The public edition has no Drama 3 engine, and its binary names no build folder.
#[cfg(feature = "public")]
fn drama3_model() -> String {
    String::new()
}
/// An idle encoder helper ends this long after its last request.
const IDLE: Duration = Duration::from_secs(120);
/// A check that takes longer than this is no check: the barge-in decides as it did without one.
const CHECK_LIMIT: Duration = Duration::from_millis(1500);
/// After a failed enrolment, the same voice waits this long.
pub(crate) const RETRY: Duration = Duration::from_secs(600);

/// What Heimdall says to know its voice: varied lengths, a question, numbers, a name and a short ack.
pub(crate) const LINES: [&str; 7] = [
    "Hi, I'm Heimdall. This is how I sound.",
    "The build finished in four minutes, and every test passed on the first try.",
    "Do you want me to open the file now, or should I wait until the call is over?",
    "It's twenty past nine. Your next meeting starts at ten thirty, in the small room.",
    "Okay, stopping here. I'll keep the notes and pick it up again tonight.",
    "There's one warning left: the linker could not find the Metal library.",
    "Got it. I'll tell you as soon as it's ready.",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Speaker {
    Heimdall,
    Unsure,
    Other,
}
/// The verdict on one window's best cosine against Heimdall's voices.
pub(crate) fn judge(cosine: f32) -> Speaker {
    if !cosine.is_finite() {
        // A broken score is never a cut.
        Speaker::Unsure
    } else if cosine >= HEIMDALL_MIN {
        Speaker::Heimdall
    } else if cosine < OTHER_MAX {
        Speaker::Other
    } else {
        Speaker::Unsure
    }
}

/// Second-order section (RBJ cookbook), applied in place.
fn biquad(x: &mut [f32], b: [f64; 3], a: [f64; 3]) {
    let (b0, b1, b2) = (b[0] / a[0], b[1] / a[0], b[2] / a[0]);
    let (a1, a2) = (a[1] / a[0], a[2] / a[0]);
    let (mut x1, mut x2, mut y1, mut y2) = (0f64, 0f64, 0f64, 0f64);
    for v in x.iter_mut() {
        let x0 = *v as f64;
        let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        (x2, x1, y2, y1) = (x1, x0, y1, y0);
        *v = y0 as f32;
    }
}
fn highpass(x: &mut [f32], hz: f64) {
    let w = 2. * std::f64::consts::PI * hz / 16000.;
    let alpha = w.sin() / (2. * std::f64::consts::FRAC_1_SQRT_2);
    let c = w.cos();
    biquad(x, [(1. + c) / 2., -(1. + c), (1. + c) / 2.], [1. + alpha, -2. * c, 1. - alpha]);
}
fn lowpass(x: &mut [f32], hz: f64) {
    let w = 2. * std::f64::consts::PI * hz / 16000.;
    let alpha = w.sin() / (2. * std::f64::consts::FRAC_1_SQRT_2);
    let c = w.cos();
    biquad(x, [(1. - c) / 2., 1. - c, (1. - c) / 2.], [1. + alpha, -2. * c, 1. - alpha]);
}
/// A fixed pseudo-random sequence in [-1, 1) (the same profile on every Mac and every run).
fn noise(seed: u64) -> impl FnMut() -> f32 {
    let mut s = seed;
    move || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((s >> 40) as f32 / (1u64 << 24) as f32) * 2. - 1.
    }
}
/// Heimdall's voice as the microphone hears it through speakers: laptop-speaker band (no bass below about 220 Hz,
/// little above 6.5 kHz), a small room (direct sound, three early reflections, a 0.35 s decaying tail), the echo's
/// level on the Shure (RMS 0.01, measured 24 Oct) and the room's noise floor (RMS 0.003). 16 kHz in and out.
pub(crate) fn room_path(clean: &[f32]) -> Vec<f32> {
    let mut x = clean.to_vec();
    highpass(&mut x, 220.);
    highpass(&mut x, 220.);
    lowpass(&mut x, 6500.);
    const TAIL: usize = 4000; // 0.25 s
    let mut ir = vec![0f32; TAIL];
    ir[0] = 1.;
    for (ms, gain) in [(7usize, 0.5f32), (13, 0.35), (21, 0.25)] {
        ir[ms * 16] += gain;
    }
    // The diffuse tail, one tap in four (a sparse tail sounds the same to the encoder and costs a quarter).
    let mut n = noise(0x6865_696d_6461_6c6c);
    for (i, v) in ir.iter_mut().enumerate().skip(16 * 25).step_by(4) {
        let t = i as f32 / 16000.;
        *v += 0.24 * n() * (-6.9 * t / 0.35).exp();
    }
    let taps: Vec<(usize, f32)> = ir.iter().enumerate().filter(|(_, v)| v.abs() > 1e-4).map(|(i, v)| (i, *v)).collect();
    let mut y = vec![0f32; x.len()];
    for (i, out) in y.iter_mut().enumerate() {
        let mut acc = 0f32;
        for &(k, h) in &taps {
            if k > i {
                break;
            }
            acc += h * x[i - k];
        }
        *out = acc;
    }
    let level = rms(&y);
    let gain = if level > 1e-6 { 0.01 / level } else { 0. };
    let mut floor = noise(0x6d69_6372_6f70_686f);
    y.iter_mut().for_each(|v| *v = *v * gain + 0.003 * 1.732 * floor());
    y
}
/// The part of a line one embedding reads: at most the encoder's six seconds, from the middle.
pub(crate) fn window(x: &[f32]) -> &[f32] {
    const MAX: usize = speakers::embedding::WINDOW_SAMPLES;
    if x.len() <= MAX { x } else { &x[(x.len() - MAX) / 2..][..MAX] }
}
/// One voice's profile from its rendered lines (16 kHz, natural speed): each line as rendered and through
/// `room_path`, at natural speed and at the user's speed (`speed`, when it differs).
pub(crate) fn profile(lines: &[Vec<f32>], speed: f32, embed: &mut dyn FnMut(&[f32]) -> Result<Embedding>, now_ms: u64) -> Result<HeimdallVoice> {
    ensure!(!lines.is_empty(), "no lines rendered");
    let mut embeddings = vec![];
    for line in lines {
        ensure!(line.len() >= 16000 / 2 && line.iter().all(|v| v.is_finite()), "a rendered line is too short");
        let mut versions = vec![line.clone()];
        if (speed - 1.).abs() >= 0.02 {
            versions.push(speak::stretch::stretch(line, speed));
        }
        for v in versions {
            embeddings.push(embed(window(&v))?);
            embeddings.push(embed(window(&room_path(&v)))?);
        }
    }
    Ok(HeimdallVoice { embeddings, recipe: RECIPE.into(), lines: lines.len(), enrolled_ms: now_ms })
}

enum Job {
    /// A barge-in window: answered on the encoder's thread through `done`.
    #[cfg_attr(test, allow(dead_code))]
    Check(Vec<f32>, Box<dyn FnOnce(Result<Embedding>) + Send>),
    /// An enrolment line, or a whole microphone utterance, answered to its waiting thread.
    Enrol(Vec<f32>, crossbeam_channel::Sender<Result<Embedding>>),
    Warm,
}
/// The speaker encoder for Heimdall's own voice: one thread, one helper process, checks before enrolment.
pub(crate) struct Encoder {
    checks: crossbeam_channel::Sender<Job>,
    /// Whole microphone utterances (`identify`): after barge-in checks, before enrolment.
    ids: crossbeam_channel::Sender<Job>,
    enrol: crossbeam_channel::Sender<Job>,
    /// Until when (unix ms) the encoder is out after repeated failures; 0: available.
    out_until_ms: Arc<AtomicU64>,
}
fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
impl Encoder {
    pub(crate) fn start(models: PathBuf, cpu: Arc<crate::metrics::SpeakerCpu>) -> Self {
        let (checks, check_rx) = crossbeam_channel::bounded::<Job>(2);
        let (ids, ids_rx) = crossbeam_channel::bounded::<Job>(4);
        let (enrol, enrol_rx) = crossbeam_channel::bounded::<Job>(1);
        let out_until_ms = Arc::new(AtomicU64::new(0));
        let out = out_until_ms.clone();
        let _ = std::thread::Builder::new().name("heimdall-voice-encoder".into()).spawn(move || {
            let executable = match std::env::current_exe() {
                Ok(e) => e.with_file_name("heimdall-speakers-worker"),
                Err(_) => return,
            };
            let mut helper: Option<speakers::call::EncoderHelper> = None;
            let mut failures = 0u32;
            let run = |helper: &mut Option<speakers::call::EncoderHelper>, audio: &[f32]| -> Result<Embedding> {
                if helper.is_none() {
                    let h = speakers::call::EncoderHelper::start(&executable, &models, |pid| cpu.register(pid))?;
                    *helper = Some(h);
                }
                let result = helper.as_mut().unwrap().embed_short(audio, CHECK_LIMIT);
                if result.is_err() {
                    // A helper that failed or missed its deadline is replaced on the next request.
                    if let Some(h) = helper.take() {
                        cpu.retire(h.pid());
                    }
                }
                result
            };
            loop {
                // Checks first, then utterances: an enrolment line never delays a barge-in or a wake.
                let job = match check_rx.try_recv().or_else(|_| ids_rx.try_recv()) {
                    Ok(j) => j,
                    Err(_) => crossbeam_channel::select! {
                        recv(check_rx) -> j => match j { Ok(j) => j, Err(_) => break },
                        recv(ids_rx) -> j => match j { Ok(j) => j, Err(_) => break },
                        recv(enrol_rx) -> j => match j { Ok(j) => j, Err(_) => break },
                        default(IDLE) => {
                            if let Some(h) = helper.take() { cpu.retire(h.pid()); }
                            continue;
                        }
                    },
                };
                let (audio, answer): (Vec<f32>, Box<dyn FnOnce(Result<Embedding>)>) = match job {
                    Job::Check(a, done) => (a, Box::new(done)),
                    Job::Enrol(a, tx) => (a, Box::new(move |r| { let _ = tx.send(r); })),
                    Job::Warm => {
                        if helper.is_none() {
                            match speakers::call::EncoderHelper::start(&executable, &models, |pid| cpu.register(pid)) {
                                Ok(h) => helper = Some(h),
                                Err(e) => eprintln!("voice check: encoder did not start: {e:#}"),
                            }
                        }
                        continue;
                    }
                };
                let result = run(&mut helper, &audio);
                if result.is_ok() {
                    failures = 0;
                    out.store(0, Ordering::Release);
                } else {
                    failures += 1;
                    if failures >= 3 {
                        // Three in a row: out for a minute, then tried again.
                        out.store(unix_ms() + 60_000, Ordering::Release);
                        failures = 0;
                    }
                }
                answer(result);
            }
            if let Some(h) = helper.take() {
                cpu.retire(h.pid());
            }
        });
        Self { checks, ids, enrol, out_until_ms }
    }
    /// The encoder can answer now (not out after repeated failures).
    pub(crate) fn available(&self) -> bool {
        self.out_until_ms.load(Ordering::Acquire) <= unix_ms()
    }
    /// Start the helper ahead of the first check (Heimdall began speaking through speakers).
    pub(crate) fn warm(&self) {
        let _ = self.checks.try_send(Job::Warm);
    }
    /// One barge-in window; `done` runs on the encoder's thread. False when the queue is full (no check).
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn check(&self, audio: Vec<f32>, done: Box<dyn FnOnce(Result<Embedding>) + Send>) -> bool {
        self.checks.try_send(Job::Check(audio, done)).is_ok()
    }
    /// One whole microphone utterance, answered on the returned channel (`None`: the queue is full).
    pub(crate) fn identify(&self, audio: Vec<f32>) -> Option<crossbeam_channel::Receiver<Result<Embedding>>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ids.try_send(Job::Enrol(audio, tx)).ok().map(|_| rx)
    }
    /// One enrolment embedding, waited for (the enrolment thread).
    pub(crate) fn embed_blocking(&self, audio: &[f32]) -> Result<Embedding> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.enrol.send_timeout(Job::Enrol(audio.to_vec(), tx), Duration::from_secs(60)).ok().context("voice encoder busy")?;
        rx.recv_timeout(Duration::from_secs(60)).context("voice encoder did not answer")?
    }
}

/// The pipeline's view: profiles in memory, the encoder, and the enrolment in flight.
pub(crate) struct State {
    pub(crate) profiles: std::collections::BTreeMap<String, HeimdallVoice>,
    /// The user's voice (`UserVoice`), and its file.
    pub(crate) user: UserVoice,
    pub(crate) user_path: Option<PathBuf>,
    /// A user sample is being embedded.
    pub(crate) user_pending: bool,
    pub(crate) encoder: Option<Arc<Encoder>>,
    pub(crate) enrolling: Option<String>,
    pub(crate) retry: std::collections::HashMap<String, Instant>,
    /// The last voice's key (an Own Engine voice's key reads the Keychain: once per voice, not per barge-in).
    key: std::sync::Mutex<Option<(crate::speech_voice::Route, String)>>,
}
impl State {
    pub(crate) fn new(profiles: std::collections::BTreeMap<String, HeimdallVoice>, encoder: Option<Arc<Encoder>>, user_path: Option<PathBuf>) -> Self {
        let user = user_path.as_deref().map(UserVoice::load).unwrap_or_default();
        Self { profiles, user, user_path, user_pending: false, encoder, enrolling: None, retry: Default::default(), key: Default::default() }
    }
    /// A window's scores against the user's profile and Heimdall's voices.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn scores(&self, query: &Embedding) -> Scores {
        Scores { user: self.user.similarity(query), heimdall: self.similarity(query) }
    }
    /// `route`'s profile key, remembered for the last voice asked about.
    pub(crate) fn key(&self, tools: &crate::bridge::Tools, route: &crate::speech_voice::Route) -> String {
        let mut last = self.key.lock().unwrap();
        if let Some((_, k)) = last.as_ref().filter(|(r, _)| r == route) {
            return k.clone();
        }
        let k = tools.voice_sample_key(route);
        *last = Some((route.clone(), k.clone()));
        k
    }
    /// The best cosine of `query` against every enrolled voice of Heimdall's.
    pub(crate) fn similarity(&self, query: &Embedding) -> Option<(String, f32)> {
        self.profiles
            .iter()
            .filter_map(|(k, v)| v.embeddings.iter().map(|e| e.cosine(query)).max_by(f32::total_cmp).map(|c| (k.clone(), c)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }
    /// Whether `key` has a profile of `recipe` (`recipe_for`).
    pub(crate) fn enrolled(&self, key: &str, recipe: &str) -> bool {
        self.profiles.get(key).is_some_and(|v| v.recipe == recipe)
    }
}

impl super::Pipeline {
    /// The own-voice check applies to this barge-in: through speakers, with the speaking voice enrolled and the
    /// encoder up.
    pub(super) fn voice_check_ready(&self) -> bool {
        #[cfg(test)]
        return !self.hooks.voice.is_empty() || !self.hooks.user_voice.is_empty() || self.hooks.voice_enrolled || self.hooks.voice_fn.is_some() || self.hooks.user_fn.is_some();
        #[cfg(not(test))]
        {
            // The user's profile or the speaking voice's (decision 31: his voice stops Heimdall; its own never does).
            let Some(encoder) = &self.heimdall_voice.encoder else { return false };
            // A profile of an older recipe still tells its voice while the new one enrols.
            let key = self.heimdall_voice.key(&self.tools, &self.tools.speech_voices.route());
            encoder.available() && (self.heimdall_voice.profiles.contains_key(&key) || self.heimdall_voice.user.ready())
        }
    }
    /// Ask the encoder for one window's verdict; the answer comes back on the live loop (`barge_voice`).
    /// False: no check can run now (the barge-in decides without one).
    pub(super) fn request_voice_check(&mut self, id: u64, samples: Vec<f32>) -> bool {
        #[cfg(test)]
        {
            let heimdall = match self.hooks.voice.pop_front() {
                Some(c) => Some(c),
                None => self.hooks.voice_fn.as_mut().map(|f| f(&samples)),
            };
            let user = match self.hooks.user_voice.pop_front() {
                Some(c) => Some(c),
                None => self.hooks.user_fn.as_mut().map(|f| f(&samples)),
            };
            if heimdall.is_none() && user.is_none() {
                return false;
            }
            let result = if heimdall == Some(None) || user == Some(None) {
                Err("injected encoder failure".to_string())
            } else {
                Ok(Scores { user: user.flatten(), heimdall: heimdall.flatten().map(|c| (String::from("test"), c)) })
            };
            if self.hooks.lose_voice_answers > 0 {
                self.hooks.lose_voice_answers -= 1;
            } else {
                self.hooks.voice_answers.push_back((id, result));
            }
            true
        }
        #[cfg(not(test))]
        {
            let Some(encoder) = self.heimdall_voice.encoder.clone() else { return false };
            let tx = self.internal.0.clone();
            encoder.check(samples, Box::new(move |result| {
                let result = result.map_err(|e| format!("{e:#}"));
                let _ = tx.send(super::Internal::Inference(Box::new(move |p: &mut super::Pipeline| {
                    let scored = result.map(|e| p.heimdall_voice.scores(&e));
                    p.barge_voice(id, scored)
                })));
            }))
        }
    }
    /// One of his own turns woke Heimdall (outside a call, Heimdall silent): a sample of the user's voice, embedded off
    /// this thread and added to his profile (`UserVoice::add`).
    pub(super) fn user_voice_heard(&mut self, samples: &[f32], misfit: bool) {
        if samples.len() < (WINDOW_NS / 62_500) as usize || self.heimdall_voice.user_pending {
            return;
        }
        #[cfg(test)]
        if let Some(e) = self.hooks.user_samples.pop_front() {
            return self.user_voice_embedded(Ok(e), misfit);
        }
        let Some(encoder) = self.heimdall_voice.encoder.clone().filter(|e| e.available()) else { return };
        self.heimdall_voice.user_pending = true;
        let audio = window(samples).to_vec();
        let tx = self.internal.0.clone();
        let spawned = std::thread::Builder::new().name("user-voice-enrol".into()).spawn(move || {
            let _ = heimdall_night::gate::background_thread();
            let result = encoder.embed_blocking(&audio);
            let _ = tx.send(super::Internal::Inference(Box::new(move |p: &mut super::Pipeline| {
                p.user_voice_embedded(result, misfit);
                Ok(())
            })));
        });
        if spawned.is_err() {
            self.heimdall_voice.user_pending = false;
        }
    }
    pub(super) fn user_voice_embedded(&mut self, result: Result<Embedding>, misfit: bool) {
        self.heimdall_voice.user_pending = false;
        let e = match result {
            Ok(e) => e,
            Err(e) => {
                eprintln!("voice check: a sample of the user's voice failed: {e:#}");
                return;
            }
        };
        let was = self.heimdall_voice.user.ready();
        let kept = if misfit { self.heimdall_voice.user.misfit(e, unix_ms()) } else { self.heimdall_voice.user.add(e, unix_ms()) };
        if let Some(path) = &self.heimdall_voice.user_path {
            if let Err(e) = self.heimdall_voice.user.save(path) {
                eprintln!("voice check: the user's voice was not saved: {e:#}");
            }
        }
        if kept && !was && self.heimdall_voice.user.ready() {
            eprintln!("voice check: the user's voice is known ({} samples); barge-in now stops on it", self.heimdall_voice.user.embeddings.len());
        }
    }
    pub(super) fn warm_voice_check(&self) {
        if let Some(e) = &self.heimdall_voice.encoder {
            e.warm();
        }
    }

    /// Every live-loop tick: enrol a voice that can speak now and has no profile yet (one at a time, quietly).
    pub(crate) fn heimdall_voice_tick(&mut self) {
        if self.replay || self.heimdall_voice.enrolling.is_some() {
            return;
        }
        let Some(encoder) = self.heimdall_voice.encoder.clone() else { return };
        // Speaking off: nothing speaks, so nothing is enrolled (and nothing is loaded for it).
        if !self.tools.enabled.load(Ordering::Acquire) || !encoder.available() || self.tools.heimdall_speaking() || self.tools.agent_speech_busy() {
            return;
        }
        let now = Instant::now();
        let Some((route, key)) = self.tools.speaking_voices().into_iter().map(|r| { let k = self.heimdall_voice.key(&self.tools, &r); (r, k) }).find(|(r, key)| {
            !self.heimdall_voice.enrolled(key, &recipe_for(r))
                && self.heimdall_voice.retry.get(key).is_none_or(|at| now >= *at)
                && self.tools.voice_renderable(r)
        }) else {
            return;
        };
        self.heimdall_voice.enrolling = Some(key.clone());
        let tools = self.tools.clone();
        let tx = self.internal.0.clone();
        let speed = self.tools.voice_speed.load(Ordering::Acquire) as f32 / 100.;
        let recipe = recipe_for(&route);
        let _ = std::thread::Builder::new().name("heimdall-voice-enrol".into()).spawn(move || {
            // This thread only (background QoS); `setpriority(PRIO_PROCESS)` would renice the whole app.
            let _ = heimdall_night::gate::background_thread();
            let busy = tools.voice_busy();
            let result = (|| -> Result<HeimdallVoice> {
                // The user talking, a reply or a sample stops the enrolment; it is tried again later.
                ensure!(busy(false).is_none(), "Heimdall or User started talking");
                let rendered = tools.render_voice_lines(&route, &LINES, &|| busy(false).is_some())?;
                let lines = rendered.iter().map(|a| crate::audio::mono_16k(&a.samples, a.sample_rate, 1)).collect::<Result<Vec<_>>>()?;
                let mut voice = profile(&lines, speed, &mut |x| encoder.embed_blocking(x), unix_ms())?;
                voice.recipe = recipe;
                Ok(voice)
            })();
            let _ = tx.send(super::Internal::Inference(Box::new(move |p: &mut super::Pipeline| p.heimdall_voice_enrolled(key, result))));
        });
    }
    /// An enrolment finished: the profile is saved with the other voices (`speakers.json`, under the enrolment lock)
    /// and used from the next check on. A failure waits `RETRY` for that voice.
    pub(crate) fn heimdall_voice_enrolled(&mut self, key: String, result: Result<HeimdallVoice>) -> Result<()> {
        self.heimdall_voice.enrolling = None;
        let voice = match result {
            Ok(v) => v,
            Err(e) => {
                let busy = format!("{e:#}").contains("started talking") || format!("{e:#}").contains("interrupted");
                // Interrupted by speech: tried again in a minute; anything else waits ten.
                let wait = if busy { Duration::from_secs(60) } else { RETRY };
                self.heimdall_voice.retry.insert(key.clone(), Instant::now() + wait);
                if !busy {
                    eprintln!("voice check: could not enrol Heimdall's {key} voice: {e:#}");
                }
                return Ok(());
            }
        };
        // A failed write waits like a failed enrolment: never a new enrolment every tick.
        self.heimdall_voice.retry.insert(key.clone(), Instant::now() + RETRY);
        let saved = (|| -> Result<speakers::identity::EnrollmentStore> {
            let _guard = self.enrollment_lock.lock().unwrap();
            let path = self.data.join("speakers.json");
            let mut store = if path.exists() { speakers::identity::EnrollmentStore::load(&path)? } else { speakers::identity::EnrollmentStore::default() };
            store.set_heimdall_voice(&key, voice.clone())?;
            store.save(&path)?;
            Ok(store)
        })();
        let saved = match saved {
            Ok(s) => s,
            Err(e) => {
                eprintln!("voice check: Heimdall's {key} voice not saved: {e:#}");
                return Ok(());
            }
        };
        self.heimdall_voice.retry.remove(&key);
        for d in self.discord.values() {
            let _ = d.actor.send(crate::speakers_runtime::Work::Store(saved.clone()));
        }
        eprintln!("voice check: Heimdall's {key} voice enrolled from {} silent lines ({} samples)", voice.lines, voice.embeddings.len());
        let _ = self.index.evidence.event(
            "heimdall_voice_enrolled",
            "heimdall",
            &[],
            capture::host_now_ns(),
            serde_json::json!({"schema":1,"voice":key,"recipe":voice.recipe,"lines":voice.lines,"samples":voice.embeddings.len()}),
        );
        self.heimdall_voice.profiles.insert(key, voice);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verdicts_leave_a_gap_between_heimdall_and_another_voice() {
        assert_eq!(judge(0.53), Speaker::Heimdall, "the lowest echo window measured on 24 Oct");
        assert_eq!(judge(0.45), Speaker::Heimdall);
        assert_eq!(judge(0.42), Speaker::Unsure);
        assert_eq!(judge(0.40), Speaker::Unsure);
        assert_eq!(judge(0.39), Speaker::Other);
        assert_eq!(judge(0.26), Speaker::Other, "the highest window of User's own speech measured on 24 Oct");
        assert_eq!(judge(-0.02), Speaker::Other);
        assert_eq!(judge(f32::NAN), Speaker::Unsure, "a broken score is never a cut");
    }
    #[test]
    fn the_room_path_is_fixed_band_limited_and_at_the_echo_level() {
        let tone = |hz: f32| (0..32000).map(|i| (i as f32 * hz * 2. * std::f32::consts::PI / 16000.).sin() * 0.3).collect::<Vec<f32>>();
        let a = room_path(&tone(1000.));
        assert_eq!(a, room_path(&tone(1000.)), "the same profile every run");
        assert!((rms(&a[8000..]) - 0.0104).abs() < 0.002, "{}", rms(&a[8000..]));
        // Bass is cut before the level is set: a 60 Hz hum comes out as mostly the noise floor.
        let low = room_path(&[tone(60.), tone(1000.)].concat());
        assert!(rms(&low[4000..24000]) < 0.6 * rms(&low[40000..60000]), "{} {}", rms(&low[4000..24000]), rms(&low[40000..60000]));
        assert!(room_path(&vec![0.; 16000]).iter().all(|v| v.abs() <= 0.0053), "silence: the noise floor only");
    }
    #[test]
    fn a_profile_holds_each_line_as_rendered_and_through_the_room_at_both_speeds() -> Result<()> {
        let line: Vec<f32> = (0..40000).map(|i| ((i as f32) * 0.05).sin() * 0.2).collect();
        let long: Vec<f32> = (0..160000).map(|i| ((i as f32) * 0.03).sin() * 0.2).collect();
        let mut seen = vec![];
        let mut embed = |x: &[f32]| { seen.push(x.len()); let mut v = vec![0.; 192]; v[seen.len() % 192] = 1.; Embedding::new(v) };
        let p = profile(&[line.clone(), long], 1.25, &mut embed, 7)?;
        assert_eq!((p.embeddings.len(), p.lines, p.recipe.as_str(), p.enrolled_ms), (8, 2, RECIPE, 7));
        assert!(seen.iter().all(|n| *n <= speakers::embedding::WINDOW_SAMPLES), "{seen:?}");
        assert_eq!(seen[0], 40000);
        assert!(seen[2] < 40000, "User's speed is faster: a shorter line");
        let p = profile(&[line], 1.0, &mut |_| Embedding::new([1.; 192].to_vec()), 0)?;
        assert_eq!(p.embeddings.len(), 2, "natural speed only once");
        assert!(profile(&[vec![0.1; 100]], 1.0, &mut |_| unreachable!(), 0).is_err());
        Ok(())
    }
    #[test]
    fn similarity_is_the_best_sample_of_the_closest_voice() -> Result<()> {
        let unit = |i: usize| { let mut v = vec![0.; 192]; v[i] = 1.; Embedding::new(v).unwrap() };
        let voice = |e: Vec<Embedding>, recipe: &str| HeimdallVoice { embeddings: e, recipe: recipe.into(), lines: 1, enrolled_ms: 0 };
        let s = State::new([("drama3".to_string(), voice(vec![unit(0), unit(1)], RECIPE)), ("supertonic".to_string(), voice(vec![unit(2)], "old"))].into(), None, None);
        assert_eq!(s.similarity(&unit(1)), Some(("drama3".into(), 1.)));
        assert_eq!(s.similarity(&unit(2)).unwrap().0, "supertonic");
        assert!(s.similarity(&unit(5)).unwrap().1.abs() < 1e-6);
        assert!(s.enrolled("drama3", RECIPE) && !s.enrolled("supertonic", RECIPE), "an old recipe enrols again");
        assert!(!s.enrolled("drama3", &recipe_for(&crate::speech_voice::Route::Jarvis)), "new Drama 3 weights enrol Jarvis again");
        assert_eq!(recipe_for(&crate::speech_voice::Route::Supertonic), RECIPE);
        Ok(())
    }
}
