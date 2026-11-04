//! Supported upstream models: a target repository and the draft trained for its family, as an assembly
//! (install/upstream.py).
//!
//! The target is inspected from metadata alone (a GGUF header read by range requests, or an MLX config and shard
//! index) and refused before any weight download when Splash cannot serve it. Its family is the one its
//! configuration describes, never its repository's name. Every start follows the target's revision, then its
//! draft's, with one Hub request each, and publishes a new commit's assembly atomically; the verified installation
//! starts when the Hub cannot answer or the new commit cannot be installed.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use super::assembly::{self, GGUF_VISION};
use super::cache::{self, Hub, Repository};
use super::gguf::{self, Metadata};
use super::{json as pyjson, Error, Event, Inspection, PlannedFile, Prepared, PrepareOptions, Progress, Result, Selection, Source};
use crate::chat::format::{self, Family, FAMILIES};

/// The tokenizer files an MLX target may supply, linked when present.
const TOKENIZER_FILES: [&str; 7] = [
    "tokenizer.json",
    "tokenizer_config.json",
    "chat_template.jinja",
    "vocab.json",
    "merges.txt",
    "added_tokens.json",
    "special_tokens_map.json",
];
/// The name prefixes of a safetensors checkpoint's vision tower tensors: MLX's and transformers'.
const VISION_TOWER: [&str; 2] = ["vision_tower.", "model.visual."];
const PROCESSOR_FILES: [&str; 2] = ["preprocessor_config.json", "processor_config.json"];
/// A DFlash2 release's BF16 embedding of its mask token, beside its weights.
const DRAFT_MASK_EMBEDDING: &str = "mask_embedding.safetensors";

/// What one prepare or inspect shares: the Hub, the engine that checks models, and where status goes.
pub struct Context {
    pub hub: Hub,
    engine: Option<PathBuf>,
    progress: Option<Progress>,
    scratch: OnceLock<Scratch>,
}

/// A private temporary folder for the engine's model check, removed on drop.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Context {
    pub fn new(options: &PrepareOptions) -> Result<Context> {
        Ok(Context {
            hub: Hub::new(options.hf_cache.clone(), options.offline),
            engine: options.engine.clone(),
            progress: options.progress.clone(),
            scratch: OnceLock::new(),
        })
    }

    pub fn emit(&self, event: &Event) {
        if let Some(progress) = &self.progress {
            progress(event);
        }
    }

    fn status(&self, message: impl Into<String>) {
        self.emit(&Event::Status(message.into()));
    }

    fn scratch(&self) -> Result<&Path> {
        if self.scratch.get().is_none() {
            let dir = std::env::temp_dir().join(format!("splash-target-{:016x}", rand::random::<u64>()));
            // Owner-only: the engine reads configs from here, and nobody else should write them.
            std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(&dir)?;
            let _ = self.scratch.set(Scratch(dir));
        }
        Ok(&self.scratch.get().expect("set").0)
    }
}

/// The raw message of an error, without the Vision suffix, for wrapping it in another.
fn message(error: &Error) -> String {
    match error {
        Error::Model(m) | Error::Vision(m) | Error::Hub(m) => m.clone(),
        Error::Io(e) => e.to_string(),
    }
}

/// What the target repository supplies, known before any weight download.
#[derive(Clone, Debug)]
pub struct Target {
    /// The record's target_format: "mlx-affine" or "gguf".
    pub format: &'static str,
    /// The record's vision_format: "none", "safetensors" or "gguf".
    pub vision_format: &'static str,
    /// The configuration the engine reads: the MLX repository's, or the one derived from the GGUF header.
    pub config: Value,
    /// A GGUF's scalar metadata for the engine's check (gguf::scalar_metadata); None for MLX.
    pub gguf_metadata: Option<Value>,
    pub family: &'static Family,
    /// Assembly path -> repository file, for every path a repository file serves.
    pub files: BTreeMap<String, String>,
}

/// The GGUF files at a repository's root, as the native loader finds a target.
fn root_ggufs(files: &BTreeSet<String>) -> Vec<&String> {
    files.iter().filter(|n| !n.contains('/') && n.ends_with(".gguf")).collect()
}

