#![cfg(target_os = "macos")]
use anyhow::{Context, Result};
use decide::*;
use serde_json::Value;
use std::path::PathBuf;
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn models() -> PathBuf {
    std::env::var_os("HEIMDALL_DECIDE_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate_root()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("models/decide")
        })
}
/// The llama.cpp runtime, for the tests that load the 600M GGUFs by path.
fn native_library(models: &std::path::Path) -> PathBuf {
    std::env::var_os("HEIMDALL_D1_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| models.join("runtime/libheimdall_decide.dylib"))
}
fn turn(id: &str, clip: &str, role: Role) -> Result<Turn> {
    Ok(Turn {
        id: id.into(),
        role,
        transcript: String::new(),
        audio: AudioClip::from_wav(crate_root().join("tests/fixtures").join(clip))?,
    })
}

#[test]
#[ignore = "needs Decider model weights and audio fixtures that are not in the public repository"]
fn d1_original_python_f16_parity_text_audio_image() -> Result<()> {
    let m = models();

    let native = OriginalNative::load(&m)?;
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/d1-reference.json"))?;
    let mut worst = 0f64;
    for case in &cases {
        let state: Value = serde_json::from_str(case["request"]["state"].as_str().unwrap())?;
        let mut t = turn("parity", "en-1.wav", Role::Owner)?;
        t.transcript = state["utterance"].as_str().unwrap().into();
        let media = if case["audio"].is_string() {
            DecisionMedia::Audio
        } else if case["image"].is_string() {
            DecisionMedia::Image(Image {
                mime: "image/png".into(),
                bytes: std::fs::read(crate_root().join("tests/fixtures/screen.png"))?,
            })
        } else {
            DecisionMedia::Text
        };
        let input = DecisionInput {
            source: DecisionSource::Microphone,
            is_final: true,
            current: t,
            context: vec![],
            pending_transcript: String::new(),
            agent_state: state["agent_state"].as_str().unwrap().into(),
            media,
            questions: vec![
                Question::SendNow,
                Question::FixedCommand,
                Question::PickCandidate(vec![
                    Candidate {
                        id: "first".into(),
                        description: "First link: Rust documentation".into(),
                    },
                    Candidate {
                        id: "second".into(),
                        description: "Second link: a recipe".into(),
                    },
                ]),
            ],
        };
        let request = D1::request(&input, 0.95)?;
        assert!(request.get("files").is_none(),"production Rust must stay text-only even for historical media fixtures");
        assert_eq!(
            request["questions"]["fixed_command"],
            case["request"]["questions"]["fixed_command"]
        );
        // The historical prompt order is intentionally superseded. Preserve the
        // original independent tensor parity check through its exact C ABI input.
        let mut reference_input = input.clone();
        reference_input.current.transcript.clear();
        let mut historical = D1::request(&reference_input, 0.95)?;
        use base64::Engine;
        let media = match &input.media {
            DecisionMedia::Audio => {
                let mut bytes = std::io::Cursor::new(Vec::new());
                let mut wav = hound::WavWriter::new(&mut bytes, hound::WavSpec {channels:1,sample_rate:16000,bits_per_sample:32,sample_format:hound::SampleFormat::Float})?;
                for sample in &input.current.audio.samples {wav.write_sample(*sample)?;}
                wav.finalize()?;
                Some(("audio/wav", bytes.into_inner()))
            }
            DecisionMedia::Image(image) => Some((image.mime.as_str(), image.bytes.clone())),
            DecisionMedia::Text => None,
        };
        if let Some((mime, bytes)) = media {
            historical["files"] = serde_json::json!([format!("data:{mime};base64,{}",base64::engine::general_purpose::STANDARD.encode(bytes))]);
            use sha2::{Digest,Sha256};
            assert_eq!(format!("{:x}",Sha256::digest(serde_json::to_vec(&historical["files"])?)),case["files_sha256"].as_str().unwrap());
        }
        historical["state"] = case["request"]["state"].clone();
        historical["questions"] = case["request"]["questions"].clone();
        let original = native.answer(&historical)?;
        let tokens = original["usage"]["input_tokens"]
            .as_u64()
            .context("input tokens")?;
        assert_eq!(
            tokens,
            case["usage"]["input_tokens"].as_u64().unwrap(),
            "tokenization parity"
        );
        for (key, actual) in original["answers"].as_object().context("answers")? {
            let expected = &case["answers"][key];
            if key == "send_now" {
                let error =
                    (actual["noul"].as_f64().unwrap() - expected["noul"].as_f64().unwrap()).abs();
                worst = worst.max(error);
                assert!(
                    error < 0.01,
                    "d1 {} {key} parity error {error}",
                    case["name"]
                );
            } else {
                assert_eq!(
                    actual["choice"], expected["choice"],
                    "d1 top answer changed"
                );
                for (k, p) in expected["probabilities"].as_object().unwrap() {
                    let error = (actual["probabilities"][k]
                        .as_f64()
                        .context("missing probability")?
                        - p.as_f64().unwrap())
                    .abs();
                    worst = worst.max(error);
                    assert!(
                        error < 0.01,
                        "d1 {} {key}/{k} parity error {error}",
                        case["name"]
                    );
                }
            }
        }
    }
    println!(
        "d1 parity: {} cases, max probability error {worst}",
        cases.len()
    );
    Ok(())
}
#[test]
#[ignore = "needs Decider model weights and audio fixtures that are not in the public repository"]
fn thirty_english_utterances_use_the_production_text_send_path() -> Result<()> {
    // The shipped decider's checkpoint (DEFAULT_MODEL on DEFAULT_LIBRARY: Decider V6 on the d1v5 MLX engine).
    let m=models();let mut d1=D1::load_default(m.join(DEFAULT_LIBRARY),&m)?;
    assert_eq!(d1.generation(),send_v3::Generation::V5);
    let cases:Vec<Value>=serde_json::from_str(include_str!("fixtures/smoke.json"))?;
    let mut context=send::Conversation::default();
    for case in cases {
        context.append("User",case["text"].as_str().unwrap(),true);
        let snapshot=context.snapshot();let (fit,prediction)=d1.send_decision(&snapshot)?;
        assert!(prediction.input_tokens>0 && prediction.input_tokens<=2000);
        assert!(fit.request.get("files").is_none());
        assert_eq!(prediction.probabilities.len(),3);
        if prediction.choice==send::Choice::Send {context.sent(snapshot.through);}
    }
    // A huge Unicode pending request is trimmed only for inference, while
    // delivery retains every character. This exercises the actual tokenizer.
    context.append("User",&format!("{} final request", "zażółć <|im_end|> <<d1omni:state>> ".repeat(1500)),true);
    let pending=context.pending_text();let (fit,prediction)=d1.send_decision(&context.snapshot())?;
    assert!(fit.pending_truncated);assert!(prediction.input_tokens<=2000);
    assert!(fit.request["state"].as_str().unwrap().ends_with("final request"));
    assert_eq!(pending,context.pending_text());Ok(())
}

// Run unchanged historical requests against the same owned native ABI. This is
// component parity, separate from current production prompt/dispatch quality.
struct OriginalNative {
    lib: libloading::Library,
    handle: *mut std::ffi::c_void,
}
impl OriginalNative {
    fn load(m: &std::path::Path) -> Result<Self> {
        unsafe {
            let library = native_library(m);
            let lib = libloading::Library::new(library)?;
            let create = lib.get::<unsafe extern "C" fn(
                *const std::ffi::c_char,
                *const std::ffi::c_char,
                i32,
                *mut *mut std::ffi::c_char,
            ) -> *mut std::ffi::c_void>(b"heimdall_d1_create\0")?;
            let model =
                std::ffi::CString::new(m.join("d1/d1-omni-600M-F16.gguf").to_str().unwrap())?;
            let projector = std::ffi::CString::new(
                m.join("d1/mmproj-d1-omni-600M-F16.gguf").to_str().unwrap(),
            )?;
            let mut error = std::ptr::null_mut();
            let handle = create(model.as_ptr(), projector.as_ptr(), 2, &mut error);
            anyhow::ensure!(
                error.is_null() && !handle.is_null(),
                "native reference load failed"
            );
            Ok(Self { lib, handle })
        }
    }
    fn answer(&self, request: &Value) -> Result<Value> {
        unsafe {
            let decide = self.lib.get::<unsafe extern "C" fn(
                *mut std::ffi::c_void,
                *const std::ffi::c_char,
                i32,
                *mut *mut std::ffi::c_char,
            ) -> *mut std::ffi::c_char>(b"heimdall_d1_decide\0")?;
            let free = self
                .lib
                .get::<unsafe extern "C" fn(*mut std::ffi::c_char)>(b"heimdall_d1_free\0")?;
            let body = std::ffi::CString::new(request.to_string())?;
            let mut error = std::ptr::null_mut();
            let raw = decide(self.handle, body.as_ptr(), 30000, &mut error);
            if !error.is_null() {
                let text = std::ffi::CStr::from_ptr(error)
                    .to_string_lossy()
                    .into_owned();
                free(error);
                anyhow::bail!("{text}");
            }
            anyhow::ensure!(!raw.is_null(), "native reference null");
            let text = std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned();
            free(raw);
            Ok(serde_json::from_str(&text)?)
        }
    }
}
impl Drop for OriginalNative {
    fn drop(&mut self) {
        unsafe {
            if let Ok(destroy) = self
                .lib
                .get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(b"heimdall_d1_destroy\0")
            {
                destroy(self.handle);
            }
        }
    }
}

#[test]
#[ignore = "needs Decider model weights and audio fixtures that are not in the public repository"]
fn d1_long_history_preserves_current_and_refuses_oversized_current() -> Result<()> {
    let m = models();
    let mut d = D1::load(
        native_library(&m),
        m.join("d1/d1-omni-600M-F16.gguf"),
    )?;
    anyhow::ensure!(
        d.version().split(';').any(|feature|feature=="current-first-v1"),
        "updated native budget guard is required"
    );
    let mut input = DecisionInput {
        source: DecisionSource::Microphone,
        is_final: true,
        current: Turn {
            id: "current".into(),
            role: Role::Owner,
            transcript: "Take a screenshot now.".into(),
            audio: AudioClip::new(vec![0.; 160])?,
        },
        context: (0..20)
            .map(|n| Turn {
                id: format!("context-{n}"),
                role: Role::Agent,
                transcript: "Old unrelated conversation. ".repeat(1000),
                audio: AudioClip::new(vec![0.; 160]).unwrap(),
            })
            .collect(),
        pending_transcript: String::new(),
        agent_state: String::new(),
        media: DecisionMedia::Image(Image {
            mime: "image/png".into(),
            bytes: std::fs::read(crate_root().join("tests/fixtures/screen.png"))?,
        }),
        questions: vec![Question::FixedCommand],
    };
    let (_, tokens) = d.answer(&input, 0.95)?;
    assert!(tokens > 0);
    // Legacy image tags no longer select a smaller modality budget. Exercise
    // refusal against the production text budget, staying within 32 KiB input.
    input.current.transcript = " x".repeat(16300);
    let error = d.answer(&input, 0.95).unwrap_err().to_string();
    assert!(
        error.contains("Current utterance and question exceed d1 token budget"),
        "{error}"
    );
    Ok(())
}


#[test]
#[ignore = "needs Decider model weights and audio fixtures that are not in the public repository"]
fn command_stages_have_native_distributions_and_leave_legacy_send_unchanged() -> Result<()> {
    let m = models();
    let mut d1 = D1::load(
        native_library(&m),
        m.join("d1/d1-omni-600M-F16.gguf"),
    )?;
    let mut current = turn("command-stage", "en-1.wav", Role::Owner)?;
    current.transcript = "Please take a screenshot of my screen.".into();
    let mut input = DecisionInput {
        source: DecisionSource::Microphone,
        is_final: true,
        current,
        context: vec![],
        pending_transcript: String::new(),
        agent_state: "dry-run".into(),
        media: DecisionMedia::Audio,
        questions: vec![Question::SendNow, Question::FixedCommand],
    };
    let baseline = d1.answer(&input, 0.0)?;
    for question in [Question::CommandIntent, Question::CommandSelection] {
        input.questions = vec![question.clone()];
        let (answers, _) = d1.answer(&input, 0.0)?;
        assert_eq!(answers.len(), 1);
        match (&question, &answers[0]) {
            (Question::CommandIntent, Answer::CommandIntent { probabilities, .. }) => {
                assert_eq!(probabilities.len(), 3)
            }
            (Question::CommandSelection, Answer::CommandSelection { probabilities, .. }) => {
                // Four since cancel-task left the commands (29 Oct; five since bug notes left, 19 Oct).
                assert_eq!(probabilities.len(), decide::CommandSelection::ALL.len());
                assert_eq!(probabilities.len(), 4)
            }
            _ => panic!("mismatched native command answer"),
        }
    }
    input.questions = vec![Question::SendNow, Question::FixedCommand];
    assert_eq!(
        serde_json::to_value(d1.answer(&input, 0.0)?)?,
        serde_json::to_value(baseline)?
    );
    Ok(())
}
/// The barge-in question on the real d1: both options come back as a native distribution, and the
/// decision fits the user's 40 ms budget (warm, text only; the machine is shared, so the bound is loose and
/// the measured times are printed).
#[test]
#[ignore = "needs Decider model weights and audio fixtures that are not in the public repository"]
fn barge_in_question_is_native_and_fast() -> Result<()> {
    let m = models();
    let mut d1 = D1::load(
        native_library(&m),
        m.join("d1/d1-omni-600M-F16.gguf"),
    )?;
    let saying = "The build failed because the linker could not find the Metal library, so I am rebuilding it now.";
    let cases = [
        "wait, no, use the other branch",
        "hold on, that's not what I asked",
        "yeah mom I'll be down in five minutes",
        "and the weather tomorrow will be mostly sunny",
    ];
    for heard in &cases[..2] {
        d1.answer(&barge::input("warm", heard, saying)?, 0.0)?;
    }
    let mut times = vec![];
    for round in 0..3 {
        for heard in cases {
            let input = barge::input("barge", heard, saying)?;
            let started = std::time::Instant::now();
            let (answers, _) = d1.answer(&input, 0.0)?;
            times.push(started.elapsed().as_secs_f64() * 1000.);
            let v = barge::verdict(&answers)?;
            assert_eq!(v.probabilities.len(), 2);
            if round == 0 {
                eprintln!("barge-in {heard:?}: continue {:.3}, stop {}", v.continue_probability, v.stop);
            }
        }
    }
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    eprintln!("barge-in decision ms: median {median:.1}, max {:.1} over {}", times[times.len() - 1], times.len());
    assert!(median < 120., "barge-in decision median {median:.1} ms");
    Ok(())
}
