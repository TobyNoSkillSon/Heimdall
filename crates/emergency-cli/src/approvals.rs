//! Claude Code's permission prompts and Heimdall's deny list (docs/harness-autonomy.md, docs/t3-orchestration.md,
//! "Approvals").
//!
//! no agent on this Mac stops to ask. Every harness is set to its most autonomous mode
//! (`harness.rs`), and Claude Code runs in `bypassPermissions`. Two hooks in each Claude config cover what that mode
//! leaves:
//! - `hook pretool` (`PreToolUse`) runs before every tool call in every mode and refuses only what is on the deny list
//!   (`deny.rs`: deleting outside the Vault and scratch, disk formatting, the Keychain, system settings). A refusal is an
//!   answer the agent reads at once, so nothing waits; anything else it leaves alone.
//! - `hook permission` (`PermissionRequest`) answers every prompt Claude would show: approve, unless the deny list
//!   refuses it. Claude still asks through T3 for a few checks bypass mode keeps (a critical-path `rm`, a command its
//!   parser cannot trace, such as a here-document, "Dangerous rm operation not ruled out"); T3 shows a card nobody can
//!   click, and the hook answers first, in milliseconds. It answers every session: the user's own threads, Heimdall's,
//!   their `delegate_task` children at any depth (bug note 2026-11-02T22-45-25: a nested child got no answer) and
//!   sessions outside T3. Two exceptions: a question (`AskUserQuestion`) is left to the harness, and a background call's
//!   one-call thread (`[heimdall-bg]`, no tools by design, `app::background_t3`) is declined so it stops at once.
//!
//! The user's click on an ask card that grants an exact delete or a login item's removal (`grants.rs`) lets exactly that
//! through both hooks for a short time; an agent's call into the grant folder is refused first.
//!
//! `record` and `record_threads` still note Heimdall's launches and threads: the hook uses the launches only to find
//! background calls, and the T3 client's tests keep the record.
use crate::grants;
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

/// Digests of the first messages of the threads Heimdall launched, one hex line each, under the data folder.
pub const LAUNCHES: &str = "approvals/launched";
/// The same for background calls' one-call threads (`app::background_call::t3`): their own list, so many island calls a
/// day never push a long-running task out of `LAUNCHES` (review 30 Oct). Their prompts get the same answers.
pub const BACKGROUND_LAUNCHES: &str = "approvals/launched-background";
/// T3 thread ids of Heimdall's own threads (its launches, and the task and relay threads it follows), one per line.
pub const THREADS: &str = "approvals/threads";
/// The environment variable T3 sets for the provider it runs a thread in (the hook inherits it from Claude).
pub const THREAD_ENV: &str = "T3CODE_THREAD_ID";
/// A background call's first message (and thread title) starts with this.
pub const BACKGROUND_PREFIX: &str = "[heimdall-bg]";
/// The hook's decisions, one line each (time, decision, tool, what), under the data folder.
pub const LOG: &str = "logs/approvals.log";
/// The file keeps the newest this many launches.
const KEEP: usize = 2000;

