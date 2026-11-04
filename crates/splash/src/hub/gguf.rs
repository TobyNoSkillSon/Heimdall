//! GGUF headers, read without touching tensor data (install/gguf.py): the configuration, tokenizer and chat template
//! the engine and server read are derived from the selected GGUF, and a file the native loader would refuse is
//! refused from its header, before any weight download.
//!
//! The derived files are byte-identical to Splash 1.4.1's, so `ADAPTER` names that release's gguf.py and
//! `TOKENIZERS` the Python tokenizers release whose tokenizer.json bytes ours reproduce: together they key the
//! `.metadata` entry as assembly._metadata_key does.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use serde_json::{json, Map, Value as Json};

use super::{json as pyjson, Error, Result};

/// SHA-256 of Splash 1.4.1's install/gguf.py, the adapter whose output this module reproduces.
pub const ADAPTER: &str = "061e260ce60c20e13f83579a3927cc8e82625f1472ea5138e22e9489df23339b";
/// The Python tokenizers release Splash 1.4.1 ships, whose serialized tokenizer.json ours matches.
pub const TOKENIZERS: &str = "0.22.2";

/// The files `derived_files` derives from a target GGUF, by assembly path.
pub const DERIVED_FILES: [&str; 4] =
    ["config.json", "tokenizer/tokenizer.json", "tokenizer/tokenizer_config.json", "tokenizer/chat_template.jinja"];

const VERSION: u32 = 3;
const MAX_FIELDS: u64 = 16384;
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ITEMS: u64 = 1_000_000;
const MAX_DIMENSIONS: u32 = 4;
const STRING: u32 = 8;
const ARRAY: u32 = 9;

/// One metadata value as Python's struct reading of it types it: every integer width is an int, `?` a bool.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i128),
    Bool(bool),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
}

impl Value {
    fn json(&self) -> Result<Json> {
        Ok(match self {
            Value::Int(i) => int_json(*i)?,
            Value::Bool(b) => Json::Bool(*b),
            Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
            Value::Str(s) => Json::String(s.clone()),
            Value::Array(items) => Json::Array(items.iter().map(Value::json).collect::<Result<_>>()?),
        })
    }
}

/// An integer as JSON; one beyond 64 bits (only a derived one can be) is refused, never a panic.
fn int_json(i: i128) -> Result<Json> {
    if let Ok(v) = i64::try_from(i) {
        return Ok(json!(v));
    }
    u64::try_from(i).map(|v| json!(v)).map_err(|_| Error::model("GGUF metadata value out of range"))
}

/// A bounded little-endian GGUF v3 header (gguf.Metadata).
#[derive(Debug, Default)]
pub struct Metadata {
    pub values: HashMap<String, Value>,
    /// Tensor name -> GGML type, read only when asked for.
    pub tensors: HashMap<String, u32>,
    pub extents: HashMap<String, Vec<u64>>,
    pub offsets: HashMap<String, u64>,
}

struct Reader<R> {
    stream: R,
    consumed: u64,
}

impl<R: Read> Reader<R> {
    fn read(&mut self, size: u64) -> Result<Vec<u8>> {
        if size > MAX_BYTES - self.consumed {
            return Err(Error::model("GGUF metadata exceeds the size limit"));
        }
        let mut data = vec![0; size as usize];
        // A short file is truncated; a failed read (a denied or lost range request) is reported as itself.
        self.stream.read_exact(&mut data).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Error::model("truncated GGUF metadata"),
            _ => Error::from_io(e),
        })?;
        self.consumed += size;
        Ok(data)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.read(N as u64)?.try_into().expect("sized read"))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn string(&mut self) -> Result<String> {
        let size = self.u64()?;
        String::from_utf8(self.read(size)?).map_err(|_| Error::model("invalid UTF-8 in GGUF metadata"))
    }
    fn value(&mut self, kind: u32) -> Result<Value> {
        Ok(match kind {
            STRING => Value::Str(self.string()?),
            ARRAY => {
                let (element, count) = (self.u32()?, self.u64()?);
                if !(element <= 12 && element != ARRAY) || count > MAX_ITEMS {
                    return Err(Error::model("unsupported or oversized GGUF metadata array"));
                }
                (0..count).map(|_| self.value(element)).collect::<Result<_>>().map(Value::Array)?
            }
            0 => Value::Int(self.array::<1>()?[0] as i128),
            1 => Value::Int(i8::from_le_bytes(self.array()?) as i128),
            2 => Value::Int(u16::from_le_bytes(self.array()?) as i128),
            3 => Value::Int(i16::from_le_bytes(self.array()?) as i128),
            4 => Value::Int(self.u32()? as i128),
            5 => Value::Int(i32::from_le_bytes(self.array()?) as i128),
            6 => Value::Float(f32::from_le_bytes(self.array()?) as f64),
            7 => Value::Bool(self.array::<1>()?[0] != 0),
            10 => Value::Int(self.u64()? as i128),
            11 => Value::Int(i64::from_le_bytes(self.array()?) as i128),
            12 => Value::Float(f64::from_le_bytes(self.array()?)),
            _ => return Err(Error::model("unknown GGUF metadata type")),
        })
    }
}

