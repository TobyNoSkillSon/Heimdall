//! The GGUF adapter: Splash's derived files reproduced byte for byte from a real header, and the header reader's
//! rules on small synthetic headers. No download; the real GGUF is only mapped and its header read.
mod hub_support;

use std::path::PathBuf;

use hub_support::{gguf, s, tiny_qwen, SplashModel, V};
use splash::hub::gguf::{derived_files, merged_split, model_config, require_loadable, scalar_metadata, vision_config, Metadata};
use splash::hub::{assembly, json};

fn temp(name: &str, data: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, data).unwrap();
    (dir, path)
}

/// The key check: tokenizer.json (~10 MB), config.json, tokenizer_config.json and chat_template.jinja derived from
/// the real Qwen3.8-27B GGUF and its projector equal Splash's .metadata files, and so does files.json.
#[test]
fn derived_files_match_splash_byte_for_byte() {
    let Some(splash) = SplashModel::find() else {
        return hub_support::skip("no GGUF model prepared by Splash on this Mac");
    };
    let target = std::env::var_os("SPLASH_GGUF").map(PathBuf::from).unwrap_or_else(|| splash.target());
    let vision = splash.path("vision/mmproj.gguf");
    let derived = derived_files(&target, Some(&vision)).unwrap();
    let entry = splash.metadata();
    for (name, data) in &derived {
        let expected = std::fs::read(entry.join(name)).unwrap();
        assert!(*data == expected, "{name} differs from Splash's ({} bytes vs {})", data.len(), expected.len());
    }
    assert_eq!(derived.len(), 4);
    let hashes: serde_json::Map<String, serde_json::Value> = derived
        .iter()
        .map(|(name, data)| (name.to_string(), serde_json::json!(splash::hub::cache::sha256_hex(data))))
        .collect();
    assert_eq!(json::canonical(&serde_json::Value::Object(hashes)), std::fs::read(entry.join("files.json")).unwrap());
}

/// The metadata key is assembly._metadata_key over Splash 1.4.1's adapter and tokenizers release. The entry on this
/// Mac was keyed by the Splash release that prepared it (its gguf.py predates 1.4.1's), so the expected key here is
/// the formula's value under 1.4.1, computed with Python from the same records.
#[test]
fn metadata_key_follows_splash_formula() {
    let Some(splash) = SplashModel::find() else {
        return hub_support::skip("no GGUF model prepared by Splash on this Mac");
    };
    let files = splash.record["files"].as_object().unwrap();
    let key = assembly::metadata_key(files);
    if splash.record["files"]["target/Qwen3.8-27B-UD-Q4_K_M.gguf"]["digest"]
        == "322e194ff79741c7baa497c240f677f54b201b0efab44ca8e50f122b39123482"
    {
        assert_eq!(key, "b037e846fe5d26593f43584e6088905df26d484fd6f4dbd7456aa248449ff630");
    }
    assert_eq!(key.len(), 64);
}

#[test]
fn scalar_metadata_keeps_types_apart_as_the_native_reader() {
    let (_dir, path) = temp("t.gguf", &tiny_qwen(12));
    let metadata = Metadata::open(&path, false).unwrap();
    let scalars = scalar_metadata(&metadata);
    assert_eq!(scalars["string"]["general.architecture"], "qwen35");
    assert_eq!(scalars["unsigned"]["qwen35.block_count"], 4);
    // An f32 widens to the f64 Python reads, and prints as Python's repr.
    let text = String::from_utf8(json::canonical(&scalars)).unwrap();
    assert!(text.contains("\"qwen35.rope.freq_base\": 9.999999974752427e-07"), "{text}");
    assert!(scalars["unsigned"].get("tokenizer.ggml.tokens").is_none());
}

#[test]
fn tiny_header_derives_a_working_tokenizer_and_config() {
    let (dir, path) = temp("t.gguf", &tiny_qwen(12));
    let files = derived_files(&path, None).unwrap();
    let config: serde_json::Value = serde_json::from_slice(&files["config.json"]).unwrap();
    assert_eq!(config["model_type"], "qwen3_5");
    assert_eq!(config["text_config"]["num_hidden_layers"], 4);
    assert_eq!(config["text_config"]["vocab_size"], 259);
    let tokenizer_config: serde_json::Value = serde_json::from_slice(&files["tokenizer/tokenizer_config.json"]).unwrap();
    assert_eq!(tokenizer_config["eos_token"], "<|im_end|>");
    assert_eq!(tokenizer_config["pad_token"], "<|endoftext|>");
    assert_eq!(tokenizer_config["bos_token"], serde_json::Value::Null);
    let tokenizer_path = dir.path().join("tokenizer.json");
    std::fs::write(&tokenizer_path, &files["tokenizer/tokenizer.json"]).unwrap();
    let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).unwrap();
    let ids = tokenizer.encode("he<|im_end|>", false).unwrap().get_ids().to_vec();
    assert_eq!(ids, vec![256, 258]);
}