fn stem(name: &str) -> String {
    Path::new(name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Whether a GGUF is named as a vision projector: mmproj-*.gguf, or MODEL-mmproj-*.gguf.
fn projector_named(name: &str) -> bool {
    stem(name).to_lowercase().contains("mmproj")
}

/// The target GGUF among a repository's root files, as its files (one, or a split GGUF's parts), and whether it
/// was taken by its -VARIANT ending from several (upstream.select_gguf). :VARIANT names the file whose name is the
/// model name all of them share then -VARIANT; failing that, the only file ending in -VARIANT; failing that, a
/// split GGUF in a VARIANT/ folder.
pub fn select_gguf(files: &BTreeSet<String>, variant: Option<&str>) -> Result<(Vec<String>, bool)> {
    let candidates: Vec<&String> = root_ggufs(files).into_iter().filter(|n| !projector_named(n)).collect();
    let choice = match variant {
        None => {
            if candidates.len() == 1 {
                return Ok((vec![candidates[0].clone()], false));
            }
            "select a GGUF with OWNER/REPO:VARIANT".to_string()
        }
        Some(variant) => {
            let parts: Vec<Vec<String>> =
                candidates.iter().map(|n| stem(n).split('-').map(str::to_string).collect()).collect();
            let shared = parts.first().map_or(0, |first| {
                (0..first.len()).take_while(|&i| parts.iter().all(|p| p.get(i) == Some(&first[i]))).count()
            });
            let lower = variant.to_lowercase();
            let exact: Vec<&String> = candidates
                .iter()
                .zip(&parts)
                .filter(|(_, words)| words[shared.min(words.len())..].join("-").to_lowercase() == lower)
                .map(|(n, _)| *n)
                .collect();
            if exact.len() == 1 {
                return Ok((vec![exact[0].clone()], false));
            }
            let suffix = format!("-{lower}");
            let ending: Vec<&&String> = candidates.iter().filter(|n| stem(n).to_lowercase().ends_with(&suffix)).collect();
            if ending.len() == 1 {
                return Ok((vec![(*ending[0]).clone()], candidates.len() > 1));
            }
            if let Some(split) = split_gguf(files, variant)? {
                return Ok((split, false));
            }
            format!("no single GGUF matches :{variant}")
        }
    };
    let listed = if candidates.is_empty() {
        "none".to_string()
    } else {
        candidates.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    };
    Err(Error::model(format!("{choice} (files in the repository root: {listed})")))
}

/// The parts of a split GGUF kept in a folder named after its variant, in split order; None without such a folder.
fn split_gguf(files: &BTreeSet<String>, variant: &str) -> Result<Option<Vec<String>>> {
    let prefix = format!("{}/", variant.to_lowercase());
    let names: Vec<&String> = files
        .iter()
        .filter(|n| {
            n.to_lowercase().starts_with(&prefix)
                && n.matches('/').count() == 1
                && n.ends_with(".gguf")
                && !projector_named(n)
        })
        .collect();
    if names.is_empty() {
        return Ok(None);
    }
    split_order(&names, &format!("the split GGUF in {variant}/")).map(Some)
}

/// NAME-0000K-of-0000N.gguf -> (K, N).
fn split_number(name: &str) -> Option<(u32, u32)> {
    let base = name.strip_suffix(".gguf")?;
    let tail = base.get(base.len().checked_sub(15)?..)?;
    let digits = |s: &str| s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit());
    let (k, n) = (tail.get(1..6)?, tail.get(10..15)?);
    (tail.starts_with('-') && &tail[6..10] == "-of-" && digits(k) && digits(n)).then(|| (k.parse().ok(), n.parse().ok())).and_then(|(k, n)| Some((k?, n?)))
}

/// The files of one split GGUF in split order: every part from 1 to N present once.
pub fn split_order(names: &[&String], split: &str) -> Result<Vec<String>> {
    let mut parts: BTreeMap<u32, (u32, String)> = BTreeMap::new();
    for name in names {
        let (number, count) = split_number(name).ok_or_else(|| {
            Error::model(format!("{name} is not one part of a split GGUF (NAME-0000K-of-0000N.gguf)"))
        })?;
        if let Some((_, other)) = parts.get(&number) {
            return Err(Error::model(format!("{other} and {name} are both part {number} of {split}")));
        }
        parts.insert(number, (count, name.to_string()));
    }
    let counts: BTreeSet<u32> = parts.values().map(|(c, _)| *c).collect();
    let complete = counts.len() == 1 && parts.keys().copied().eq(1..=*counts.first().expect("one count"));
    if !complete {
        let mut sorted: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        sorted.sort();
        return Err(Error::model(format!("{split} is incomplete: {}", sorted.join(", "))));
    }
    Ok(parts.into_values().map(|(_, n)| n).collect())
}

/// The name and header of the repository's vision projector, chosen by content among its root GGUFs named mmproj:
/// one of BF16, F32 or F16 weights, preferred in that order.
fn select_vision(context: &Context, repo: &Repository) -> Result<(String, Metadata)> {
    let mut usable: BTreeMap<&str, Vec<(String, Metadata)>> = BTreeMap::new();
    let mut found = Vec::new();
    for name in root_ggufs(&repo.files).into_iter().filter(|n| projector_named(n)) {
        let header = Metadata::read(repo.open(&context.hub, name)?, true)?;
        let architecture = match header.values.get("general.architecture") {
            Some(gguf::Value::Str(s)) => s.clone(),
            _ => "None".into(),
        };
        let types: BTreeSet<String> = header.tensors.values().map(|k| gguf::type_name(*k)).collect();
        found.push(format!("{name} ({architecture}: {})", types.iter().cloned().collect::<Vec<_>>().join(", ")));
        let floats = ["BF16", "F32", "F16"];
        if architecture == "clip" && !types.is_empty() && types.iter().all(|t| floats.contains(&t.as_str())) {
            let precision = ["F16", "BF16", "F32"].into_iter().find(|p| types.contains(*p)).expect("one");
            usable.entry(precision).or_default().push((name.clone(), header));
        }
    }
    for precision in ["BF16", "F32", "F16"] {
        let Some(mut projectors) = usable.remove(precision) else { continue };
        if projectors.len() == 1 {
            return Ok(projectors.pop().expect("one"));
        }
        let names: Vec<String> = projectors.into_iter().map(|(n, _)| n).collect();
        return Err(Error::vision(format!(
            "several {precision} vision projectors, {}, describe no single tower",
            names.join(", ")
        )));
    }
    let found = if found.is_empty() { "no GGUF named mmproj".to_string() } else { found.join("; ") };
    Err(Error::vision(format!("the GGUF repository has no BF16, F32 or F16 vision projector ({found})")))
}

/// The engine's identifying text sizes of each family, as its model check names them in a refusal
/// (`splash model-check`, Splash 1.4.1; read from the engine's own refusals, not guessed).
struct Signature {
    family: &'static str,
    model_type: &'static str,
    hidden_size: u64,
    num_hidden_layers: u64,
    num_attention_heads: u64,
    num_key_value_heads: u64,
    head_dim: u64,
    vocab_size: u64,
}

const SIGNATURES: [Signature; 5] = [
    Signature {
        family: "Qwen3.8-27B",
        model_type: "qwen3_5_text",
        hidden_size: 5120,
        num_hidden_layers: 64,
        num_attention_heads: 24,
        num_key_value_heads: 4,
        head_dim: 256,
        vocab_size: 248320,
    },
    Signature {
        family: "Qwen3.6-35B-A3B",
        model_type: "qwen3_5_moe_text",
        hidden_size: 2048,
        num_hidden_layers: 40,
        num_attention_heads: 16,
        num_key_value_heads: 2,
        head_dim: 256,
        vocab_size: 248320,
    },
    Signature {
        family: "Qwen3.5-122B-A10B",
        model_type: "qwen3_5_moe_text",
        hidden_size: 3072,
        num_hidden_layers: 48,
        num_attention_heads: 32,
        num_key_value_heads: 2,
        head_dim: 256,
        vocab_size: 248320,
    },
    Signature {
        family: "Qwen3.8-Flash-Next",
        model_type: "qwen4_exp_text",
        hidden_size: 2560,
        num_hidden_layers: 48,
        num_attention_heads: 24,
        num_key_value_heads: 2,
        head_dim: 256,
        vocab_size: 248320,
    },
    Signature {
        family: "Flint",
        model_type: "flint",
        hidden_size: 2048,
        num_hidden_layers: 49,
        num_attention_heads: 32,
        num_key_value_heads: 4,
        head_dim: 128,
        vocab_size: 150144,
    },
];

/// The family a configuration describes, by the text sizes the engine identifies families by: Qwen's text model
/// under text_config, or Flint's flat. This identifies; the engine's `model-check`, when given, also validates.
pub fn identify_family(config: &Value) -> Result<&'static Family> {
    let text = if config["text_config"].is_object() { &config["text_config"] } else { config };
    let found = SIGNATURES.iter().find(|s| {
        text["model_type"] == s.model_type
            && [
                ("hidden_size", s.hidden_size),
                ("num_hidden_layers", s.num_hidden_layers),
                ("num_attention_heads", s.num_attention_heads),
                ("num_key_value_heads", s.num_key_value_heads),
                ("head_dim", s.head_dim),
                ("vocab_size", s.vocab_size),
            ]
            .iter()
            .all(|(k, v)| text[*k].as_u64() == Some(*v))
    });
    let names: Vec<&str> = FAMILIES.iter().map(|f| f.name).collect();
    found.and_then(|s| format::family(s.family)).ok_or_else(|| {
        Error::model(format!("no supported model has this architecture; supported: {}", names.join(", ")))
    })
}

