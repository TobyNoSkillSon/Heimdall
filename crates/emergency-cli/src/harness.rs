//! Every agent harness on this Mac in its most autonomous mode, checked at each Heimdall start ("when
//! we set up any harness, the agents get the most YOLO mode possible, so they literally don't ask you anything ... We
//! can't keep patching these changes"; docs/harness-autonomy.md).
//!
//! `ensure` reads each harness's config, says what has drifted from the settings below and, with `apply`, writes them
//! back, keeping everything else in the file. A harness that is not installed is skipped. What it sets:
//! - **Claude Code**, every config folder (`~/.claude`, each `~/.claude-*`, each T3 Claude instance's `homePath`):
//!   `permissions.defaultMode = "bypassPermissions"`, `skipDangerousModePermissionPrompt = true` (a background or
//!   first session would otherwise refuse bypass), no `ask` rules, no `disableBypassPermissionsMode`, no
//!   `block{Reads,Writes}OutsideWorkingDirectories`, the deny rules in `CLAUDE_DENY` (the plainest forms of
//!   `deny.rs`'s list, so they hold even without the hook; `AskUserQuestion` among them, so agents never stop on a
//!   question), and, given the installed CLI, Heimdall's `PermissionRequest` and `PreToolUse` hooks (`approvals.rs`).
//!   A `settings.local.json` loses any non-bypass `defaultMode` and its `ask` rules. Managed settings are only reported.
//! - **T3 Code** (`~/.t3/userdata/settings.json`, which T3 reloads by itself): each Claude instance launches with
//!   `CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1`, the only switch for the checks `bypassPermissions` keeps
//!   (a critical-path `rm`, "Dangerous rm operation not ruled out", which a here-document or a chained command
//!   trips); Claude reads it from the launch environment only, never from settings. The default `claudeAgent` instance
//!   gets an entry for it. T3's own `full-access` mode is already its default and maps to each driver's maximum.
//! - **Codex** (`~/.codex/config.toml`): `approval_policy = "never"`, `sandbox_mode = "danger-full-access"`, and in
//!   `~/.codex/rules/default.rules` an allow rule for `rm` (under `never`, Codex forbids a forced `rm` outright).
//! - **OpenCode** (`~/.config/opencode/opencode.json[c]`): `permission = {"*": "allow", "question": "deny"}`.
//! - **Grok** (`~/.grok/config.toml`): `[ui] permission_mode = "always-approve"`, `yolo = true`;
//!   `[features] ask_user_question = false`.
//!
//! Each file is copied to `<backups>/<harness>-<name>.<unix time>` before it is first rewritten.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Claude Code deny rules (permissions doc: a deny rule holds in every mode, bypassPermissions included). Deletes are
/// not here: a Bash rule matches as a prefix (live 2 Nov: `Bash(rm -rf /)` refused every `rm -rf /Users/…`), so the
/// deny list's path rules live in the `PreToolUse` hook (`deny.rs`).
pub const CLAUDE_DENY: &[&str] = &[
    "AskUserQuestion",
    "Bash(diskutil erase*)", "Bash(diskutil partitionDisk*)", "Bash(diskutil zeroDisk*)", "Bash(diskutil secureErase*)",
    "Bash(diskutil apfs delete*)", "Bash(diskutil apfs erase*)", "Bash(newfs*)",
    "Bash(security find-generic-password*)", "Bash(security find-internet-password*)", "Bash(security dump-keychain*)",
    "Bash(security delete-*)", "Bash(security add-generic-password*)", "Bash(security add-internet-password*)",
    "Bash(security export*)", "Bash(security unlock-keychain*)",
    "Read(~/Library/Keychains/**)", "Edit(~/Library/Keychains/**)",
    "Bash(sudo *)", "Bash(systemsetup *)", "Bash(csrutil disable*)", "Bash(csrutil enable*)", "Bash(tccutil *)",
    "Bash(defaults write -g *)", "Bash(defaults write NSGlobalDomain *)", "Bash(defaults write com.apple.*)",
    "Bash(defaults delete com.apple.*)",
    "Bash(launchctl bootstrap *)", "Bash(launchctl bootout *)", "Bash(launchctl load *)", "Bash(launchctl unload *)",
    "Bash(launchctl enable *)", "Bash(launchctl disable *)", "Bash(launchctl kickstart *)",
    "Edit(~/Library/LaunchAgents/**)", "Edit(/Library/LaunchAgents/**)", "Edit(/Library/LaunchDaemons/**)",
];
/// The launch variable T3 gives each Claude instance.
pub const CLAUDE_ENV: &str = "CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS";
/// Rules an earlier build wrote that do harm, taken out again.
const CLAUDE_RETIRED: &[&str] = &["Bash(rm -rf /)", "Bash(rm -rf /*)", "Bash(rm -rf ~)", "Bash(rm -rf ~/)", "Bash(rm -rf ~/*)", "Bash(rm -rf $HOME)", "Bash(rm -fr /)", "Bash(rm -fr ~)"];
const CODEX_RM_RULE: &str = "prefix_rule(pattern=[\"rm\"], decision=\"allow\")";

