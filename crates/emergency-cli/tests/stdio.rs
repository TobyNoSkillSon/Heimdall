//! `heimdall-emergency mcp` as Claude Code runs it: the real binary over stdio, one JSON message per line.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn session(data: &std::path::Path, thread: Option<&str>, lines: &[Value]) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heimdall-emergency"));
    cmd.args(["--data-dir", data.to_str().unwrap(), "mcp"]).stdin(Stdio::piped()).stdout(Stdio::piped()).env_remove("T3CODE_THREAD_ID");
    if let Some(t) = thread { cmd.env("T3CODE_THREAD_ID", t); }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut answers = Vec::new();
    for l in lines {
        writeln!(stdin, "{l}").unwrap();
        if l.get("id").is_none() { continue; }
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        answers.push(serde_json::from_str(&line).unwrap());
    }
    drop(stdin);
    assert!(child.wait().unwrap().success(), "the server ends when Claude closes stdin");
    answers
}

/// Claude's handshake, then a list and a call, from a thread that is not the relay's: it is served, lists nothing,
/// and is refused.
#[test]
fn claude_s_handshake_works_and_another_thread_gets_nothing() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("emergency")).unwrap();
    std::fs::write(d.path().join("emergency/connection.json"), json!({"url":"http://127.0.0.1:9/mcp","token":"t","thread":"t-relay"}).to_string()).unwrap();
    let a = session(d.path(), Some("t-worker"), &[
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"2"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"speak","arguments":{"text":"hi"}}}),
    ]);
    assert_eq!(a[0]["result"]["serverInfo"]["name"], "heimdall");
    assert_eq!(a[0]["result"]["capabilities"]["tools"]["listChanged"], true);
    assert_eq!(a[1]["result"]["tools"], json!([]));
    assert_eq!((a[2]["id"].clone(), a[2]["result"]["isError"].clone()), (json!(2), json!(true)));
    assert_eq!(a[2]["result"]["content"][0]["text"], heimdall_emergency::mcp::NOT_RELAY);
}
