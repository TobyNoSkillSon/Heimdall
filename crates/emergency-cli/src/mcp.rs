//! `heimdall-emergency mcp`: Heimdall's MCP server for the relay thread (docs/claude-relay.md, "How the thread reaches
//! the tools"). T3 offers no way to give one thread an MCP server, so Claude Code's user settings name this stdio proxy
//! as the `heimdall` server, for every session. Each session runs its own copy with T3's `T3CODE_THREAD_ID`, and the
//! proxy serves Heimdall's tools only to the thread the relay grant names (`connection.json`): the relay sees
//! `mcp__heimdall__*`, the same names, schemas and answers (images inline) as Pi's agent, because both are the main
//! server's `tools/list` and dispatch. Any other session sees no tools and is refused if it calls one anyway.
use crate::{connection, rpc, Connection};
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// The server's name in Claude's settings: tools appear as `mcp__heimdall__<tool>`, Pi's names.
pub const NAME: &str = "heimdall";
/// T3's id for the thread a Claude session runs in, in the environment of every process it starts.
pub const THREAD_ENV: &str = "T3CODE_THREAD_ID";
/// The grant's thread before T3 has answered the launch with the thread's id (`Emergency::issue("pending")`).
pub const PENDING: &str = "pending";
/// The rule for every session that has the server, sent as the server's instructions (Claude puts them in the system
/// prompt) and written into Heimdall's task rules (`prompt/worker.md`) and the relay prompt (`prompt/relay.md`).
pub const RULE: &str = "Heimdall's own tools (mcp__heimdall__*) belong to the T3 thread acting as Heimdall's agent (the relay). \
In any other thread or sub-agent, leave them alone even when they are listed: use one only when the user directly asks \
you to act through Heimdall, or when your thread's first message is Heimdall's relay prompt, in which case you must use \
them for everything Heimdall does. Calls from any thread but the relay's are refused.";
/// Why a call was refused: not the relay's thread.
pub const NOT_RELAY: &str = "Refused: Heimdall's tools are only for the T3 thread acting as Heimdall's agent (the relay), and this is another thread.";
const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// How long a call waits for the grant to settle: T3 naming a launched thread (`pending`), or a new token replacing the
/// old one (the file is briefly gone, the old server refuses or is gone).
pub const SETTLE: Duration = Duration::from_secs(20);
/// Of that, how long a missing grant file is waited for (a re-issue writes the new one at once; a relay that ended
/// does not come back).
const GAP: Duration = Duration::from_secs(5);
/// Whether `caller` (this session's T3 thread) holds the grant: the connection's thread, once T3 has named it. Only a
/// T3 thread can: a caller without one is refused (every relay runs in a T3 thread, and T3 gives each its id).
fn holds(data: &Path, caller: Option<&str>, wait: Duration) -> Result<Connection, String> {
    let began = std::time::Instant::now();
    loop {
        let settling = began.elapsed() < wait;
        match (connection(data), caller) {
            (Err(off), None) => return Err(off),
            (Ok(_), None) => return Err(NOT_RELAY.into()),
            (Err(_), Some(_)) if began.elapsed() < wait.min(GAP) => {}
            (Err(off), Some(_)) => return Err(off),
            (Ok(c), Some(me)) if c.thread == me => return Ok(c),
            (Ok(c), Some(_)) if c.thread == PENDING && settling => {}
            (Ok(_), Some(_)) => return Err(NOT_RELAY.into()),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The CLI's check (`call`): the same as the MCP server's.
pub fn cli_holds(data: &Path, caller: Option<&str>) -> Result<Connection, String> {
    holds(data, caller, SETTLE)
}

/// One `tools/call` for the grant's thread. A refusal by a retired token, a server that is gone or Agent off never
/// reached Heimdall's dispatch, so it is tried again, on the new grant when one comes, for a few seconds.
pub fn call_tool(data: &Path, caller: Option<&str>, params: &Value, wait: Duration) -> Value {
    let refused = |text: String| json!({"content":[{"type":"text","text":text}],"isError":true});
    let began = std::time::Instant::now();
    let action = format!("mcp-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let params = json!({"name":params["name"],"arguments":params.get("arguments").cloned().unwrap_or(json!({})),"_meta":{"heimdallActionId":action}});
    loop {
        let c = match holds(data, caller, wait.saturating_sub(began.elapsed())) { Ok(c) => c, Err(why) => return refused(why) };
        match rpc(&c, "tools/call", params.clone()) {
            Ok(result) => return result,
            Err((2, _)) if began.elapsed() < wait.min(GAP) => std::thread::sleep(Duration::from_millis(200)),
            Err((_, why)) => return refused(why),
        }
    }
}

/// Whether `caller`'s session should list the tools now: it is the grant's thread, or one is being launched.
fn listable(data: &Path, caller: Option<&str>) -> Option<Connection> {
    connection(data).ok().filter(|c| caller.is_some() && (c.thread == PENDING || caller == Some(c.thread.as_str())))
}
/// What the session's list depends on: whether it is listed, and the token (a new grant may serve other tools, as the
/// end-only one after a stand-down does).
fn list_state(data: &Path, caller: Option<&str>) -> Option<String> {
    listable(data, caller).map(|c| c.token_digest())
}

/// A digest of the tool definitions Heimdall serves this session now (`None` when it lists none or cannot be asked).
fn tools_digest(data: &Path, caller: Option<&str>) -> Option<String> {
    use sha2::Digest;
    let tools = listable(data, caller).and_then(|c| rpc(&c, "tools/list", json!({})).ok())?;
    Some(sha2::Sha256::digest(tools["tools"].to_string().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect())
}
/// How often (in the watcher's one-second ticks) the definitions are compared when the grant has not changed.
const TOOLS_EVERY: u64 = 10;
/// When the session should be told to list again (bug note 2026-11-04T23-37-22: the relay thread kept the `ask`
/// schema it loaded on 3 Nov, before grants, through the 4 Nov update). It is told once its grant (the token) or the
/// tool definitions Heimdall serves have changed and held for a tick, so a re-issue's brief gap (no file, then the new
/// one) is one notice for the settled list, never a notice for the empty list in between. A definition change with the
/// same token (an update that kept the relay's grant) is seen within `TOOLS_EVERY` ticks.
struct Watch { told: (Option<String>, Option<String>), last: (Option<String>, Option<String>), tick: u64 }
impl Watch {
    fn new(data: &Path, caller: Option<&str>) -> Watch {
        let now = (list_state(data, caller), tools_digest(data, caller));
        Watch { told: now.clone(), last: now, tick: 0 }
    }
    /// One tick: whether to send `notifications/tools/list_changed` now.
    fn step(&mut self, data: &Path, caller: Option<&str>) -> bool {
        self.tick += 1;
        let token = list_state(data, caller);
        let fresh = token != self.last.0 || self.tick % TOOLS_EVERY == 0;
        let tools = match &token {
            None => None,
            Some(_) if fresh => tools_digest(data, caller).or_else(|| if token == self.last.0 { self.last.1.clone() } else { None }),
            Some(_) => self.last.1.clone(),
        };
        let now = (token, tools);
        if now != self.last { self.last = now; return false; }
        if now != self.told { self.told = now; return true; }
        false
    }
}

/// One JSON-RPC message from Claude: the answer, or `None` for a notification.
pub fn handle(msg: &Value, data: &Path, caller: Option<&str>) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let ok = |result: Value| Some(json!({"jsonrpc":"2.0","id":id,"result":result}));
    match msg["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"].as_str().unwrap_or_default();
            let version = PROTOCOLS.iter().find(|p| **p == asked).copied().unwrap_or(PROTOCOLS[0]);
            ok(json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":true}},
                "serverInfo":{"name":NAME,"version":env!("CARGO_PKG_VERSION")},"instructions":RULE}))
        }
        "ping" => ok(json!({})),
        // Only the grant's thread (or the one being launched) lists them: every other session pays nothing for them.
        "tools/list" => {
            let tools = listable(data, caller).and_then(|c| rpc(&c, "tools/list", json!({})).ok()).map_or(json!([]), |r| r["tools"].clone());
            ok(json!({"tools":tools}))
        }
        "tools/call" => ok(call_tool(data, caller, &msg["params"], SETTLE)),
        "resources/list" => ok(json!({"resources":[]})),
        "prompts/list" => ok(json!({"prompts":[]})),
        other => Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("{other} is not served")}})),
    }
}