impl Metadata {
    /// Read a header from a stream at the start of the file: a mapped local file or a Hub range reader.
    /// `tensors` also reads each tensor's type, extents and offset (never its payload).
    pub fn read(stream: impl Read, tensors: bool) -> Result<Metadata> {
        let mut r = Reader { stream, consumed: 0 };
        if r.read(4)? != b"GGUF" || r.u32()? != VERSION {
            return Err(Error::model("unsupported GGUF header (expected little-endian v3)"));
        }
        let tensor_count = r.u64()?;
        let key_count = r.u64()?;
        if key_count > MAX_FIELDS || tensor_count > MAX_FIELDS {
            return Err(Error::model("GGUF metadata has too many fields"));
        }
        let mut m = Metadata::default();
        for _ in 0..key_count {
            let key = r.string()?;
            if m.values.contains_key(&key) {
                return Err(Error::model(format!("duplicate GGUF metadata key: {key}")));
            }
            let kind = r.u32()?;
            let value = r.value(kind)?;
            m.values.insert(key, value);
        }
        for _ in 0..if tensors { tensor_count } else { 0 } {
            let name = r.string()?;
            let dimensions = r.u32()?;
            if dimensions > MAX_DIMENSIONS {
                return Err(Error::model(format!("invalid GGUF tensor rank: {name}")));
            }
            let extents = (0..dimensions).map(|_| r.u64()).collect::<Result<Vec<_>>>()?;
            let kind = r.u32()?;
            let offset = r.u64()?;
            if m.tensors.contains_key(&name) {
                return Err(Error::model(format!("duplicate GGUF tensor: {name}")));
            }
            m.tensors.insert(name.clone(), kind);
            m.extents.insert(name.clone(), extents);
            m.offsets.insert(name, offset);
        }
        Ok(m)
    }

    /// Read a local file's header through a read-only map: only the header's pages are ever touched.
    pub fn open(path: &Path, tensors: bool) -> Result<Metadata> {
        let file = File::open(path)?;
        // SAFETY: the map is read-only and dropped before return; a file truncated meanwhile faults, as Splash's
        // native reader would.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Metadata::read(Cursor::new(&map[..]), tensors)
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }
    fn invalid(key: &str) -> Error {
        Error::model(format!("missing or invalid GGUF metadata: {key}"))
    }
    pub fn str(&self, key: &str) -> Result<&str> {
        match self.get(key) {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(Self::invalid(key)),
        }
    }
    pub fn int(&self, key: &str) -> Result<i128> {
        match self.get(key) {
            Some(Value::Int(i)) => Ok(*i),
            _ => Err(Self::invalid(key)),
        }
    }
    pub fn bool(&self, key: &str) -> Result<bool> {
        match self.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            _ => Err(Self::invalid(key)),
        }
    }
    pub fn list(&self, key: &str) -> Result<&[Value]> {
        match self.get(key) {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(Self::invalid(key)),
        }
    }
    pub fn positive(&self, key: &str) -> Result<i128> {
        let value = self.int(key)?;
        if value <= 0 {
            return Err(Error::model(format!("GGUF metadata must be positive: {key}")));
        }
        Ok(value)
    }
}

// The qwen35 profile extends Qwen2's letter class to include combining marks.
const QWEN35_PATTERN: &str = concat!(
    r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?[\p{L}\p{M}]+|\p{N}",
    r"| ?[^\s\p{L}\p{M}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+"
);
// Flint's own pre-tokenizer: digit runs of up to three, runs of Han, hiragana and katakana, then o200k's pattern.
const FLINT_PATTERNS: [&str; 3] = [
    r"\p{N}{1,3}",
    "[\u{4e00}-\u{9fa5}\u{3040}-\u{309f}\u{30a0}-\u{30ff}]+",
    concat!(
        r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+",
        r"(?i:'s|'t|'re|'ve|'m|'ll|'d)?",
        r"|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*",
        r"(?i:'s|'t|'re|'ve|'m|'ll|'d)?",
        r"| ?[\p{P}\p{S}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+"
    ),
];

/// The split patterns and NFC flag of a tokenizer profile (tokenizer.ggml.pre).
fn tokenizer_profile(name: &str) -> Option<(&'static [&'static str], bool)> {
    match name {
        "qwen35" => Some((std::slice::from_ref(&QWEN35_PATTERN), true)),
        "flint" => Some((&FLINT_PATTERNS, false)),
        _ => None,
    }
}

const NORMAL_TOKEN: i128 = 1;
const CONTROL_TOKEN: i128 = 3;
const USER_DEFINED_TOKEN: i128 = 4;
const UNUSED_TOKEN: i128 = 5;

/// DERIVED_FILES' contents from a target GGUF's header and its vision projector's (local paths).
pub fn derived_files(target: &Path, vision: Option<&Path>) -> Result<BTreeMap<&'static str, Vec<u8>>> {
    let metadata = Metadata::open(target, false)?;
    let vision = vision.map(|v| Metadata::open(v, false)).transpose()?;
    let mut files = tokenizer_files(&metadata)?;
    files.insert("config.json", pyjson::canonical(&model_config(&metadata, vision.as_ref())?));
    Ok(files)
}

