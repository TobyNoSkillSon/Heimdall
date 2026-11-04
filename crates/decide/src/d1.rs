use crate::{Answer, DecisionInput, FixedCommand, Question};
use anyhow::{anyhow, ensure, Context, Result};
use libloading::Library;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::{c_char, c_void, CStr, CString},
    path::Path,
    ptr::NonNull,
};

/// Decider V6 (4 Nov), a LiquidAI d1-2B fine-tune from V5, as its trained 16-bit checkpoint folder (config.json,
/// tokenizer files, model.safetensors, calibration.json): the parity tests' and replay tools' model. The app loads the
/// version and precision in its settings (`precision::Pick`: V6 in 4-bit by default, V5 the rollback; the 8-bit and
/// 4-bit folders and V5's three sit beside this one). The 600M GGUFs stay loadable through the llama.cpp runtime.
pub const DEFAULT_MODEL: &str = "d1/d1-2B-heimdall-v6";
/// Its runtime, relative to the decide models folder: the d1v5 MLX engine (crates/decide/engine-v5,
/// scripts/build_decider_v5.sh) with `mlx.metallib` beside it.
pub const DEFAULT_LIBRARY: &str = "runtime-v5/libd1v5_engine.dylib";
/// The llama.cpp runtime for the 600M GGUFs (crates/decide/native, scripts/build_native.sh).
pub const LLAMA_LIBRARY: &str = "runtime/libheimdall_decide.dylib";

type Decide =
    unsafe extern "C" fn(*mut c_void, *const c_char, i32, *mut *mut c_char) -> *mut c_char;
type Free = unsafe extern "C" fn(*mut c_char);
type Destroy = unsafe extern "C" fn(*mut c_void);
/// Owns an in-process native model. No HTTP, token generation, Python, or external app.
pub struct D1 {
    state: crate::gpu_recovery::Recovery<NativeD1>,
    paths: [std::path::PathBuf; 2],
    _runtime: Library,
    version: String,
    /// Which dispatch question the model was trained on (`send_v3::Generation::of` its file).
    generation: crate::send_v3::Generation,
    pub(crate) last_output: Option<Value>,
    pub last_send_request: Option<crate::send::Fitted>,
}
impl D1 {
    pub fn load_default(library: impl AsRef<Path>, models: impl AsRef<Path>) -> Result<Self> {
        Self::load(library, models.as_ref().join(DEFAULT_MODEL))
    }

