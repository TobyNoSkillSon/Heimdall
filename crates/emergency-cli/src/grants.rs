//! The user's clicked yes for one thing the deny list refuses (bug notes 2026-11-03T23-04-19 and 2026-11-03T18-04-11:
//! he clicked "Delete nightly" and approved removing a login item, and the hook still refused both, because his yes had
//! no way to reach it). docs/harness-autonomy.md, "the user's yes".
//!
//! - An `ask` option may carry a grant: exact paths to delete, or login items to remove, each named on the card
//!   (`agent::actions::parse_ask` checks that). Only the app writes one, from his real click on that option
//!   (`app::bridge::Tools::answer_question`); a spoken answer, typed Other or the agent's `ask_close` never does.
//! - A grant lasts `LIFETIME_MS` from the click and each of its scopes (one path, one login item) is used once: the
//!   hook lets a command through only when every refusal in it is one of those scopes, exactly (`rm -rf` of the named
//!   path, never a parent, a child, a wildcard or a variable), and marks each scope used. The same command may repeat
//!   within `REPEAT_MS` (the permission hook after the PreToolUse hook, a retry after a failure).
//! - The store is `<data>/approvals/grants`: one file per clicked option, signed with a key kept beside them. The hook
//!   refuses any agent tool call that reaches into the folder (`touches`), and a file without a valid signature, past its
//!   time or longer-lived than `LIFETIME_MS` is ignored. Like the rest of the deny list this is a guard against an
//!   agent's mistake, not a security boundary: the agent runs as the same user as the app.
//!
//! On in the personal edition (`ENABLED`, 4 Nov evening: the user turned them on) now that both review blockers have
//! landed: the island draws each grant's paths and login items in full under its option (`ui::island::object::layout`,
//! the ask card), and only a real pointer click counts (`ui::island::click_source`: a mouse-down and mouse-up from the
//! HID system, never an accessibility press, a key, his voice, typed Other or an event another process posted). The
//! public edition keeps them off (`public` feature): there `write` refuses and the hook reads no grant.
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Component, Path, PathBuf},
};

/// Whether clicks grant anything in this build: the personal edition, not the public one (module notes).
pub const ENABLED: bool = !cfg!(feature = "public");
fn on() -> bool { ENABLED || cfg!(test) }

/// `heimdall-emergency grants status`: whether this build's clicks grant anything, read-only.
pub fn status() -> &'static str { if ENABLED { "grants: on" } else { "grants: off" } }

/// The grant folder under Heimdall's data folder.
pub const DIR: &str = "approvals/grants";
const KEY: &str = "key";
const USED: &str = "used";
/// How long a click's grant lasts.
pub const LIFETIME_MS: u64 = 30 * 60 * 1000;
/// How long the command that used a scope may run again.
pub const REPEAT_MS: u64 = 2 * 60 * 1000;
/// The most paths, and login items, one option may grant.
pub const MAX_PATHS: usize = 8;
pub const MAX_LOGIN_ITEMS: usize = 4;

pub const TOUCHES: &str = "Heimdall's deny list: only Heimdall writes User's grants, from his click on an ask card. Leave \
the approvals/grants folder alone.";
const USED_UP: &str = "Heimdall's deny list: User's yes for this was already used. Ask him again in your final reply.";

/// One thing a click approves.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Deleting exactly this path (absolute, normalised).
    Delete(PathBuf),
    /// Removing the login item of exactly this name.
    LoginItem(String),
}
impl Scope {
    fn key(&self) -> String {
        match self {
            Scope::Delete(p) => format!("delete\n{}", p.display()),
            Scope::LoginItem(n) => format!("login-item\n{n}"),
        }
    }
}

/// Home folders a grant never deletes whole (their contents, by exact path, it may).
const HOME_FOLDERS: [&str; 14] = ["library", "documents", "desktop", "downloads", "pictures", "music", "movies", "applications", "public", "vault", ".ssh", ".gnupg", ".claude", ".codex"];
/// Top-level folders a grant never reaches into.
const SYSTEM: [&str; 10] = ["/system", "/usr", "/bin", "/sbin", "/private", "/etc", "/var", "/dev", "/opt", "/library/keychains"];
/// Top-level folders whose first two levels a grant never deletes whole (`/Volumes/<disk>`, `/Library/Application Support`).
const SHALLOW: [&str; 3] = ["/users", "/volumes", "/library"];