/// What the T3 client records and the hook looks up: SHA-256 of the message as Claude receives it (T3 trims it).
pub fn digest(message: &str) -> String {
    use sha2::Digest;
    let text = message.replace("\r\n", "\n");
    let hash = sha2::Sha256::digest(text.trim().as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Records one launch's first message (before the launch call, so the thread's first prompt finds it). The folder is
/// 0700 and the file 0600; past `2 * KEEP` lines it is rewritten with the newest `KEEP`.
pub fn record(data: &Path, message: &str) -> std::io::Result<()> {
    append(&data.join(if message.trim_start().starts_with(BACKGROUND_PREFIX) { BACKGROUND_LAUNCHES } else { LAUNCHES }), &[digest(message)])
}

/// Records T3 thread ids as Heimdall's (`THREADS`); ids already there are not added again.
pub fn record_threads(data: &Path, threads: &[&str]) -> std::io::Result<()> {
    let path = data.join(THREADS);
    let known = std::fs::read_to_string(&path).unwrap_or_default();
    let mut new: Vec<String> = vec![];
    for t in threads.iter().map(|t| t.trim()).filter(|t| !t.is_empty() && !t.contains('\n')) {
        if !known.lines().any(|l| l == t) && !new.iter().any(|n| n == t) { new.push(t.to_owned()); }
    }
    if new.is_empty() { return Ok(()); }
    append(&path, &new)
}

/// Appends lines to one of the record files (folder 0700, file 0600), keeping the newest `KEEP` past `2 * KEEP`.
fn append(path: &Path, add: &[String]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = path.parent().expect("launches file has a folder");
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut file = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(&path)?;
    file.write_all(add.iter().map(|l| format!("{l}\n")).collect::<String>().as_bytes())?;
    drop(file);
    let text = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > 2 * KEEP {
        let tmp = dir.join(format!(".launched.{}", std::process::id()));
        let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        out.write_all((lines[lines.len() - KEEP..].join("\n") + "\n").as_bytes())?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(())
}

/// Whether Heimdall launched the thread whose first message has this digest.
pub fn launched(data: &Path, digest: &str) -> bool {
    [LAUNCHES, BACKGROUND_LAUNCHES].iter().any(|f| std::fs::read_to_string(data.join(f)).is_ok_and(|t| t.lines().any(|l| l.trim() == digest)))
}

/// The parent of a thread T3's `delegate_task` made: its id is `thread:delegated-task:` and the encoded command id
/// `command:<parent thread id>:delegate-task:<task id>` (seen live 2 Nov in T3 0.1.6's database and in the children's
/// `T3CODE_THREAD_ID`). A parent that is itself a delegate child stays encoded inside, so one decode per level.
pub fn delegate_parent(thread: &str) -> Option<String> {
    let encoded = thread.strip_prefix("thread:delegated-task:")?;
    let command = percent_decode(encoded)?;
    let (parent, _task) = command.strip_prefix("command:")?.rsplit_once(":delegate-task:")?;
    (!parent.is_empty()).then(|| parent.to_owned())
}
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a T3 thread is in one of Heimdall's trees: the thread or the parent of any `delegate_task` level above it
/// is recorded in `THREADS`.
pub fn heimdall_thread(data: &Path, thread: &str) -> bool {
    let known = std::fs::read_to_string(data.join(THREADS)).unwrap_or_default();
    let mut id = Some(thread.trim().to_owned());
    for _ in 0..8 {
        let Some(t) = id.take().filter(|t| !t.is_empty()) else { return false };
        if known.lines().any(|l| l.trim() == t) { return true; }
        id = delegate_parent(&t);
    }
    false
}

/// The first user message in a Claude transcript (JSONL): the thread's first message as T3 delivered it.
pub fn first_message(transcript: &Path) -> Option<String> {
    let file = std::fs::File::open(transcript).ok()?;
    for line in std::io::BufReader::new(file).lines().take(400) {
        let Ok(v) = serde_json::from_str::<Value>(&line.ok()?) else { continue };
        if v["type"] != "user" || v["isMeta"] == true { continue; }
        let content = &v["message"]["content"];
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect(),
            _ => continue,
        };
        return Some(text);
    }
    None
}

/// The hook's answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

const BACKGROUND: &str = "Heimdall's approval hook: a background call answers with JSON only and uses no tools.";

/// What the permission hook answers for one prompt, or None to leave it to the harness (a question for the user).
/// `vault` is Heimdall's home (the Vault root), `user` the user's home folder, `cwd` the session's folder.
pub fn decide(tool: &str, input: &Value, vault: &Path, user: &Path, cwd: Option<&Path>, background: bool) -> Option<Decision> {
    if tool == "AskUserQuestion" { return None; }
    if background { return Some(Decision::Deny(BACKGROUND.into())); }
    Some(match crate::deny::refuse(tool, input, vault, user, cwd) {
        Some(reason) => Decision::Deny(reason),
        None => Decision::Allow,
    })
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/empty"))
}

/// `heimdall-emergency hook permission`: reads Claude's PermissionRequest input on stdin and prints the answer. Every
/// answer is logged. `home` is Heimdall's home folder (the Vault root).
pub fn hook(data: &Path, home: &Path, stdin: &str) -> String {
    hook_for(data, home, &user_home(), stdin)
}
/// `hook` with the user's home folder given (tests).
pub fn hook_for(data: &Path, vault: &Path, user: &Path, stdin: &str) -> String {
    let Ok(input) = serde_json::from_str::<Value>(stdin) else { return String::new() };
    if input["hook_event_name"] != "PermissionRequest" { return String::new(); }
    let first = input["transcript_path"].as_str().and_then(|p| first_message(Path::new(p)));
    let background = first.is_some_and(|f| f.trim_start().starts_with(BACKGROUND_PREFIX));
    let tool = input["tool_name"].as_str().unwrap_or("?");
    let cwd = input["cwd"].as_str().map(Path::new);
    let Some(mut decision) = decide(tool, &input["tool_input"], vault, user, cwd, background) else { return String::new() };
    if !background {
        if grants::touches(tool, &input["tool_input"], data, cwd) {
            decision = Decision::Deny(grants::TOUCHES.into());
        } else if let Decision::Deny(_) = decision {
            if let Some(granted) = granted(data, tool, &input["tool_input"], vault, user, cwd) {
                decision = match granted { Ok(_) => Decision::Allow, Err(why) => Decision::Deny(why) };
            }
        }
    }
    log(data, &decision, tool, what(&input["tool_input"]));
    let decision = match decision {
        Decision::Allow => json!({"behavior": "allow"}),
        Decision::Deny(message) => json!({"behavior": "deny", "message": message}),
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}).to_string()
}