/// The family the engine finds a model's configuration to describe (upstream.check_model), and its refusal as an
/// error. With an engine binary its `model-check` decides, as Splash's installer; without one, identify_family.
fn check_model(
    context: &Context,
    target_format: &str,
    vision_format: &str,
    config: &Value,
    gguf_metadata: Option<&Value>,
    draft: Option<&[u8]>,
) -> Result<&'static Family> {
    let Some(engine) = &context.engine else {
        if let Some(draft) = draft {
            let parsed: Option<Value> = serde_json::from_slice(draft).ok();
            if !parsed.is_some_and(|d| d.is_object()) {
                return Err(Error::model("the draft's config.json is not a JSON object"));
            }
        }
        return identify_family(config);
    };
    let scratch = context.scratch()?;
    let write = |name: &str, data: &[u8]| -> Result<OsString> {
        let path = scratch.join(name);
        fs::write(&path, data)?;
        Ok(path.into_os_string())
    };
    let mut arguments: Vec<OsString> = vec!["model-check".into(), target_format.into(), vision_format.into()];
    arguments.push(write("config.json", &pyjson::canonical(config))?);
    if let Some(metadata) = gguf_metadata {
        arguments.push(write("gguf-metadata.json", &pyjson::canonical(metadata))?);
    }
    if let Some(draft) = draft {
        arguments.push(write("draft-config.json", draft)?);
    }
    let output = run_engine(engine, &arguments, "model check")?;
    let name = serde_json::from_str::<Value>(&output)
        .ok()
        .and_then(|v| v["family"].as_str().map(str::to_string))
        .ok_or_else(|| Error::model(format!("the engine's model check gave no family: {}", output.trim())))?;
    format::family(&name).ok_or_else(|| Error::model(format!("no DFlash2 draft is known for the {name} family")))
}

/// The output of one of the engine's checks; its refusal, the last `error:` line, as an error (models.run_engine).
fn run_engine(engine: &Path, arguments: &[OsString], check: &str) -> Result<String> {
    let output = Command::new(engine).args(arguments).output().map_err(|e| {
        Error::model(format!("cannot run the engine {} ({e}); reinstall Splash", engine.display()))
    })?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Some(64) => Err(Error::model(format!(
            "the engine {} does not know this {check}; reinstall Splash",
            engine.display()
        ))),
        code => {
            let refusal = stderr.lines().filter_map(|l| l.strip_prefix("error: ")).next_back();
            Err(Error::model(match (code, refusal) {
                (Some(_), Some(refusal)) => refusal.to_string(),
                _ if stderr.is_empty() => format!("the engine's {check} failed: status {code:?}"),
                _ => format!("the engine's {check} failed: {stderr}"),
            }))
        }
    }
}

/// What the target repository supplies, from metadata alone (upstream.inspect_target). A :VARIANT names a GGUF,
/// and so does a repository without a safetensors checkpoint.
pub fn inspect_target(context: &Context, repo: &Repository, variant: Option<&str>, language_only: bool) -> Result<Target> {
    if variant.is_some() || !repo.files.iter().any(|n| n.ends_with(".safetensors")) {
        let (names, by_ending) = select_gguf(&repo.files, variant)?;
        let name =
            if names.len() == 1 { names[0].clone() } else { format!("{} split files {} ...", names.len(), names[0]) };
        if by_ending {
            context.status(format!(
                "No GGUF is named for :{} alone; using {name}, the only one whose name ends in -{}.",
                variant.unwrap_or_default(),
                variant.unwrap_or_default()
            ));
        }
        let target = gguf_target(context, repo, &names, language_only)?;
        context.status(format!("Selected {name} from {}.", repo.name));
        return Ok(target);
    }
    mlx_target(context, repo, language_only)
}

/// The GGUF target whose files are `names`, checked from its headers, with its projector unless language_only or
/// its family has no vision tower (upstream.gguf_target).
fn gguf_target(context: &Context, repo: &Repository, names: &[String], language_only: bool) -> Result<Target> {
    let headers =
        names.iter().map(|part| Metadata::read(repo.open(&context.hub, part)?, true)).collect::<Result<Vec<_>>>()?;
    let header = gguf::merged_split(headers)?;
    // A split GGUF's parts lie side by side in target/, where the loader finds them by their -0000K-of-0000N names.
    let mut files: BTreeMap<String, String> = names
        .iter()
        .map(|part| (format!("target/{}", part.rsplit('/').next().expect("a name")), part.clone()))
        .collect();
    let mut config = gguf::model_config(&header, None)?;
    let metadata = gguf::scalar_metadata(&header);
    let family = check_model(context, "gguf", "none", &config, Some(&metadata), None)?;
    gguf::require_loadable(&header)?;
    let mut vision_format = "none";
    if !language_only && family.vision {
        let (name, vision) = select_vision(context, repo)?;
        let checked = (|| {
            validate_processor(&gguf::processor_config(&vision)?)?;
            let with_vision = gguf::model_config(&header, Some(&vision))?;
            check_model(context, "gguf", "gguf", &with_vision, Some(&metadata), None)?;
            Ok(with_vision)
        })();
        config = checked.map_err(|e| Error::vision(format!("vision projector {name}: {}", message(&e))))?;
        files.insert(GGUF_VISION.into(), name);
        vision_format = "gguf";
    }
    Ok(Target { format: "gguf", vision_format, config, gguf_metadata: Some(metadata), family, files })
}