/// A character the card could not show for what it is (review 4 Nov evening): a control character, any space but the
/// plain one (tab, no-break, thin), a zero-width or joining mark, a direction override, a soft hyphen or a byte-order
/// mark. A path or login item holding one is refused, so every character the user reads is the one the hook matches.
pub fn hidden_char(c: char) -> bool {
    (c.is_control()) || (c.is_whitespace() && c != ' ')
        || matches!(c, '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{E0000}'..='\u{E0FFF}')
}

/// A path as the case-insensitive file system sees it, for comparing.
fn folded(p: &Path) -> String { p.to_string_lossy().to_lowercase() }
/// Whether `a` is `b` or inside it (folded strings).
fn within(a: &str, b: &str) -> bool { a == b || a.strip_prefix(b).is_some_and(|r| r.starts_with('/')) || b == "/" }

/// A path the user may approve deleting, as the grant keeps it: absolute (`~/` is the user's home), exact (no wildcard,
/// variable, `.` or `..`) and narrow. Never the root or a top-level folder, the first two levels of `/Users`,
/// `/Volumes` or `/Library`, another user's home or `/Users/Shared`, his home, one of its main folders or a
/// `~/Library` folder itself, the Vault or a folder right inside it, the Keychains or the system. Compared without case,
/// as APFS does.
pub fn grant_path(raw: &str, user: &Path, vault: &Path) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().count() > 1024 { return Err("grant.delete: each path is 1 to 1024 characters".into()); }
    if raw.contains(['*', '?', '[', ']', '{', '}', '$', '`', '\n', '\0', '\\']) {
        return Err(format!("grant.delete: {raw:?} must be one exact path, with no wildcard, variable or backslash"));
    }
    if raw.chars().any(hidden_char) {
        return Err(format!("grant.delete: {raw:?} holds a character the card cannot show (a tab, an invisible or direction mark)"));
    }
    let path = if let Some(rest) = raw.strip_prefix("~/") { user.join(rest) } else if raw.starts_with('/') { PathBuf::from(raw) } else {
        return Err(format!("grant.delete: {raw:?} must be absolute (or start with ~/)"));
    };
    if path.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) {
        return Err(format!("grant.delete: {raw:?} must not contain . or .."));
    }
    let path: PathBuf = path.components().collect();
    let (p, home, vault) = (folded(&path), folded(user), folded(vault));
    let depth = path.components().count() - 1;
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let parent = path.parent().map(folded).unwrap_or_default();
    let broad = depth < 2
        || within(&home, &p) || within(&vault, &p)
        || (within(&p, &vault) && depth <= vault.matches('/').count() + 1)
        || (parent == home && HOME_FOLDERS.contains(&name.as_str()))
        || parent == format!("{home}/library")
        || within(&p, &format!("{home}/library/keychains"))
        || SYSTEM.iter().any(|s| within(&p, s))
        || SHALLOW.iter().any(|s| within(&p, s) && depth <= 2)
        || (within(&p, "/users") && !within(&p, &home));
    if broad { return Err(format!("grant.delete: {raw:?} is too broad to delete whole; name the folders inside it")); }
    Ok(path)
}

/// A login item's name as a grant keeps it: 1 to 128 characters, no quote, backslash or new line.
pub fn grant_login_item(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 128 || name.contains(['"', '\\', '\n', '\r']) || name.chars().any(hidden_char) {
        return Err(format!("grant.loginItem: {raw:?} must be the item's name, 1 to 128 characters, with no quotes"));
    }
    Ok(name.to_owned())
}

/// What one clicked option granted.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Grant {
    card: String,
    /// Its signature: what its use marks are keyed by, so a renamed or copied file is the same grant.
    mac: String,
    scopes: Vec<Scope>,
}

fn signed_text(card: &str, option: &str, clicked_ms: u64, expires_ms: u64, scopes: &[Scope]) -> String {
    let mut text = format!("heimdall-grant-v2\n{card}\n{option}\n{clicked_ms}\n{expires_ms}\n");
    for s in scopes { text.push_str(&s.key()); text.push('\0'); }
    text
}