/// `heimdall-emergency hook pretool`: Claude's PreToolUse hook. Refuses what the deny list names (logged), prints
/// nothing for anything else, so the permission mode and rules decide as usual.
pub fn pretool(data: &Path, home: &Path, stdin: &str) -> String {
    pretool_for(data, home, &user_home(), stdin)
}
/// `pretool` with the user's home folder given (tests).
pub fn pretool_for(data: &Path, vault: &Path, user: &Path, stdin: &str) -> String {
    let Ok(input) = serde_json::from_str::<Value>(stdin) else { return String::new() };
    if input["hook_event_name"] != "PreToolUse" { return String::new(); }
    let tool = input["tool_name"].as_str().unwrap_or("?");
    let cwd = input["cwd"].as_str().map(Path::new);
    let tool_input = &input["tool_input"];
    let deny = |reason: String| {
        log(data, &Decision::Deny(reason.clone()), tool, what(tool_input));
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}).to_string()
    };
    if grants::touches(tool, tool_input, data, cwd) { return deny(grants::TOUCHES.into()); }
    let Some(reason) = crate::deny::refuse(tool, tool_input, vault, user, cwd) else { return String::new() };
    match granted(data, tool, tool_input, vault, user, cwd) {
        None => deny(reason),
        Some(Err(why)) => deny(why),
        Some(Ok(cards)) => {
            log(data, &Decision::Allow, tool, &format!("[User's yes on {}] {}", cards.join(", "), what(tool_input)));
            let reason = format!("User clicked yes on ask card {}, which named exactly this.", cards.join(", "));
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": reason}}).to_string()
        }
    }
}