/// tokenizer.json, tokenizer_config.json and chat_template.jinja from the header's vocabulary, merges, special
/// tokens and template, serialized by the same tokenizers core Python's wraps.
pub fn tokenizer_files(metadata: &Metadata) -> Result<BTreeMap<&'static str, Vec<u8>>> {
    use tokenizers::models::bpe::BPE;
    use tokenizers::normalizers::unicode::NFC;
    use tokenizers::pre_tokenizers::byte_level::ByteLevel;
    use tokenizers::pre_tokenizers::sequence::Sequence;
    use tokenizers::pre_tokenizers::split::{Split, SplitPattern};
    use tokenizers::{AddedToken, PreTokenizerWrapper, SplitDelimiterBehavior, Tokenizer};

    let profile = (metadata.str("tokenizer.ggml.model")? == "gpt2")
        .then(|| metadata.str("tokenizer.ggml.pre").map(tokenizer_profile))
        .transpose()?
        .flatten();
    let Some((patterns, nfc)) = profile else {
        return Err(Error::model("unsupported GGUF tokenizer profile; expected gpt2/qwen35 or gpt2/flint"));
    };
    let invalid_vocabulary = || Error::model("invalid or unsupported GGUF tokenizer vocabulary");
    let tokens: Vec<&str> = metadata
        .list("tokenizer.ggml.tokens")?
        .iter()
        .map(|t| match t {
            Value::Str(s) if !s.is_empty() => Ok(s.as_str()),
            _ => Err(invalid_vocabulary()),
        })
        .collect::<Result<_>>()?;
    let types: Vec<i128> = metadata
        .list("tokenizer.ggml.token_type")?
        .iter()
        .map(|t| match t {
            Value::Int(k) if [NORMAL_TOKEN, CONTROL_TOKEN, USER_DEFINED_TOKEN, UNUSED_TOKEN].contains(k) => Ok(*k),
            _ => Err(invalid_vocabulary()),
        })
        .collect::<Result<_>>()?;
    let merges = metadata.list("tokenizer.ggml.merges")?;
    let vocab: HashMap<String, u32> = tokens.iter().enumerate().map(|(i, t)| (t.to_string(), i as u32)).collect();
    if tokens.is_empty() || vocab.len() != tokens.len() || types.len() != tokens.len() {
        return Err(invalid_vocabulary());
    }
    if !ByteLevel::alphabet().iter().all(|c| vocab.contains_key(&c.to_string())) {
        return Err(Error::model("GGUF byte-level tokenizer is missing byte tokens"));
    }
    let pairs = merges
        .iter()
        .map(|m| match m {
            Value::Str(s) => match s.split(' ').collect::<Vec<_>>()[..] {
                [a, b] => Ok((a.to_string(), b.to_string())),
                _ => Err(Error::model("invalid GGUF BPE merge")),
            },
            _ => Err(Error::model("invalid GGUF BPE merge")),
        })
        .collect::<Result<Vec<_>>>()?;
    for key in ["tokenizer.ggml.add_bos_token", "tokenizer.ggml.add_eos_token"] {
        if !matches!(metadata.get(key), None | Some(Value::Bool(false))) {
            return Err(Error::model(format!("GGUF automatic BOS/EOS insertion is unsupported: {key}")));
        }
    }
    let invalid_bpe = |_| Error::model("invalid GGUF BPE vocabulary or merges");
    let bpe = BPE::builder().vocab_and_merges(vocab.into_iter().collect::<tokenizers::models::bpe::Vocab>(), pairs).build().map_err(invalid_bpe)?;
    let mut backend = Tokenizer::new(bpe);
    if nfc {
        backend.with_normalizer(Some(NFC)).map_err(invalid_bpe)?;
    }
    let mut steps: Vec<PreTokenizerWrapper> = Vec::new();
    for pattern in patterns {
        let split = Split::new(SplitPattern::Regex(pattern.to_string()), SplitDelimiterBehavior::Isolated, false)
            .map_err(|e| Error::model(format!("invalid tokenizer pattern: {e}")))?;
        steps.push(split.into());
    }
    steps.push(ByteLevel::new(false, false, false).into());
    backend.with_pre_tokenizer(Some(Sequence::new(steps)));
    backend.with_post_processor(Some(ByteLevel::new(false, false, false)));
    backend.with_decoder(Some(ByteLevel::default()));
    // Each added token's index and whether it is special, in vocabulary order.
    let added: Vec<(usize, bool)> = types
        .iter()
        .enumerate()
        .filter(|(_, k)| [CONTROL_TOKEN, USER_DEFINED_TOKEN].contains(k))
        .map(|(i, k)| (i, *k == CONTROL_TOKEN))
        .collect();
    backend
        .add_tokens(added.iter().map(|&(i, special)| AddedToken::from(tokens[i], special).normalized(false)))
        .map_err(invalid_bpe)?;

    let special_token = |name: &str, optional: bool| -> Result<Json> {
        let key = format!("tokenizer.ggml.{name}_token_id");
        if optional && metadata.get(&key).is_none() {
            return Ok(Json::Null);
        }
        let index = metadata.int(&key)?;
        if !(0..tokens.len() as i128).contains(&index) || types[index as usize] != CONTROL_TOKEN {
            return Err(Error::model(format!("invalid GGUF special token: {key}")));
        }
        Ok(json!(tokens[index as usize]))
    };
    let template = metadata.str("tokenizer.chat_template")?;
    if template.trim().is_empty() {
        return Err(Error::model("GGUF chat template is empty"));
    }
    let decoder: Map<String, Json> = added
        .iter()
        .map(|&(i, special)| {
            let entry = json!({"content": tokens[i], "special": special, "normalized": false,
                "single_word": false, "lstrip": false, "rstrip": false});
            (i.to_string(), entry)
        })
        .collect();
    let arch = text_architecture(metadata)?;
    let config = json!({
        "tokenizer_class": "TokenizersBackend",
        "model_max_length": int_json(metadata.positive(&format!("{}.context_length", arch.name))?)?,
        "clean_up_tokenization_spaces": false,
        "bos_token": special_token("bos", true)?,
        "eos_token": special_token("eos", false)?,
        "pad_token": special_token("padding", true)?,
        "added_tokens_decoder": decoder,
    });
    let serialized = backend.to_string(false).map_err(|e| Error::model(format!("tokenizer serialization: {e}")))?;
    Ok(BTreeMap::from([
        ("tokenizer/tokenizer.json", serialized.into_bytes()),
        ("tokenizer/tokenizer_config.json", pyjson::canonical(&config)),
        ("tokenizer/chat_template.jinja", template.as_bytes().to_vec()),
    ]))
}