/// One setting that was not as it should be, and whether it is fixed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift { pub harness: &'static str, pub file: PathBuf, pub what: String, pub fixed: bool }

#[derive(Debug, Default)]
pub struct Report { pub drift: Vec<Drift>, pub errors: Vec<String>, pub checked: Vec<&'static str>, pub unreadable: Vec<&'static str> }

impl Report {
    /// The harnesses that drifted, once each, in the order found.
    pub fn harnesses(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = vec![];
        for d in &self.drift { if !out.contains(&d.harness) { out.push(d.harness); } }
        out
    }
    /// One line per drift and error, for the log and the CLI.
    pub fn lines(&self) -> String {
        let mut out: Vec<String> = self.drift.iter().map(|d| format!("{} {}: {} ({})", if d.fixed { "fixed" } else { "drift" }, d.harness, d.what, d.file.display())).collect();
        out.extend(self.errors.iter().map(|e| format!("error: {e}")));
        if out.is_empty() { out.push(format!("All set: {}.", self.checked.join(", "))); }
        out.join("\n")
    }
}

/// Where the harnesses keep their settings: the user's home folder, and the installed Heimdall CLI for the hooks (None:
/// hooks are left as they are, as in a test or a CLI run outside the bundle).
pub struct Setup<'a> { pub user: &'a Path, pub hook_command: Option<String>, pub backups: Option<&'a Path> }

/// Checks every installed harness; with `apply`, writes the settings back. Never fails as a whole: a file it cannot
/// read or write is an error line and the rest goes on.
pub fn ensure(setup: &Setup, apply: bool) -> Report {
    // One pass at a time in this process (start, setup and the T3 connection can overlap).
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut r = Report::default();
    let t3 = setup.user.join(".t3/userdata/settings.json");
    let t3_settings: Value = std::fs::read_to_string(&t3).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    for dir in claude_dirs(setup.user, &t3_settings) { claude(setup, &dir, apply, &mut r); }
    if t3.exists() { t3_env(setup, &t3, apply, &mut r); }
    codex(setup, apply, &mut r);
    opencode(setup, apply, &mut r);
    grok(setup, apply, &mut r);
    let managed = Path::new("/Library/Application Support/ClaudeCode/managed-settings.json");
    if let Ok(text) = std::fs::read_to_string(managed) {
        if text.contains("disableBypassPermissionsMode") { r.errors.push(format!("{} turns bypass mode off; only an administrator can change it", managed.display())); }
    }
    r
}

/// The harnesses whose agents the deny list guards: Claude Code's hooks, and T3 Code's Claude instances (they read the
/// same settings). Codex, OpenCode and Grok run allow-all with no Heimdall hook.
pub const GUARDED: [&str; 2] = ["Claude Code", "T3 Code"];

/// Setup's view: each installed harness once, in `ensure`'s order, and whether it already never asks (nothing about it
/// has drifted, and its files could be read). Reads only.
pub fn status(setup: &Setup) -> Vec<(&'static str, bool)> {
    let r = ensure(setup, false);
    r.checked.iter().map(|h| (*h, !r.drift.iter().any(|d| d.harness == *h) && !r.unreadable.contains(h))).collect()
}