/// The stdio loop: one JSON message per line in, one per line out, until Claude closes stdin. Each call runs on its
/// own thread (a slow tool holds up nothing else); a watcher tells Claude to list again when the grant or Heimdall's
/// tool definitions change for this session (`notifications/tools/list_changed`, `Watch`), so a session started before
/// its grant, or before an update, gets the tools as they are. A
/// cancelled call is not stopped: Heimdall's dispatch has no cancel, as for Pi.
pub fn serve(data: &Path) -> (i32, String) {
    use std::sync::{Arc, Mutex};
    let caller = std::env::var(THREAD_ENV).ok().filter(|t| !t.is_empty());
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let send = |out: &Mutex<std::io::Stdout>, v: &Value| { let mut o = out.lock().unwrap_or_else(|e| e.into_inner()); writeln!(o, "{v}").and_then(|()| o.flush()).is_ok() };
    {
        let (data, caller, out) = (data.to_path_buf(), caller.clone(), out.clone());
        std::thread::spawn(move || {
            let mut watch = Watch::new(&data, caller.as_deref());
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if watch.step(&data, caller.as_deref()) && !send(&out, &json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})) { break; }
            }
        });
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => msg,
            Err(_) => { if !send(&out, &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"not JSON"}})) { break; } continue; }
        };
        if msg["method"] == "tools/call" {
            let (data, caller, out) = (data.to_path_buf(), caller.clone(), out.clone());
            std::thread::spawn(move || { if let Some(a) = handle(&msg, &data, caller.as_deref()) { send(&out, &a); } });
        } else if let Some(a) = handle(&msg, data, caller.as_deref()) {
            if !send(&out, &a) { break; }
        }
    }
    (0, String::new())
}