// GGML tensor types by id, and type sets as bitmasks over this table (the sets gguf.py compares by value).
const TENSOR_TYPES: [(u32, &str); 26] = [
    (0, "F32"),
    (1, "F16"),
    (2, "Q4_0"),
    (3, "Q4_1"),
    (6, "Q5_0"),
    (7, "Q5_1"),
    (8, "Q8_0"),
    (10, "Q2_K"),
    (11, "Q3_K"),
    (12, "Q4_K"),
    (13, "Q5_K"),
    (14, "Q6_K"),
    (16, "IQ2_XXS"),
    (17, "IQ2_XS"),
    (18, "IQ3_XXS"),
    (19, "IQ1_S"),
    (20, "IQ4_NL"),
    (21, "IQ3_S"),
    (22, "IQ2_S"),
    (23, "IQ4_XS"),
    (29, "IQ1_M"),
    (30, "BF16"),
    (39, "MXFP4"),
    (40, "NVFP4"),
    (42, "Q2_0"),
    (142, "PQ2_0"),
];

/// A GGML type's name, or `type N` for one Splash does not know.
pub fn type_name(kind: u32) -> String {
    TENSOR_TYPES.iter().find(|(id, _)| *id == kind).map_or_else(|| format!("type {kind}"), |(_, n)| n.to_string())
}

type Types = u64;

fn bit(name: &str) -> Types {
    TENSOR_TYPES.iter().position(|(_, n)| *n == name).map_or(0, |i| 1 << i)
}
fn set(names: &[&str]) -> Types {
    names.iter().map(|n| bit(n)).fold(0, |a, b| a | b)
}
fn quantized() -> Types {
    set(&[
        "Q2_K", "Q3_K", "Q4_K", "Q5_K", "Q6_K", "Q8_0", "Q4_0", "Q4_1", "Q5_0", "Q5_1", "IQ1_S", "IQ1_M", "IQ2_XXS",
        "IQ2_XS", "IQ2_S", "IQ3_XXS", "IQ3_S", "IQ4_NL", "IQ4_XS", "MXFP4", "NVFP4", "Q2_0", "PQ2_0",
    ])
}
fn format_types() -> Types {
    quantized() | set(&["BF16", "F16"])
}
fn unquantized() -> Types {
    set(&["BF16", "F16", "F32"])
}
fn norm() -> Types {
    set(&["F32", "BF16", "F16"])
}
fn float_projection() -> Types {
    set(&["F32", "BF16"])
}
fn float_row() -> Types {
    set(&["F32", "BF16", "Q8_0"])
}
fn f32() -> Types {
    bit("F32")
}

type Tensors = Vec<(String, Types)>;