fn read_json(context: &Context, repo: &Repository, name: &str) -> Result<Value> {
    let data = repo.read(&context.hub, name)?;
    let value: Value = serde_json::from_slice(&data)
        .map_err(|e| Error::model(format!("could not read {name} in {}: {e}", repo.name)))?;
    if !value.is_object() {
        return Err(Error::model(format!("expected a JSON object in {name} of {}", repo.name)));
    }
    Ok(value)
}

/// The MLX target in repo, which serves images unless language_only (upstream.mlx_target).
fn mlx_target(context: &Context, repo: &Repository, language_only: bool) -> Result<Target> {
    let mut missing: Vec<String> = ["config.json", "tokenizer.json", "tokenizer_config.json"]
        .into_iter()
        .filter(|n| !repo.files.contains(*n))
        .map(str::to_string)
        .collect();
    let processor = language_only || PROCESSOR_FILES.iter().any(|n| repo.files.contains(*n));
    if !missing.is_empty() || !processor {
        let text_ok = missing.is_empty();
        if !processor {
            missing.push(PROCESSOR_FILES.join(" or "));
        }
        let message = format!(
            "target repository {} is missing: {}. Configuration, tokenizer and processor must come from the \
             target repository",
            repo.name,
            missing.join(", ")
        );
        return Err(if text_ok { Error::vision(message) } else { Error::model(message) });
    }
    let config = read_json(context, repo, "config.json")?;
    let family = check_model(context, "mlx-affine", "none", &config, None, None)?;
    let mut files: BTreeMap<String, String> = ["config.json", "target/config.json", "tokenizer/config.json"]
        .into_iter()
        .map(|p| (p.to_string(), "config.json".to_string()))
        .collect();
    for name in TOKENIZER_FILES.into_iter().filter(|n| repo.files.contains(*n)) {
        files.insert(format!("tokenizer/{name}"), name.into());
    }
    for name in weight_files(context, repo, "", Some(&VISION_TOWER))? {
        files.insert(format!("target/{name}"), name);
    }
    let mut vision_format = "none";
    if !language_only {
        // The text passed, so a refusal of the tower's processor, shards or configuration refuses the vision alone.
        let shards = image_processor(context, repo)
            .and_then(|p| validate_processor(&p))
            .and_then(|()| weight_files(context, repo, &VISION_TOWER, None))
            .map_err(|e| Error::vision(message(&e)))?;
        if shards.is_empty() {
            return Err(Error::vision(format!("{} has no vision tower", repo.name)));
        }
        check_model(context, "mlx-affine", "safetensors", &config, None, None).map_err(|e| Error::vision(message(&e)))?;
        vision_format = "safetensors";
        files.insert("vision/config.json".into(), "config.json".into());
        for name in shards {
            files.insert(format!("vision/{name}"), name);
        }
    }
    Ok(Target { format: "mlx-affine", vision_format, config, gguf_metadata: None, family, files })
}

fn image_processor(context: &Context, repo: &Repository) -> Result<Value> {
    if repo.files.contains("preprocessor_config.json") {
        return read_json(context, repo, "preprocessor_config.json");
    }
    let config = read_json(context, repo, "processor_config.json")?["image_processor"].clone();
    if !config.is_object() {
        return Err(Error::vision(format!("{}'s processor_config.json has no image_processor", repo.name)));
    }
    Ok(config)
}

/// Tensor-name prefixes a shard is chosen by: "" for any.
trait Prefixes {
    fn matches(&self, name: &str) -> bool;
}
impl Prefixes for &str {
    fn matches(&self, name: &str) -> bool {
        name.starts_with(*self)
    }
}
impl Prefixes for &[&str; 2] {
    fn matches(&self, name: &str) -> bool {
        self.iter().any(|p| name.starts_with(p))
    }
}

/// The checkpoint's shards holding a tensor that starts with prefix and not with exclude (upstream._weight_files).
fn weight_files(
    context: &Context,
    repo: &Repository,
    prefix: impl Prefixes,
    exclude: Option<&[&str; 2]>,
) -> Result<BTreeSet<String>> {
    let names: BTreeSet<String> = if repo.files.contains("model.safetensors.index.json") {
        let index = read_json(context, repo, "model.safetensors.index.json")?;
        let weights = index["weight_map"].as_object().filter(|w| !w.is_empty());
        let weights = weights.ok_or_else(|| Error::model("invalid safetensors shard index"))?;
        if !weights.values().all(Value::is_string) {
            return Err(Error::model("invalid safetensors shard filename"));
        }
        weights
            .iter()
            .filter(|(tensor, _)| prefix.matches(tensor) && !exclude.is_some_and(|e| e.iter().any(|p| tensor.starts_with(p))))
            .map(|(_, file)| file.as_str().expect("checked").to_string())
            .collect()
    } else if repo.files.contains("model.safetensors") {
        // One file: its header is read only to look for prefix.
        let holds = prefix.matches("") || safetensors_tensors(context, repo, "model.safetensors")?.iter().any(|t| prefix.matches(t));
        if holds { BTreeSet::from(["model.safetensors".to_string()]) } else { BTreeSet::new() }
    } else {
        return Err(Error::model("model has no safetensors checkpoint"));
    };
    let unusable: Vec<&String> = names
        .iter()
        .filter(|n| !repo.files.contains(*n) || n.contains('/') || !n.ends_with(".safetensors"))
        .collect();
    if !unusable.is_empty() {
        let listed: Vec<&str> = unusable.iter().map(|s| s.as_str()).collect();
        return Err(Error::model(format!("checkpoint has missing or unsupported shards: {}", listed.join(", "))));
    }
    Ok(names)
}

/// The tensor names in a safetensors header (8-byte little-endian length, then JSON), read without a download.
fn safetensors_tensors(context: &Context, repo: &Repository, name: &str) -> Result<BTreeSet<String>> {
    let invalid = || Error::model(format!("invalid safetensors header in {name}"));
    let mut stream = repo.open(&context.hub, name)?;
    let mut size = [0u8; 8];
    let failed = |e: std::io::Error| if e.kind() == std::io::ErrorKind::UnexpectedEof { invalid() } else { Error::from_io(e) };
    stream.read_exact(&mut size).map_err(failed)?;
    let size = u64::from_le_bytes(size);
    if !(2..=100_000_000).contains(&size) {
        return Err(invalid());
    }
    let mut header = vec![0; size as usize];
    stream.read_exact(&mut header).map_err(failed)?;
    let header: Value = serde_json::from_slice(&header).map_err(|_| invalid())?;
    let map = header.as_object().ok_or_else(invalid)?;
    Ok(map.keys().filter(|k| *k != "__metadata__").cloned().collect())
}