    pub fn load(
        library: impl AsRef<Path>,
        model: impl AsRef<Path>,
    ) -> Result<Self> {
        let paths: [std::path::PathBuf; 2] = [
            library.as_ref().into(),
            model.as_ref().into(),
        ];
        let runtime = unsafe { Library::new(&paths[0])? };
        let native = NativeD1::load(&paths[0], &paths[1], true)?;
        let version = native.version.clone();
        let generation = crate::send_v3::Generation::of(&paths[1]);
        Ok(Self {
            state: crate::gpu_recovery::Recovery::new(native, true),
            paths,
            version,
            generation,
            last_output: None,
            last_send_request: None,
            _runtime: runtime,
        })
    }
    pub fn send_decision(
        &mut self,
        snapshot: &crate::send::Snapshot,
    ) -> Result<(crate::send::Fitted, crate::send::Prediction)> {
        self.send_decision_interruptible(snapshot, &std::sync::atomic::AtomicUsize::new(0))
    }
    pub fn send_decision_interruptible(
        &mut self,
        snapshot: &crate::send::Snapshot,
        urgent: &std::sync::atomic::AtomicUsize,
    ) -> Result<(crate::send::Fitted, crate::send::Prediction)> {
        let paths = &self.paths;
        let generation = self.generation;
        self.last_send_request = None;
        let request = &mut self.last_send_request;
        self.state.run(
            |gpu| {
                if gpu {
                    unsafe {
                        let library = Library::new(&paths[0])?;
                        let reset = library.get::<unsafe extern "C" fn() -> bool>(
                            b"heimdall_d1_recreate_device\0",
                        )?;
                        if !reset() {
                            return Err(crate::gpu_recovery::GpuError(
                                "device still in use".into(),
                            )
                            .into());
                        }
                    }
                }
                NativeD1::load(&paths[0], &paths[1], gpu)
            },
            |model| {
                let (fitted, lang) = match generation {
                    crate::send_v3::Generation::V2 => (snapshot.fit(|text| model.tokens(text))?, None),
                    crate::send_v3::Generation::V3 => {
                        let (fitted, lang) = crate::send_v3::fit(snapshot, |text| model.tokens(text))?;
                        (fitted, Some(lang))
                    }
                    crate::send_v3::Generation::V5 => (crate::send_v3::fit_v5(snapshot, |text| model.tokens(text))?, None),
                };
                *request = Some(fitted.clone());
                ensure!(
                    urgent.load(std::sync::atomic::Ordering::Acquire) == 0,
                    "send preempted; transcript retained"
                );
                let value = model.raw_request_interruptible(&fitted.request, Some(urgent))?;
                let prediction = match (generation, lang) {
                    (crate::send_v3::Generation::V5, _) => crate::send_v3::parse_v5(value)?,
                    (_, Some(lang)) => crate::send_v3::parse(value, lang)?,
                    (_, None) => crate::send::parse(value)?,
                };
                Ok((fitted, prediction))
            },
        )
    }
    /// Decider V5's suggest question for this snapshot (`crate::suggest`). Only V5 learned it; older deciders refuse.
    pub fn suggest_decision_interruptible(
        &mut self,
        snapshot: &crate::send::Snapshot,
        urgent: &std::sync::atomic::AtomicUsize,
    ) -> Result<(crate::send::Fitted, crate::suggest::Suggestion)> {
        ensure!(self.generation == crate::send_v3::Generation::V5, "the suggest question needs Decider V5");
        let paths = &self.paths;
        self.state.run(
            |gpu| {
                if gpu {
                    unsafe {
                        let library = Library::new(&paths[0])?;
                        let reset = library.get::<unsafe extern "C" fn() -> bool>(b"heimdall_d1_recreate_device\0")?;
                        if !reset() {
                            return Err(crate::gpu_recovery::GpuError("device still in use".into()).into());
                        }
                    }
                }
                NativeD1::load(&paths[0], &paths[1], gpu)
            },
            |model| {
                let fitted = crate::suggest::fit(snapshot, |text| model.tokens(text))?;
                ensure!(urgent.load(std::sync::atomic::Ordering::Acquire) == 0, "suggest preempted");
                let value = model.raw_request_interruptible(&fitted.request, Some(urgent))?;
                Ok((fitted, crate::suggest::parse(value)?))
            },
        )
    }
    pub fn last_question_request(&self)->Option<&Value> {
        self.last_output.as_ref().and_then(|v|v.get("heimdall_request"))
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn generation(&self) -> crate::send_v3::Generation {
        self.generation
    }
    pub fn questions(input: &DecisionInput) -> Value {
        NativeD1::questions(input)
    }
    pub fn request(input: &DecisionInput, p: f32) -> Result<Value> {
        NativeD1::request(input, p)
    }
    pub fn answer(&mut self, input: &DecisionInput, p: f32) -> Result<(Vec<Answer>, u64)> {
        self.last_output = None;
        let layout = crate::StateLayout::for_question(self.generation, input);
        let result = self.state.run(
            |gpu| {
                if gpu {
                    unsafe {
                        let library = Library::new(&self.paths[0])?;
                        let reset = library.get::<unsafe extern "C" fn() -> bool>(
                            b"heimdall_d1_recreate_device\0",
                        )?;
                        if !reset() {
                            return Err(crate::gpu_recovery::GpuError(
                                "device still in use".into(),
                            )
                            .into());
                        }
                    }
                }
                NativeD1::load(&self.paths[0], &self.paths[1], gpu)
            },
            |model| model.answer(input, p, layout),
        );
        self.last_output = self
            .state
            .model
            .as_ref()
            .and_then(|m| m.last_output.clone());
        result
    }
}
struct NativeD1 {
    handle: NonNull<c_void>,
    gpu: bool,
    decide: Decide,
    free: Free,
    destroy: Destroy,
    version: String,
    pub(crate) last_output: Option<Value>,
    _library: Library,
}
// Safety: native handle points to a heap allocation, remains stable on a Rust move,
// and has its own inference thread. &mut self excludes same-handle overlap;
// the native process-wide ownership lock serializes work across all handles.
unsafe impl Send for NativeD1 {}
impl NativeD1 {
    pub fn load(
        library: impl AsRef<Path>,
        model: impl AsRef<Path>,
        gpu: bool,
    ) -> Result<Self> {
        let model = CString::new(model.as_ref().as_os_str().as_encoded_bytes())?;
        // An empty projector path leaves the native audio/image frontend unloaded.
        let proj = CString::new("")?;
        // Safety: fixed C ABI, copied function pointers kept alive by _library; exclusive calls via &mut self.
        unsafe {
            let lib = Library::new(library.as_ref()).context("load bundled d1 runtime")?;
            let create = *lib.get::<unsafe extern "C" fn(
                *const c_char,
                *const c_char,
                i32,
                bool,
                *mut *mut c_char,
            ) -> *mut c_void>(b"heimdall_d1_create_text_backend\0")?;
            let decide = *lib.get::<Decide>(b"heimdall_d1_decide\0")?;
            let free = *lib.get::<Free>(b"heimdall_d1_free\0")?;
            let destroy = *lib.get::<Destroy>(b"heimdall_d1_destroy\0")?;
            let version_fn =
                *lib.get::<unsafe extern "C" fn() -> *const c_char>(b"heimdall_d1_version\0")?;
            let version = CStr::from_ptr(version_fn()).to_string_lossy().into_owned();
            check_version(&version)?;
            let mut error = std::ptr::null_mut();
            let ptr = create(model.as_ptr(), proj.as_ptr(), 2, gpu, &mut error);
            if !error.is_null() {
                let text = CStr::from_ptr(error).to_string_lossy().into_owned();
                free(error);
                return Err(anyhow!(text));
            }
            Ok(Self {
                gpu,
                handle: NonNull::new(ptr).context("native d1 returned null")?,
                decide,
                free,
                destroy,
                version,
                last_output: None,
                _library: lib,
            })
        }
    }
    fn tokens(&self,text:&str)->Result<usize> {
        let text=CString::new(text)?;
        unsafe {
            let f=self._library.get::<unsafe extern "C" fn(*mut c_void,*const c_char)->i32>(b"heimdall_d1_tokens\0")?;
            let n=f(self.handle.as_ptr(),text.as_ptr());ensure!(n>=0,"tokenizer failed");Ok(n as usize)
        }
    }
    fn raw_request_interruptible(&mut self,request:&Value,urgent:Option<&std::sync::atomic::AtomicUsize>)->Result<Value> {
        let body=CString::new(serde_json::to_string(request)?)?;
        if self.gpu && crate::gpu_recovery::injected() {unsafe {
            self._library.get::<unsafe extern "C" fn(*mut c_void)>(b"heimdall_d1_test_fail_next\0")?(self.handle.as_ptr());
        }}
        let mut error=std::ptr::null_mut();
        let raw=unsafe{
            if let Some(urgent)=urgent {
                unsafe extern "C" fn interrupted(ctx:*mut c_void)->bool {(*(ctx as *const std::sync::atomic::AtomicUsize)).load(std::sync::atomic::Ordering::Acquire)>0}
                let f=self._library.get::<unsafe extern "C" fn(*mut c_void,*const c_char,i32,unsafe extern "C" fn(*mut c_void)->bool,*mut c_void,*mut *mut c_char)->*mut c_char>(b"heimdall_d1_decide_interruptible\0")?;
                f(self.handle.as_ptr(),body.as_ptr(),30000,interrupted,urgent as *const _ as *mut c_void,&mut error)
            } else {(self.decide)(self.handle.as_ptr(),body.as_ptr(),30000,&mut error)}
        };
        if !error.is_null() {let message=unsafe{CStr::from_ptr(error)}.to_string_lossy().into_owned();unsafe{(self.free)(error)};if message.contains("HEIMDALL_GPU_FAULT"){return Err(crate::gpu_recovery::record(message).into());}return Err(anyhow!(message));}
        ensure!(!raw.is_null(),"native null send answer");
        let text=unsafe{CStr::from_ptr(raw)}.to_string_lossy().into_owned();unsafe{(self.free)(raw)};
        Ok(serde_json::from_str(&text)?)
    }
    /// Fixed instructions only; caller can supply candidate descriptions, never a new question.
    pub fn questions(input: &DecisionInput) -> Value {
        let mut map = serde_json::Map::new();
        for q in &input.questions {
            let v = match q {
                Question::SendNow => {
                    json!({"type":"noul","instructions":"Is the pending thought complete enough to send now? Judge the accumulated pending thought, including the current utterance, with recent text context. Say no if the speaker is still developing it or likely to continue after this pause."})
                }
                Question::CommandIntent => {
                    json!({"type":"choice","instructions":"Is the user requesting an action?","criteria":{"yes":"A spoken instruction to the assistant.", "false_alarm":"Only discussion, quotation or narration.", "unsure":"An ambiguous utterance."}})
                }
                Question::CommandCheck(command) => {
                    // The original criteria: "Cannot tell." pulled clear commands to unsure (training split, 13 Oct).
                    json!({"type":"choice","instructions":command.check_question(),"criteria":{"yes":"A spoken instruction to the assistant.", "false_alarm":"Only discussion, quotation or narration.", "unsure":"An ambiguous utterance."}})
                }
                Question::CommandSelection => {
                    let opts: BTreeMap<_, _> = crate::CommandSelection::ALL.into_iter().map(|c| (c.key(), c.description())).collect();
                    json!({"type":"choice","instructions":"Which action is requested?","criteria":opts})
                }
                Question::FixedCommand => {
                    let opts: BTreeMap<_, _> = FixedCommand::ALL
                        .into_iter()
                        .map(|c| (c.key(), c.description()))
                        .collect();
                    json!({"type":"choice","instructions":"Which fixed computer command is requested by the current utterance? Choose none for ordinary conversation, quoted commands, or requests addressed to another person.","criteria":opts})
                }
                Question::PickCandidate(c) => {
                    let opts: BTreeMap<_, _> = c
                        .iter()
                        .map(|c| (c.id.clone(), c.description.clone()))
                        .collect();
                    json!({"type":"choice","instructions":"Which of these supplied links or items does the user's utterance refer to? Select only from these candidates.","criteria":opts})
                }
                Question::ReplyLine(c) => {
                    let opts: BTreeMap<_, _> = c
                        .iter()
                        .map(|c| (c.id.clone(), c.description.clone()))
                        .collect();
                    json!({"type":"choice","instructions":crate::reply::INSTRUCTIONS,"criteria":opts})
                }
                Question::BargeIn => {
                    json!({"type":"choice","instructions":crate::barge::INSTRUCTIONS,"criteria":crate::barge::criteria()})
                }
            };
            map.insert(q.key().into(), v);
        }
        Value::Object(map)
    }
    pub fn request(input: &DecisionInput, p: f32) -> Result<Value> {
        // Legacy archived inputs may still carry media/PCM. Only their labelled
        // transcript and text state cross the d1 boundary, for every question.
        Self::request_state(input, p)
    }
    fn request_state(input: &DecisionInput, p: f32) -> Result<Value> {
        input.validate()?;
        ensure!(
            p.is_finite() && (0.0..=1.0).contains(&p),
            "invalid addressedness"
        );
        // Heimdall and the user: the questions as trained; other names read in their place (`crate::names`).
        Ok(json!({"state":input.state_text(p)?,"questions":crate::names::current().question(&Self::questions(input))}))
    }
    /// Exposed for parity/evaluation of the same narrow request; no public arbitrary native route.
    pub fn answer(&mut self, input: &DecisionInput, p: f32, layout: crate::StateLayout) -> Result<(Vec<Answer>, u64)> {
        // A refused current utterance must never inherit an earlier answer in
        // its training evidence.
        self.last_output = None;
        let request = input.fit_text_as(layout, p, |text| self.tokens(text))?;
        let mut value = self.raw_request_interruptible(&request, None)?;
        // Exact executed prompt, also carried through command/reply/barge journals.
        value["heimdall_request"] = request;
        self.last_output = Some(value.clone());
        let result=Self::parse(input, &value)?;
        ensure!(result.1 <= crate::send::WINDOW as u64,"native question exceeded 2000 tokens");
        Ok(result)
    }
    pub(crate) fn parse(input: &DecisionInput, value: &Value) -> Result<(Vec<Answer>, u64)> {
        ensure!(
            value["usage"]["output_tokens"].as_u64() == Some(0),
            "d1 unexpectedly generated tokens"
        );
        let mut result = Vec::new();
        for q in &input.questions {
            let a = &value["answers"][q.key()];
            match q {
                Question::SendNow => {
                    let p = a["noul"].as_f64().context("missing noul probability")?;
                    ensure!(
                        p.is_finite() && (0.0..=1.).contains(&p),
                        "invalid send probability"
                    );
                    result.push(Answer::SendNow {
                        probability: p as f32,
                    });
                }
                Question::CommandIntent | Question::CommandCheck(_) => {
                    let keys: Vec<_> = crate::CommandIntent::ALL.iter().map(|c| c.key()).collect();
                    let probabilities = distribution(a, &keys)?;
                    let pick = a["choice"].as_str().context("missing command intent")?;
                    let intent = crate::CommandIntent::ALL.into_iter().find(|c| c.key() == pick).context("unknown command intent")?;
                    ensure_max(pick, &probabilities)?;
                    result.push(Answer::CommandIntent { intent, probabilities });
                }
                Question::CommandSelection => {
                    let keys: Vec<_> = crate::CommandSelection::ALL.iter().map(|c| c.key()).collect();
                    let probabilities = distribution(a, &keys)?;
                    let pick = a["choice"].as_str().context("missing command selection")?;
                    let command = crate::CommandSelection::ALL.into_iter().find(|c| c.key() == pick).context("unknown command selection")?;
                    ensure_max(pick, &probabilities)?;
                    result.push(Answer::CommandSelection { command, probabilities });
                }
                Question::FixedCommand => {
                    let probs = distribution(
                        a,
                        &FixedCommand::ALL
                            .iter()
                            .map(|c| c.key())
                            .collect::<Vec<_>>(),
                    )?;
                    let pick = a["choice"].as_str().context("missing command")?;
                    let command = FixedCommand::ALL
                        .into_iter()
                        .find(|c| c.key() == pick)
                        .context("native returned unknown command")?;
                    ensure_max(pick, &probs)?;
                    result.push(Answer::FixedCommand {
                        command,
                        probabilities: probs,
                    });
                }
                Question::PickCandidate(c) => {
                    let keys: Vec<_> = c.iter().map(|c| c.id.as_str()).collect();
                    let probs = distribution(a, &keys)?;
                    let id = a["choice"].as_str().context("missing candidate")?;
                    ensure!(keys.contains(&id), "unknown candidate");
                    ensure_max(id, &probs)?;
                    result.push(Answer::PickCandidate {
                        id: id.into(),
                        probabilities: probs,
                    });
                }
                Question::ReplyLine(c) => {
                    let keys: Vec<_> = c.iter().map(|c| c.id.as_str()).collect();
                    let probs = distribution(a, &keys)?;
                    let id = a["choice"].as_str().context("missing reply line")?;
                    ensure!(keys.contains(&id), "unknown reply line");
                    ensure_max(id, &probs)?;
                    result.push(Answer::ReplyLine {
                        id: id.into(),
                        probabilities: probs,
                    });
                }
                Question::BargeIn => {
                    let probs = distribution(a, &[crate::barge::STOP, crate::barge::CONTINUE])?;
                    let choice = a["choice"].as_str().context("missing barge-in choice")?;
                    ensure!(probs.contains_key(choice), "unknown barge-in choice");
                    ensure_max(choice, &probs)?;
                    result.push(Answer::BargeIn { choice: choice.into(), probabilities: probs });
                }
            }
        }
        Ok((
            result,
            value["usage"]["input_tokens"]
                .as_u64()
                .context("missing input token count")?,
        ))
    }
}
/// The runtimes the loader accepts. The llama.cpp runtime prints the guarantees Heimdall relies on as markers; the
/// d1v5 MLX engine gives the same ones by construction (crates/decide/engine-v5/INTEGRATION.md): one process-wide lock
/// serialises all MLX work (ownership), `heimdall_d1_decide_interruptible` polls before each question's forward (async
/// owner), and `heimdall_d1_tokens` counts with the checkpoint's own tokenizer (text window).
pub(crate) fn check_version(version: &str) -> Result<()> {
    if version.starts_with("d1v5-engine@") {
        return Ok(());
    }
    ensure!(
        version.starts_with("llama.cpp@62a6f74bab748b1755f4cd8dfc0871caf865c6c8;"),
        "unexpected native ABI version"
    );
    ensure!(
        version.contains(";native-ownership-v1"),
        "d1 runtime lacks native ownership isolation"
    );
    ensure!(version.contains(";async-owner-v1"),"d1 runtime lacks asynchronous owner ABI");
    ensure!(version.contains(";text-window-v1"),"d1 runtime lacks bounded text slot ABI");
    Ok(())
}
impl Drop for NativeD1 {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.handle.as_ptr());
        }
    }
}
pub(crate) fn distribution(answer: &Value, expected: &[&str]) -> Result<BTreeMap<String, f32>> {
    let obj = answer["probabilities"]
        .as_object()
        .context("missing option distribution")?;
    ensure!(obj.len() == expected.len(), "option distribution changed");
    let mut out = BTreeMap::new();
    let mut sum = 0.;
    for key in expected {
        let p = obj.get(*key).and_then(Value::as_f64).context("missing option")?;
        ensure!(
            p.is_finite() && (0.0..=1.).contains(&p),
            "invalid option probability"
        );
        sum += p;
        out.insert((*key).into(), p as f32);
    }
    ensure!((sum - 1.).abs() < 0.0001, "probabilities do not sum to one");
    Ok(out)
}
#[cfg(test)]
pub(crate) fn parse_for_tests(input: &DecisionInput, value: &Value) -> Result<(Vec<Answer>, u64)> {
    NativeD1::parse(input, value)
}
pub(crate) fn ensure_max(id: &str, probs: &BTreeMap<String, f32>) -> Result<()> {
    let p = probs.get(id).context("unknown chosen option")?;
    ensure!(
        probs.values().all(|v| v <= &(p + 1e-6)),
        "choice disagrees with probabilities"
    );
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod fault_model_tests {
    use super::*;
    #[test]
    #[ignore = "needs Decider V6 model weights that are not in the public repository"]
    fn live_decider_default_loads_the_tuned_model() -> Result<()> {
        use crate::Decider;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let archive = tempfile::tempdir()?;
        let mut decider = crate::LocalDecider::load(&root, archive.path())?;
        assert_eq!(decider.d1.paths[1], root.join(DEFAULT_MODEL));
        assert_eq!(decider.d1.paths.len(), 2);
        let input = crate::barge::input("tuned-default", "wait, that's not what I asked", "The build passed.")?;
        let decision = decider.decide(&input)?;
        assert_eq!(decision.answers.len(), 1);
        assert!(decision.input_tokens > 0);
        // The V5 MLX engine on its default fast path (it names the path that ran) and the V5 generation.
        assert!(decider.d1.version().starts_with("d1v5-engine@"), "{}", decider.d1.version());
        assert!(decider.d1.last_output.as_ref().unwrap()["backend"].as_str().is_some_and(|b| b.starts_with("mlx")));
        assert_eq!(decider.d1.generation(), crate::send_v3::Generation::V5);
        Ok(())
    }
    #[test]
    #[ignore = "requires freshly built fault-aware d1 runtime and pinned assets"]
    fn gpu_fault_d1_does_not_touch_another_instances_context() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let library = std::env::var_os("HEIMDALL_D1_LIBRARY").context("fresh library path")?;
        let load = || {
            D1::load(
                &library,
                root.join(DEFAULT_MODEL),
            )
        };
        let mut healthy = load()?;
        let mut faulty = load()?;
        let input = DecisionInput {
            current: crate::Turn {
                id: "isolation".into(),
                role: crate::Role::Owner,
                transcript: "Please send this now.".into(),
                audio: crate::AudioClip {
                    sample_rate: 16000,
                    samples: vec![0.; 1600],
                },
            },
            source: crate::DecisionSource::Microphone,
            is_final: true,
            context: vec![],
            pending_transcript: String::new(),
            agent_state: String::new(),
            questions: vec![Question::SendNow],
            media: crate::DecisionMedia::Text,
        };
        let baseline = serde_json::to_value(healthy.answer(&input, 0.8)?)?;
        // Arm A, run B first: B must not consume A's injection or sync A.
        let native = faulty.state.model.as_ref().unwrap();
        unsafe {
            native
                ._library
                .get::<unsafe extern "C" fn(*mut c_void)>(b"heimdall_d1_test_fail_next\0")?(
                native.handle.as_ptr(),
            );
        }
        assert_eq!(
            serde_json::to_value(healthy.answer(&input, 0.8)?)?,
            baseline
        );
        assert!(healthy.state.is_gpu());
        faulty.answer(&input, 0.8)?;
        // llama.cpp: device reset is refused while B owns live GPU backends; only A falls back. The d1v5 MLX engine
        // has no shared device to reset (`heimdall_d1_recreate_device` is always true): A reloads on the GPU.
        assert_eq!(faulty.state.is_gpu(), faulty.version().starts_with("d1v5-engine@"));
        assert_eq!(
            serde_json::to_value(healthy.answer(&input, 0.8)?)?,
            baseline
        );
        assert!(healthy.state.is_gpu());
        drop(faulty);
        crate::gpu_recovery::inject(1);
        healthy.answer(&input, 0.8)?;
        assert!(healthy.state.is_gpu());
        Ok(())
    }
    #[test]
    #[ignore = "requires freshly built fault-aware d1 runtime and pinned assets"]
    fn gpu_fault_d1_recreates_falls_back_and_recovers() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let library = std::env::var_os("HEIMDALL_D1_LIBRARY").context("fresh library path")?;
        let mut model = D1::load(
            library,
            root.join(DEFAULT_MODEL),
        )?;
        let input = DecisionInput {
            current: crate::Turn {
                id: "fault".into(),
                role: crate::Role::Owner,
                transcript: "Please send this now.".into(),
                audio: crate::AudioClip {
                    sample_rate: 16000,
                    samples: vec![0.; 1600],
                },
            },
            source: crate::DecisionSource::Microphone,
            is_final: true,
            context: vec![],
            pending_transcript: String::new(),
            agent_state: String::new(),
            questions: vec![Question::SendNow],
            media: crate::DecisionMedia::Text,
        };
        model.answer(&input, 0.8).context("healthy baseline")?;
        for faults in [1, 2, 0] {
            if faults == 0 {
                model.state.retry_now();
            }
            crate::gpu_recovery::inject(faults);
            let (answer, _) = model
                .answer(&input, 0.8)
                .with_context(|| format!("fault cycle {faults}, GPU={}", model.state.is_gpu()))?;
            assert_eq!(answer.len(), 1);
            assert_eq!(model.state.is_gpu(), faults != 2);
        }
        Ok(())
    }
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn both_runtimes_load_and_others_are_refused() {
        assert!(super::check_version("d1v5-engine@0.1.0;mlx-rs-0.32;text-only;json-v1").is_ok());
        let llama = "llama.cpp@62a6f74bab748b1755f4cd8dfc0871caf865c6c8;F16;native-ownership-v1;async-owner-v1;text-window-v1";
        assert!(super::check_version(llama).is_ok());
        assert!(super::check_version(&llama.replace(";async-owner-v1", "")).is_err(), "the llama.cpp markers still hold");
        assert!(super::check_version("llama.cpp@0000;native-ownership-v1;async-owner-v1;text-window-v1").is_err());
        assert!(super::check_version("d1v4-engine@0.1.0").is_err());
    }
}

