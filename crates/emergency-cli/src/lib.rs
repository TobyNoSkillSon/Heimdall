//! `heimdall-emergency`: the relay agent's hands, a T3 thread acting as Heimdall's agent (docs/claude-relay.md): in
//! relay mode, or while Heimdall's own agent is down (docs/emergency.md). Heimdall's MCP server is the one source of
//! its tools: `mcp` serves them to the relay thread as `mcp__heimdall__*`, Pi's names (`mcp.rs`), and `tools` / `call`
//! reach the same server from bash, by the same names, for a harness without the server. Nothing here knows a tool:
//! the list and every answer are the server's. The grant is `<data>/emergency/connection.json` (0600, written only
//! while a relay is on, removed when it ends), and only the thread it names may act.
pub mod approvals;
pub mod deny;
pub mod grants;
pub mod harness;
pub mod mcp;

use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

pub const USAGE: &str = "heimdall-emergency [--data-dir DIR] <command>
  tools                                   every tool Heimdall's agent has, with its arguments (the MCP server's list)
  call <tool> '<json>'                    run one by its MCP name, as Pi's agent does; images come back as file paths
  mcp                                     Heimdall's MCP server over stdio, for the relay thread's Claude (mcp__heimdall__*)
  mcp install|uninstall                   name it in Claude Code's user config (every ~/.claude*.json), or take it out
  hook permission                         Claude Code's PermissionRequest hook: approves all but the deny list (stdin JSON)
  hook pretool                            Claude Code's PreToolUse hook: refuses the deny list in every mode (stdin JSON)
  hook install|uninstall [--settings F]   put both hooks in Claude's user settings (~/.claude/settings.json), or take them out
  harness check|apply                     every agent harness in its no-prompt mode (docs/harness-autonomy.md)
  grants status                           whether User's clicks on ask cards grant exact deletes in this build (read-only)";

/// Heimdall's default data folder (`HEIMDALL_DATA` from the app, else `<home>/.heimdall`).
pub fn default_data() -> PathBuf {
    heimdall_home::data_dir()
}

/// Runs one command as the T3 thread this process runs in (`T3CODE_THREAD_ID`, none outside T3); the exit code (0
/// done, 1 the call failed, 2 refused or misused) and the line to print.
pub fn run(args: &[String], data: Option<PathBuf>) -> (i32, String) {
    let caller = std::env::var(mcp::THREAD_ENV).ok().filter(|t| !t.is_empty());
    run_as(args, data, caller.as_deref())
}
/// `run` as the T3 thread `caller`.
pub fn run_as(args: &[String], data: Option<PathBuf>, caller: Option<&str>) -> (i32, String) {
    let mut args: Vec<String> = args.to_vec();
    let mut data = data.unwrap_or_else(default_data);
    let mut explicit_data = false;
    if let Some(i) = args.iter().position(|a| a == "--data-dir") {
        if i + 1 >= args.len() { return (2, "--data-dir needs a folder".into()); }
        data = PathBuf::from(args.remove(i + 1));
        args.remove(i);
        explicit_data = true;
    }
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") { return (2, USAGE.into()); }
    if args[0] == "hook" { return hook(&args, &data); }
    if args[0] == "harness" { return harness_command(&args, &data); }
    if args[0] == "grants" {
        return match args.get(1).map(String::as_str) {
            Some("status") if args.len() == 2 => (0, grants::status().into()),
            _ => (2, format!("grants: only `grants status`\n\n{USAGE}")),
        };
    }
    if args[0] == "mcp" { return mcp_command(&args, &data, explicit_data); }
    if args[0] == "tools" {
        let connection = match connection(&data) { Ok(c) => c, Err(e) => return (2, e) };
        return match rpc(&connection, "tools/list", json!({})) {
            Ok(v) => (0, tool_list(&v)),
            Err((code, text)) => (code, text),
        };
    }
    let (tool, input) = match request(&args) {
        Ok(r) => r,
        Err(e) => return (2, format!("{e}\n\n{USAGE}")),
    };
    let connection = match mcp::cli_holds(&data, caller) {
        Ok(c) => c,
        Err(e) => return (2, e),
    };
    match call(&connection, &tool, &input, &data.join("emergency/images")) {
        Ok(text) => (0, text),
        Err((code, text)) => (code, text),
    }
}