/// Numbers compare as Python compares them (16 == 16.0).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b)),
        _ => a == b,
    }
}

/// Splash prepares images one way: 16-pixel patches in two temporal slices, merged 2x2, normalized to [-1, 1].
fn validate_processor(config: &Value) -> Result<()> {
    let expected = json!({"patch_size": 16, "temporal_patch_size": 2, "merge_size": 2,
        "image_mean": [0.5, 0.5, 0.5], "image_std": [0.5, 0.5, 0.5]});
    if expected.as_object().expect("an object").iter().any(|(k, v)| !same(&config[k], v)) {
        return Err(Error::model("unsupported vision preprocessing configuration"));
    }
    Ok(())
}

/// The DFlash2 draft's files in repo for the target's family, checked with the target's configuration before any
/// weight is fetched (upstream.draft_files, without the download).
fn draft_names(context: &Context, repo: &Repository, target: &Target) -> Result<BTreeSet<String>> {
    let family = target.family.name;
    let weights = (|| {
        if !repo.files.contains("config.json") {
            return Err(Error::model("no config.json"));
        }
        weight_files(context, repo, "", None)
    })()
    .map_err(|e| {
        Error::model(format!("{} does not contain a DFlash2 checkpoint for {family} ({})", repo.name, message(&e)))
    })?;
    let config = repo.read(&context.hub, "config.json")?;
    check_model(
        context,
        target.format,
        target.vision_format,
        &target.config,
        target.gguf_metadata.as_ref(),
        Some(&config),
    )
    .map_err(|e| {
        Error::model(format!("{}'s draft configuration is incompatible with {family}: {}", repo.name, message(&e)))
    })?;
    let mut names = weights;
    names.insert("config.json".into());
    if repo.files.contains(DRAFT_MASK_EMBEDDING) {
        names.insert(DRAFT_MASK_EMBEDDING.into());
    }
    Ok(names)
}

/// Whether the Hub answered for repo this start: listed, not from the cache in its stead.
fn answered(repo: &Repository) -> bool {
    !repo.files.is_empty() && repo.paths.is_none() && repo.unreachable.is_none()
}

fn at(repo: &Repository) -> String {
    match &repo.revision {
        Some(revision) => format!("{}@{}", repo.name, &revision[..12.min(revision.len())]),
        None => repo.name.clone(),
    }
}

fn draft_source(installed: Option<&Value>) -> Option<&Value> {
    installed.map(|i| &i["sources"]["draft"])
}

/// The draft's repository at the commit its default branch names now, asked only when the Hub answered for the
/// target; else the installed draft stands in (upstream._resolve_draft).
fn resolve_draft(
    context: &Context,
    family: &Family,
    selection: &Selection,
    installed: Option<&Value>,
    target: &Repository,
) -> Result<Repository> {
    let name = selection.draft_model.as_deref().or(family.draft_repo).ok_or_else(|| {
        Error::model(format!("no DFlash2 draft is released for {}; name one with a draft choice", family.name))
    })?;
    let recorded = draft_source(installed);
    let recorded_repo = recorded.and_then(|r| r["repo"].as_str());
    let recorded_revision = recorded.and_then(|r| r["revision"].as_str());
    let asked = answered(target);
    if let (Some(repo), false) = (recorded_repo, asked) {
        return Ok(Repository::unlisted(repo, recorded_revision, None));
    }
    let commit = recorded_revision.filter(|_| recorded_repo == Some(name));
    let unreachable = (!asked)
        .then(|| target.unreachable.clone().unwrap_or_else(|| "the Hub did not list the target".into()));
    match Repository::resolve(&context.hub, name, None, Some(&selection.link), commit, unreachable) {
        Ok(repo) => Ok(repo),
        Err(error) => match recorded_repo {
            Some(repo) => Ok(Repository::unlisted(repo, recorded_revision, Some(error.to_string()))),
            None => Err(error),
        },
    }
}

/// The draft to install and its files: `draft` as this start resolved it when it is not the installed one, else
/// the installed one, which also stands in when `draft` cannot be used (upstream._draft).
fn choose_draft(
    context: &Context,
    target: &Target,
    installed: Option<&Value>,
    draft: Repository,
) -> Result<(Repository, BTreeSet<String>)> {
    let recorded = draft_source(installed);
    if Some(&draft.identity()) != recorded {
        match draft_names(context, &draft, target) {
            Ok(names) => return Ok((draft, names)),
            Err(error) if recorded.is_none() => {
                return Err(Error::model(format!("cannot fetch the {} draft: {}", target.family.name, message(&error))))
            }
            Err(error) => context.status(format!(
                "Warning: cannot use the {} draft {}; keeping the installed one: {error}",
                target.family.name,
                at(&draft)
            )),
        }
    }
    let recorded = recorded.expect("an installed draft");
    let repo = Repository::recorded(&context.hub, recorded["repo"].as_str().unwrap_or_default(), recorded["revision"].as_str())?;
    let names = draft_names(context, &repo, target)?;
    Ok((repo, names))
}

/// Everything an installation of a commit uses, known before any weight download.
struct Plan {
    repo: Repository,
    target: Target,
    draft: Repository,
    draft_names: BTreeSet<String>,
}

fn plan(
    context: &Context,
    selection: &Selection,
    repo: Repository,
    installed: Option<&Value>,
    draft: Option<Repository>,
) -> Result<Plan> {
    let target = inspect_target(context, &repo, selection.variant.as_deref(), selection.language_only)?;
    let draft = match draft {
        Some(draft) => draft,
        None => resolve_draft(context, target.family, selection, installed, &repo)?,
    };
    let (draft, draft_names) = choose_draft(context, &target, installed, draft)?;
    Ok(Plan { repo, target, draft, draft_names })
}

/// Assemble repo's target and its draft, and publish the assembly at the selection link (upstream._install).
fn install(
    context: &Context,
    selection: &Selection,
    repo: Repository,
    installed: Option<&Value>,
    draft: Option<Repository>,
) -> Result<()> {
    // Every Hub request happens here; a Hub, network or file failure names what it stopped.
    install_plan(context, selection, repo, installed, draft).map_err(|e| e.context(&format!("cannot install {}", selection.model)))
}

