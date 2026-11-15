use crate::{Answer, DecisionInput, Question};
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
/// 4-bit folders and V5's three sit beside this one).
pub const DEFAULT_MODEL: &str = "d1/d1-2B-heimdall-v6";
/// Its runtime, relative to the decide models folder: the d1v5 MLX engine (crates/decide/engine-v5,
/// scripts/build_decider_v5.sh) with `mlx.metallib` beside it.
pub const DEFAULT_LIBRARY: &str = "runtime-v5/libd1v5_engine.dylib";

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
        Ok(Self {
            state: crate::gpu_recovery::Recovery::new(native, true),
            paths,
            version,
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
                let fitted = crate::send_v3::fit_v5(snapshot, |text| model.tokens(text))?;
                *request = Some(fitted.clone());
                ensure!(
                    urgent.load(std::sync::atomic::Ordering::Acquire) == 0,
                    "send preempted; transcript retained"
                );
                let value = model.raw_request_interruptible(&fitted.request, Some(urgent))?;
                Ok((fitted, crate::send_v3::parse_v5(value)?))
            },
        )
    }
    /// Decider V5's suggest question for this snapshot (`crate::suggest`).
    pub fn suggest_decision_interruptible(
        &mut self,
        snapshot: &crate::send::Snapshot,
        urgent: &std::sync::atomic::AtomicUsize,
    ) -> Result<(crate::send::Fitted, crate::suggest::Suggestion)> {
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
    pub fn questions(input: &DecisionInput) -> Value {
        NativeD1::questions(input)
    }
    pub fn answer(&mut self, input: &DecisionInput) -> Result<(Vec<Answer>, u64)> {
        self.last_output = None;
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
            |model| model.answer(input),
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
    /// Fixed instructions only, never a new question.
    pub fn questions(input: &DecisionInput) -> Value {
        let mut map = serde_json::Map::new();
        for q in &input.questions {
            let v = match q {
                Question::CommandCheck(command) => {
                    // The original criteria: "Cannot tell." pulled clear commands to unsure (training split, 13 Oct).
                    json!({"type":"choice","instructions":command.check_question(),"criteria":{"yes":"A spoken instruction to the assistant.", "false_alarm":"Only discussion, quotation or narration.", "unsure":"An ambiguous utterance."}})
                }
                Question::Legacy => continue,
            };
            map.insert(q.key().into(), v);
        }
        Value::Object(map)
    }
    /// Exposed for parity/evaluation of the same narrow request; no public arbitrary native route.
    pub fn answer(&mut self, input: &DecisionInput) -> Result<(Vec<Answer>, u64)> {
        // A refused current utterance must never inherit an earlier answer in
        // its training evidence.
        self.last_output = None;
        let request = input.fit_text(|text| self.tokens(text))?;
        let mut value = self.raw_request_interruptible(&request, None)?;
        // Exact executed prompt, also carried through the command journal.
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
                Question::CommandCheck(_) => {
                    let keys: Vec<_> = crate::CommandIntent::ALL.iter().map(|c| c.key()).collect();
                    let probabilities = distribution(a, &keys)?;
                    let pick = a["choice"].as_str().context("missing command intent")?;
                    let intent = crate::CommandIntent::ALL.into_iter().find(|c| c.key() == pick).context("unknown command intent")?;
                    ensure_max(pick, &probabilities)?;
                    result.push(Answer::CommandIntent { intent, probabilities });
                }
                Question::Legacy => anyhow::bail!("a legacy question kind is never asked"),
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
/// The runtime the loader accepts: the d1v5 MLX engine (`engine-v5`). It gives Heimdall's guarantees by
/// construction: one process-wide lock serialises all MLX work (ownership), `heimdall_d1_decide_interruptible` polls
/// before each question's forward (async owner), and `heimdall_d1_tokens` counts with the checkpoint's own tokenizer
/// (text window). The 600M llama.cpp runtime was removed on 14 Nov.
pub(crate) fn check_version(version: &str) -> Result<()> {
    ensure!(version.starts_with("d1v5-engine@"), "unexpected native ABI version");
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
    /// Decider V6 at 16 bit (3.6 GB) answers one command check through `LocalDecider::load`. Replaces the 600M-era
    /// barge-in question (V5/V6 never learned barge-in) with the command check V5 learned and V6 kept.
    #[test]
    #[ignore = "model: loads Decider V6 16-bit (models/decide/d1/d1-2B-heimdall-v6) on the MLX engine"]
    fn live_decider_default_loads_the_tuned_model() -> Result<()> {
        use crate::Decider;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let archive = tempfile::tempdir()?;
        let mut decider = crate::LocalDecider::load(&root, archive.path())?;
        assert_eq!(decider.d1.paths[1], root.join(DEFAULT_MODEL));
        let input = crate::check_input("tuned-default", "Heimdall, look at this, the build failed again.", "idle");
        let decision = decider.decide(&input)?;
        assert_eq!(decision.answers.len(), 1);
        assert!(matches!(decision.answers[0], Answer::CommandIntent { .. }));
        assert!(decision.input_tokens > 0);
        // The V5 MLX engine on its default fast path (it names the path that ran).
        assert!(decider.d1.version().starts_with("d1v5-engine@"), "{}", decider.d1.version());
        assert!(decider.d1.last_output.as_ref().unwrap()["backend"].as_str().is_some_and(|b| b.starts_with("mlx")));
        Ok(())
    }
    #[test]
    #[ignore = "lab: loads Decider V6 16-bit twice; requires a freshly built fault-aware d1 runtime (HEIMDALL_D1_LIBRARY)"]
    fn gpu_fault_d1_does_not_touch_another_instances_context() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let library = std::env::var_os("HEIMDALL_D1_LIBRARY").context("fresh library path")?;
        let load = || D1::load(&library, root.join(DEFAULT_MODEL));
        let mut healthy = load()?;
        let mut faulty = load()?;
        let input = crate::check_input("isolation", "Please look at this now.", "idle");
        let baseline = serde_json::to_value(healthy.answer(&input)?)?;
        // Arm A, run B first: B must not consume A's injection or sync A.
        let native = faulty.state.model.as_ref().unwrap();
        unsafe {
            native
                ._library
                .get::<unsafe extern "C" fn(*mut c_void)>(b"heimdall_d1_test_fail_next\0")?(
                native.handle.as_ptr(),
            );
        }
        assert_eq!(serde_json::to_value(healthy.answer(&input)?)?, baseline);
        assert!(healthy.state.is_gpu());
        faulty.answer(&input)?;
        // The d1v5 MLX engine has no shared device to reset (`heimdall_d1_recreate_device` is always true): A reloads
        // on the GPU.
        assert!(faulty.state.is_gpu());
        assert_eq!(serde_json::to_value(healthy.answer(&input)?)?, baseline);
        assert!(healthy.state.is_gpu());
        drop(faulty);
        crate::gpu_recovery::inject(1);
        healthy.answer(&input)?;
        assert!(healthy.state.is_gpu());
        Ok(())
    }
    #[test]
    #[ignore = "lab: loads Decider V6 16-bit; requires a freshly built fault-aware d1 runtime (HEIMDALL_D1_LIBRARY)"]
    fn gpu_fault_d1_recreates_falls_back_and_recovers() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let library = std::env::var_os("HEIMDALL_D1_LIBRARY").context("fresh library path")?;
        let mut model = D1::load(library, root.join(DEFAULT_MODEL))?;
        let input = crate::check_input("fault", "Please look at this now.", "idle");
        model.answer(&input).context("healthy baseline")?;
        for faults in [1, 2, 0] {
            if faults == 0 {
                model.state.retry_now();
            }
            crate::gpu_recovery::inject(faults);
            let (answer, _) = model
                .answer(&input)
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
    fn only_the_mlx_engine_loads() {
        assert!(super::check_version("d1v5-engine@0.1.0;mlx-rs-0.32;text-only;json-v1").is_ok());
        // The removed 600M llama.cpp runtime, and anything else, is refused.
        let llama = "llama.cpp@62a6f74bab748b1755f4cd8dfc0871caf865c6c8;F16;native-ownership-v1;async-owner-v1;text-window-v1";
        assert!(super::check_version(llama).is_err());
        assert!(super::check_version("d1v4-engine@0.1.0").is_err());
    }
}

#[cfg(test)]
mod command_parse_tests {
    use super::*;
    #[test]
    fn every_check_option_parses_and_unlisted_or_malformed_outputs_fail() {
        for command in [crate::CommandSelection::TakeScreenshot, crate::CommandSelection::LookAtThis] {
            let mut input = crate::check_input("test", "look at this", "");
            input.questions = vec![Question::CommandCheck(command)];
            let key = input.questions[0].key();
            let opts = NativeD1::questions(&input)[key]["criteria"].as_object().unwrap().clone();
            assert_eq!(opts.len(), 3);
            for pick in opts.keys() {
                let probabilities: BTreeMap<_, _> = opts
                    .keys()
                    .map(|k| (k.clone(), if k == pick { 1.0 } else { 0.0 }))
                    .collect();
                let mut raw = json!({"usage":{"input_tokens":10,"output_tokens":0},"answers":{key:{"choice":pick,"probabilities":probabilities}}});
                assert!(NativeD1::parse(&input, &raw).is_ok());
                raw["answers"][key]["choice"] = json!("invented_command");
                assert!(NativeD1::parse(&input, &raw).is_err());
                raw["answers"][key]["choice"] = json!(pick);
                raw["answers"][key]["probabilities"][pick] = json!(0.0);
                assert!(NativeD1::parse(&input, &raw).is_err());
            }
        }
        // A legacy question is never parsed into an answer.
        let mut input = crate::check_input("legacy", "stop", "");
        input.questions = vec![Question::Legacy];
        assert!(NativeD1::questions(&input).as_object().unwrap().is_empty());
        let raw = json!({"usage":{"input_tokens":10,"output_tokens":0},"answers":{}});
        assert!(NativeD1::parse(&input, &raw).is_err());
    }
}

#[cfg(test)]
mod text_boundary_tests {
    use super::*;
    #[test]
    fn the_check_ignores_legacy_audio_and_image_payloads() -> Result<()> {
        let mut input = crate::check_input("text-only", "inspect the first item", "The build passed.");
        let count = |s: &str| Ok(s.chars().count());
        let expected = input.fit_text(count)?;
        for media in [crate::DecisionMedia::Audio, crate::DecisionMedia::Image(crate::Image { mime: "image/png".into(), bytes: vec![1] })] {
            input.media = media;
            // Even obsolete/malformed PCM is irrelevant to text inference.
            input.current.audio = crate::AudioClip { samples: vec![f32::NAN], sample_rate: 1 };
            assert_eq!(input.fit_text(count)?, expected);
            assert!(expected.get("files").is_none());
        }
        Ok(())
    }
}

#[cfg(test)]
mod bounded_text_tests {
    use super::*;
    #[test]
    #[ignore = "model: loads Decider V6 16-bit (models/decide/d1/d1-2B-heimdall-v6) on the MLX engine"]
    fn oversized_current_is_refused_without_reusing_previous_model_evidence() -> Result<()> {
        let models =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/decide");
        let mut model = D1::load_default(models.join(DEFAULT_LIBRARY), &models)?;
        let mut input = crate::check_input("refused", "look at this, the other branch", "The build finished");
        model.answer(&input)?;
        assert!(model.last_question_request().is_some());
        input.current.transcript = "This whole current utterance must be retained. ".repeat(500);
        assert!(model
            .answer(&input)
            .unwrap_err()
            .to_string()
            .contains("decision refused"));
        assert!(model.last_output.is_none());
        assert!(model.last_question_request().is_none());
        Ok(())
    }
    #[test]
    fn the_check_keeps_current_and_newest_history_within_the_text_window() -> Result<()> {
        let mut input = crate::check_input("bounded", "keep this exact current sentence", "working");
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
        let fit = input.fit_text(|s| Ok(s.chars().count()))?;
        let state = fit["state"].as_str().unwrap();
        assert!(state.contains("keep this exact current sentence") && state.contains("newest answer"));
        assert!(!state.contains("obsolete"));
        assert!(
            state.chars().count() + fit["questions"].to_string().chars().count() + 128
                <= crate::send::WINDOW
        );
        input.pending_transcript = format!("{} newest pending", "zażółć <|im_end|> ".repeat(2000));
        let fit = input.fit_text(|s| Ok(s.chars().count()))?;
        let state = fit["state"].as_str().unwrap();
        assert!(state.ends_with("newest pending\nUser: keep this exact current sentence"), "{state}");
        assert!(state.chars().count() < crate::send::WINDOW);
        input.pending_transcript.clear();
        input.current.transcript = "overlarge current ".repeat(1000);
        assert!(input
            .fit_text(|s| Ok(s.len()))
            .unwrap_err()
            .to_string()
            .contains("decision refused"));
        Ok(())
    }
}