/// Every Claude Code config file on this Mac whose user-scope servers T3's sessions may load: the one
/// `CLAUDE_CONFIG_DIR` names, `~/.claude.json`, and each `~/.claude-*/.claude.json` (a second account, as T3 runs
/// Claude with one). Only files that exist, except `~/.claude.json`.
pub fn config_files(home: &Path, config_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut files = vec![home.join(".claude.json")];
    if let Some(dir) = config_dir { files.push(dir.join(".claude.json")); }
    if let Ok(entries) = std::fs::read_dir(home) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(".claude-")) && p.is_dir()).collect();
        dirs.sort();
        files.extend(dirs.into_iter().map(|d| d.join(".claude.json")));
    }
    let mut seen = Vec::new();
    files.retain(|f| (f == &home.join(".claude.json") || f.exists()) && !seen.contains(f) && { seen.push(f.clone()); true });
    files
}
/// The user-scope entry for the bundled CLI at `exe`.
pub fn entry(exe: &Path, data: &Path, default_data: bool) -> Value {
    let mut args = vec![];
    if !default_data { args.extend([json!("--data-dir"), json!(data.display().to_string())]); }
    args.push(json!("mcp"));
    json!({"type":"stdio","command":exe.display().to_string(),"args":args,"env":{}})
}
/// Claude's config with Heimdall's server in place of any earlier copy (`None` removes it); everything else kept.
pub fn with_server(mut config: Value, entry: Option<&Value>) -> Value {
    if !config.is_object() { config = json!({}); }
    if !config["mcpServers"].is_object() { config["mcpServers"] = json!({}); }
    let servers = config["mcpServers"].as_object_mut().unwrap();
    match entry { Some(e) => { servers.insert(NAME.into(), e.clone()); } None => { servers.remove(NAME); } }
    config
}
/// `mcp install` / `mcp uninstall` over every config file; one line per file.
pub fn install(files: &[PathBuf], entry: Option<&Value>) -> (i32, String) {
    let mut code = 0;
    let lines: Vec<String> = files.iter().map(|f| match crate::approvals::rewrite_json(f, |c| with_server(c, entry)) {
        Ok(true) => format!("{} {}", if entry.is_some() { "Heimdall's MCP server in" } else { "Heimdall's MCP server out of" }, f.display()),
        Ok(false) => format!("{} unchanged", f.display()),
        Err(e) => { code = 1; e }
    }).collect();
    (code, lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OFF;
    use std::{io::Read, net::TcpListener};

    fn grant(dir: &Path, url: &str, thread: &str) {
        std::fs::create_dir_all(dir.join("emergency")).unwrap();
        std::fs::write(dir.join("emergency/connection.json"), json!({"url":url,"token":"tok-1","thread":thread}).to_string()).unwrap();
    }
    /// A one-shot server answering `result`; the request it saw.
    fn server(result: Value) -> (String, std::thread::JoinHandle<String>) {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", server.local_addr().unwrap());
        (url, std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut req = String::new();
            let mut buf = vec![0u8; 65536];
            while !req.ends_with('}') { let n = s.read(&mut buf).unwrap(); if n == 0 { break; } req.push_str(&String::from_utf8_lossy(&buf[..n])); }
            let body = json!({"jsonrpc":"2.0","id":1,"result":result}).to_string();
            write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
            req
        }))
    }
    fn call(name: &str) -> Value { json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":{"text":"hi"}}}) }

    #[test]
    fn initialize_carries_the_rule_and_notifications_get_no_answer() {
        let d = tempfile::tempdir().unwrap();
        let a = handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}), d.path(), None).unwrap();
        assert_eq!(a["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(a["result"]["serverInfo"]["name"], NAME);
        assert_eq!(a["result"]["instructions"], RULE);
        assert!(handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}), d.path(), None).is_none());
        assert_eq!(handle(&json!({"jsonrpc":"2.0","id":2,"method":"nope"}), d.path(), None).unwrap()["error"]["code"], -32601);
    }

    /// Scenario: another T3 thread (a worker, a sub-agent's session) has the server in its settings. It lists nothing,
    /// and a call it makes anyway never reaches Heimdall.
    #[test]
    fn another_thread_sees_no_tools_and_cannot_drive_heimdall() {
        let d = tempfile::tempdir().unwrap();
        // No relay: nothing listed, calls refused with the CLI's own line.
        let list = handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}), d.path(), Some("t-worker")).unwrap();
        assert_eq!(list["result"]["tools"], json!([]));
        let r = call_tool(d.path(), Some("t-worker"), &call("speak")["params"], Duration::ZERO);
        assert!(r["isError"] == true && r["content"][0]["text"].as_str().unwrap().starts_with(OFF), "{r}");
        // A relay on in another thread: still nothing listed, and the call is refused without a request.
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        silent.set_nonblocking(true).unwrap();
        grant(d.path(), &format!("http://{}/mcp", silent.local_addr().unwrap()), "t-relay");
        assert_eq!(handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}), d.path(), Some("t-worker")).unwrap()["result"]["tools"], json!([]));
        for caller in [Some("t-worker"), None] {
            let r = handle(&call("speak"), d.path(), caller).unwrap();
            assert_eq!(r["result"]["content"][0]["text"], NOT_RELAY, "{caller:?}");
        }
        assert!(silent.accept().is_err(), "no request reached Heimdall");
        // The CLI from that thread is refused too, and so is a process outside T3 (no thread id): only the relay acts.
        assert_eq!(cli_holds(d.path(), Some("t-worker")).err().as_deref(), Some(NOT_RELAY));
        assert_eq!(cli_holds(d.path(), None).err().as_deref(), Some(NOT_RELAY));
        assert_eq!(crate::run_as(&["call".into(), "speak".into(), "{\"text\":\"x\"}".into()], Some(d.path().into()), None), (2, NOT_RELAY.into()));
    }

    #[test]
    fn the_relay_thread_gets_heimdall_s_answers_whole_with_images_inline() {
        let d = tempfile::tempdir().unwrap();
        let answer = json!({"content":[{"type":"text","text":"Screen."},{"type":"image","data":"AAAA","mimeType":"image/png"}]});
        let (url, seen) = server(answer.clone());
        grant(d.path(), &url, "t-relay");
        let r = handle(&call("look"), d.path(), Some("t-relay")).unwrap();
        assert_eq!((r["id"].clone(), r["result"].clone()), (json!(7), answer));
        let req = seen.join().unwrap();
        for h in ["Authorization: Bearer tok-1", "\"name\":\"look\"", "heimdallActionId", "\"text\":\"hi\""] { assert!(req.contains(h), "{h} in {req}"); }
        // Listing forwards the main server's list as it is.
        let tools = json!({"tools":[{"name":"speak","inputSchema":{"type":"object"}}]});
        let (url, _seen) = server(tools.clone());
        grant(d.path(), &url, "t-relay");
        assert_eq!(handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}), d.path(), Some("t-relay")).unwrap()["result"], tools);
    }

    /// The launch's first seconds: the grant says `pending` until T3 answers with the thread's id; a call meanwhile
    /// waits for it instead of failing.
    #[test]
    fn a_call_during_the_launch_waits_for_the_thread_to_be_named() {
        let d = tempfile::tempdir().unwrap();
        let (url, _seen) = server(json!({"content":[{"type":"text","text":"Speech queued: 1."}]}));
        grant(d.path(), &url, PENDING);
        let dir = d.path().to_path_buf();
        let namer = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(400)); grant(&dir, &url, "t-relay"); });
        let r = handle(&call("speak"), d.path(), Some("t-relay")).unwrap();
        assert_eq!(r["result"]["content"][0]["text"], "Speech queued: 1.", "{r}");
        namer.join().unwrap();
    }

    /// A new token mid-session (a hand-back trial that failed, a restart): the old server is gone, the call goes to the
    /// new one; and the session is told to list again whenever its grant changes.
    #[test]
    fn a_call_follows_a_new_grant_and_the_list_changes_with_it() {
        let d = tempfile::tempdir().unwrap();
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let old = format!("http://{}/mcp", gone.local_addr().unwrap());
        drop(gone);
        grant(d.path(), &old, "t-relay");
        let before = list_state(d.path(), Some("t-relay"));
        assert!(before.is_some() && list_state(d.path(), Some("t-worker")).is_none());
        let (url, _seen) = server(json!({"content":[{"type":"text","text":"Speech queued: 2."}]}));
        let dir = d.path().to_path_buf();
        let reissue = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(dir.join("emergency/connection.json"), json!({"url":url,"token":"tok-2","thread":"t-relay"}).to_string()).unwrap();
        });
        // The call starts while the file still names the dead server, and lands on the new one.
        let r = call_tool(d.path(), Some("t-relay"), &call("speak")["params"], SETTLE);
        assert_eq!(r["content"][0]["text"], "Speech queued: 2.", "{r}");
        reissue.join().unwrap();
        assert!(list_state(d.path(), Some("t-relay")).is_some_and(|now| Some(now) != before), "a new token changes the list");
        std::fs::remove_file(d.path().join("emergency/connection.json")).unwrap();
        assert!(list_state(d.path(), Some("t-relay")).is_none());
    }

    /// A server answering every request with `result` until dropped.
    fn steady(result: Value) -> String {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", server.local_addr().unwrap());
        std::thread::spawn(move || for s in server.incoming() {
            let Ok(mut s) = s else { break };
            let mut req = String::new();
            let mut buf = vec![0u8; 65536];
            while !req.ends_with('}') { let Ok(n) = s.read(&mut buf) else { break }; if n == 0 { break; } req.push_str(&String::from_utf8_lossy(&buf[..n])); }
            let body = json!({"jsonrpc":"2.0","id":1,"result":result}).to_string();
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        });
        url
    }

    /// Bug note 2026-11-04T23-37-22: the session is told to list again when Heimdall's definitions change under the
    /// same grant, and once per re-issue (after the gap settles), never for the gap itself.
    #[test]
    fn the_session_is_told_when_the_definitions_or_the_grant_change() {
        let d = tempfile::tempdir().unwrap();
        let old = steady(json!({"tools":[{"name":"ask","inputSchema":{"type":"object"}}]}));
        grant(d.path(), &old, "t-relay");
        let me = Some("t-relay");
        let mut w = Watch::new(d.path(), me);
        assert!(w.told.1.is_some());
        assert!((0..2 * TOOLS_EVERY).all(|_| !w.step(d.path(), me)), "nothing changed");
        // An update with the same token: new definitions, seen within TOOLS_EVERY ticks, told once.
        let new = steady(json!({"tools":[{"name":"ask","inputSchema":{"type":"object","properties":{"grant":{}}}}]}));
        grant(d.path(), &new, "t-relay");
        let told: Vec<bool> = (0..2 * TOOLS_EVERY).map(|_| w.step(d.path(), me)).collect();
        assert_eq!(told.iter().filter(|t| **t).count(), 1, "{told:?}");
        // A re-issue: the file goes, then a new token comes. One notice, after the new grant holds.
        std::fs::remove_file(d.path().join("emergency/connection.json")).unwrap();
        assert!(!w.step(d.path(), me), "the gap is not told");
        std::fs::write(d.path().join("emergency/connection.json"), json!({"url":new,"token":"tok-2","thread":"t-relay"}).to_string()).unwrap();
        let told: Vec<bool> = (0..3).map(|_| w.step(d.path(), me)).collect();
        assert_eq!(told, [false, true, false]);
        // Another thread never lists, so is never told.
        let mut other = Watch::new(d.path(), Some("t-worker"));
        assert!((0..TOOLS_EVERY + 1).all(|_| !other.step(d.path(), Some("t-worker"))));
    }

    #[test]
    fn install_puts_one_entry_in_every_claude_config_and_keeps_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::write(home.join(".claude.json"), json!({"mcpServers":{"vault":{"command":"v"}},"numStartups":3}).to_string()).unwrap();
        std::fs::create_dir_all(home.join(".claude-account2")).unwrap();
        std::fs::write(home.join(".claude-account2/.claude.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join(".claude-empty")).unwrap();
        let files = config_files(home, None);
        assert_eq!(files, [home.join(".claude.json"), home.join(".claude-account2/.claude.json")]);
        let e = entry(Path::new("/A/heimdall-emergency"), Path::new("/D"), false);
        assert_eq!(e["args"], json!(["--data-dir", "/D", "mcp"]));
        assert_eq!(install(&files, Some(&e)).0, 0);
        let main: Value = serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!((main["mcpServers"]["heimdall"].clone(), main["mcpServers"]["vault"]["command"].clone(), main["numStartups"].clone()), (e.clone(), json!("v"), json!(3)));
        assert!(install(&files, Some(&e)).1.lines().all(|l| l.ends_with("unchanged")));
        assert_eq!(install(&files, None).0, 0);
        let second: Value = serde_json::from_slice(&std::fs::read(home.join(".claude-account2/.claude.json")).unwrap()).unwrap();
        assert!(second["mcpServers"].get(NAME).is_none());
    }
}
