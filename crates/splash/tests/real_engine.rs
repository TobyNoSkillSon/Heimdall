//! The one real check, opt-in (`cargo test -p splash --release --test real_engine -- --ignored`): the Rust port
//! against Splash 1.4.1's own Python on the same prompt and the same native engine. It loads a model (about 22 GB),
//! so it runs only when SPLASH_REFERENCE names the JSON lab/real-check/reference.py wrote. See
//! lab/real-check/README.md.
//!
//! 1. The model is prepared by the Rust installer from the Hugging Face cache (Hub metadata only; it refuses to run
//!    if a file is missing), into a temporary models root.
//! 2. The prompt the Rust template and tokenizer render equals Python's token for token.
//! 3. Greedy generation through the Rust engine client equals Python's.
//! 4. Teacher-forced: at every position of Python's output, the Rust client fed Python's prefix picks Python's next
//!    token.
//! 5. The Rust detokenizer turns those tokens into Python's text.
//! 6. Three timed runs, as Python's were.
use serde_json::{json, Value};
use splash::chat::prompt::{render_prompt, PromptRequest};
use splash::chat::template::ChatTemplates;
use splash::chat::tokens::{PromptTokenizer, Streamer};
use splash::engine::{Engine, EngineOptions, RequestEvent, Submission};
use splash::hub::{self, PrepareOptions};
use splash::server::options::ServeOptions;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn generate(engine: &Engine, prompt: &[u32], generation: u32, count: u32, salt: u128) -> (Vec<u32>, splash::protocol::DoneEvent, f64) {
    let started = Instant::now();
    let request = engine
        .submit(Submission { prompt_tokens: prompt.to_vec(), max_output_tokens: count, generation_prompt_tokens: generation, cache_salt: salt, ..Default::default() })
        .unwrap();
    let collected = request.collect(Duration::from_secs(600)).expect("the engine answered");
    match collected.end {
        RequestEvent::Done(done) => (collected.tokens, done, started.elapsed().as_secs_f64()),
        other => panic!("request failed: {other:?}"),
    }
}

