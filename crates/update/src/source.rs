//! Where releases come from: GitHub releases of [`REPO`], or the `HEIMDALL_UPDATE_BASE` override.
//!
//! Default: the release list at `https://api.github.com/repos/<REPO>/releases?per_page=20` (the list, not
//! `releases/latest`, because GitHub's latest skips prereleases and an alpha build must see newer alphas) and assets
//! at `https://github.com/<REPO>/releases/download/<tag>/<asset>`.
//!
//! Override `HEIMDALL_UPDATE_BASE=<base>`: the list at `<base>/releases.json`, assets at `<base>/<tag>/<asset>`.
//! The base must be HTTPS. A `file://` base is accepted only in this crate's tests and in builds with the
//! `local-releases` feature (a VM run against a release packaged on the host); release bundles never have it.

use crate::{Result, UpdateError};

/// The one place the release repository is named.
pub const REPO: &str = "TobyNoSkillSon/Heimdall";
pub const ENV_BASE: &str = "HEIMDALL_UPDATE_BASE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub list_url: String,
    /// `None`: GitHub's download path for the tag.
    pub base: Option<String>,
}

impl Default for Source {
    fn default() -> Self {
        Source { list_url: format!("https://api.github.com/repos/{REPO}/releases?per_page=20"), base: None }
    }
}

impl Source {
    pub fn from_env() -> Result<Source> {
        Source::from_base(std::env::var(ENV_BASE).ok().as_deref())
    }

    pub fn from_base(base: Option<&str>) -> Result<Source> {
        let Some(base) = base.map(str::trim).filter(|b| !b.is_empty()) else { return Ok(Source::default()) };
        let base = base.trim_end_matches('/').to_owned();
        if !scheme_allowed(&base) {
            return Err(UpdateError::new(format!("{ENV_BASE} must use HTTPS")));
        }
        Ok(Source { list_url: format!("{base}/releases.json"), base: Some(base) })
    }

    pub fn asset_url(&self, tag: &str, asset: &str) -> String {
        match &self.base {
            Some(base) => format!("{base}/{tag}/{asset}"),
            None => format!("https://github.com/{REPO}/releases/download/{tag}/{asset}"),
        }
    }
}

/// HTTPS always; `file://` only in tests and `local-releases` builds; plain HTTP to the loopback address only in
/// this crate's tests (the interrupted-download fixture server).
pub fn scheme_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    if url.starts_with("file://") {
        return cfg!(any(test, feature = "local-releases"));
    }
    if url.starts_with("http://127.0.0.1:") {
        return cfg!(test);
    }
    false
}