/// `~/.claude`, every `~/.claude-*` folder with a config in it, and each T3 Claude instance's `homePath`.
pub fn claude_dirs(user: &Path, t3: &Value) -> Vec<PathBuf> {
    let mut dirs = vec![user.join(".claude")];
    if let Ok(entries) = std::fs::read_dir(user) {
        let mut more: Vec<PathBuf> = entries.flatten().map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(".claude-")))
            .filter(|p| p.join("settings.json").exists() || p.join(".claude.json").exists())
            .collect();
        more.sort();
        dirs.extend(more);
    }
    for inst in t3["providerInstances"].as_object().into_iter().flat_map(|m| m.values()) {
        if inst["driver"] != "claudeAgent" { continue; }
        if let Some(h) = inst["config"]["homePath"].as_str().filter(|h| !h.trim().is_empty()) {
            let h = h.trim();
            let p = if let Some(rest) = h.strip_prefix("~/") { user.join(rest) } else { PathBuf::from(h) };
            if !dirs.contains(&p) { dirs.push(p); }
        }
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

/// The Claude settings this module wants, applied over `old`; each change described.
pub fn claude_settings(mut s: Value, hook: Option<&str>) -> (Value, Vec<String>) {
    let mut what = vec![];
    if !s.is_object() { s = json!({}); }
    if !s["permissions"].is_object() { s["permissions"] = json!({}); }
    if s["permissions"]["defaultMode"] != "bypassPermissions" {
        what.push(format!("defaultMode was {}", s["permissions"]["defaultMode"]));
        s["permissions"]["defaultMode"] = json!("bypassPermissions");
    }
    if s["skipDangerousModePermissionPrompt"] != true {
        what.push("skipDangerousModePermissionPrompt was off".into());
        s["skipDangerousModePermissionPrompt"] = json!(true);
    }
    let perms = s["permissions"].as_object_mut().unwrap();
    for key in ["disableBypassPermissionsMode", "disableAutoMode", "blockReadsOutsideWorkingDirectories", "blockWritesOutsideWorkingDirectories"] {
        if perms.remove(key).is_some() { what.push(format!("removed {key}")); }
    }
    if perms.get("ask").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        what.push(format!("removed ask rules {}", perms["ask"]));
    }
    perms.remove("ask");
    let mut deny: Vec<Value> = perms.get("deny").and_then(Value::as_array).cloned().unwrap_or_default();
    let before = deny.len();
    deny.retain(|d| !d.as_str().is_some_and(|d| CLAUDE_RETIRED.contains(&d)));
    if deny.len() != before { what.push(format!("removed {} retired deny rules", before - deny.len())); }
    let missing: Vec<&str> = CLAUDE_DENY.iter().copied().filter(|r| !deny.iter().any(|d| d == r)).collect();
    if !missing.is_empty() {
        what.push(format!("{} deny rules missing", missing.len()));
        deny.extend(missing.iter().map(|m| json!(m)));
    }
    perms.insert("deny".into(), Value::Array(deny));
    if let Some(hook) = hook {
        let with = crate::approvals::with_hook(s.clone(), Some(hook));
        if with != s { what.push("Heimdall's hooks were missing or old".into()); s = with; }
    }
    (s, what)
}

/// A `settings.local.json`: only what would bring prompts back.
fn claude_local(mut s: Value) -> (Value, Vec<String>) {
    let mut what = vec![];
    let Some(perms) = s.get_mut("permissions").and_then(Value::as_object_mut) else { return (s, what) };
    if perms.get("defaultMode").is_some_and(|m| m != "bypassPermissions") { what.push(format!("defaultMode was {}", perms["defaultMode"])); perms.remove("defaultMode"); }
    if perms.get("ask").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) { what.push("removed ask rules".into()); perms.remove("ask"); }
    for key in ["disableBypassPermissionsMode", "blockReadsOutsideWorkingDirectories", "blockWritesOutsideWorkingDirectories"] {
        if perms.remove(key).is_some() { what.push(format!("removed {key}")); }
    }
    (s, what)
}

