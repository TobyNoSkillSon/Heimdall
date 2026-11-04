//! Shared by the hub tests: small synthetic GGUF headers, and the prepared model Splash left on this Mac.
#![allow(dead_code)]
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use serde_json::Value;

/// A GGUF metadata value to write.
pub enum V {
    U32(u32),
    I32(i32),
    U64(u64),
    F32(f32),
    Bool(bool),
    Str(String),
    Arr(u32, Vec<V>),
}

pub fn s(value: &str) -> V {
    V::Str(value.into())
}

fn kind(v: &V) -> u32 {
    match v {
        V::U32(_) => 4,
        V::I32(_) => 5,
        V::F32(_) => 6,
        V::Bool(_) => 7,
        V::Str(_) => 8,
        V::Arr(..) => 9,
        V::U64(_) => 10,
    }
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    out.extend((value.len() as u64).to_le_bytes());
    out.extend(value.as_bytes());
}

fn put(out: &mut Vec<u8>, v: &V) {
    match v {
        V::U32(x) => out.extend(x.to_le_bytes()),
        V::I32(x) => out.extend(x.to_le_bytes()),
        V::U64(x) => out.extend(x.to_le_bytes()),
        V::F32(x) => out.extend(x.to_le_bytes()),
        V::Bool(x) => out.push(*x as u8),
        V::Str(x) => put_str(out, x),
        V::Arr(element, items) => {
            out.extend(element.to_le_bytes());
            out.extend((items.len() as u64).to_le_bytes());
            for item in items {
                put(out, item);
            }
        }
    }
}

/// A GGUF v3 header (no tensor data): keys, then tensors as (name, extents, GGML type).
pub fn gguf(keys: &[(&str, V)], tensors: &[(String, Vec<u64>, u32)]) -> Vec<u8> {
    let mut out = b"GGUF".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend((tensors.len() as u64).to_le_bytes());
    out.extend((keys.len() as u64).to_le_bytes());
    for (key, value) in keys {
        put_str(&mut out, key);
        out.extend(kind(value).to_le_bytes());
        put(&mut out, value);
    }
    for (offset, (name, extents, ty)) in tensors.iter().enumerate() {
        put_str(&mut out, name);
        out.extend((extents.len() as u32).to_le_bytes());
        for e in extents {
            out.extend(e.to_le_bytes());
        }
        out.extend(ty.to_le_bytes());
        out.extend((offset as u64 * 32).to_le_bytes());
    }
    out
}

/// GPT-2's byte-level alphabet, in byte order.
pub fn byte_alphabet() -> Vec<String> {
    let printable = |b: u32| (33..=126).contains(&b) || (161..=172).contains(&b) || (174..=255).contains(&b);
    let mut extra = 0;
    (0u32..256)
        .map(|b| {
            if printable(b) {
                char::from_u32(b).unwrap().to_string()
            } else {
                extra += 1;
                char::from_u32(255 + extra).unwrap().to_string()
            }
        })
        .collect()
}

/// A small dense qwen35 target: byte tokens, one merge, two control tokens, four layers (three GDN, one attention),
/// every projection of type `projection` (GGML id).
pub fn tiny_qwen(projection: u32) -> Vec<u8> {
    let mut tokens = byte_alphabet();
    tokens.push("he".into());
    tokens.push("<|endoftext|>".into());
    tokens.push("<|im_end|>".into());
    let mut types = vec![1i32; tokens.len()];
    let n = types.len();
    types[n - 2] = 3;
    types[n - 1] = 3;
    let strings = |items: Vec<String>| V::Arr(8, items.into_iter().map(V::Str).collect());
    let keys = vec![
        ("general.architecture", s("qwen35")),
        ("qwen35.embedding_length", V::U32(64)),
        ("qwen35.context_length", V::U32(4096)),
        ("qwen35.attention.head_count", V::U32(4)),
        ("qwen35.attention.head_count_kv", V::U32(2)),
        ("qwen35.attention.key_length", V::U32(16)),
        ("qwen35.block_count", V::U32(4)),
        ("qwen35.full_attention_interval", V::U32(4)),
        ("qwen35.rope.freq_base", V::F32(1e-6)),
        ("tokenizer.ggml.model", s("gpt2")),
        ("tokenizer.ggml.pre", s("qwen35")),
        ("tokenizer.ggml.tokens", strings(tokens.clone())),
        ("tokenizer.ggml.token_type", V::Arr(5, types.into_iter().map(V::I32).collect())),
        ("tokenizer.ggml.merges", strings(vec!["h e".into()])),
        ("tokenizer.ggml.eos_token_id", V::U32(n as u32 - 1)),
        ("tokenizer.ggml.padding_token_id", V::U32(n as u32 - 2)),
        ("tokenizer.chat_template", s("{% for m in messages %}{{ m.content }}<|im_end|>{% endfor %}")),
    ];
    let (f32, q) = (0u32, projection);
    let mut tensors: Vec<(String, Vec<u64>, u32)> = vec![
        ("token_embd.weight".into(), vec![64, n as u64], q),
        ("output_norm.weight".into(), vec![64], f32),
        ("output.weight".into(), vec![64, n as u64], q),
    ];
    for layer in 0..4 {
        let mut add = |name: &str, ty: u32| tensors.push((format!("blk.{layer}.{name}"), vec![64], ty));
        add("attn_norm.weight", f32);
        add("post_attention_norm.weight", f32);
        if layer == 3 {
            for name in ["attn_q.weight", "attn_k.weight", "attn_v.weight", "attn_output.weight"] {
                add(name, q);
            }
            add("attn_q_norm.weight", f32);
            add("attn_k_norm.weight", f32);
        } else {
            for name in ["attn_qkv.weight", "attn_gate.weight", "ssm_alpha.weight", "ssm_beta.weight", "ssm_out.weight"] {
                add(name, q);
            }
            for name in ["ssm_conv1d.weight", "ssm_a", "ssm_dt.bias", "ssm_norm.weight"] {
                add(name, f32);
            }
        }
        for name in ["ffn_gate.weight", "ffn_up.weight", "ffn_down.weight"] {
            add(name, q);
        }
    }
    gguf(&keys, &tensors)
}