/// `mcp` (the stdio server), `mcp install` and `mcp uninstall` (Claude Code's user-scope entry).
fn mcp_command(args: &[String], data: &Path, explicit_data: bool) -> (i32, String) {
    match args.get(1).map(String::as_str) {
        None => mcp::serve(data),
        Some(verb @ ("install" | "uninstall")) => {
            let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return (2, "no home folder for Claude's config".into()) };
            let files = mcp::config_files(&home, std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).as_deref());
            let entry = if verb == "install" {
                let exe = match std::env::current_exe() { Ok(e) => e, Err(e) => return (1, format!("this CLI's path is unknown: {e}")) };
                Some(mcp::entry(&exe, data, !explicit_data))
            } else { None };
            mcp::install(&files, entry.as_ref())
        }
        _ => (2, format!("mcp: no argument, install or uninstall\n\n{USAGE}")),
    }
}

/// `hook permission` (needs no relay: it runs inside Claude for every permission prompt), `hook install` and
/// `hook uninstall` (`approvals`).
fn hook(args: &[String], data: &Path) -> (i32, String) {
    let settings = args.iter().position(|a| a == "--settings").and_then(|i| args.get(i + 1)).map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/settings.json")));
    match args.get(1).map(String::as_str) {
        Some("permission") => {
            let mut stdin = String::new();
            if std::io::stdin().take(4 << 20).read_to_string(&mut stdin).is_err() { return (0, String::new()); }
            (0, approvals::hook(data, &heimdall_home::home_dir(), &stdin))
        }
        Some("pretool") => {
            let mut stdin = String::new();
            if std::io::stdin().take(4 << 20).read_to_string(&mut stdin).is_err() { return (0, String::new()); }
            (0, approvals::pretool(data, &heimdall_home::home_dir(), &stdin))
        }
        Some(verb @ ("install" | "uninstall")) => {
            let Some(settings) = settings else { return (2, "no home folder for Claude's settings".into()) };
            let exe = match std::env::current_exe() { Ok(e) => e, Err(e) => return (1, format!("this CLI's path is unknown: {e}")) };
            let command = (verb == "install").then(|| approvals::hook_command(&exe, data));
            match approvals::install(&settings, command.as_deref()) {
                Ok(true) => (0, format!("{} {}: {}", if verb == "install" { "Hook in" } else { "Hook out of" }, settings.display(), command.unwrap_or_default())),
                Ok(false) => (0, format!("{} already {}.", settings.display(), if verb == "install" { "has this hook" } else { "has no Heimdall hook" })),
                Err(e) => (1, e),
            }
        }
        _ => (2, format!("hook: permission, pretool, install or uninstall\n\n{USAGE}")),
    }
}

/// `harness check` (what drifted, nothing written) and `harness apply` (written back, each file backed up under
/// `<data>/harness-backups`). The hooks point at this CLI when it runs from an app bundle.
fn harness_command(args: &[String], data: &Path) -> (i32, String) {
    let apply = match args.get(1).map(String::as_str) {
        Some("check") => false,
        Some("apply") => true,
        _ => return (2, format!("harness: check or apply\n\n{USAGE}")),
    };
    let Some(user) = std::env::var_os("HOME").map(PathBuf::from) else { return (2, "no home folder".into()) };
    let exe = std::env::current_exe().ok();
    let hook_command = exe.as_deref().filter(|e| e.to_string_lossy().contains(".app/Contents/MacOS/")).map(|e| approvals::hook_command(e, data));
    let backups = data.join("harness-backups");
    let report = harness::ensure(&harness::Setup { user: &user, hook_command, backups: Some(&backups) }, apply);
    let code = if !report.errors.is_empty() || (!apply && !report.drift.is_empty()) { 1 } else { 0 };
    (code, report.lines())
}

/// Not in an emergency: the line every refusal starts with.
pub const OFF: &str = "No relay is on, so this CLI is off: it works only for the T3 thread acting as Heimdall's agent (relay mode, or emergency mode while his own agent is down).";

/// The grant: the scoped server, its token, and the thread it was issued for.
pub struct Connection { url: String, token: String, pub thread: String }
impl Connection {
    /// A short digest of the token, to tell grants apart without keeping the token around.
    pub fn token_digest(&self) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(self.token.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}
fn connection(data: &Path) -> Result<Connection, String> {
    let path = data.join("emergency/connection.json");
    let bytes = std::fs::read(&path).map_err(|_| OFF.to_string())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| format!("{OFF} ({} is unreadable)", path.display()))?;
    match (v["url"].as_str(), v["token"].as_str()) {
        (Some(url), Some(token)) if url.starts_with("http://127.0.0.1:") => Ok(Connection { url: url.into(), token: token.into(), thread: v["thread"].as_str().unwrap_or_default().into() }),
        _ => Err(format!("{OFF} ({} is incomplete)", path.display())),
    }
}