fn claude(setup: &Setup, dir: &Path, apply: bool, r: &mut Report) {
    if !r.checked.contains(&"Claude Code") { r.checked.push("Claude Code"); }
    json_file(setup, "Claude Code", &dir.join("settings.json"), apply, r, |v| claude_settings(v, setup.hook_command.as_deref()), true);
    let local = dir.join("settings.local.json");
    if local.exists() { json_file(setup, "Claude Code", &local, apply, r, claude_local, false); }
}

/// T3's Claude instances, the default `claudeAgent` included, launch with `CLAUDE_ENV=1`.
pub fn t3_settings(mut s: Value) -> (Value, Vec<String>) {
    let mut what = vec![];
    if !s.is_object() { return (s, what); }
    if !s["providerInstances"].is_object() { s["providerInstances"] = json!({}); }
    let instances = s["providerInstances"].as_object_mut().unwrap();
    if !instances.contains_key("claudeAgent") {
        instances.insert("claudeAgent".into(), json!({"driver": "claudeAgent"}));
    }
    for (id, inst) in instances.iter_mut() {
        if inst["driver"] != "claudeAgent" { continue; }
        let mut env: Vec<Value> = inst["environment"].as_array().cloned().unwrap_or_default();
        let good = json!({"name": CLAUDE_ENV, "value": "1", "sensitive": false});
        let others: Vec<Value> = env.iter().filter(|e| e["name"] != CLAUDE_ENV).cloned().collect();
        let has = env.iter().any(|e| e["name"] == CLAUDE_ENV && e["value"] == "1");
        if !has || env.len() != others.len() + 1 {
            what.push(format!("{id} launched without {CLAUDE_ENV}=1"));
            env = others;
            env.push(good);
            inst["environment"] = Value::Array(env);
        }
    }
    (s, what)
}

fn t3_env(setup: &Setup, file: &Path, apply: bool, r: &mut Report) {
    r.checked.push("T3 Code");
    json_file(setup, "T3 Code", file, apply, r, t3_settings, false);
}

fn codex(setup: &Setup, apply: bool, r: &mut Report) {
    let dir = setup.user.join(".codex");
    if !dir.is_dir() { return; }
    r.checked.push("Codex");
    let file = dir.join("config.toml");
    text_file(setup, "Codex", &file, apply, r, |t| {
        let mut what = vec![];
        let mut t = t.to_owned();
        for (key, value) in [("approval_policy", "\"never\""), ("sandbox_mode", "\"danger-full-access\"")] {
            if toml_value(&t, None, key).as_deref() != Some(value) { what.push(format!("{key} was {}", toml_value(&t, None, key).unwrap_or_else(|| "unset".into()))); t = set_toml(&t, None, key, value); }
            // A profile that sets the key itself overrides the top level when it is chosen.
            let profiles: Vec<String> = t.lines().filter_map(header_name).filter(|n| n.starts_with("profiles.")).collect();
            for p in profiles {
                if let Some(v) = toml_value(&t, Some(&p), key).filter(|v| v != value) {
                    what.push(format!("[{p}] {key} was {v}"));
                    t = set_toml(&t, Some(&p), key, value);
                }
            }
        }
        (t, what)
    });
    let rules = dir.join("rules/default.rules");
    text_file(setup, "Codex", &rules, apply, r, |t| {
        if t.lines().any(|l| l.trim() == CODEX_RM_RULE) { return (t.to_owned(), vec![]); }
        let mut t = t.to_owned();
        if !t.is_empty() && !t.ends_with('\n') { t.push('\n'); }
        t.push_str(CODEX_RM_RULE);
        t.push('\n');
        (t, vec!["no allow rule for rm".into()])
    });
}

fn opencode(setup: &Setup, apply: bool, r: &mut Report) {
    let dir = setup.user.join(".config/opencode");
    if !dir.is_dir() { return; }
    r.checked.push("OpenCode");
    let file = ["opencode.jsonc", "opencode.json"].iter().map(|n| dir.join(n)).find(|p| p.exists()).unwrap_or_else(|| dir.join("opencode.json"));
    json_file(setup, "OpenCode", &file, apply, r, |mut v| {
        let want = json!({"*": "allow", "question": "deny"});
        if !v.is_object() { v = json!({}); }
        if v["permission"] == want { return (v, vec![]); }
        let was = v["permission"].to_string();
        v["permission"] = want;
        (v, vec![format!("permission was {was}")])
    }, true);
}