#[cfg(test)]
mod command_parse_tests {
    use super::*;
    #[test]
    fn every_chain_option_parses_and_unlisted_or_malformed_outputs_fail() {
        let mut input = crate::DecisionInput {
            source: crate::DecisionSource::Microphone,
            is_final: true,
            current: crate::Turn {
                id: "test".into(),
                role: crate::Role::Owner,
                transcript: "look at this".into(),
                audio: crate::AudioClip::new(vec![0.; 160]).unwrap(),
            },
            context: vec![],
            pending_transcript: String::new(),
            agent_state: String::new(),
            media: crate::DecisionMedia::Audio,
            questions: vec![],
        };
        for question in [Question::CommandIntent, Question::CommandCheck(crate::CommandSelection::TakeScreenshot), Question::CommandSelection] {
            input.questions = vec![question.clone()];
            let opts = NativeD1::questions(&input)[question.key()]["criteria"]
                .as_object()
                .unwrap()
                .clone();
            for key in opts.keys() {
                let probabilities: BTreeMap<_, _> = opts
                    .keys()
                    .map(|k| (k.clone(), if k == key { 1.0 } else { 0.0 }))
                    .collect();
                let mut raw = json!({"usage":{"input_tokens":10,"output_tokens":0},"answers":{question.key():{"choice":key,"probabilities":probabilities}}});
                assert!(NativeD1::parse(&input, &raw).is_ok());
                raw["answers"][question.key()]["choice"] = json!("invented_command");
                assert!(NativeD1::parse(&input, &raw).is_err());
                raw["answers"][question.key()]["choice"] = json!(key);
                raw["answers"][question.key()]["probabilities"][key] = json!(0.0);
                assert!(NativeD1::parse(&input, &raw).is_err());
            }
        }
    }
}