/// A call the deny list refuses, judged again with the user's live grants: None when no grant covers every refusal in it,
/// else the scopes used (the cards they came from) or why they could not be.
fn granted(data: &Path, tool: &str, input: &Value, vault: &Path, user: &Path, cwd: Option<&Path>) -> Option<Result<Vec<String>, String>> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    granted_at(data, tool, input, vault, user, cwd, now)
}
fn granted_at(data: &Path, tool: &str, input: &Value, vault: &Path, user: &Path, cwd: Option<&Path>, now_ms: u64) -> Option<Result<Vec<String>, String>> {
    // Review 4 Nov: beside the delete or the removal, only a few read-only programs; nothing that sets HOME.
    if tool == "Bash" {
        let command = input["command"].as_str().unwrap_or("");
        let programs = crate::deny::programs(command);
        const BESIDE: [&str; 8] = ["rm", "rmdir", "unlink", "du", "ls", "stat", "echo", "test"];
        let removal = programs == ["osascript"];
        if command.contains("HOME=") || !(removal || programs.iter().all(|p| BESIDE.contains(&p.as_str()))) { return None; }
    }
    let ledger = grants::Ledger::load(data, &format!("{tool}\n{input}"), now_ms);
    if ledger.is_empty() { return None; }
    let used = std::cell::RefCell::new(Vec::<grants::Scope>::new());
    let covered = |s: &grants::Scope| { let ok = ledger.covers(s); if ok { used.borrow_mut().push(s.clone()); } ok };
    if crate::deny::refuse_with(tool, input, vault, user, cwd, &covered).is_some() { return None; }
    let used = used.into_inner();
    if used.is_empty() { return None; }
    Some(ledger.consume(&used))
}

fn what(input: &Value) -> &str {
    input["command"].as_str().or(input["file_path"].as_str()).or(input["path"].as_str()).unwrap_or("")
}
fn log(data: &Path, decision: &Decision, tool: &str, what: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let path = data.join(LOG);
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let what: String = what.replace('\n', " ⏎ ").chars().take(300).collect();
    let verdict = if *decision == Decision::Allow { "approved" } else { "declined" };
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path) {
        let _ = writeln!(f, "{secs} {verdict} {tool}: {what}");
    }
}

