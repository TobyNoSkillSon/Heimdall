//! Model download, verification and preparation from Hugging Face, without Python (install/hub.py, gguf.py,
//! assembly.py, models.py, upstream.py).
//!
//! `prepare` turns a selector, `owner/repo[:VARIANT]`, into the directory the native engine serves: an assembly of
//! links to Hugging Face cache files and to metadata derived from the GGUF, laid out and recorded exactly as Splash
//! 1.4.1 lays it out, under a models root the caller owns. `inspect` answers the same question without downloading,
//! with byte counts, so Heimdall can say what a download costs before it starts.
//!
//! Every Hub request is blocking (reqwest's blocking client): call these from a plain thread or `spawn_blocking`,
//! never directly inside a tokio runtime.
pub mod assembly;
pub mod cache;
pub mod gguf;
pub mod json;
pub mod upstream;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::chat::format::Family;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A refusal or failure Splash reports as a ModelError: printed as is, without a trace.
    #[error("{0}")]
    Model(String),
    /// A refusal of the model's vision alone, which `language_only` avoids.
    #[error("{0}; use language_only to serve text only")]
    Vision(String),
    /// The Hub or the network failed (denied, unreachable, cut off), with the Hub's reason and the token redacted.
    #[error("{0}")]
    Hub(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn model(message: impl Into<String>) -> Error {
        Error::Model(message.into())
    }
    fn vision(message: impl Into<String>) -> Error {
        Error::Vision(message.into())
    }
    /// The error behind a failed read: a Hub failure a range reader carried through `io::Error`, else the I/O error.
    pub(crate) fn from_io(error: std::io::Error) -> Error {
        if error.get_ref().is_some_and(|inner| inner.is::<Error>()) {
            return *error.into_inner().expect("checked").downcast::<Error>().expect("checked");
        }
        Error::Io(error)
    }
    /// Report a Hub, network or file failure as "context: reason" (hub.as_model_errors); refusals pass unchanged.
    pub(crate) fn context(self, context: &str) -> Error {
        match self {
            Error::Hub(reason) => Error::Hub(format!("{context}: {reason}")),
            Error::Io(error) => Error::Model(format!("{context}: {error}")),
            other => other,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Which DFlash2 draft an installation pairs with its target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DraftChoice {
    /// The draft released for the target's family (families.py).
    #[default]
    Family,
    /// Another Hub repository, `owner/repo`.
    Repo(String),
    /// A local DFlash2 directory (config.json and safetensors).
    Local(PathBuf),
}

/// What `prepare` reports while it works: Splash's status lines, and download progress per file.
#[derive(Clone, Debug)]
pub enum Event {
    Status(String),
    Download { repo: String, file: String, done: u64, total: u64 },
}

pub type Progress = Arc<dyn Fn(&Event) + Send + Sync>;

#[derive(Clone)]
pub struct PrepareOptions {
    /// Where assemblies, derived metadata and selection links live: Heimdall's own, never Splash's.
    pub models_root: PathBuf,
    /// Use the Hub cache and the installed assembly only (also set by HF_HUB_OFFLINE).
    pub offline: bool,
    /// Skip the vision projector: serve text only.
    pub language_only: bool,
    pub draft: DraftChoice,
    /// A branch, tag or commit of the target in place of its default branch.
    pub revision: Option<String>,
    /// The Hub cache; None for HF_HUB_CACHE / HF_HOME / ~/.cache/huggingface/hub, as huggingface_hub finds it.
    pub hf_cache: Option<PathBuf>,
    /// The `splash` engine binary: when given, its `model-check` decides the family and refuses what it cannot
    /// serve, as Splash's installer does; otherwise the family comes from `upstream::identify_family`.
    pub engine: Option<PathBuf>,
    pub progress: Option<Progress>,
}

impl PrepareOptions {
    pub fn new(models_root: impl Into<PathBuf>) -> PrepareOptions {
        PrepareOptions {
            models_root: models_root.into(),
            offline: false,
            language_only: false,
            draft: DraftChoice::Family,
            revision: None,
            hf_cache: None,
            engine: None,
            progress: None,
        }
    }
}

/// A prepared model: `root` is its selection link, the stable path to start the engine with.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub root: PathBuf,
    /// The assembly the link names now (`<models root>/.resolved/<sha256 of model.json>`).
    pub assembly: PathBuf,
    pub family: &'static Family,
    pub model: String,
}

/// One repository at the commit a plan uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub repo: String,
    /// None for a local directory.
    pub revision: Option<String>,
}

/// A file a prepared model links from a source repository.
#[derive(Clone, Debug)]
pub struct PlannedFile {
    pub repo: String,
    pub file: String,
    /// Its paths in the assembly (config.json may serve several).
    pub assembly_paths: Vec<String>,
    /// None when neither the Hub listing nor the cache gives its size.
    pub bytes: Option<u64>,
    /// Whether the Hub cache (or the local directory) already holds it.
    pub present: bool,
}

/// What preparing a selector would use and fetch, found without downloading any weights.
#[derive(Clone, Debug)]
pub struct Inspection {
    pub model: String,
    pub family: &'static Family,
    pub target: Source,
    pub draft: Source,
    pub target_format: &'static str,
    pub vision_format: &'static str,
    pub files: Vec<PlannedFile>,
    pub present_bytes: u64,
    pub missing_bytes: u64,
    /// Whether the selection link already serves exactly this plan.
    pub installed: bool,
    /// Why the Hub was not asked or did not answer, when the cache stood in.
    pub unreachable: Option<String>,
}

/// Prepare the model `selector` names, downloading what the Hub cache lacks, and return its selection link.
pub fn prepare(selector: &str, options: &PrepareOptions) -> Result<Prepared> {
    let context = upstream::Context::new(options)?;
    let selection = Selection::of(selector, options)?;
    upstream::prepare(&context, &selection)
}

/// What `prepare` would do for `selector`: the family, the commits, every file with its size and whether the cache
/// holds it. Reads GGUF headers by range requests when they are not cached; writes nothing.
pub fn inspect(selector: &str, options: &PrepareOptions) -> Result<Inspection> {
    let context = upstream::Context::new(options)?;
    let selection = Selection::of(selector, options)?;
    upstream::inspect(&context, &selection).map_err(|e| e.context(&format!("cannot inspect {selector}")))
}

/// What a selector and its source options select, and its selection link (models.Selection).
#[derive(Clone, Debug)]
pub struct Selection {
    pub model: String,
    pub repo_id: String,
    pub variant: Option<String>,
    pub revision: Option<String>,
    pub language_only: bool,
    /// A repository ID, or a local directory's absolute path.
    pub draft_model: Option<String>,
    pub models_root: PathBuf,
    pub link: PathBuf,
}

impl Selection {
    pub fn of(selector: &str, options: &PrepareOptions) -> Result<Selection> {
        let (repo_id, variant) = split_model_id(selector)?;
        let draft_model = match &options.draft {
            DraftChoice::Family => None,
            DraftChoice::Repo(id) => Some(check_repo_id(id)?.to_string()),
            DraftChoice::Local(path) => {
                if !path.is_dir() {
                    return Err(Error::model(format!("local draft directory not found: {}", path.display())));
                }
                Some(path.canonicalize()?.to_string_lossy().into_owned())
            }
        };
        std::fs::create_dir_all(&options.models_root)?;
        let models_root = options.models_root.canonicalize()?;
        let link = selection_link(
            &models_root,
            selector,
            options.revision.as_deref(),
            options.language_only,
            draft_model.as_deref(),
        )?;
        Ok(Selection {
            model: selector.to_string(),
            repo_id,
            variant,
            revision: options.revision.clone(),
            language_only: options.language_only,
            draft_model,
            models_root,
            link,
        })
    }
}

/// `owner/repo[:variant]` -> (repository ID, variant), as serve_options.split_model_id checks it.
pub fn split_model_id(value: &str) -> Result<(String, Option<String>)> {
    let (repo_id, variant) = match value.split_once(':') {
        Some((repo, variant)) => (repo, Some(variant)),
        None => (value, None),
    };
    check_repo_id(repo_id)?;
    if let Some(variant) = variant {
        let ok = (1..=64).contains(&variant.len())
            && variant.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && variant.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            && !variant.contains("..");
        if !ok {
            return Err(Error::model(
                "model variant must be a short name such as UD-Q4_K_M (owner/repo:VARIANT)",
            ));
        }
    }
    Ok((repo_id.to_string(), variant.map(str::to_string)))
}

/// A full Hugging Face repository ID (serve_options.check_repo_id).
pub fn check_repo_id(value: &str) -> Result<&str> {
    let part = |s: &str, max: usize| {
        let edge = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let inner = |c: char| edge(c) || c == '.' || c == '-';
        !s.is_empty()
            && s.chars().next().is_some_and(edge)
            && s.chars().last().is_some_and(edge)
            && s.chars().all(inner)
            && s.len() <= max
    };
    let ok = match value.split_once('/') {
        Some((owner, repo)) => part(owner, usize::MAX) && part(repo, 96),
        None => false,
    };
    if !ok || value.contains("--") || value.contains("..") || value.ends_with(".git") {
        return Err(Error::model("model must be a full Hugging Face repository ID (owner/repo)"));
    }
    Ok(value)
}

/// OWNER/REPO[:VARIANT] under the models root for the model alone, else `.selections/<SHA-256 of the selection>`
/// (models.selection_link), so another option set never replaces this one's installation.
pub fn selection_link(
    models: &Path,
    model: &str,
    revision: Option<&str>,
    language_only: bool,
    draft_model: Option<&str>,
) -> Result<PathBuf> {
    split_model_id(model)?;
    if revision.is_some() || language_only || draft_model.is_some() {
        let encoded = json::compact(&json!([model, revision, language_only, draft_model]));
        return Ok(models.join(".selections").join(cache::sha256_hex(encoded.as_bytes())));
    }
    Ok(models.join(model))
}

/// Whether name is a plain relative POSIX path a Hub listing may safely name (models.is_safe_path).
pub fn is_safe_path(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
        && !name.chars().any(|c| "\\*?[]".contains(c) || (c as u32) < 32)
}

pub(crate) fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_parse_as_serve_options_does() {
        assert_eq!(
            split_model_id("unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M").unwrap(),
            ("unsloth/Qwen3.8-27B-GGUF".into(), Some("UD-Q4_K_M".into()))
        );
        assert_eq!(split_model_id("a/b").unwrap(), ("a/b".into(), None));
        for bad in ["a", "a/b/c", "a--b/c", "a/b.git", "a/b:", "a/b:-x", "a/b:x..y", "/b", "a/.b"] {
            assert!(split_model_id(bad).is_err(), "{bad}");
        }
        assert!(is_safe_path("Q4/model-00001-of-00002.gguf"));
        for bad in ["", "/a", "a/../b", "a//b", "a*", "./a"] {
            assert!(!is_safe_path(bad), "{bad}");
        }
    }

    #[test]
    fn selection_links_match_splash() {
        let root = Path::new("/m");
        assert_eq!(selection_link(root, "o/r:V", None, false, None).unwrap(), Path::new("/m/o/r:V"));
        // sha256 of ["o/r:V",null,true,null], as models._hashed_link encodes it.
        let hashed = selection_link(root, "o/r:V", None, true, None).unwrap();
        assert_eq!(hashed, root.join(".selections").join(cache::sha256_hex(br#"["o/r:V",null,true,null]"#)));
    }
}