/// Splash's prepared model on this Mac (read-only): its assembly, record and the record's files.
pub struct SplashModel {
    pub assembly: PathBuf,
    pub record: Value,
}

impl SplashModel {
    pub fn find() -> Option<SplashModel> {
        let home = std::env::var_os("HOME")?;
        let resolved = Path::new(&home).join("Library/Application Support/Splash/models/.resolved");
        for entry in std::fs::read_dir(resolved).ok()?.flatten() {
            let Ok(data) = std::fs::read(entry.path().join("model.json")) else { continue };
            let record: Value = serde_json::from_slice(&data).ok()?;
            let gguf = record["target_format"] == "gguf" && record["vision_format"] == "gguf";
            let present = record["files"].as_object()?.values().all(|f| Path::new(f["path"].as_str().unwrap_or("")).exists());
            if gguf && present {
                return Some(SplashModel { assembly: entry.path(), record });
            }
        }
        None
    }

    pub fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(self.record["files"][name]["path"].as_str().expect("a recorded file"))
    }

    pub fn target(&self) -> PathBuf {
        let files = self.record["files"].as_object().unwrap();
        let name = files.keys().find(|k| k.starts_with("target/") && k.ends_with(".gguf")).unwrap();
        self.path(name)
    }

    /// Splash's derived metadata entry.
    pub fn metadata(&self) -> PathBuf {
        self.path("config.json").parent().unwrap().to_path_buf()
    }
}

/// Splash's engine binary, for its model check (no inference).
pub fn engine() -> Option<PathBuf> {
    let path = std::env::var_os("SPLASH_ENGINE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/Cellar/splash/1.4.1/libexec/engine/splash"));
    path.is_file().then_some(path)
}

pub fn skip(why: &str) {
    eprintln!("SKIPPED: {why}");
}

pub struct Stub {
    pub url: String,
    /// Each request: path and Range header.
    pub requests: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

/// A stand-in for the Hub: a one-request-per-connection HTTP/1.1 server: `routes` maps a path (query included) to its body.
pub fn serve(routes: HashMap<String, Vec<u8>>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut range = None;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header.to_lowercase().strip_prefix("range:") {
                    range = Some(value.trim().to_string());
                }
            }
            log.lock().unwrap().push((path.clone(), range.clone()));
            let mut stream = stream;
            if path.starts_with("/o/denied/") {
                let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let Some(body) = routes.get(&path) else {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            };
            let (status, slice) = match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
                Some(spec) => {
                    let (start, end) = spec.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end = end.parse::<usize>().map_or(body.len(), |e| (e + 1).min(body.len()));
                    if start >= body.len() {
                        ("416 Range Not Satisfiable", &body[0..0])
                    } else {
                        ("206 Partial Content", &body[start..end])
                    }
                }
                None => ("200 OK", &body[..]),
            };
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", slice.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(slice);
        }
    });
    Stub { url, requests }
}

pub fn git_sha1(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", data.len()).as_bytes());
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn listing(commit: &str, files: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let siblings: Vec<_> = files
        .iter()
        .map(|(name, data, lfs)| {
            let mut sibling = json!({"rfilename": name, "size": data.len(), "blobId": git_sha1(data)});
            if *lfs {
                sibling["lfs"] = json!({"sha256": hex::encode(Sha256::digest(data)), "size": data.len(), "pointerSize": 134});
            }
            sibling
        })
        .collect();
    serde_json::to_vec(&json!({"sha": commit, "siblings": siblings})).unwrap()
}