/// The MCP tool and arguments for `call <tool> '<json>'`: the tool's own name and arguments, nothing translated.
fn request(args: &[String]) -> Result<(String, Value), String> {
    if args[0] != "call" { return Err(format!("unknown command {}: run `tools`, then `call <tool> '<json>'`", args[0])); }
    let tool = args.get(1).cloned().ok_or("call: which tool (`tools` lists them)")?;
    let input = match args.get(2) { Some(j) => serde_json::from_str(j).map_err(|e| format!("call: the arguments are not JSON ({e})"))?, None => json!({}) };
    Ok((tool, input))
}

/// One `tools/call`: the answer's text with each image written to a private file (its path printed), or the exit code
/// and why it failed.
fn call(c: &Connection, tool: &str, input: &Value, images: &Path) -> Result<String, (i32, String)> {
    let action = format!("cli-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let result = rpc(c, "tools/call", json!({"name":tool,"arguments":input,"_meta":{"heimdallActionId":action}}))?;
    let mut out = Vec::new();
    for (n, item) in result["content"].as_array().into_iter().flatten().enumerate() {
        match item["type"].as_str() {
            Some("text") => out.push(item["text"].as_str().unwrap_or_default().to_owned()),
            Some("image") => {
                use base64::Engine;
                let ext = item["mimeType"].as_str().and_then(|m| m.rsplit('/').next()).unwrap_or("png");
                let bytes = base64::engine::general_purpose::STANDARD.decode(item["data"].as_str().unwrap_or_default()).map_err(|_| (1, "an image in Heimdall's answer was unreadable".to_string()))?;
                let path = images.join(format!("{action}-{n}.{ext}"));
                write_private(&path, &bytes).map_err(|e| (1, format!("an image was not saved: {e}")))?;
                out.push(format!("Image: {}", path.display()));
            }
            _ => {}
        }
    }
    let text = out.join("\n");
    if result["isError"] == true { Err((1, text)) } else { Ok(text) }
}
/// `tools/list` as one line per tool and its arguments' schema.
fn tool_list(result: &Value) -> String {
    result["tools"].as_array().into_iter().flatten().map(|t| {
        let description = t["description"].as_str().unwrap_or_default().split(". ").next().unwrap_or_default().trim_end_matches('.');
        format!("{}: {description}.\n  {}", t["name"].as_str().unwrap_or("?"), t["inputSchema"]["properties"])
    }).collect::<Vec<_>>().join("\n")
}
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?; }
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?.write_all(bytes)
}
/// One JSON-RPC request over loopback HTTP/1.1: its `result`, or the exit code and why it failed.
fn rpc(c: &Connection, method: &str, params: Value) -> Result<Value, (i32, String)> {
    let address = c.url.trim_start_matches("http://").trim_end_matches("/mcp").to_owned();
    let gone = || (2, format!("{OFF} (Heimdall's relay server is gone: the relay ended)"));
    let mut socket = TcpStream::connect(&address).map_err(|_| gone())?;
    let _ = socket.set_read_timeout(Some(Duration::from_secs(120)));
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let head = format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", c.token, body.len());
    socket.write_all(format!("{head}{body}").as_bytes()).map_err(|_| gone())?;
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).map_err(|e| (1, format!("No answer from Heimdall: {e}")))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or((1, "Heimdall's answer was cut off".to_string()))?;
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    match status {
        200 => {}
        401 => return Err((2, format!("{OFF} (the relay's token was revoked)"))),
        503 => return Err((2, "Heimdall's Agent switch is off, so the relay agent cannot act.".into())),
        s => return Err((1, format!("Heimdall answered HTTP {s}"))),
    }
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { unchunk(body) } else { body.to_owned() };
    let v: Value = serde_json::from_str(body.trim()).map_err(|_| (1, "Heimdall's answer was not JSON".to_string()))?;
    if let Some(e) = v["error"]["message"].as_str() { return Err((1, e.into())); }
    Ok(v["result"].clone())
}
fn unchunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else { break };
        if n == 0 || tail.len() < n { break; }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn args(a: &[&str]) -> Vec<String> { a.iter().map(|s| s.to_string()).collect() }

    /// The integrator's check against the installed app: read-only, no relay needed, one line.
    #[test]
    fn grants_status_prints_one_line_and_needs_no_relay() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_as(&args(&["grants", "status"]), Some(dir.path().into()), None), (0, grants::status().to_string()));
        assert_eq!(grants::status(), if cfg!(feature = "public") { "grants: off" } else { "grants: on" });
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "writes nothing");
        assert_eq!(run_as(&args(&["grants"]), Some(dir.path().into()), None).0, 2);
        assert_eq!(run_as(&args(&["grants", "status", "x"]), Some(dir.path().into()), None).0, 2);
    }

    #[test]
    fn without_emergency_mode_every_command_refuses_and_says_why() {
        let dir = std::env::temp_dir().join(format!("heimdall-emergency-cli-{}", std::process::id()));
        for cmd in [&["tools"][..], &["call", "emergency_status"], &["call", "speak", "{\"text\":\"x\"}"]] {
            let (code, out) = run_as(&args(cmd), Some(dir.clone()), None);
            assert_eq!(code, 2);
            assert!(out.starts_with(OFF), "{out}");
        }
        // A connection left behind by a crash points at a server that is gone.
        std::fs::create_dir_all(dir.join("emergency")).unwrap();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", free.local_addr().unwrap());
        drop(free);
        std::fs::write(dir.join("emergency/connection.json"), json!({"url":url,"token":"t","thread":"t-relay"}).to_string()).unwrap();
        let (code, out) = run_as(&args(&["--data-dir", dir.to_str().unwrap(), "call", "emergency_status"]), None, Some("t-relay"));
        assert_eq!(code, 2);
        assert!(out.contains("the relay ended"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The CLI knows no tool: `call` passes the MCP name and arguments as they are, so a tool added to Heimdall's
    /// server is in the CLI at once. The shortcuts it had (speak, notice, plan, ...) are gone.
    #[test]
    fn call_passes_the_tool_and_its_json_through_and_nothing_else_is_a_command() {
        assert_eq!(request(&args(&["call", "t3_call", "{\"tool\":\"t3_project_list\"}"])).unwrap(), ("t3_call".into(), json!({"tool":"t3_project_list"})));
        assert_eq!(request(&args(&["call", "emergency_status"])).unwrap(), ("emergency_status".into(), json!({})));
        for retired in ["speak", "notice", "island", "island-remove", "plan", "bug", "tasks", "task", "threads", "read", "send", "status", "end"] {
            assert!(request(&args(&[retired, "x"])).unwrap_err().starts_with(&format!("unknown command {retired}")), "{retired}");
        }
        assert!(request(&args(&["call"])).is_err() && request(&args(&["call", "speak", "not json"])).is_err());
    }

    /// The exact HTTP the scoped server takes (crates/agent/tests/mcp_scoped.rs serves it for real).
    #[test]
    fn a_call_sends_the_bearer_and_protocol_headers_and_prints_the_text() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut req = String::new();
            let mut buf = vec![0u8; 65536];
            while !req.ends_with('}') { let n = s.read(&mut buf).unwrap(); if n == 0 { break; } req.push_str(&String::from_utf8_lossy(&buf[..n])); }
            let body = json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Speech queued: 3."}]}}).to_string();
            write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
            req
        });
        let dir = std::env::temp_dir().join(format!("heimdall-emergency-cli-call-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("emergency")).unwrap();
        std::fs::write(dir.join("emergency/connection.json"), json!({"url":format!("http://{address}/mcp"),"token":"tok-1","thread":"t"}).to_string()).unwrap();
        assert_eq!(run_as(&args(&["call", "speak", "{\"text\":\"Covering for Heimdall.\"}"]), Some(dir.clone()), Some("t")), (0, "Speech queued: 3.".into()));
        let req = handle.join().unwrap();
        for h in ["POST /mcp HTTP/1.1", &format!("Host: {address}"), "Authorization: Bearer tok-1", "MCP-Protocol-Version: 2025-06-18", "Accept: application/json, text/event-stream", "\"name\":\"speak\"", "heimdallActionId"] {
            assert!(req.contains(h), "{h} in {req}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