fn tensors(entries: &[(&str, Types)]) -> Tensors {
    entries.iter().map(|(n, t)| (n.to_string(), *t)).collect()
}
fn model_tensors() -> Tensors {
    tensors(&[("token_embd.weight", format_types()), ("output_norm.weight", norm()), ("output.weight", format_types())])
}
fn layer_tensors() -> Tensors {
    tensors(&[("attn_norm.weight", norm()), ("post_attention_norm.weight", norm())])
}
fn attention_tensors() -> Tensors {
    let f = format_types();
    tensors(&[
        ("attn_q.weight", f),
        ("attn_k.weight", f),
        ("attn_v.weight", f),
        ("attn_q_norm.weight", norm()),
        ("attn_k_norm.weight", norm()),
        ("attn_output.weight", f),
    ])
}
fn gdn_tensors() -> Tensors {
    let f = format_types();
    tensors(&[
        ("attn_qkv.weight", f),
        ("attn_gate.weight", f),
        ("ssm_alpha.weight", f | f32()),
        ("ssm_beta.weight", f | f32()),
        ("ssm_conv1d.weight", f32()),
        ("ssm_a", f32()),
        ("ssm_dt.bias", f32()),
        ("ssm_norm.weight", norm()),
        ("ssm_out.weight", f),
    ])
}
fn dense_tensors() -> Tensors {
    let f = format_types();
    tensors(&[("ffn_gate.weight", f), ("ffn_up.weight", f), ("ffn_down.weight", f)])
}
fn moe_tensors() -> Tensors {
    let f = format_types();
    tensors(&[
        ("ffn_gate_inp.weight", float_projection()),
        ("ffn_gate_exps.weight", f),
        ("ffn_up_exps.weight", f),
        ("ffn_down_exps.weight", f),
        ("ffn_gate_inp_shexp.weight", float_row()),
        ("ffn_gate_shexp.weight", f),
        ("ffn_up_shexp.weight", f),
        ("ffn_down_shexp.weight", f),
    ])
}
/// A MoE layer whose gate and up experts are llama.cpp's fused tensor.
fn fused_experts(moe: &Tensors) -> Tensors {
    let mut fused: Tensors =
        moe.iter().filter(|(n, _)| n != "ffn_gate_exps.weight" && n != "ffn_up_exps.weight").cloned().collect();
    fused.push(("ffn_gate_up_exps.weight".into(), format_types()));
    fused
}
fn qwen4_model_tensors() -> Tensors {
    let f = format_types();
    tensors(&[
        ("token_embd.weight", f),
        ("per_layer_token_embd.weight", f),
        ("output.weight", f),
        ("output_hc_norm.weight", norm()),
        ("output_hc_down.weight", f),
        ("output_hc_up.weight", f),
    ])
}
fn qwen4_layer_tensors() -> Tensors {
    let mut out = Tensors::new();
    for part in ["attn", "ffn"] {
        for (name, types) in [("norm", norm()), ("down", format_types()), ("up", format_types()), ("inject", float_row())]
        {
            out.push((format!("hc_{part}_{name}.weight"), types));
        }
    }
    out
}
fn qwen4_indexer_tensors() -> Tensors {
    tensors(&[
        ("indexer.q_proj.weight", float_projection()),
        ("indexer.k_proj.weight", float_projection()),
        ("indexer.q_norm.weight", norm()),
        ("indexer.k_norm.weight", norm()),
    ])
}
fn qwen4_ple_tensors() -> Tensors {
    let f = format_types();
    tensors(&[
        ("ple_key.weight", f),
        ("ple_value.weight", f),
        ("ple_norm_key.weight", norm()),
        ("ple_norm_query.weight", norm()),
        ("ple_norm_conv.weight", norm()),
        ("ple_conv1d.weight", norm()),
    ])
}
fn flint_layer_tensors() -> Tensors {
    tensors(&[("attn_norm.weight", norm()), ("ffn_norm.weight", norm())])
}

/// A GGUF text architecture Splash serves, as the native loader reads it (gguf.Architecture).
pub struct Architecture {
    pub name: &'static str,
    pub model_type: &'static str,
    pub moe: bool,
    qsa_block: Option<i128>,
    ple_layer: Option<usize>,
    hybrid: bool,
    leading_dense: bool,
    /// Flint's: its configuration is flat and states more than the shared sizes.
    flint: bool,
}

const ARCHITECTURES: [Architecture; 4] = [
    Architecture {
        name: "qwen35",
        model_type: "qwen3_5_text",
        moe: false,
        qsa_block: None,
        ple_layer: None,
        hybrid: true,
        leading_dense: false,
        flint: false,
    },
    Architecture {
        name: "qwen35moe",
        model_type: "qwen3_5_moe_text",
        moe: true,
        qsa_block: None,
        ple_layer: None,
        hybrid: true,
        leading_dense: false,
        flint: false,
    },
    Architecture {
        name: "qwen4exp",
        model_type: "qwen4_exp_text",
        moe: true,
        qsa_block: Some(4),
        ple_layer: Some(1),
        hybrid: true,
        leading_dense: false,
        flint: false,
    },
    Architecture {
        name: "flint",
        model_type: "flint",
        moe: true,
        qsa_block: None,
        ple_layer: None,
        hybrid: false,
        leading_dense: true,
        flint: true,
    },
];

impl Architecture {
    fn model_tensors(&self) -> Tensors {
        if self.name == "qwen4exp" { qwen4_model_tensors() } else { model_tensors() }
    }
    fn layer_tensors(&self) -> Tensors {
        match self.name {
            "qwen4exp" => qwen4_layer_tensors(),
            "flint" => flint_layer_tensors(),
            _ => layer_tensors(),
        }
    }
    fn attention_tensors(&self) -> Tensors {
        match self.name {
            "qwen4exp" => qwen4_indexer_tensors(),
            "flint" => tensors(&[("attn_sinks.weight", f32())]),
            _ => Tensors::new(),
        }
    }
    fn moe_tensors(&self) -> Tensors {
        let all = moe_tensors();
        if self.flint { all.into_iter().filter(|(n, _)| n != "ffn_gate_inp_shexp.weight").collect() } else { all }
    }
}

pub fn text_architecture(metadata: &Metadata) -> Result<&'static Architecture> {
    let arch = metadata.str("general.architecture")?;
    ARCHITECTURES
        .iter()
        .find(|a| a.name == arch)
        .ok_or_else(|| Error::model(format!("unsupported GGUF model architecture: {arch}")))
}