fn install_plan(
    context: &Context,
    selection: &Selection,
    repo: Repository,
    installed: Option<&Value>,
    draft: Option<Repository>,
) -> Result<()> {
    let plan = plan(context, selection, repo, installed, draft)?;
    let vision = if plan.target.vision_format == "none" { "disabled" } else { "enabled" };
    context.status(format!(
        "Installing {} as {} ({}); draft {}; vision {vision}.",
        selection.model, plan.target.family.name, plan.target.format, plan.draft.name
    ));
    let emit = |event: &Event| context.emit(event);
    let (draft, drafted) = download_draft(context, &plan, installed)?;
    let names: BTreeSet<String> = plan.target.files.values().cloned().collect();
    if names.iter().any(|n| !plan.repo.present(&context.hub, n)) {
        eprintln!("{}", licence_line(&plan.repo.name));
    }
    let downloaded = plan.repo.download(&context.hub, &names, &emit)?;
    let mut files: BTreeMap<String, PathBuf> = drafted.into_iter().map(|(n, p)| (format!("draft/{n}"), p)).collect();
    for (path, name) in &plan.target.files {
        files.insert(path.clone(), downloaded[name].clone());
    }
    let mut record = Map::new();
    record.insert("version".into(), json!(1));
    record.insert("model".into(), json!(selection.model));
    record.insert("family".into(), json!(plan.target.family.name));
    record.insert("target_format".into(), json!(plan.target.format));
    record.insert("vision_format".into(), json!(plan.target.vision_format));
    record.insert("sources".into(), json!({"target": plan.repo.identity(), "draft": draft.identity()}));
    publish(context, &selection.models_root, &selection.link, record, files)?;
    Ok(())
}

/// The plan's draft downloaded; when that fails for a draft other than the installed one, the installed draft
/// stands in, with a warning (upstream._draft).
fn download_draft(
    context: &Context,
    plan: &Plan,
    installed: Option<&Value>,
) -> Result<(Repository, BTreeMap<String, PathBuf>)> {
    let emit = |event: &Event| context.emit(event);
    let error = match plan.draft.download(&context.hub, &plan.draft_names, &emit) {
        Ok(paths) => return Ok((plan.draft.clone(), paths)),
        Err(error) => error,
    };
    let recorded = draft_source(installed);
    let Some(recorded) = recorded.filter(|r| **r != plan.draft.identity()) else {
        return Err(error.context(&format!("cannot fetch the {} draft", plan.target.family.name)));
    };
    context.status(format!(
        "Warning: cannot use the {} draft {}; keeping the installed one: {error}",
        plan.target.family.name,
        at(&plan.draft)
    ));
    let repo = Repository::recorded(&context.hub, recorded["repo"].as_str().unwrap_or_default(), recorded["revision"].as_str())?;
    let names = draft_names(context, &repo, &plan.target)?;
    let paths = repo.download(&context.hub, &names, &emit)?;
    Ok((repo, paths))
}

/// The licence line printed when a target's weights start downloading.
fn licence_line(repo: &str) -> String {
    if repo.starts_with("unsloth/Qwen3.8-") {
        "Licence · Qwen3.8 · Apache-2.0 · Copyright 2026 Alibaba Cloud".into()
    } else {
        format!("Licence · {repo} · see the model card")
    }
}

/// The record of each of paths, keeping the digest `previous` holds of one unchanged since.
fn file_records<'a>(
    previous: Option<&Value>,
    paths: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<BTreeMap<PathBuf, Value>> {
    let mut recorded = BTreeMap::new();
    for entry in previous.and_then(|p| p["files"].as_object()).into_iter().flat_map(|f| f.values()) {
        if let Some(path) = entry["path"].as_str() {
            recorded.insert(path.to_string(), entry);
        }
    }
    let mut out = BTreeMap::new();
    for path in paths {
        let key = std::path::absolute(path)?.to_string_lossy().into_owned();
        out.insert(path.clone(), assembly::file_record(path, recorded.get(&key).copied())?);
    }
    Ok(out)
}

/// Publish the assembly of record and files at link and return it (upstream.publish). Digests are read before the
/// installation lock; every write under the models root happens under it.
pub fn publish(
    context: &Context,
    models_root: &Path,
    link: &Path,
    mut record: Map<String, Value>,
    mut files: BTreeMap<String, PathBuf>,
) -> Result<PathBuf> {
    let emit = |event: &Event| context.emit(event);
    fs::create_dir_all(models_root)?;
    let sources: BTreeSet<PathBuf> = files.values().cloned().collect();
    let records = file_records(assembly::recorded(link).as_ref(), &sources)?;
    let _lock = assembly::installation_lock(models_root, &emit)?;
    let previous = assembly::recorded(link);
    // A file changed since its record was taken is read again.
    let mut records: BTreeMap<PathBuf, Value> = records
        .into_iter()
        .map(|(path, entry)| assembly::file_record(&path, Some(&entry)).map(|r| (path, r)))
        .collect::<Result<_>>()?;
    if record["target_format"] == "gguf" {
        let (key, derived) = assembly::derived_metadata(models_root, &files, &records, &emit)?;
        record.insert("metadata".into(), json!(key));
        records.extend(file_records(previous.as_ref(), derived.values())?);
        files.extend(derived);
    }
    let file_map: Map<String, Value> = files.iter().map(|(name, path)| (name.clone(), records[path].clone())).collect();
    record.insert("files".into(), Value::Object(file_map));
    let record = Value::Object(record);
    let pins = assembly::pins(&record)
        .iter()
        .map(|(snapshot, repo)| cache::pin(snapshot, repo, link))
        .collect::<Result<Vec<_>>>()?;
    let replaced: Vec<PathBuf> = previous
        .as_ref()
        .map(|p| assembly::pins(p).iter().map(|(s, _)| cache::pinned(s, link)).collect())
        .unwrap_or_default();
    let published = assembly::build(models_root, &record, &files, &emit)?;
    assembly::link_selection(link, &published)?;
    cache::retire_other_pins(&pins, &replaced, &emit);
    assembly::collect_garbage(models_root)?;
    Ok(published)
}