/// Writes the grant for the user's click on option `label` of question `question` on ask card `card` (the app only).
/// Returns the grant's file. Paths and names are checked again here. Old grants and marks are pruned. Refused while
/// grants are switched off (`ENABLED`).
#[allow(clippy::too_many_arguments)]
pub fn write(data: &Path, user: &Path, vault: &Path, card: &str, question: &str, label: &str, delete: &[String], login_items: &[String], now_ms: u64) -> Result<PathBuf, String> {
    if !on() { return Err("grants are switched off in this build (heimdall_emergency::grants::ENABLED)".into()); }
    let option = format!("{question}\n{label}");
    let option = option.as_str();
    if delete.is_empty() && login_items.is_empty() { return Err("a grant names a path or a login item".into()); }
    if delete.len() > MAX_PATHS || login_items.len() > MAX_LOGIN_ITEMS { return Err(format!("a grant names at most {MAX_PATHS} paths and {MAX_LOGIN_ITEMS} login items")); }
    let mut scopes = Vec::new();
    for p in delete { scopes.push(Scope::Delete(grant_path(p, user, vault)?)); }
    for n in login_items { scopes.push(Scope::LoginItem(grant_login_item(n)?)); }
    let dir = data.join(DIR);
    private_dir(&dir)?;
    prune(&dir, now_ms);
    let key = key(&dir, true)?;
    let expires_ms = now_ms + LIFETIME_MS;
    let mac = hex(&hmac(&key, signed_text(card, option, now_ms, expires_ms, &scopes).as_bytes()));
    let id = hex(&sha(format!("{card}\n{option}\n{now_ms}").as_bytes())[..12]);
    let body = json!({
        "card": card, "option": option, "clickedMs": now_ms, "expiresMs": expires_ms,
        "delete": scopes.iter().filter_map(|s| match s { Scope::Delete(p) => Some(p.display().to_string()), _ => None }).collect::<Vec<_>>(),
        "loginItems": scopes.iter().filter_map(|s| match s { Scope::LoginItem(n) => Some(n.clone()), _ => None }).collect::<Vec<_>>(),
        "mac": mac,
    });
    let path = dir.join(format!("{id}.json"));
    write_private(&dir, &path, (serde_json::to_string_pretty(&body).map_err(|e| e.to_string())? + "\n").as_bytes())?;
    Ok(path)
}

/// The grants that hold now, read for one command: `what` is the tool call as text, so a repeat is recognised.
pub struct Ledger { dir: PathBuf, grants: Vec<Grant>, digest: String, now_ms: u64 }
impl Ledger {
    pub fn load(data: &Path, what: &str, now_ms: u64) -> Ledger {
        let dir = data.join(DIR);
        let grants = match key(&dir, false).ok().filter(|_| on()) {
            Some(key) => read(&dir, &key, now_ms),
            None => vec![],
        };
        Ledger { dir, grants, digest: hex(&sha(what.as_bytes())), now_ms }
    }
    /// Whether some grant holds `scope` and it is still free for this command.
    pub fn covers(&self, scope: &Scope) -> bool {
        self.grants.iter().any(|g| g.scopes.contains(scope) && self.free(g, scope))
    }
    fn mark(&self, g: &Grant, scope: &Scope) -> PathBuf {
        self.dir.join(USED).join(hex(&sha(format!("{}\n{}", g.mac, scope.key()).as_bytes())[..16]))
    }
    /// Unused, or used by this same command within `REPEAT_MS`.
    fn free(&self, g: &Grant, scope: &Scope) -> bool {
        match std::fs::read_to_string(self.mark(g, scope)) {
            Err(_) => true,
            Ok(text) => {
                let mut parts = text.split_whitespace();
                let at: u64 = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
                parts.next() == Some(self.digest.as_str()) && self.now_ms.saturating_sub(at) <= REPEAT_MS
            }
        }
    }
    /// Uses each scope (one grant each), atomically per scope. Returns the cards they came from, or why not.
    pub fn consume(&self, scopes: &[Scope]) -> Result<Vec<String>, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let used = self.dir.join(USED);
        private_dir(&used)?;
        let mut cards = Vec::new();
        'scope: for scope in scopes {
            for g in self.grants.iter().filter(|g| g.scopes.contains(scope)) {
                let mark = self.mark(g, scope);
                match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&mark) {
                    Ok(mut f) => { let _ = writeln!(f, "{} {}", self.now_ms, self.digest); }
                    Err(_) if self.free(g, scope) => {}
                    Err(_) => continue,
                }
                if !cards.contains(&g.card) { cards.push(g.card.clone()); }
                continue 'scope;
            }
            return Err(USED_UP.into());
        }
        Ok(cards)
    }
    pub fn is_empty(&self) -> bool { self.grants.is_empty() }
}