/// The configuration the header describes, in its family's own config.json shape: Qwen's text model under
/// text_config with its vision tower beside it, or Flint's flat.
pub fn model_config(metadata: &Metadata, vision: Option<&Metadata>) -> Result<Json> {
    let architecture = text_architecture(metadata)?;
    let arch = architecture.name;
    let mut fields = vec![
        ("hidden_size", "embedding_length"),
        ("max_position_embeddings", "context_length"),
        ("num_attention_heads", "attention.head_count"),
        ("num_key_value_heads", "attention.head_count_kv"),
        ("head_dim", "attention.key_length"),
    ];
    if architecture.moe {
        // The routed experts, which a pruned release keeps fewer of; the engine sizes them by this count.
        fields.push(("num_experts", "expert_count"));
    }
    let mut text = Map::new();
    for (name, key) in fields {
        text.insert(name.into(), int_json(metadata.positive(&format!("{arch}.{key}"))?)?);
    }
    text.insert("num_hidden_layers".into(), int_json(loaded_layers(metadata, arch)?)?);
    text.insert("model_type".into(), json!(architecture.model_type));
    text.insert("vocab_size".into(), json!(metadata.list("tokenizer.ggml.tokens")?.len()));
    if architecture.flint {
        text.extend(flint_text_config(metadata, arch)?);
        return Ok(Json::Object(text));
    }
    let mut config = Map::new();
    config.insert("model_type".into(), json!(architecture.model_type.trim_end_matches("_text")));
    config.insert("text_config".into(), Json::Object(text));
    if let Some(vision) = vision {
        config.insert("vision_config".into(), vision_config(vision)?);
    }
    Ok(Json::Object(config))
}

/// The header's scalar values as the native reader keeps them for the engine's model check: integers and booleans
/// unsigned (a negative one as its 64-bit two's complement), finite floats, strings; arrays left out.
pub fn scalar_metadata(metadata: &Metadata) -> Json {
    let (mut unsigned, mut float, mut string) = (Map::new(), Map::new(), Map::new());
    for (key, value) in &metadata.values {
        match value {
            Value::Int(i) => {
                unsigned.insert(key.clone(), json!(i.rem_euclid(1 << 64) as u64));
            }
            Value::Bool(b) => {
                unsigned.insert(key.clone(), json!(*b as u64));
            }
            Value::Float(f) if f.is_finite() => {
                float.insert(key.clone(), Value::Float(*f).json().expect("a float"));
            }
            Value::Str(s) => {
                string.insert(key.clone(), json!(s));
            }
            _ => {}
        }
    }
    json!({"unsigned": unsigned, "float": float, "string": string})
}

/// The target's layer count without its MTP layers, which are never loaded.
pub fn loaded_layers(metadata: &Metadata, arch: &str) -> Result<i128> {
    let layers = metadata.positive(&format!("{arch}.block_count"))?;
    let mtp = match metadata.get(&format!("{arch}.nextn_predict_layers")) {
        None => 0,
        Some(Value::Int(i)) => *i,
        Some(_) => -1,
    };
    if !(0..layers).contains(&mtp) {
        return Err(Error::model("invalid GGUF MTP layer count"));
    }
    Ok(layers - mtp)
}

fn dense_layers(metadata: &Metadata, architecture: &Architecture) -> Result<i128> {
    if !architecture.leading_dense {
        return Ok(0);
    }
    let key = format!("{}.leading_dense_block_count", architecture.name);
    let dense = metadata.int(&key)?;
    if !(0..loaded_layers(metadata, architecture.name)?).contains(&dense) {
        return Err(Error::model(format!("invalid GGUF metadata: {key}")));
    }
    Ok(dense)
}

fn layer_pattern(metadata: &Metadata, arch: &str, name: &str) -> Result<Vec<bool>> {
    let key = format!("{arch}.{name}");
    let layers = loaded_layers(metadata, arch)? as usize;
    match metadata.get(&key) {
        Some(Value::Array(values)) if values.len() == layers => values
            .iter()
            .map(|v| if let Value::Bool(b) = v { Ok(*b) } else { Err(Metadata::invalid(&key)) })
            .collect(),
        _ => Err(Metadata::invalid(&key)),
    }
}

/// What Flint's config.json states beyond the shared sizes, from its header (gguf.flint_text_config).
fn flint_text_config(metadata: &Metadata, arch: &str) -> Result<Map<String, Json>> {
    let k = |name: &str| format!("{arch}.{name}");
    let gating = metadata.int(&k("expert_gating_func"))?;
    let router = match gating {
        1 => "softmax",
        2 => "sigmoid",
        _ => return Err(Error::model(format!("unsupported GGUF expert gating function: {gating}"))),
    };
    let stops = metadata.list("tokenizer.ggml.eog_token_ids")?;
    let stops: Vec<Json> = stops
        .iter()
        .map(|t| if let Value::Int(i) = t { int_json(*i) } else { Err(Error::model("bad stop")) })
        .collect::<Result<_>>()
        .ok()
        .filter(|s: &Vec<Json>| !s.is_empty())
        .ok_or_else(|| Error::model("invalid GGUF metadata: tokenizer.ggml.eog_token_ids"))?;
    let sliding = layer_pattern(metadata, arch, "attention.sliding_window_pattern")?;
    let rotated = layer_pattern(metadata, arch, "attention.rope_pattern")?;
    let architecture = text_architecture(metadata)?;
    let mut out = Map::new();
    out.insert("intermediate_size".into(), int_json(metadata.positive(&k("feed_forward_length"))?)?);
    out.insert("moe_intermediate_size".into(), int_json(metadata.positive(&k("expert_feed_forward_length"))?)?);
    out.insert(
        "shared_expert_intermediate_size".into(),
        int_json(metadata.positive(&k("expert_shared_feed_forward_length"))?)?,
    );
    out.insert("num_experts_per_tok".into(), int_json(metadata.positive(&k("expert_used_count"))?)?);
    out.insert("norm_topk_prob".into(), json!(metadata.bool(&k("expert_weights_norm"))?));
    out.insert("router_score_function".into(), json!(router));
    out.insert("mlp_only_layers".into(), json!((0..dense_layers(metadata, architecture)? as i64).collect::<Vec<_>>()));
    out.insert("sliding_window".into(), int_json(metadata.positive(&k("attention.sliding_window"))?)?);
    out.insert(
        "layer_types".into(),
        json!(sliding.iter().map(|s| if *s { "sliding_attention" } else { "full_attention" }).collect::<Vec<_>>()),
    );
    out.insert(
        "no_rope_layer_indices".into(),
        json!(rotated.iter().enumerate().filter(|(_, r)| !**r).map(|(i, _)| i).collect::<Vec<_>>()),
    );
    out.insert("eos_token_id".into(), Json::Array(stops));
    Ok(out)
}