fn grok(setup: &Setup, apply: bool, r: &mut Report) {
    let dir = setup.user.join(".grok");
    if !dir.join("bin").exists() && !dir.join("config.toml").exists() { return; }
    r.checked.push("Grok");
    text_file(setup, "Grok", &dir.join("config.toml"), apply, r, |t| {
        let mut what = vec![];
        let mut t = t.to_owned();
        for (table, key, value) in [("ui", "permission_mode", "\"always-approve\""), ("ui", "yolo", "true"), ("features", "ask_user_question", "false")] {
            if toml_value(&t, Some(table), key).as_deref() != Some(value) { what.push(format!("[{table}] {key} was {}", toml_value(&t, Some(table), key).unwrap_or_else(|| "unset".into()))); t = set_toml(&t, Some(table), key, value); }
        }
        (t, what)
    });
}

/// Applies `change` to a JSON file. `create`: a missing file is made; otherwise it is skipped. A file that is not JSON
/// (comments in a `.jsonc`) is an error, left alone.
fn json_file(setup: &Setup, harness: &'static str, file: &Path, apply: bool, r: &mut Report, change: impl FnOnce(Value) -> (Value, Vec<String>), create: bool) {
    let old = match std::fs::read_to_string(file) {
        Ok(text) if text.trim().is_empty() => json!({}),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => v,
            Err(e) => { r.errors.push(format!("{} is not plain JSON ({e}); left alone", file.display())); r.unreadable.push(harness); return; }
        },
        Err(_) if !create => return,
        Err(_) => json!({}),
    };
    let (new, what) = change(old.clone());
    if new == old { return; }
    let fixed = apply && backup(setup, harness, file).and_then(|_| crate::approvals::rewrite_json(file, |_| new.clone()).map(|_| ())).map_err(|e| r.errors.push(e)).is_ok();
    for w in what { r.drift.push(Drift { harness, file: file.into(), what: w, fixed }); }
}

/// Applies `change` to a text file (a missing one is empty and made).
fn text_file(setup: &Setup, harness: &'static str, file: &Path, apply: bool, r: &mut Report, change: impl FnOnce(&str) -> (String, Vec<String>)) {
    let old = std::fs::read_to_string(file).unwrap_or_default();
    let (new, what) = change(&old);
    if new == old { return; }
    let fixed = apply && backup(setup, harness, file).and_then(|_| write_text(file, &new)).map_err(|e| r.errors.push(e)).is_ok();
    for w in what { r.drift.push(Drift { harness, file: file.into(), what: w, fixed }); }
}

fn backup(setup: &Setup, harness: &str, file: &Path) -> Result<(), String> {
    let (Some(dir), true) = (setup.backups, file.exists()) else { return Ok(()) };
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = format!("{}-{}.{secs}", harness.to_lowercase().replace(' ', "-"), file.file_name().and_then(|n| n.to_str()).unwrap_or("file"));
    std::fs::copy(file, dir.join(name)).map(|_| ()).map_err(|e| format!("backup of {}: {e}", file.display()))
}