/// Every valid grant in `dir` at `now_ms`: signed with `key`, clicked no later than now (a minute's grace), not expired,
/// and no longer-lived than `LIFETIME_MS`.
fn read(dir: &Path, key: &[u8], now_ms: u64) -> Vec<Grant> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") { continue; }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let (Some(card), Some(option), Some(clicked_ms), Some(expires_ms), Some(mac)) =
            (v["card"].as_str(), v["option"].as_str(), v["clickedMs"].as_u64(), v["expiresMs"].as_u64(), v["mac"].as_str()) else { continue };
        let strings = |k: &str| v[k].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_owned)).collect::<Vec<_>>();
        let mut scopes: Vec<Scope> = strings("delete").into_iter().map(|p| Scope::Delete(PathBuf::from(p))).collect();
        scopes.extend(strings("loginItems").into_iter().map(Scope::LoginItem));
        let expected = hex(&hmac(key, signed_text(card, option, clicked_ms, expires_ms, &scopes).as_bytes()));
        if !equal(expected.as_bytes(), mac.as_bytes()) { continue; }
        if clicked_ms > now_ms + 60_000 || now_ms >= expires_ms || expires_ms.saturating_sub(clicked_ms) > LIFETIME_MS { continue; }
        out.push(Grant { card: card.into(), mac: mac.to_owned(), scopes });
    }
    out
}

/// Removes grants a day past their time and use marks a day old.
fn prune(dir: &Path, now_ms: u64) {
    const DAY: u64 = 24 * 60 * 60 * 1000;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") { continue; }
        let expires = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["expiresMs"].as_u64()).unwrap_or(0);
        if expires + DAY < now_ms { let _ = std::fs::remove_file(&p); }
    }
    for e in std::fs::read_dir(dir.join(USED)).into_iter().flatten().flatten() {
        let at = std::fs::read_to_string(e.path()).ok().and_then(|t| t.split_whitespace().next().and_then(|n| n.parse::<u64>().ok())).unwrap_or(0);
        if at + DAY < now_ms { let _ = std::fs::remove_file(e.path()); }
    }
}

/// Whether an agent's tool call reaches into the grant folder (only while grants are on; review 4 Nov: the user's data
/// folder is `~/Library/Application Support/Heimdall`, so nothing may match on a word like "heimdall" alone). A file
/// tool: its path, through its folders' symlinks, inside `<data>/approvals`. A Bash command: one whose text names
/// `<data>/approvals` (as an absolute path or from `~/`, without case, `\ ` read as a space) or a wildcard right after
/// `<data>/`, or that runs inside `<data>/approvals`.
pub fn touches(tool: &str, input: &Value, data: &Path, cwd: Option<&Path>) -> bool {
    if !on() { return false; }
    let lexical: PathBuf = data.components().collect();
    let mut datas = vec![lexical.clone()];
    if let Ok(real) = std::fs::canonicalize(data) { if real != lexical { datas.push(real); } }
    let guarded: Vec<PathBuf> = datas.iter().map(|d| d.join("approvals")).collect();
    let inside = |p: &Path| guarded.iter().any(|r| within(&folded(p), &folded(r)));
    match tool {
        "Bash" => {
            let Some(command) = input["command"].as_str() else { return false };
            if cwd.is_some_and(|c| inside(&normalize(c))) { return true; }
            let text = command.to_lowercase().replace("\\ ", " ").replace(['\'', '"'], "");
            let home = std::env::var_os("HOME").map(|h| folded(Path::new(&h))).unwrap_or_default();
            let mut forms: Vec<String> = datas.iter().map(|d| folded(d)).collect();
            for d in datas.iter().map(|d| folded(d)) {
                if !home.is_empty() { if let Some(rest) = d.strip_prefix(&home) { forms.push(format!("~{rest}")); forms.push(format!("$home{rest}")); forms.push(format!("${{home}}{rest}")); } }
            }
            forms.iter().any(|f| text.match_indices(&format!("{f}/")).any(|(i, m)| {
                let rest = &text[i + m.len()..];
                let segment = rest.split(['/', ' ', ';', '&', '|', ')']).next().unwrap_or("");
                segment == "approvals" || segment.contains(['*', '?', '['])
            }))
        }
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Grep" | "Glob" => {
            let Some(path) = input["file_path"].as_str().or(input["notebook_path"].as_str()).or(input["path"].as_str()) else { return false };
            let path = if let Some(rest) = path.strip_prefix("~/") { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest) }
                else if path.starts_with('/') { PathBuf::from(path) }
                else if let Some(c) = cwd { c.join(path) } else { return false };
            let path = resolved(&normalize(&path));
            let pattern = input["pattern"].as_str().unwrap_or("").to_lowercase();
            let over_data = matches!(tool, "Grep" | "Glob") && datas.iter().any(|d| folded(&path) == folded(d)) && (pattern.contains("appr") || pattern.starts_with("**"));
            inside(&path) || over_data
        }
        _ => false,
    }
}