/// A qwen3vl_merger CLIP projector's vision_config.
pub fn vision_config(vision: &Metadata) -> Result<Json> {
    if vision.str("general.architecture")? != "clip"
        || vision.str("clip.projector_type")? != "qwen3vl_merger"
        || !vision.bool("clip.use_gelu")?
    {
        return Err(Error::model("unsupported GGUF vision architecture"));
    }
    let mut out = Map::new();
    let mut patch = 0;
    for (name, key) in [
        ("depth", "block_count"),
        ("hidden_size", "embedding_length"),
        ("num_heads", "attention.head_count"),
        ("intermediate_size", "feed_forward_length"),
        ("out_hidden_size", "projection_dim"),
        ("patch_size", "patch_size"),
        ("spatial_merge_size", "spatial_merge_size"),
    ] {
        let value = vision.positive(&format!("clip.vision.{key}"))?;
        if name == "patch_size" {
            patch = value;
        }
        out.insert(name.into(), int_json(value)?);
    }
    let image_size = vision.positive("clip.vision.image_size")?;
    if image_size % patch != 0 {
        return Err(Error::model("invalid GGUF vision position grid"));
    }
    if vision.list("clip.vision.is_deepstack_layers")?.iter().any(truthy) {
        return Err(Error::model("GGUF vision deepstack layers are unsupported"));
    }
    out.insert("temporal_patch_size".into(), json!(2));
    out.insert("in_channels".into(), json!(3));
    out.insert("num_position_embeddings".into(), int_json((image_size / patch).checked_pow(2).ok_or_else(|| Error::model("invalid GGUF vision position grid"))?)?);
    out.insert("hidden_act".into(), json!("gelu_pytorch_tanh"));
    out.insert("deepstack_visual_indexes".into(), json!([]));
    Ok(Json::Object(out))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Int(i) => *i != 0,
        Value::Bool(b) => *b,
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
    }
}

/// The image processor a projector asks for (gguf.processor_config).
pub fn processor_config(vision: &Metadata) -> Result<Json> {
    let config = vision_config(vision)?;
    Ok(json!({
        "patch_size": config["patch_size"],
        "temporal_patch_size": config["temporal_patch_size"],
        "merge_size": config["spatial_merge_size"],
        "image_mean": Value::Array(vision.list("clip.vision.image_mean")?.to_vec()).json()?,
        "image_std": Value::Array(vision.list("clip.vision.image_std")?.to_vec()).json()?,
    }))
}

/// Each tensor the native loader reads from the target, with the types it accepts (gguf.loaded_tensors).
fn loaded_tensors(metadata: &Metadata) -> Result<HashMap<String, Types>> {
    let architecture = text_architecture(metadata)?;
    let arch = architecture.name;
    let period = if architecture.hybrid { metadata.positive(&format!("{arch}.full_attention_interval"))? } else { 1 };
    let dense = dense_layers(metadata, architecture)?;
    let moe = architecture.moe_tensors();
    let fused = fused_experts(&moe);
    let mut out: HashMap<String, Types> = architecture.model_tensors().into_iter().collect();
    for layer in 0..loaded_layers(metadata, arch)? {
        let mixer = if (layer + 1) % period == 0 {
            [attention_tensors(), architecture.attention_tensors()].concat()
        } else {
            gdn_tensors()
        };
        let ffn = if !architecture.moe || layer < dense {
            dense_tensors()
        } else if metadata.tensors.contains_key(&format!("blk.{layer}.ffn_gate_up_exps.weight")) {
            fused.clone()
        } else {
            moe.clone()
        };
        let mut parts = [architecture.layer_tensors(), mixer, ffn].concat();
        if architecture.ple_layer == Some(layer as usize) {
            parts.extend(qwen4_ple_tensors());
        }
        for (name, types) in parts {
            out.insert(format!("blk.{layer}.{name}"), types);
        }
    }
    Ok(out)
}

/// Refuse a target whose QSA and PLE arrays are not those the native planner requires.
fn require_arrays(metadata: &Metadata) -> Result<()> {
    let architecture = text_architecture(metadata)?;
    let arch = architecture.name;
    let mut mismatched = Vec::new();
    if let Some(block) = architecture.qsa_block {
        let period = metadata.positive(&format!("{arch}.full_attention_interval"))?;
        let ratios: Vec<Value> = (0..loaded_layers(metadata, arch)?)
            .map(|l| Value::Int(if (l + 1) % period == 0 { block } else { 0 }))
            .collect();
        let ok = matches!(metadata.get(&format!("{arch}.attention.compress_ratios")),
            Some(Value::Array(found)) if found.iter().take(ratios.len()).eq(ratios.iter()) && found.len() >= ratios.len());
        if !ok {
            mismatched.push("attention.compress_ratios");
        }
    }
    if let Some(ple) = architecture.ple_layer {
        if metadata.get(&format!("{arch}.ple.layers")) != Some(&Value::Array(vec![Value::Int(ple as i128)])) {
            mismatched.push("ple.layers");
        }
    }
    if !mismatched.is_empty() {
        return Err(Error::model(format!("GGUF metadata does not match the target: {}", mismatched.join(", "))));
    }
    Ok(())
}