#[test]
fn header_reader_refuses_what_splash_refuses() {
    let mut v2 = tiny_qwen(12);
    v2[4] = 2;
    let refusal = |data: &[u8]| Metadata::read(data, false).unwrap_err().to_string();
    assert!(refusal(&v2).contains("expected little-endian v3"));
    assert!(refusal(&tiny_qwen(12)[..200]).contains("truncated"));
    let duplicate = gguf(&[("a", V::U32(1)), ("a", V::U32(2))], &[]);
    assert!(refusal(&duplicate).contains("duplicate GGUF metadata key: a"));
    let nested = gguf(&[("a", V::Arr(9, vec![]))], &[]);
    assert!(refusal(&nested).contains("unsupported or oversized"));
}

#[test]
fn loadability_is_judged_from_the_header() {
    let quantized = Metadata::read(&tiny_qwen(12)[..], true).unwrap();
    require_loadable(&quantized).unwrap();
    let bf16 = Metadata::read(&tiny_qwen(30)[..], true).unwrap();
    assert!(require_loadable(&bf16).unwrap_err().to_string().contains("unquantized"));
    let unknown = Metadata::read(&tiny_qwen(99)[..], true).unwrap();
    let message = require_loadable(&unknown).unwrap_err().to_string();
    assert!(message.contains("attn_q.weight type 99 (1 tensor)"), "{message}");
    assert!(message.contains("ffn_down.weight type 99 (4 tensors)"), "{message}");
}

#[test]
fn split_headers_merge_in_order() {
    let part = |no: u32, tensors: &[&str]| {
        let data = gguf(
            &[("split.no", V::U32(no)), ("split.count", V::U32(2)), ("split.tensors.count", V::U32(2)), ("x", s("y"))],
            &tensors.iter().map(|t| (t.to_string(), vec![1], 12)).collect::<Vec<_>>(),
        );
        Metadata::read(&data[..], true).unwrap()
    };
    let merged = merged_split(vec![part(0, &["a"]), part(1, &["b"])]).unwrap();
    assert_eq!(merged.tensors.len(), 2);
    assert!(merged_split(vec![part(1, &["a"]), part(0, &["b"])]).unwrap_err().to_string().contains("out of order"));
    assert!(merged_split(vec![part(0, &["a"])]).unwrap_err().to_string().contains("split.count"));
    assert!(merged_split(vec![part(0, &["a"]), part(1, &["a"])]).unwrap_err().to_string().contains("duplicate"));
}

#[test]
fn unsupported_architectures_are_named() {
    let data = gguf(&[("general.architecture", s("llama"))], &[]);
    let metadata = Metadata::read(&data[..], false).unwrap();
    assert_eq!(model_config(&metadata, None).unwrap_err().to_string(), "unsupported GGUF model architecture: llama");
}

#[test]
fn hostile_vision_headers_are_refused_not_a_panic() {
    let data = gguf(
        &[
            ("general.architecture", s("clip")),
            ("clip.projector_type", s("qwen3vl_merger")),
            ("clip.use_gelu", V::Bool(true)),
            ("clip.vision.block_count", V::U32(1)),
            ("clip.vision.embedding_length", V::U32(1)),
            ("clip.vision.attention.head_count", V::U32(1)),
            ("clip.vision.feed_forward_length", V::U32(1)),
            ("clip.vision.projection_dim", V::U32(1)),
            ("clip.vision.patch_size", V::U32(1)),
            ("clip.vision.spatial_merge_size", V::U32(1)),
            ("clip.vision.image_size", V::U64(u64::MAX)),
            ("clip.vision.is_deepstack_layers", V::Arr(7, vec![])),
        ],
        &[],
    );
    let vision = Metadata::read(&data[..], false).unwrap();
    assert!(vision_config(&vision).is_err());
}