#[cfg(test)]
mod joint_graph_safety_tests {
    #[test]
    fn joint_graph_does_not_write_const_batch_row_count() {
        // n_tokens is const in the pinned llm_graph_context. Even an unchanged
        // probability receipt cannot make writing it through const_cast defined.
        let graph = include_str!("../native/joint-decision.inc");
        assert!(!graph.contains("const_cast"), "joint graph writes a const graph-context member");
    }
}

#[cfg(test)]
mod text_boundary_tests {
    use super::*;
    #[test]
    fn every_question_ignores_legacy_audio_and_image_payloads() -> Result<()> {
        let mut input=crate::barge::input("text-only","inspect the first item","The build passed.")?;
        let candidates=vec![crate::Candidate{id:"none".into(),description:"Say nothing".into()},crate::Candidate{id:"first".into(),description:"Rust documentation".into()}];
        for question in [Question::SendNow,Question::CommandIntent,Question::CommandCheck(crate::CommandSelection::LookAtThis),Question::CommandSelection,Question::FixedCommand,Question::PickCandidate(candidates.clone()),Question::ReplyLine(candidates),Question::BargeIn] {
            input.questions=vec![question];
            input.media=crate::DecisionMedia::Text;
            let expected=D1::request(&input,0.)?;
            for media in [crate::DecisionMedia::Audio,crate::DecisionMedia::Image(crate::Image{mime:"image/png".into(),bytes:vec![1]})] {
                input.media=media;
                // Even obsolete/malformed PCM is irrelevant to text inference.
                input.current.audio=crate::AudioClip {samples:vec![f32::NAN],sample_rate:1};
                assert_eq!(D1::request(&input,0.)?,expected);
                assert!(expected.get("files").is_none());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod bounded_text_tests {
    use super::*;
    #[test]
    #[ignore = "needs Decider V6 model weights that are not in the public repository"]
    fn oversized_current_is_refused_without_reusing_previous_model_evidence() -> Result<()> {
        let models =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let mut model = D1::load_default(models.join(DEFAULT_LIBRARY), &models)?;
        let mut input =
            crate::barge::input("refused", "wait use the other branch", "The build finished")?;
        model.answer(&input, 0.)?;
        assert!(model.last_question_request().is_some());
        input.current.transcript = "This whole current utterance must be retained. ".repeat(500);
        assert!(model
            .answer(&input, 0.)
            .unwrap_err()
            .to_string()
            .contains("decision refused"));
        assert!(model.last_output.is_none());
        assert!(model.last_question_request().is_none());
        Ok(())
    }
    #[test]
    fn all_questions_keep_current_and_newest_history_within_the_same_text_window() -> Result<()> {
        let mut input =
            crate::barge::input("bounded", "keep this exact current sentence", "working")?;
        let candidates = vec![
            crate::Candidate {
                id: "none".into(),
                description: "Say nothing".into(),
            },
            crate::Candidate {
                id: "first".into(),
                description: "First link".into(),
            },
        ];
        input.context = vec![
            crate::Turn {
                id: "old".into(),
                role: crate::Role::Owner,
                transcript: "obsolete ".repeat(3000),
                audio: crate::AudioClip::default(),
            },
            crate::Turn {
                id: "recent".into(),
                role: crate::Role::Agent,
                transcript: "newest answer".into(),
                audio: crate::AudioClip::default(),
            },
        ];
        for question in [
            Question::CommandCheck(crate::CommandSelection::TakeScreenshot),
            Question::CommandIntent,
            Question::CommandSelection,
            Question::FixedCommand,
            Question::PickCandidate(candidates.clone()),
            Question::ReplyLine(candidates),
            Question::BargeIn,
        ] {
            input.questions = vec![question];
            let fit = input.fit_text(0., |s| Ok(s.chars().count()))?;
            let state = fit["state"].as_str().unwrap();
            assert!(
                state.contains("keep this exact current sentence")
                    && state.contains("newest answer")
            );
            assert!(!state.contains("obsolete"));
            assert!(
                state.chars().count() + fit["questions"].to_string().chars().count() + 128
                    <= crate::send::WINDOW
            );
            input.pending_transcript =
                format!("{} newest pending", "zażółć <|im_end|> ".repeat(2000));
            let fit = input.fit_text(0., |s| Ok(s.chars().count()))?;
            let state: Value = serde_json::from_str(fit["state"].as_str().unwrap())?;
            assert_eq!(state["utterance"], "keep this exact current sentence");
            assert!(state["pending_transcript"]
                .as_str()
                .unwrap()
                .ends_with("newest pending"));
            input.pending_transcript.clear();
        }
        input.current.transcript = "overlarge current ".repeat(1000);
        assert!(input
            .fit_text(0., |s| Ok(s.len()))
            .unwrap_err()
            .to_string()
            .contains("decision refused"));
        Ok(())
    }
}