const ROTATION_PREFIX: &str = "prism.hadamard.";

/// Refuse a rotated target (Prism ML's GGUFs) whose input rotation the native loader does not run.
fn require_rotation(metadata: &Metadata) -> Result<()> {
    let keys: HashSet<&str> = metadata.values.keys().filter_map(|k| k.strip_prefix(ROTATION_PREFIX)).collect();
    if keys.is_empty() {
        return Ok(());
    }
    let expected = [
        ("version", Value::Int(1)),
        ("block_size", Value::Int(1024)),
        ("transform", Value::Str("normalized-sylvester-walsh-hadamard".into())),
        ("axis", Value::Str("input-last-dimension".into())),
        ("sign_mode", Value::Str("explicit".into())),
        ("gdn_v_grouped", Value::Bool(true)),
    ];
    let known = |k: &&str| {
        expected.iter().any(|(n, _)| n == k)
            || ["weight_names", "inverse_weight_names", "sign_widths", "sign_values"].contains(k)
    };
    if text_architecture(metadata)?.name != "qwen35"
        || !keys.iter().all(known)
        || expected.iter().any(|(k, v)| metadata.get(&format!("{ROTATION_PREFIX}{k}")) != Some(v))
    {
        return Err(Error::model("this GGUF's input rotation is not one Splash runs; choose another variant"));
    }
    Ok(())
}

/// Refuse a target Splash does not load, from its header alone, so an unusable file is never downloaded.
pub fn require_loadable(metadata: &Metadata) -> Result<()> {
    require_rotation(metadata)?;
    require_arrays(metadata)?;
    let loaded = loaded_tensors(metadata)?;
    let name_of = |name: &str| metadata.tensors.get(name).map(|k| type_name(*k));
    let weights: HashSet<Option<String>> = loaded
        .iter()
        .filter(|(n, t)| **t == format_types() && n.as_str() != "token_embd.weight")
        .map(|(n, _)| name_of(n).filter(|t| bit(t) != 0))
        .collect();
    if weights.iter().all(|w| w.as_deref().is_some_and(|t| bit(t) & unquantized() != 0)) {
        return Err(Error::model(
            "this GGUF stores every projection and expert unquantized (BF16, F16 or F32): Splash loads quantized \
             GGUFs, not whole-BF16 or F16 models; choose a quantized variant of the repository",
        ));
    }
    let mut unsupported: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (name, types) in &loaded {
        let found = name_of(name).unwrap_or_else(|| "missing".into());
        let short = if name.starts_with("blk.") { name.splitn(3, '.').last().unwrap_or(name) } else { name };
        if bit(&found) & types == 0 {
            *unsupported.entry((short.into(), found.clone())).or_default() += 1;
        }
        // llama.cpp multiplies a weight by a .scale tensor beside it; the loader folds one into NVFP4's rows only.
        if let Some(stem) = name.strip_suffix(".weight") {
            if metadata.tensors.contains_key(&format!("{stem}.scale")) && found != "NVFP4" {
                let scale = format!("{}.scale", short.trim_end_matches(".weight"));
                *unsupported.entry((scale, format!("beside {found}"))).or_default() += 1;
            }
        }
    }
    if !unsupported.is_empty() {
        let listed: Vec<String> = unsupported
            .iter()
            .map(|((t, f), n)| format!("{t} {f} ({n} {})", if *n == 1 { "tensor" } else { "tensors" }))
            .collect();
        return Err(Error::model(format!(
            "this GGUF stores tensors Splash cannot load: {}; choose another variant",
            listed.join(", ")
        )));
    }
    Ok(())
}

/// One target's header from its split files' headers, in split order: the first file's keys, every file's tensors.
pub fn merged_split(mut headers: Vec<Metadata>) -> Result<Metadata> {
    let count = match headers[0].get("split.count") {
        None => 1,
        Some(Value::Int(i)) => *i,
        Some(_) => -1,
    };
    if count != headers.len() as i128 {
        return Err(Error::model("the GGUF's split files do not match its split.count"));
    }
    let many = headers.len() > 1;
    let mut tensors = HashMap::new();
    for (number, header) in headers.iter().enumerate() {
        if many
            && (header.get("split.no") != Some(&Value::Int(number as i128))
                || header.get("split.count") != Some(&Value::Int(count)))
        {
            return Err(Error::model(format!("GGUF split file {} is out of order", number + 1)));
        }
        for (name, kind) in &header.tensors {
            if tensors.insert(name.clone(), *kind).is_some() {
                return Err(Error::model(format!("duplicate GGUF tensor across split files: {name}")));
            }
        }
    }
    let total = match headers[0].get("split.tensors.count") {
        None => tensors.len() as i128,
        Some(Value::Int(i)) => *i,
        Some(_) => -1,
    };
    if total != tensors.len() as i128 {
        return Err(Error::model("the GGUF's split files hold another number of tensors"));
    }
    let mut first = headers.swap_remove(0);
    first.tensors = tensors;
    Ok(first)
}
