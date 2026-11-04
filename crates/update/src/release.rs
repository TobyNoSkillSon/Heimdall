//! Releases from the GitHub releases API, the offer rule, and the pure checks on a release's files.
//!
//! Offer rule (docs/updates.md): a release is offered when it is published (not a draft, has `published_at`), carries
//! the zip, `SHA256SUMS` and `SHA256SUMS.sig`, and is newer than the running build. A release counts as a prerelease
//! when GitHub marks it so or its version has a prerelease part (`-alpha.2`). A prerelease build (an alpha) is offered
//! the newest newer release, prerelease or stable; a stable build is never offered a prerelease. Once an alpha user
//! takes a stable release, they stay on stable releases.

use crate::{Product, Result, UpdateError, Version};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub name: String,
    /// The release body (Markdown), as written on GitHub.
    pub body: String,
    pub draft: bool,
    /// GitHub's prerelease flag.
    pub prerelease: bool,
    pub published: bool,
    pub assets: Vec<String>,
}

/// The checksum file and its detached Ed25519 signature, beside the zip in every release.
pub const SUMS: &str = "SHA256SUMS";
pub const SUMS_SIG: &str = "SHA256SUMS.sig";

impl Release {
    /// GitHub's prerelease flag or a prerelease version.
    pub fn is_prerelease(&self) -> bool {
        self.prerelease || self.version.is_prerelease()
    }

    /// Parses one release object. Unknown fields are ignored; `tag_name` must be a version (`v0.1.0-alpha.2`).
    pub fn from_json(object: &Value) -> Result<Release> {
        let tag = object
            .get("tag_name")
            .and_then(Value::as_str)
            .ok_or_else(|| UpdateError::new("the release answer has no tag_name"))?;
        let version =
            Version::parse(tag).ok_or_else(|| UpdateError::new(format!("release tag {tag} is not a version")))?;
        let text = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let flag = |key: &str| object.get(key).and_then(Value::as_bool).unwrap_or(false);
        let assets = object
            .get("assets")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_owned)).collect())
            .unwrap_or_default();
        Ok(Release {
            tag: tag.to_owned(),
            version,
            name: text("name"),
            body: text("body"),
            draft: flag("draft"),
            prerelease: flag("prerelease"),
            published: object.get("published_at").and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
            assets,
        })
    }

    /// Parses the releases list (`GET /repos/{owner}/{repo}/releases`). Answers over 1 MB are refused; entries that
    /// are not releases with a version tag are skipped.
    pub fn parse_list(data: &[u8]) -> Result<Vec<Release>> {
        if data.len() > 1_048_576 {
            return Err(UpdateError::new("the release answer is too large"));
        }
        let value: Value =
            serde_json::from_slice(data).map_err(|_| UpdateError::new("the release answer is not JSON"))?;
        let items = match &value {
            Value::Array(items) => items.clone(),
            Value::Object(_) => vec![value.clone()],
            _ => return Err(UpdateError::new("the release answer is not a release list")),
        };
        Ok(items.iter().filter_map(|item| Release::from_json(item).ok()).collect())
    }

    /// The zip, SHA256SUMS and its signature are all attached.
    pub fn complete(&self, product: &Product) -> bool {
        let zip = product.zip_name(&self.version);
        [zip.as_str(), SUMS, SUMS_SIG].iter().all(|name| self.assets.iter().any(|a| a == name))
    }

    /// Short plain notes for the confirmation alert: the body's opening text up to its first heading after some
    /// text, Markdown marks dropped, at most `max` characters cut at a word.
    pub fn short_notes(&self, max: usize) -> String {
        let mut lines: Vec<String> = vec![];
        let mut fence = false;
        for raw in self.body.replace("\r\n", "\n").lines() {
            let trimmed = raw.trim();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                fence = !fence;
                continue;
            }
            if fence || trimmed.starts_with('<') {
                continue;
            }
            if raw.starts_with('#') {
                if lines.is_empty() {
                    continue;
                }
                break;
            }
            let mut line = trimmed.trim_start_matches('>').trim().replace("**", "").replace('`', "");
            if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                line = format!("• {rest}");
            }
            if line.is_empty() {
                continue;
            }
            lines.push(line);
            if lines.join("\n").chars().count() > max {
                break;
            }
        }
        let text = lines.join("\n");
        if text.chars().count() <= max {
            return text;
        }
        let cut: String = text.chars().take(max).collect();
        let end = cut.rfind([' ', '\n']).unwrap_or(cut.len());
        format!("{}…", cut[..end].trim_end_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation()))
    }
}

/// The release to offer `current` (see the module comment), the newest eligible one, or None.
pub fn choose(releases: &[Release], current: &Version, product: &Product) -> Option<Release> {
    releases
        .iter()
        .filter(|r| !r.draft && r.published && r.complete(product))
        .filter(|r| current.is_prerelease() || !r.is_prerelease())
        .filter(|r| r.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))
        .cloned()
}

/// The expected SHA-256 of `name` in a SHA256SUMS file (`<hex>  <name>` lines, as `shasum -a 256` writes them).
/// Exactly one well-formed line must name the file; anything else is None (missing or ambiguous).
pub fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    let mut found = vec![];
    for line in sums.lines() {
        let fields: Vec<&str> = line.split([' ', '\t']).filter(|f| !f.is_empty()).collect();
        if fields.len() != 2 {
            continue;
        }
        let file = fields[1].strip_prefix('*').unwrap_or(fields[1]);
        if file == name {
            found.push(fields[0]);
        }
    }
    match found.as_slice() {
        [hash] if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) => Some(hash.to_ascii_lowercase()),
        _ => None,
    }
}

/// Most entries a release zip may hold.
pub const MAX_ENTRIES: usize = 50_000;

/// The zip listing (`zipinfo -1`) holds only the app folder and its contents: at least one and at most
/// [`MAX_ENTRIES`] entries, each `<app>` or under `<app>/`, with no `..` component, no backslash, and no AppleDouble
/// (`._*`) or `__MACOSX` entry (they would unpack into the bundle and break its signature).
pub fn archive_entries_are_safe(entries: &[String], app: &str) -> bool {
    if entries.is_empty() || entries.len() > MAX_ENTRIES {
        return false;
    }
    let prefix = format!("{app}/");
    entries.iter().all(|entry| {
        (entry == app || entry.starts_with(&prefix))
            && !entry.contains('\\')
            && !entry.split('/').any(|c| c == ".." || c.starts_with("._") || c == "__MACOSX")
    })
}