/// Its folders through their symlinks (the last part kept as named).
fn resolved(path: &Path) -> PathBuf {
    let Some(name) = path.file_name() else { return path.into() };
    let mut parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&parent) {
            let mut out = real;
            for part in rest.iter().rev() { out.push(part); }
            out.push(name);
            return out;
        }
        match (parent.file_name().map(|n| n.to_owned()), parent.parent().map(Path::to_path_buf)) {
            (Some(n), Some(up)) => { rest.push(n); parent = up; }
            _ => return path.into(),
        }
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => { out.pop(); }
            c => out.push(c),
        }
    }
    out
}

/// The signing key (32 random bytes, 0600), made on the first grant when `create`.
fn key(dir: &Path, create: bool) -> Result<Vec<u8>, String> {
    let path = dir.join(KEY);
    if let Ok(k) = std::fs::read(&path) { if k.len() == 32 { return Ok(k); } }
    if !create { return Err("no grant key".into()); }
    use std::io::Read;
    let mut k = vec![0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut k)).map_err(|e| format!("no randomness for the grant key: {e}"))?;
    write_private(dir, &path, &k)?;
    Ok(k)
}

fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))
}
fn write_private(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let tmp = dir.join(format!(".tmp.{}.{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(bytes).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}
/// HMAC-SHA256 (RFC 2104).
fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut k = [0u8; 64];
    if key.len() > 64 { k[..32].copy_from_slice(&sha(key)); } else { k[..key.len()].copy_from_slice(key); }
    let (mut inner, mut outer) = ([0x36u8; 64], [0x5cu8; 64]);
    for i in 0..64 { inner[i] ^= k[i]; outer[i] ^= k[i]; }
    let first = sha2::Sha256::new().chain_update(inner).chain_update(message).finalize();
    sha2::Sha256::new().chain_update(outer).chain_update(first).finalize().into()
}
fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
fn equal(a: &[u8], b: &[u8]) -> bool { a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 }

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> PathBuf { PathBuf::from("/Users/t") }
    fn vault() -> PathBuf { PathBuf::from("/Users/t/Work") }
    fn put(data: &Path, card: &str, q: &str, delete: &[&str], items: &[&str], now: u64) -> PathBuf {
        let d: Vec<String> = delete.iter().map(|s| s.to_string()).collect();
        let i: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        write(data, &user(), &vault(), card, q, "Yes", &d, &i, now).unwrap()
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(hex(&hmac(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn grants_are_on_in_the_personal_edition_only() {
        assert_eq!(ENABLED, !cfg!(feature = "public"));
        assert_eq!(status(), if ENABLED { "grants: on" } else { "grants: off" });
    }

    #[test]
    fn only_exact_narrow_paths_can_be_granted() {
        for ok in ["~/Backups/T3 Code (Nightly).app", "/Applications/T3 Code (Alpha).app", "~/.t3-backup-before-nightly-20261005", "~/Library/Application Support/t3code",
            "~/Downloads/T3-Code-0.1.6.dmg", "/Users/t/x/", "/Library/Application Support/OldApp", "/Volumes/Disk/old", "/Users/t/Work/Projects/X"] {
            assert!(grant_path(ok, &user(), &vault()).is_ok(), "{ok}");
        }
        assert_eq!(grant_path("/Users/t/x/", &user(), &vault()).unwrap(), PathBuf::from("/Users/t/x"));
        for bad in ["/", "/Users", "/Applications", "~", "/Users/t", "~/Library", "~/Documents", "~/documents", "/USERS/T/Documents", "~/Downloads", "~/Library/Application Support",
            "~/library/application support", "~/Library/Keychains/login.keychain-db", "/System/Library/x", "/usr/local/bin", "/opt/homebrew/Cellar/x", "/Volumes/Disk",
            "/Users/Shared", "/Users/Shared/x", "/Users/other/x", "/Library/Application Support", "/Users/t/Work", "/Users/t/work/projects",
            "~/Backups/*", "~/Backups/T3*", "~/$X", "~/Backups/../Documents", "~/a\\ b", "relative/path", "", "/private/var/folders/x"] {
            assert!(grant_path(bad, &user(), &vault()).is_err(), "{bad}");
        }
        assert!(grant_login_item("swiftpm-testing-helper").is_ok());
        assert!(grant_login_item("x\" & delete every login item & \"").is_err());
        // Review 4 Nov evening: nothing the card cannot show. Plain spaces and accented or other scripts are fine.
        for ok in ["~/Backups/Zdjęcia stare", "~/写真/古い", "~/Backups/café.app"] {
            assert!(grant_path(ok, &user(), &vault()).is_ok(), "{ok}");
        }
        for bad in ["~/a\tb", "~/a\u{00A0}b", "~/a\u{200B}b", "~/a\u{202E}ppa.exe", "~/a\u{FEFF}", "~/a\u{2066}b", "~/a\u{00AD}b", "~/a\rb"] {
            assert!(grant_path(bad, &user(), &vault()).is_err(), "{bad:?}");
            assert!(grant_login_item(&bad[2..]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_written_grant_reads_back_and_a_forged_or_old_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let now = 1_000_000_000_000;
        let file = put(data, "ask-1", "q1", &["~/Backups/T3 Code (Nightly).app"], &["Arsenal"], now);
        let ledger = Ledger::load(data, "rm", now + 1000);
        assert!(ledger.covers(&Scope::Delete(PathBuf::from("/Users/t/Backups/T3 Code (Nightly).app"))));
        assert!(ledger.covers(&Scope::LoginItem("Arsenal".into())));
        assert!(!ledger.covers(&Scope::Delete(PathBuf::from("/Users/t/Backups"))));
        assert!(Ledger::load(data, "rm", now + LIFETIME_MS).is_empty(), "expired");
        // Widened by hand, or a new file with no valid signature: ignored.
        let text = std::fs::read_to_string(&file).unwrap().replace("/Users/t/Backups/T3 Code (Nightly).app", "/Users/t/Documents");
        std::fs::write(&file, text).unwrap();
        assert!(Ledger::load(data, "rm", now + 1000).is_empty(), "edited grant ignored");
        let forged = json!({"card":"ask-9","option":"q1\nYes","clickedMs":now,"expiresMs":now + LIFETIME_MS,"delete":["/Users/t/Documents"],"loginItems":[],"mac":"00"});
        std::fs::write(data.join(DIR).join("forged.json"), forged.to_string()).unwrap();
        assert!(Ledger::load(data, "rm", now + 1000).is_empty(), "forged grant ignored");
        // Signed with the key but living longer than a click's grant: ignored.
        let k = key(&data.join(DIR), false).unwrap();
        let scopes = [Scope::Delete(PathBuf::from("/Users/t/Documents/x"))];
        let long = now + 10 * LIFETIME_MS;
        let mac = hex(&hmac(&k, signed_text("ask-9", "q1\nYes", now, long, &scopes).as_bytes()));
        std::fs::write(data.join(DIR).join("long.json"), json!({"card":"ask-9","option":"q1\nYes","clickedMs":now,"expiresMs":long,"delete":["/Users/t/Documents/x"],"loginItems":[],"mac":mac}).to_string()).unwrap();
        assert!(Ledger::load(data, "rm", now + 1000).is_empty());
    }

    #[test]
    fn each_scope_is_used_once_but_the_same_command_may_repeat_briefly() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let now = 1_000_000_000_000;
        let file = put(data, "ask-1", "q1", &["~/a/one", "~/a/two"], &[], now);
        let one = Scope::Delete(PathBuf::from("/Users/t/a/one"));
        let two = Scope::Delete(PathBuf::from("/Users/t/a/two"));
        assert_eq!(Ledger::load(data, "rm -rf ~/a/one", now + 1).consume(std::slice::from_ref(&one)).unwrap(), vec!["ask-1".to_string()]);
        assert!(Ledger::load(data, "rm -rf ~/a/one", now + 2).covers(&one), "the same command, at once");
        assert!(!Ledger::load(data, "rm -rf ~/a/one", now + REPEAT_MS + 10).covers(&one), "not later");
        assert!(!Ledger::load(data, "rm -r ~/a/one", now + 3).covers(&one), "nor another command");
        assert!(Ledger::load(data, "rm -r ~/a/one", now + 3).consume(&[one.clone()]).is_err());
        // A copy of the file under another name is the same grant: its use marks follow its signature.
        std::fs::copy(&file, data.join(DIR).join("copy.json")).unwrap();
        assert!(!Ledger::load(data, "rm -r ~/a/one", now + 4).covers(&one), "a copied grant is not a second yes");
        assert!(Ledger::load(data, "rm -rf ~/a/two", now + 4).consume(&[two]).is_ok(), "the other scope is its own");
    }

    /// Review 4 Nov: two questions of one card with the same option label are two grants, not one file.
    #[test]
    fn two_questions_on_one_card_keep_their_own_grants() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000_000_000_000;
        let a = put(dir.path(), "ask-1", "q1", &["~/a/one"], &[], now);
        let b = put(dir.path(), "ask-1", "q2", &["~/a/two"], &[], now);
        assert_ne!(a, b);
        let l = Ledger::load(dir.path(), "x", now + 1);
        assert!(l.covers(&Scope::Delete(PathBuf::from("/Users/t/a/one"))) && l.covers(&Scope::Delete(PathBuf::from("/Users/t/a/two"))));
    }

    #[test]
    fn agent_calls_into_the_grant_folder_are_caught() {
        let dir = tempfile::tempdir().unwrap();
        // The user's data folder is ~/Library/Application Support/Heimdall (review 4 Nov).
        let data = dir.path().join("Library/Application Support/Heimdall");
        std::fs::create_dir_all(data.join(DIR)).unwrap();
        let g = data.join(DIR);
        let d = data.display().to_string();
        let bash = |c: &str, cwd: Option<&Path>| touches("Bash", &json!({"command": c}), &data, cwd);
        assert!(bash(&format!("echo '{{}}' > '{}/x.json'", g.display()), None));
        assert!(bash(&format!("echo '{{}}' > '{}/X.json'", g.display().to_string().to_uppercase()), None), "case folded");
        assert!(bash(&format!("cat {}/approvals/gr*/key", d.replace(' ', "\\ ")), None), "escaped space, glob below");
        assert!(bash(&format!("cat '{d}'/appr*/g*/key"), None), "quoted, glob");
        assert!(bash(&format!("cat '{d}'/\"approvals\"/grants/key"), None), "quotes split the path");
        assert!(bash(&format!("python3 -c \"open('{d}/approvals/'+'grants/x.json','w')\""), None));
        assert!(bash("cp /tmp/x.json grants/", Some(&data.join("approvals"))));
        for ok in ["cargo test -p heimdall-emergency grants", "rm -rf /tmp/heimdall-*", "ls ~/Work/Projects/Heimdall/crates/*/Cargo.toml",
            "tail -5 ~/Library/Application\\ Support/Heimdall/logs/approvals.log", "cd /Users/t/Work/Projects/Heimdall/.claude/worktrees/approvals && git status",
            "rg grants ~/Library/Application\\ Support/Heimdall/logs"] {
            assert!(!bash(ok, Some(Path::new("/Users/t/Work/Projects/Heimdall"))), "{ok}");
        }
        assert!(!bash(&format!("tail -5 '{d}/logs/approvals.log'"), None));
        for tool in ["Write", "Edit", "Read"] {
            assert!(touches(tool, &json!({"file_path": g.join("key")}), &data, None), "{tool}");
        }
        assert!(touches("Write", &json!({"file_path": "../grants/x.json"}), &data, Some(&data.join("approvals/other"))));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&g, &link).unwrap();
        assert!(touches("Write", &json!({"file_path": link.join("x.json")}), &data, None), "through a link");
        assert!(touches("Glob", &json!({"path": data, "pattern": "**/key"}), &data, None));
        assert!(!touches("Grep", &json!({"path": data.join("logs"), "pattern": "grant"}), &data, None));
        assert!(!touches("Read", &json!({"file_path": data.join("logs/approvals.log")}), &data, None));
    }
}