/// What a start changes in a verified assembly of the installed commit: the draft, or the GGUF metadata.
fn changes(installed: &Value, family: Option<&Family>, draft: Option<&Repository>) -> Vec<String> {
    let (Some(_), Some(draft)) = (family, draft) else {
        return vec![format!("no supported family is named {}", installed["family"])];
    };
    let mut changes = Vec::new();
    let recorded = &installed["sources"]["draft"];
    if &draft.identity() != recorded {
        let short = |v: &Value| v.as_str().map(|s| s[..12.min(s.len())].to_string()).unwrap_or_default();
        changes.push(if recorded["repo"] == json!(draft.name) {
            format!("{} moved from {} to {}", draft.name, short(&recorded["revision"]), short(&json!(draft.revision)))
        } else {
            format!("its draft is now {}", at(draft))
        });
    }
    if installed["target_format"] == "gguf"
        && installed["metadata"].as_str() != Some(&assembly::metadata_key(installed["files"].as_object().expect("well formed")))
    {
        changes.push("the GGUF metadata adapter changed".into());
    }
    changes
}

/// Verify the installation again under the lock and repair its pins; the reason when it no longer verifies.
fn retain_installed(context: &Context, selection: &Selection) -> Option<Error> {
    let emit = |event: &Event| context.emit(event);
    let _lock = match assembly::installation_lock(&selection.models_root, &emit) {
        Ok(lock) => lock,
        Err(error) => return Some(error),
    };
    let installed = match assembly::verify(&selection.link, false) {
        Ok(record) => record,
        Err(error) => return Some(error),
    };
    cache::repair_pins(&selection.link, &assembly::pins(&installed), &emit).err()
}

/// Run an installation; if it fails, keep the verified installation instead, with a warning.
fn keeping_installation(
    context: &Context,
    selection: &Selection,
    installed: &Value,
    attempt: &str,
    work: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let Err(error) = work() else { return Ok(()) };
    let commit = installed["sources"]["target"]["revision"].as_str().unwrap_or_default();
    context.status(format!(
        "Warning: keeping the installed {}@{}; cannot {attempt}: {error}",
        selection.repo_id,
        &commit[..12.min(commit.len())]
    ));
    match retain_installed(context, selection) {
        None => Ok(()),
        Some(_) => Err(error),
    }
}

fn installed(context: &Context, selection: &Selection) -> Option<Value> {
    if !selection.link.join("model.json").exists() {
        return None;
    }
    match assembly::verify(&selection.link, false) {
        Ok(record) => Some(record),
        Err(error) => {
            context.status(format!("Reinstalling {}: {error}", selection.model));
            None
        }
    }
}

fn installed_commit(installed: Option<&Value>) -> Option<String> {
    installed.and_then(|i| i["sources"]["target"]["revision"].as_str()).map(str::to_string)
}

/// Start the selection's model, installing what it needs (upstream.prepare).
pub fn prepare(context: &Context, selection: &Selection) -> Result<Prepared> {
    let installed = installed(context, selection);
    let installed_commit = installed_commit(installed.as_ref());
    let target = Repository::resolve(
        &context.hub,
        &selection.repo_id,
        selection.revision.as_deref(),
        Some(&selection.link),
        installed_commit.as_deref(),
        None,
    )?;
    match &installed {
        Some(record) if target.revision == installed_commit => start_installed(context, selection, target, record)?,
        _ => install_commit(context, selection, target, installed.as_ref())?,
    }
    let record = assembly::verify(&selection.link, false)?;
    let name = record["family"].as_str().unwrap_or_default();
    let family =
        format::family(name).ok_or_else(|| Error::model(format!("no supported family is named {name}")))?;
    Ok(Prepared {
        root: selection.link.clone(),
        assembly: selection.link.canonicalize()?,
        family,
        model: selection.model.clone(),
    })
}

/// Start the verified installation of the target's commit, assembled again first when its draft moved or this
/// port derives its metadata otherwise (upstream._start_installed).
fn start_installed(context: &Context, selection: &Selection, target: Repository, installed: &Value) -> Result<()> {
    let revision = target.revision.clone().unwrap_or_default();
    if let Some(why) = &target.unreachable {
        context.status(format!(
            "Could not reach the Hub ({why}); using the installed {}@{}.",
            selection.repo_id,
            &revision[..12.min(revision.len())]
        ));
    }
    let recorded = &installed["sources"]["target"];
    let family = installed["family"].as_str().and_then(format::family);
    let draft = family.map(|f| resolve_draft(context, f, selection, Some(installed), &target)).transpose()?;
    if let Some(why) = draft.as_ref().and_then(|d| d.unreachable.as_ref()) {
        context.status(format!(
            "Could not reach the Hub ({why}); using the installed draft {}.",
            at(draft.as_ref().expect("a draft"))
        ));
    }
    let recorded_repo = || {
        Repository::recorded(&context.hub, recorded["repo"].as_str().unwrap_or_default(), recorded["revision"].as_str())
    };
    let changes = changes(installed, family, draft.as_ref());
    if !changes.is_empty() {
        let changes = changes.join("; ");
        context.status(format!("Updating {}: {changes}.", selection.model));
        let attempt = format!("update it ({changes})");
        return keeping_installation(context, selection, installed, &attempt, || {
            install(context, selection, recorded_repo()?, Some(installed), draft)
        });
    }
    match retain_installed(context, selection) {
        None => {
            context.status(format!(
                "Splash model {} is already installed in {}",
                selection.model,
                selection.link.display()
            ));
            Ok(())
        }
        Some(replaced) => {
            // A concurrent installation replaced or damaged the assembly after it was verified.
            context.status(format!("Reinstalling {}: {replaced}", selection.model));
            install(context, selection, recorded_repo()?, Some(installed), draft)
        }
    }
}

/// Install a commit of the target this selection has not installed; a verified installation of another commit is
/// kept if that fails (upstream._install_commit).
fn install_commit(context: &Context, selection: &Selection, target: Repository, installed: Option<&Value>) -> Result<()> {
    let revision = target.revision.clone().unwrap_or_default();
    let short = &revision[..12.min(revision.len())];
    if let Some(why) = &target.unreachable {
        context.status(format!(
            "Could not reach the Hub ({why}); installing {}@{short} from the Hub cache.",
            selection.repo_id
        ));
    } else if let Some(commit) = installed_commit(installed) {
        context.status(format!("{} moved from {} to {short}.", selection.repo_id, &commit[..12.min(commit.len())]));
    }
    match installed {
        None => install(context, selection, target, None, None),
        Some(record) => {
            let attempt = format!("install {}@{revision}", selection.repo_id);
            keeping_installation(context, selection, record, &attempt, || {
                install(context, selection, target, Some(record), None)
            })
        }
    }
}