fn write_text(file: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let file = std::fs::canonicalize(file).ok().filter(|_| file.is_symlink()).unwrap_or_else(|| file.to_path_buf());
    let dir = file.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let tmp = dir.join(format!(".heimdall-harness.{}.{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    if let Ok(meta) = std::fs::metadata(&file) { let _ = std::fs::set_permissions(&tmp, meta.permissions()); }
    std::fs::rename(&tmp, &file).map_err(|e| format!("{}: {e}", file.display()))
}

/// The lines of `table` (None: the top level, before the first header) as (start, end) line indexes; None when the
/// table has no header.
fn toml_region(lines: &[&str], table: Option<&str>) -> Option<(usize, usize)> {
    let header = |l: &str| { let t = l.trim(); t.starts_with('[') };
    let start = match table {
        None => 0,
        Some(name) => lines.iter().position(|l| header_name(l).as_deref() == Some(name))? + 1,
    };
    let end = lines[start..].iter().position(|l| header(l)).map_or(lines.len(), |p| start + p);
    Some((start, end))
}
/// A table header's name, comments and spaces dropped (`[ ui ] # x` is `ui`); None for any other line.
fn header_name(line: &str) -> Option<String> {
    let t = line.split('#').next().unwrap_or("").trim();
    let inner = t.strip_prefix('[')?.strip_suffix(']')?;
    if inner.starts_with('[') { return None; }
    Some(inner.chars().filter(|c| !c.is_whitespace()).collect())
}
fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') { return None; }
    let (k, _) = t.split_once('=')?;
    Some(k.trim().trim_matches('"'))
}
/// A key's raw value in a TOML table (comments after it dropped).
pub fn toml_value(text: &str, table: Option<&str>, key: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = toml_region(&lines, table)?;
    lines[start..end].iter().find(|l| key_of(l) == Some(key)).map(|l| {
        let v = l.split_once('=').unwrap().1.trim();
        let v = if v.starts_with('"') { v.find('"').and_then(|_| v[1..].find('"').map(|i| &v[..i + 2])).unwrap_or(v) } else { v.split('#').next().unwrap_or(v).trim() };
        v.to_owned()
    })
}
/// The TOML text with `key = value` in `table`, replacing the key's line or adding it (and the table) as needed.
pub fn set_toml(text: &str, table: Option<&str>, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let line = format!("{key} = {value}");
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    match toml_region(&refs, table) {
        Some((start, end)) => {
            if let Some(i) = (start..end).find(|&i| key_of(&lines[i]) == Some(key)) { lines[i] = line; }
            else {
                // After the table's last key, so a blank line before the next header stays.
                let at = (start..end).rev().find(|&i| !lines[i].trim().is_empty()).map_or(start, |i| i + 1);
                lines.insert(at, line);
            }
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) { lines.push(String::new()); }
            lines.push(format!("[{}]", table.unwrap_or_default()));
            lines.push(line);
        }
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir { tempfile::tempdir().unwrap() }
    fn read(p: &Path) -> Value { serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap() }

    /// This Mac on 2 Nov: both Claude folders with only Heimdall's old hook, T3 with no launch variable, Codex already
    /// at the maximum but no rm rule, OpenCode at "allow", Grok at `yolo = false`. One apply sets everything, keeps
    /// every other key, and a second check finds nothing.
    #[test]
    fn a_drifted_mac_is_set_back_and_then_stays() {
        let h = home();
        let u = h.path();
        for d in [".claude", ".claude-account2"] {
            std::fs::create_dir_all(u.join(d)).unwrap();
            std::fs::write(u.join(d).join("settings.json"), json!({"theme": "dark", "env": {"A": "1"}, "permissions": {"ask": ["Bash(git push *)"], "deny": ["Bash(curl *)"]}}).to_string()).unwrap();
        }
        std::fs::write(u.join(".claude-account2/settings.local.json"), json!({"permissions": {"defaultMode": "default", "allow": ["Bash(ls)"]}}).to_string()).unwrap();
        std::fs::create_dir_all(u.join(".t3/userdata")).unwrap();
        let t3 = json!({"theme": "x", "providerInstances": {"grok": {"driver": "grok"}, "claudeAgent_2": {"driver": "claudeAgent", "config": {"homePath": u.join(".claude-account2")}, "environment": [{"name": "OTHER", "value": "x", "sensitive": false}]}}});
        std::fs::write(u.join(".t3/userdata/settings.json"), t3.to_string()).unwrap();
        std::fs::create_dir_all(u.join(".codex/rules")).unwrap();
        std::fs::write(u.join(".codex/config.toml"), "notify = [\"x\"]\napproval_policy = \"on-request\"\n\n[desktop]\nfollowUpQueueMode = \"steer\"\n").unwrap();
        std::fs::write(u.join(".codex/rules/default.rules"), "prefix_rule(pattern=[\"ego-browser\"], decision=\"allow\")\n").unwrap();
        std::fs::create_dir_all(u.join(".config/opencode")).unwrap();
        std::fs::write(u.join(".config/opencode/opencode.jsonc"), json!({"model": "m", "permission": "allow"}).to_string()).unwrap();
        std::fs::create_dir_all(u.join(".grok/bin")).unwrap();
        std::fs::write(u.join(".grok/config.toml"), "[cli]\nauto_update = true\n\n[ui]\nmax_thoughts_width = 120\nyolo = false\n").unwrap();
        let backups = u.join("backups");
        let hook = crate::approvals::hook_command(Path::new("/A/Heimdall.app/Contents/MacOS/heimdall-emergency"), Path::new("/d"));
        let setup = Setup { user: u, hook_command: Some(hook.clone()), backups: Some(&backups) };

        let check = ensure(&setup, false);
        assert_eq!(check.harnesses(), vec!["Claude Code", "T3 Code", "Codex", "OpenCode", "Grok"], "{}", check.lines());
        assert!(check.drift.iter().all(|d| !d.fixed));
        assert_eq!(read(&u.join(".claude/settings.json"))["theme"], "dark", "a check writes nothing");

        let applied = ensure(&setup, true);
        assert!(applied.errors.is_empty(), "{}", applied.lines());
        assert!(applied.drift.iter().all(|d| d.fixed), "{}", applied.lines());
        for d in [".claude", ".claude-account2"] {
            let s = read(&u.join(d).join("settings.json"));
            assert_eq!(s["permissions"]["defaultMode"], "bypassPermissions");
            assert_eq!(s["skipDangerousModePermissionPrompt"], true);
            assert!(s["permissions"]["ask"].is_null());
            let deny = s["permissions"]["deny"].as_array().unwrap();
            assert_eq!(deny[0], "Bash(curl *)", "User's own rules kept, first");
            assert!(deny.iter().any(|r| r == "AskUserQuestion") && deny.iter().any(|r| r == "Bash(sudo *)"));
            assert_eq!((s["theme"].clone(), s["env"]["A"].clone()), (json!("dark"), json!("1")));
            assert_eq!(s["hooks"]["PermissionRequest"][0]["hooks"][0]["command"], hook.as_str());
            assert!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("hook pretool"));
        }
        let local = read(&u.join(".claude-account2/settings.local.json"));
        assert!(local["permissions"]["defaultMode"].is_null() && local["permissions"]["allow"][0] == "Bash(ls)");
        let t3 = read(&u.join(".t3/userdata/settings.json"));
        assert_eq!(t3["theme"], "x");
        assert_eq!(t3["providerInstances"]["grok"], json!({"driver": "grok"}), "other drivers untouched");
        for id in ["claudeAgent", "claudeAgent_2"] {
            let env = t3["providerInstances"][id]["environment"].as_array().unwrap();
            assert!(env.iter().any(|e| e["name"] == CLAUDE_ENV && e["value"] == "1" && e["sensitive"] == false), "{id}");
        }
        assert_eq!(t3["providerInstances"]["claudeAgent_2"]["environment"][0]["name"], "OTHER");
        let codex = std::fs::read_to_string(u.join(".codex/config.toml")).unwrap();
        assert_eq!(codex, "notify = [\"x\"]\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n\n[desktop]\nfollowUpQueueMode = \"steer\"\n");
        assert!(std::fs::read_to_string(u.join(".codex/rules/default.rules")).unwrap().lines().any(|l| l == CODEX_RM_RULE));
        let oc = read(&u.join(".config/opencode/opencode.jsonc"));
        assert_eq!((oc["model"].clone(), oc["permission"].clone()), (json!("m"), json!({"*": "allow", "question": "deny"})));
        let grok = std::fs::read_to_string(u.join(".grok/config.toml")).unwrap();
        assert_eq!(toml_value(&grok, Some("ui"), "permission_mode").as_deref(), Some("\"always-approve\""));
        assert_eq!(toml_value(&grok, Some("ui"), "yolo").as_deref(), Some("true"));
        assert_eq!(toml_value(&grok, Some("features"), "ask_user_question").as_deref(), Some("false"));
        assert_eq!(toml_value(&grok, Some("cli"), "auto_update").as_deref(), Some("true"));
        assert!(std::fs::read_dir(&backups).unwrap().count() >= 7, "each file backed up before its first rewrite");

        let again = ensure(&setup, true);
        assert!(again.drift.is_empty() && again.errors.is_empty(), "{}", again.lines());
        assert!(again.lines().starts_with("All set:"));
    }

    #[test]
    fn missing_harnesses_are_skipped_and_a_commented_jsonc_is_left_alone() {
        let h = home();
        let u = h.path();
        let r = ensure(&Setup { user: u, hook_command: None, backups: None }, true);
        assert!(r.drift.is_empty() && r.errors.is_empty() && r.checked.is_empty(), "{}", r.lines());
        std::fs::create_dir_all(u.join(".config/opencode")).unwrap();
        let text = "{\n  // mine\n  \"permission\": \"ask\"\n}\n";
        std::fs::write(u.join(".config/opencode/opencode.jsonc"), text).unwrap();
        let r = ensure(&Setup { user: u, hook_command: None, backups: None }, true);
        assert!(r.errors.iter().any(|e| e.contains("not plain JSON")), "{}", r.lines());
        assert_eq!(std::fs::read_to_string(u.join(".config/opencode/opencode.jsonc")).unwrap(), text);
    }

    /// A Bash deny rule is a prefix match, so every one must name a command plainly destructive from its first words.
    #[test]
    fn no_deny_rule_catches_ordinary_deletes() {
        for rule in CLAUDE_DENY.iter().filter(|r| r.starts_with("Bash(")) {
            assert!(rule.contains('*'), "{rule}");
            assert!(!rule.starts_with("Bash(rm"), "{rule}");
        }
        let (s, what) = claude_settings(json!({"permissions": {"deny": ["Bash(rm -rf /)", "Bash(curl *)"]}}), None);
        let deny = s["permissions"]["deny"].as_array().unwrap();
        assert!(!deny.iter().any(|d| d == "Bash(rm -rf /)") && deny[0] == "Bash(curl *)");
        assert!(what.iter().any(|w| w.contains("retired")));
    }

    #[test]
    fn toml_edits_keep_the_rest() {
        assert_eq!(set_toml("a = 1\n", Some("ui"), "yolo", "true"), "a = 1\n\n[ui]\nyolo = true\n");
        assert_eq!(set_toml("[ui]\nyolo = false # old\n[x]\n", Some("ui"), "yolo", "true"), "[ui]\nyolo = true\n[x]\n");
        assert_eq!(set_toml("[x]\nk = 1\n", None, "approval_policy", "\"never\""), "approval_policy = \"never\"\n[x]\nk = 1\n");
        assert_eq!(toml_value("approval_policy = \"never\" # c\n", None, "approval_policy").as_deref(), Some("\"never\""));
        assert_eq!(toml_value("[t]\n# yolo = true\n", Some("t"), "yolo"), None);
        // Review 2 Nov: a header with a comment or spaces is the same table, never a second one.
        assert_eq!(set_toml("[ ui ] # display\nyolo = false\n", Some("ui"), "yolo", "true"), "[ ui ] # display\nyolo = true\n");
        assert_eq!(header_name("[[marketplace.sources]]"), None);
    }

    #[test]
    fn a_codex_profile_cannot_bring_prompts_back() {
        let h = home();
        let u = h.path();
        std::fs::create_dir_all(u.join(".codex")).unwrap();
        std::fs::write(u.join(".codex/config.toml"), "profile = \"w\"\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n\n[profiles.w]\napproval_policy = \"on-request\"\nmodel = \"m\"\n").unwrap();
        let r = ensure(&Setup { user: u, hook_command: None, backups: None }, true);
        assert!(r.drift.iter().any(|d| d.what.contains("[profiles.w] approval_policy")), "{}", r.lines());
        let t = std::fs::read_to_string(u.join(".codex/config.toml")).unwrap();
        assert_eq!(toml_value(&t, Some("profiles.w"), "approval_policy").as_deref(), Some("\"never\""));
        assert_eq!(toml_value(&t, Some("profiles.w"), "model").as_deref(), Some("\"m\""));
    }
}