/// The permission hook's entry in Claude's user settings (`~/.claude/settings.json`), for the bundled CLI at `exe`.
pub fn hook_command(exe: &Path, data: &Path) -> String {
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    format!("{} hook permission --data-dir {}", quote(exe), quote(data))
}
/// The PreToolUse entry's command for the same CLI: `hook_command` with the other verb.
pub fn pretool_command(permission: &str) -> String {
    permission.replacen(" hook permission ", " hook pretool ", 1)
}
/// Whether a settings hook entry is one of Heimdall's (any build, any data folder, either verb).
fn ours(entry: &Value) -> bool {
    entry["hooks"].as_array().into_iter().flatten().any(|h| h["command"].as_str().is_some_and(|c| {
        ["hook permission", "hook pretool"].iter().any(|v| c.contains(&format!("heimdall-emergency' {v}")) || c.contains(&format!("heimdall-emergency {v}")))
    }))
}
/// Claude settings with Heimdall's two hooks (`PermissionRequest`: `command`; `PreToolUse`: its `pretool_command`) in
/// place of any earlier copy (`command: None` removes both). Everything else is kept.
pub fn with_hook(mut settings: Value, command: Option<&str>) -> Value {
    if !settings.is_object() { settings = json!({}); }
    if !settings["hooks"].is_object() { settings["hooks"] = json!({}); }
    for (event, cmd) in [("PermissionRequest", command.map(str::to_owned)), ("PreToolUse", command.map(pretool_command))] {
        let list = settings["hooks"][event].as_array().cloned().unwrap_or_default();
        let mut list: Vec<Value> = list.into_iter().filter(|e| !ours(e)).collect();
        if let Some(cmd) = cmd {
            list.push(json!({"matcher": "*", "hooks": [{"type": "command", "command": cmd, "timeout": 10}]}));
        }
        let hooks = settings["hooks"].as_object_mut().unwrap();
        if list.is_empty() { hooks.remove(event); } else { hooks.insert(event.into(), Value::Array(list)); }
    }
    if settings["hooks"].as_object().is_some_and(|h| h.is_empty()) { settings.as_object_mut().unwrap().remove("hooks"); }
    settings
}
/// Writes `with_hook` into a Claude settings file (atomic; the file's mode is kept, 0600 when new). Returns whether it
/// changed.
pub fn install(settings_file: &Path, command: Option<&str>) -> Result<bool, String> {
    rewrite_json(settings_file, |old| with_hook(old, command))
}
/// Rewrites a Claude JSON file through `change` (atomic; the file's mode is kept, 0600 when new; a symlinked file is
/// written where it points). Returns whether it changed. The hook's settings and the MCP entry (`mcp`) share it.
pub fn rewrite_json(settings_file: &Path, change: impl FnOnce(Value) -> Value) -> Result<bool, String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // A settings file linked from a dotfiles folder: write the file it points at, keep the link.
    let linked = std::fs::canonicalize(settings_file).ok().filter(|_| settings_file.is_symlink());
    let settings_file = linked.as_deref().unwrap_or(settings_file);
    let old = match std::fs::read_to_string(settings_file) {
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|e| format!("{} is not JSON ({e}); not changed", settings_file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", settings_file.display())),
    };
    let new = change(old.clone());
    if new == old { return Ok(false); }
    let mode = std::fs::metadata(settings_file).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let dir = settings_file.parent().ok_or("settings file has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let tmp = dir.join(format!(".settings.heimdall.{}.{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let text = serde_json::to_string_pretty(&new).map_err(|e| e.to_string())? + "\n";
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, settings_file).map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> PathBuf { PathBuf::from("/Users/t/Work") }
    fn user() -> PathBuf { PathBuf::from("/Users/t") }
    fn transcript(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        let lines = [json!({"type":"queue-operation"}), json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":text}]}}), json!({"type":"user","message":{"content":"later"}})];
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        p
    }
    fn ask(data: &Path, t: &Path, tool: &str, input: Value) -> Value {
        let stdin = json!({"hook_event_name":"PermissionRequest","transcript_path":t,"cwd":"/Users/t/Work/Projects/Site","tool_name":tool,"tool_input":input}).to_string();
        serde_json::from_str(&hook_for(data, &vault(), &user(), &stdin)).unwrap_or(Value::Null)
    }

    /// Bug note 2026-11-02T22-45-25 and the 2 Nov approvals.log: a heredoc append, a chained du && rm && ls, a pkill by
    /// pattern, a sed with escapes, an edit outside the Vault: all approved, in any session (the user's own, a nested
    /// delegate child Heimdall never recorded, a session outside T3).
    #[test]
    fn every_session_gets_an_approval_for_anything_off_the_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let child = transcript(dir.path(), "child.jsonl", "Act as the implementation sub-agent for this task.");
        let owner = transcript(dir.path(), "owner.jsonl", "User's own session");
        for t in [&child, &owner, &dir.path().join("missing.jsonl")] {
            for (tool, input) in [
                ("Bash", json!({"command": "cat >> /Users/t/Work/Assistant/Research/x.md <<'EOF'\n## Update\nEOF"})),
                ("Bash", json!({"command": "du -sh /Users/t/Work/Assistant/Temp/x && rm -rf /Users/t/Work/Assistant/Temp/x && ls /Users/t/Work/Assistant/Temp"})),
                ("Bash", json!({"command": "pkill -f 'yt-dlp.*IGICYFyP_EI'; sleep 1"})),
                ("Bash", json!({"command": "rm -rf build dist"})),
                ("Write", json!({"file_path": "/Users/t/elsewhere/notes.txt"})),
                ("mcp__t3-code__t3_project_delete", json!({})),
                ("ExitPlanMode", json!({})),
            ] {
                let out = ask(&data, t, tool, input.clone());
                assert_eq!(out["hookSpecificOutput"]["decision"]["behavior"], "allow", "{tool} {input}");
                assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
            }
        }
        let log = std::fs::read_to_string(data.join(LOG)).unwrap();
        assert!(log.contains("approved Bash: cat >> ") && log.contains(" ⏎ "), "{log}");
    }

    #[test]
    fn the_deny_list_is_declined_with_why_and_questions_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let t = transcript(dir.path(), "t.jsonl", "any");
        for c in ["rm -rf ~/Documents", "diskutil eraseDisk APFS X disk4", "security find-generic-password -s x -w", "launchctl bootout gui/501/x", "sudo true"] {
            let out = ask(&data, &t, "Bash", json!({"command": c}));
            assert_eq!(out["hookSpecificOutput"]["decision"]["behavior"], "deny", "{c}");
            assert!(out["hookSpecificOutput"]["decision"]["message"].as_str().unwrap().contains("deny list"), "{c}");
        }
        assert_eq!(ask(&data, &t, "AskUserQuestion", json!({})), Value::Null, "a question stays the harness's");
        assert_eq!(hook_for(&data, &vault(), &user(), "not json"), "");
        assert_eq!(hook_for(&data, &vault(), &user(), &json!({"hook_event_name":"PreToolUse","transcript_path":t}).to_string()), "");
        assert!(std::fs::read_to_string(data.join(LOG)).unwrap().contains("declined Bash: rm -rf ~/Documents"));
    }

    /// A background call's one-call thread uses no tools (`app::background_t3`): its prompt is declined so it stops.
    #[test]
    fn a_background_call_is_still_declined() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let t = transcript(dir.path(), "bg.jsonl", &format!("{BACKGROUND_PREFIX} goals shadow"));
        let out = ask(&data, &t, "Bash", json!({"command": "ls"}));
        assert_eq!(out["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    /// The PreToolUse guard refuses the deny list in every mode, bypassPermissions included, and says nothing otherwise.
    #[test]
    fn the_pretool_guard_refuses_only_the_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let input = |c: &str| json!({"hook_event_name":"PreToolUse","cwd":"/Users/t/Work","tool_name":"Bash","tool_input":{"command":c}}).to_string();
        let out: Value = serde_json::from_str(&pretool_for(&data, &vault(), &user(), &input("cd /tmp && rm -rf ~"))).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(out["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("deny list"));
        for c in ["cat > /tmp/x <<EOF\nhi\nEOF", "rm -rf Projects/Site/build", "git status"] {
            assert_eq!(pretool_for(&data, &vault(), &user(), &input(c)), "", "{c}");
        }
        assert_eq!(pretool_for(&data, &vault(), &user(), "{}"), "");
    }

    fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 }
    fn pre(data: &Path, c: &str) -> Value {
        let stdin = json!({"hook_event_name":"PreToolUse","cwd":"/Users/t/Work/Projects/Heimdall","tool_name":"Bash","tool_input":{"command":c}}).to_string();
        serde_json::from_str(&pretool_for(data, &vault(), &user(), &stdin)).unwrap_or(Value::Null)
    }
    fn verdict(v: &Value) -> &str { v["hookSpecificOutput"]["permissionDecision"].as_str().unwrap_or("none") }

    /// Bug note 2026-11-03T23-04-19: the user clicked "Delete nightly" on a card naming exact paths; the hook lets an rm of
    /// exactly those through, once each, and refuses every other delete as before.
    #[test]
    fn a_clicked_grant_lets_exactly_the_named_delete_through_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let nightly = "/Users/t/Backups/T3 Code (Nightly).app";
        assert_eq!(verdict(&pre(&data, &format!("rm -rf '{nightly}'"))), "deny", "no click, no grant");
        grants::write(&data, &user(), &vault(), "ask-7", "q1", "Delete nightly", &[nightly.into(), "~/Library/Application Support/t3code".into()], &[], now()).unwrap();
        for c in [
            "rm -rf /Users/t/Backups".to_string(), "rm -rf /Users/t/Backups/*".into(), "rm -rf /Users/t/Backups/T3*".into(),
            format!("rm -rf '{nightly}/Contents'"), format!("rm -rf '{nightly}' ~/Documents"), "rm -rf ~/Documents/old".into(),
            "rm -rf ~/Backups/$X".into(), "rm -rf ~/Library/Application\\ Support".into(), format!("rm -rf '{nightly}' && diskutil eraseDisk APFS X disk4"),
            // Review 4 Nov: only a plain rm of the absolute path; find and rsync reach past it, a cd or a slash changes it.
            format!("find -L '{nightly}' -delete"), format!("find '{nightly}' -exec rm -rf {{}} +"), format!("rsync -a --delete /tmp/empty/ '{nightly}'"),
            format!("rm -rf '{nightly}/'"), "cd ~/Backups && rm -rf 'T3 Code (Nightly).app'".into(), "cd ~/Library && rm -rf 'Application Support/t3code'".into(),
            // Second review: no . or .. or //, nothing but read-only programs beside it, no HOME, no rmdir -p.
            "rm -rf '/Users/t/Backups/./T3 Code (Nightly).app'".into(), "rm -rf '/Users/t//Backups/T3 Code (Nightly).app'".into(),
            format!("rm -rf '{nightly}' && cp -R ~/Documents /tmp/x"), format!("HOME=/Users/t rm -rf '{nightly}'"), format!("rmdir -p '{nightly}'"),
            format!("bash -c \"rm -rf '{nightly}'\""),
        ] {
            assert_eq!(verdict(&pre(&data, &c)), "deny", "{c}");
        }
        let c = format!("du -sh '{nightly}' && rm -rf '{nightly}'");
        let out = pre(&data, &c);
        assert_eq!(verdict(&out), "allow", "{out}");
        assert!(out["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("ask-7"));
        assert_eq!(verdict(&pre(&data, &c)), "allow", "the same command may repeat at once (the permission hook, a retry)");
        assert_eq!(verdict(&pre(&data, &format!("rm -r '{nightly}'"))), "deny", "used: another command needs another yes");
        assert_eq!(verdict(&pre(&data, "rm -rf ~/Library/Application\\ Support/t3code")), "deny", "only from /");
        assert_eq!(verdict(&pre(&data, "rm -rf '/Users/t/Library/Application Support/t3code'")), "allow", "the other path is its own scope");
        let ask = |c: &str| {
            let stdin = json!({"hook_event_name":"PermissionRequest","cwd":"/Users/t","tool_name":"Bash","tool_input":{"command":c}}).to_string();
            serde_json::from_str::<Value>(&hook_for(&data, &vault(), &user(), &stdin)).unwrap()["hookSpecificOutput"]["decision"]["behavior"].clone()
        };
        assert_eq!(ask(&c), "allow", "the permission hook honours the same grant for the same command");
        assert_eq!(ask("rm -rf ~/Documents"), "deny");
        assert!(std::fs::read_to_string(data.join(LOG)).unwrap().contains("[User's yes on ask-7]"));
    }

    #[test]
    fn an_expired_grant_or_one_the_agent_wrote_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let target = "/Users/t/Backups/old";
        grants::write(&data, &user(), &vault(), "ask-1", "q1", "Delete", &[target.into()], &[], now() - grants::LIFETIME_MS - 1).unwrap();
        assert_eq!(verdict(&pre(&data, &format!("rm -rf {target}"))), "deny", "expired");
        // The agent cannot write into the grant folder by any tool, and a file it planted anyway is not signed.
        let g = data.join(grants::DIR);
        for c in [format!("echo '{{}}' > {}/mine.json", g.display()), format!("cp /tmp/x.json {}/", g.display()), format!("cat {}/key", g.display())] {
            let out = pre(&data, &c);
            assert_eq!(verdict(&out), "deny", "{c}");
            assert_eq!(out["hookSpecificOutput"]["permissionDecisionReason"], grants::TOUCHES);
        }
        let write = json!({"hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":g.join("mine.json"),"content":"{}"}}).to_string();
        assert_eq!(verdict(&serde_json::from_str(&pretool_for(&data, &vault(), &user(), &write)).unwrap()), "deny");
        let forged = json!({"card":"ask-9","option":"Yes","clickedMs":now(),"expiresMs":now() + 60_000,"delete":[target],"loginItems":[],"mac":"0".repeat(64)});
        std::fs::write(g.join("forged.json"), forged.to_string()).unwrap();
        assert_eq!(verdict(&pre(&data, &format!("rm -rf {target}"))), "deny", "forged");
    }

    /// Bug note 2026-11-03T18-04-11: reading the login items needs no yes; removing one needs his click naming it.
    #[test]
    fn login_items_read_freely_and_a_named_one_is_removed_after_a_click() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        for c in [
            "osascript -e 'tell application \"System Events\" to get the name of every login item'",
            "osascript -e 'tell application \"System Events\" to get properties of every login item'",
            "osascript -e 'tell application \"System Events\"' -e 'set names to name of every login item' -e 'end tell'",
            "launchctl list | grep -i arsenal", "ls ~/Library/LaunchAgents",
        ] {
            assert_eq!(pre(&data, c), Value::Null, "{c}");
        }
        let remove = |name: &str| format!("osascript -e 'tell application \"System Events\" to delete login item \"{name}\"'");
        assert_eq!(verdict(&pre(&data, &remove("swiftpm-testing-helper"))), "deny", "no click");
        grants::write(&data, &user(), &vault(), "ask-3", "q1", "Remove it", &[], &["swiftpm-testing-helper".into()], now()).unwrap();
        assert_eq!(verdict(&pre(&data, &remove("Arsenal"))), "deny", "another item");
        assert_eq!(verdict(&pre(&data, "osascript -e 'tell application \"System Events\" to delete every login item'")), "deny");
        assert_eq!(verdict(&pre(&data, "osascript -e 'tell application \"System Events\" to make login item at end with properties {path:\"/A.app\"}'")), "deny");
        assert_eq!(verdict(&pre(&data, &format!("{} -e 'do shell script \"rm -rf ~\"'", remove("swiftpm-testing-helper")))), "deny", "only the removal itself");
        assert_eq!(verdict(&pre(&data, &remove("swiftpm-testing-helper"))), "allow");
        assert_eq!(verdict(&pre(&data, "sfltool dumpbtm")), "deny", "it asks for an admin password");
    }

    #[test]
    fn the_launch_file_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(2 * KEEP + 1) { record(dir.path(), &format!("brief {i}")).unwrap(); }
        assert!(launched(dir.path(), &digest(&format!("brief {}", 2 * KEEP))));
        assert!(!launched(dir.path(), &digest("brief 0")));
        assert_eq!(std::fs::read_to_string(dir.path().join(LAUNCHES)).unwrap().lines().count(), KEEP);
        assert_eq!(digest("a\r\nb\n"), digest("a\nb"));
    }

    #[test]
    fn installing_keeps_every_other_setting_and_replaces_an_older_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let other = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/local/bin/mine"}]});
        std::fs::write(&file, json!({"theme": "dark", "hooks": {"PermissionRequest": [other], "Stop": [{"hooks": []}]}}).to_string()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let old = hook_command(Path::new("/Applications/Old.app/Contents/MacOS/heimdall-emergency"), Path::new("/d"));
        let new = hook_command(Path::new("/Applications/Heimdall.app/Contents/MacOS/heimdall-emergency"), Path::new("/Users/t/Work/.heimdall"));
        assert!(install(&file, Some(&old)).unwrap());
        assert!(install(&file, Some(&new)).unwrap());
        assert!(!install(&file, Some(&new)).unwrap(), "a second install changes nothing");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert!(v["hooks"]["Stop"].is_array());
        let list = v["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], other);
        assert_eq!(list[1]["hooks"][0]["command"], new.as_str());
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "one guard, an older copy replaced");
        assert_eq!(pre[0]["hooks"][0]["command"], pretool_command(&new).as_str());
        assert!(pretool_command(&new).contains("' hook pretool --data-dir '"));
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(install(&file, None).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PermissionRequest"].as_array().unwrap().as_slice(), &[other]);
        assert!(v["hooks"]["PreToolUse"].is_null(), "uninstall takes the guard out too");
        // A settings file linked from a dotfiles folder keeps its link (review 28 Oct).
        let real = dir.path().join("dotfiles-settings.json");
        let link = dir.path().join("linked.json");
        std::fs::write(&real, "{\"theme\":\"dark\"}").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(install(&link, Some(&new)).unwrap());
        assert!(link.is_symlink());
        assert!(std::fs::read_to_string(&real).unwrap().contains("hook permission"));
        std::fs::write(&file, "{ not json").unwrap();
        assert!(install(&file, Some(&new)).is_err(), "a settings file it cannot read is left alone");
    }
}