/// A listed repository, or the cached snapshot of an unlisted commit, so its files can be planned.
fn listed(context: &Context, repo: Repository) -> Result<Repository> {
    if repo.paths.is_some() || !repo.files.is_empty() {
        return Ok(repo);
    }
    let unreachable = repo.unreachable.clone();
    let mut cached = Repository::recorded(&context.hub, &repo.name, repo.revision.as_deref())?;
    cached.unreachable = unreachable;
    Ok(cached)
}

/// What prepare would use and fetch, without downloading or writing anything.
pub fn inspect(context: &Context, selection: &Selection) -> Result<Inspection> {
    let installed = selection.link.join("model.json").exists().then(|| assembly::verify(&selection.link, false).ok()).flatten();
    let installed_commit = installed_commit(installed.as_ref());
    let target = Repository::resolve(
        &context.hub,
        &selection.repo_id,
        selection.revision.as_deref(),
        Some(&selection.link),
        installed_commit.as_deref(),
        None,
    )?;
    let unreachable = target.unreachable.clone().or_else(|| context.hub.offline.then(|| "HF_HUB_OFFLINE is set".into()));
    let resolved_target = target.clone();
    let repo = listed(context, target)?;
    let mut plan = plan(context, selection, repo, installed.as_ref(), None)?;
    if plan.draft.paths.is_none() && plan.draft.files.is_empty() {
        plan.draft = listed(context, plan.draft)?;
    }
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut add = |repo: &Repository, file: &str, path: String| {
        if let Some(planned) = files.iter_mut().find(|f| f.repo == repo.name && f.file == file) {
            planned.assembly_paths.push(path);
            return;
        }
        files.push(PlannedFile {
            repo: repo.name.clone(),
            file: file.into(),
            assembly_paths: vec![path],
            bytes: repo.size(&context.hub, file),
            present: repo.present(&context.hub, file),
        });
    };
    for (path, name) in &plan.target.files {
        add(&plan.repo, name, path.clone());
    }
    for name in &plan.draft_names {
        add(&plan.draft, name, format!("draft/{name}"));
    }
    let sum = |present: bool| files.iter().filter(|f| f.present == present).filter_map(|f| f.bytes).sum::<u64>();
    let (present_bytes, missing_bytes) = (sum(true), sum(false));
    let is_installed = installed.as_ref().is_some_and(|record| {
        installed_commit == resolved_target.revision
            && record["sources"]["draft"] == plan.draft.identity()
            && changes(record, Some(plan.target.family), Some(&plan.draft)).is_empty()
    });
    Ok(Inspection {
        model: selection.model.clone(),
        family: plan.target.family,
        target: Source { repo: plan.repo.name.clone(), revision: plan.repo.revision.clone() },
        draft: Source { repo: plan.draft.name.clone(), revision: plan.draft.revision.clone() },
        target_format: plan.target.format,
        vision_format: plan.target.vision_format,
        files,
        present_bytes,
        missing_bytes,
        installed: is_installed,
        unreachable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gguf_variants_select_as_splash_does() {
        let files = names(&[
            "Qwen3.8-27B-Q4_K_M.gguf",
            "Qwen3.8-27B-UD-Q4_K_M.gguf",
            "mmproj-BF16.gguf",
            "BF16/Qwen3.8-27B-BF16-00002-of-00002.gguf",
            "BF16/Qwen3.8-27B-BF16-00001-of-00002.gguf",
        ]);
        assert_eq!(select_gguf(&files, Some("UD-Q4_K_M")).unwrap(), (vec!["Qwen3.8-27B-UD-Q4_K_M.gguf".into()], false));
        // X-Q4_K_M for Q4_K_M, not X-UD-Q4_K_M.
        assert_eq!(select_gguf(&files, Some("q4_k_m")).unwrap(), (vec!["Qwen3.8-27B-Q4_K_M.gguf".into()], false));
        assert_eq!(
            select_gguf(&files, Some("BF16")).unwrap().0,
            vec!["BF16/Qwen3.8-27B-BF16-00001-of-00002.gguf", "BF16/Qwen3.8-27B-BF16-00002-of-00002.gguf"]
        );
        assert!(select_gguf(&files, None).unwrap_err().to_string().starts_with("select a GGUF with OWNER/REPO:VARIANT"));
        let one = names(&["Model-UD-Q4_K_M.gguf", "mmproj-F16.gguf"]);
        assert_eq!(select_gguf(&one, None).unwrap().0, vec!["Model-UD-Q4_K_M.gguf"]);
        assert_eq!(select_gguf(&one, Some("Q4_K_M")).unwrap(), (vec!["Model-UD-Q4_K_M.gguf".into()], false));
        let gap = names(&["Q8/M-00001-of-00003.gguf", "Q8/M-00003-of-00003.gguf"]);
        assert!(select_gguf(&gap, Some("Q8")).unwrap_err().to_string().contains("is incomplete"));
    }

    #[test]
    fn families_are_identified_from_configs() {
        let qwen = json!({"model_type": "qwen3_5", "text_config": {"model_type": "qwen3_5_text", "hidden_size": 5120,
            "num_hidden_layers": 64, "num_attention_heads": 24, "num_key_value_heads": 4, "head_dim": 256,
            "vocab_size": 248320}});
        assert_eq!(identify_family(&qwen).unwrap().name, "Qwen3.8-27B");
        let flint = json!({"model_type": "flint", "hidden_size": 2048, "num_hidden_layers": 49,
            "num_attention_heads": 32, "num_key_value_heads": 4, "head_dim": 128, "vocab_size": 150144});
        assert_eq!(identify_family(&flint).unwrap().name, "Flint");
        let mut other = qwen.clone();
        other["text_config"]["hidden_size"] = json!(4096);
        assert!(identify_family(&other).is_err());
    }

    #[test]
    fn processors_compare_numbers_as_python() {
        let ok = json!({"patch_size": 16.0, "temporal_patch_size": 2, "merge_size": 2,
            "image_mean": [0.5, 0.5, 0.5], "image_std": [0.5, 0.5, 0.5]});
        assert!(validate_processor(&ok).is_ok());
        let mut bad = ok.clone();
        bad["image_mean"] = json!([0.48, 0.45, 0.4]);
        assert!(validate_processor(&bad).is_err());
    }
}