#[test]
#[ignore = "loads the real model; set SPLASH_REFERENCE"]
fn the_rust_port_matches_splashs_python_on_the_real_engine() {
    let reference: Value = serde_json::from_slice(&std::fs::read(std::env::var("SPLASH_REFERENCE").expect("SPLASH_REFERENCE")).unwrap()).unwrap();
    let engine_binary = PathBuf::from(std::env::var("SPLASH_ENGINE").unwrap_or_else(|_| "/opt/homebrew/Cellar/splash/1.4.1/libexec/engine/splash".into()));
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let hf = home.join(".cache/huggingface/hub");
    let models = tempfile::tempdir().unwrap();
    let mut report = json!({});

    // 1. Prepare from the Hub cache.
    // Online for the Hub's metadata only: the commits the cache lacks a record of. Nothing may download.
    let mut options = PrepareOptions::new(models.path());
    options.engine = Some(engine_binary.clone());
    let plan = hub::inspect("unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M", &options).expect("inspected");
    assert_eq!(plan.missing_bytes, 0, "the Hub cache lacks files: {:?}", plan.files.iter().filter(|f| !f.present).map(|f| &f.file).collect::<Vec<_>>());
    report["present_bytes"] = json!(plan.present_bytes);
    let started = Instant::now();
    let prepared = hub::prepare("unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M", &options).expect("prepared offline");
    report["prepare_seconds"] = json!(started.elapsed().as_secs_f64());

    // 2. The prompt.
    let tokenizer = Arc::new(PromptTokenizer::from_dir(&prepared.root.join("tokenizer")).unwrap());
    let templates = ChatTemplates::load(&prepared.root.join("tokenizer"), tokenizer.encoder()).unwrap();
    let body = json!({"messages": [
        {"role": "system", "content": "You are Heimdall, a concise assistant on User's Mac."},
        {"role": "user", "content": "In two sentences: why do lighthouses use Fresnel lenses?"},
    ]});
    let mut no_files = |_: &Value| -> Result<Vec<Value>, splash::chat::ChatError> { unreachable!() };
    let request = PromptRequest::from_body(&body, None, true, &mut no_files).unwrap();
    let rendered = render_prompt(&templates, &tokenizer, &request, None, true).unwrap();
    let python_prompt: Vec<u32> = serde_json::from_value(reference["prompt_tokens"].clone()).unwrap();
    assert_eq!(rendered.text, reference["rendered"].as_str().unwrap(), "rendered prompt");
    assert_eq!(rendered.tokens, python_prompt, "prompt tokens");
    assert_eq!(rendered.thinking, reference["thinking"].as_bool().unwrap());
    assert_eq!(rendered.generation_prompt_tokens as u64, reference["generation_prompt_tokens"].as_u64().unwrap());
    report["prompt_tokens"] = json!(python_prompt.len());

    // The engine, with Python's options for the reference run.
    let serve = ServeOptions { model: "x".into(), max_context: Some(8192), max_memory: Some(24 << 30), max_cache_disk: Some(0), ..Default::default() };
    let command = serve.native_command(&engine_binary, &prepared.root);
    let started = Instant::now();
    let engine = Engine::spawn_with(command, EngineOptions { startup_timeout: Duration::from_secs(600), ..Default::default() }).unwrap();
    engine.wait_ready(Some(Duration::from_secs(600))).expect("engine ready");
    report["load_seconds"] = json!(started.elapsed().as_secs_f64());
    let generation = rendered.generation_prompt_tokens as u32;

    // 3. Greedy, free running.
    let python: Vec<u32> = serde_json::from_value(reference["greedy_tokens"].clone()).unwrap();
    let (greedy, _, _) = generate(&engine, &python_prompt, generation, python.len() as u32, 0);
    report["greedy_equal"] = json!(greedy == python);
    report["greedy_first_difference"] = json!(greedy.iter().zip(&python).position(|(a, b)| a != b));

    // 4. Teacher-forced over Python's output.
    // Python ran the same pass through its own engine client (reference.py): both sides must pick the same token at
    // every position. Where the engine's fresh prefill of a prefix picks other than its own decode did, it does so for
    // both; that is reported, not failed.
    let python_forced: Vec<Option<u32>> = serde_json::from_value(reference["teacher_forced_tokens"].clone()).unwrap();
    let mut matches = 0;
    let mut mismatches = vec![];
    let mut off_greedy = vec![];
    for i in 0..python.len() {
        let mut prompt = python_prompt.clone();
        prompt.extend_from_slice(&python[..i]);
        let (next, _, _) = generate(&engine, &prompt, if i == 0 { generation } else { 0 }, 1, 0);
        if next.first().copied() == python_forced[i] {
            matches += 1;
        } else {
            mismatches.push(json!({"position": i, "python": python_forced[i], "rust": next.first()}));
        }
        if next.first() != Some(&python[i]) {
            off_greedy.push(json!({"position": i, "greedy": python[i], "forced": next.first()}));
        }
    }
    report["teacher_forced"] = json!({"positions": python.len(), "matches_python": matches, "mismatches": mismatches, "differs_from_free_greedy": off_greedy});

    // 5. Detokenized as Python decoded it.
    let mut streamer = Streamer::new(tokenizer.clone(), vec![]);
    let mut text: String = streamer.put_tokens(&python).unwrap().concat();
    text.push_str(&streamer.end().unwrap().concat());
    report["text_equal"] = json!(text == reference["greedy_text"].as_str().unwrap());

    // 6. Timed runs, as reference.py's.
    let mut timed = vec![];
    for salt in 1..=3u128 {
        let (tokens, done, wall) = generate(&engine, &python_prompt, generation, 128, salt);
        timed.push(json!({"tokens": tokens.len(), "decode_ms": done.decode_micros as f64 / 1000.0, "prefill_ms": done.prefill_micros as f64 / 1000.0, "wall_s": wall}));
    }
    report["timed"] = json!(timed);
    engine.shutdown(Duration::from_secs(20));

    // Leave the shared Hub cache as found: remove only this installation's own pins, named by the SHA-256 of its
    // selection link (hub::cache::pin_owner), never another installation's.
    let owner = hub::cache::pin_owner(&prepared.root);
    for repo in ["models--unsloth--Qwen3.8-27B-GGUF", "models--incoai--Qwen3.8-27B-DFlash2"] {
        let pin = hf.join(repo).join("refs/splash").join(&owner);
        if pin.is_dir() {
            std::fs::remove_dir_all(&pin).unwrap();
        }
    }
    if let Ok(path) = std::env::var("SPLASH_RESULT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert_eq!(report["greedy_equal"], true);
    assert_eq!(matches, python.len());
    assert_eq!(report["text_equal"], true);
}
