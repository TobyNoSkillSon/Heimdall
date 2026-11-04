//! Every tool call the agent makes and its outcome, one plain line each, in `<data>/logs/tools.log` (
//! "those icons, they shouldn't even show anything … log it somewhere in Heimdall's files so that … I can tell you,
//! okay, something went wrong, can you check the logs?"). The agent reads it when asked; its system prompt names the
//! path (`PROMPT_PLACEHOLDER` is replaced at launch). Every tool counts: Heimdall's MCP tools, bash, file tools.
//!
//! A line: local time, tool, `ok` or `ERROR`, duration, the arguments (compact JSON) and the result's text, each cut
//! to a bound, newlines shown as ` / `. Secrets are masked first (`redact`): API keys by their prefixes, `Bearer`
//! and `Basic` credentials, and the value of any `api_key=`, `token:` or `"password": "…"` pair. Bounded on disk: past `MAX_BYTES` the file becomes `tools.log.1` (replacing
//! the older one) and a new file starts, so at most twice `MAX_BYTES` is kept. Writing never fails the harness.
use serde_json::Value;
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

/// Size at which the log rotates.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// The token in `prompt/heimdall.md` that becomes the log's path when the prompt is installed.
pub const PROMPT_PLACEHOLDER: &str = "HEIMDALL_TOOL_LOG";
const ARGS_CHARS: usize = 300;
const RESULT_CHARS: usize = 600;
const MCP_PREFIX: &str = "mcp__heimdall__";

/// The log of the profile whose data directory is `data`.
pub fn path(data: &Path) -> PathBuf {
    data.join("logs").join("tools.log")
}

pub struct ToolLog {
    path: PathBuf,
    max_bytes: u64,
    /// Calls that started: their arguments and start time, by tool call id.
    started: HashMap<String, (String, u64)>,
}
impl ToolLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path, max_bytes: MAX_BYTES, started: HashMap::new() }
    }
    #[cfg(test)]
    fn with_limit(path: PathBuf, max_bytes: u64) -> Self {
        Self { path, max_bytes, started: HashMap::new() }
    }
    /// One of Pi's events: a `tool_execution_start` remembers the call, a `tool_execution_end` writes its line.
    pub fn observe(&mut self, event: &Value, now_ms: u64) {
        let Some(id) = event["toolCallId"].as_str() else { return };
        match event["type"].as_str() {
            Some("tool_execution_start") => {
                // Bounded: a run that never ends its calls cannot grow this map without limit.
                if self.started.len() >= 256 {
                    self.started.clear();
                }
                self.started.insert(id.to_owned(), (args(&event["args"]), now_ms));
            }
            Some("tool_execution_end") => {
                let (args, started) = self.started.remove(id).unwrap_or_else(|| (args(&event["args"]), now_ms));
                let ms = event["durationMs"].as_u64().unwrap_or(now_ms.saturating_sub(started));
                let (tool, failed, output) = (event["toolName"].as_str().unwrap_or("?"), event["isError"] == true, result(&event["result"]));
                if failed {
                    eprintln!("{}", failure_line(tool, ms, &output));
                }
                let line = line(now_ms, tool, failed, ms, &args, &output);
                if let Err(e) = self.append(&line) {
                    eprintln!("Tool log not written: {e:#}");
                }
            }
            _ => {}
        }
    }
    fn append(&mut self, line: &str) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= self.max_bytes) {
            std::fs::rename(&self.path, self.path.with_extension("log.1"))?;
        }
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path)?;
        file.write_all(format!("{line}\n").as_bytes())?;
        Ok(())
    }
}

/// A failed call is also one line in heimdall.log (the output is already masked by `result`; the log adds the time).
fn failure_line(tool: &str, ms: u64, output: &str) -> String {
    format!("tool: {} failed after {ms} ms: {}", tool.strip_prefix(MCP_PREFIX).unwrap_or(tool), cut(output, 240))
}
/// `2026-10-19 12:16:41  speak  ok  0.4 s  {"text":"Yes."}  ->  Speech queued.`
fn line(at_ms: u64, tool: &str, error: bool, ms: u64, args: &str, result: &str) -> String {
    use chrono::TimeZone;
    let at = chrono::Local.timestamp_millis_opt(at_ms as i64).single().map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
    let tool = tool.strip_prefix(MCP_PREFIX).unwrap_or(tool);
    let outcome = if error { "ERROR" } else { "ok" };
    let duration = if ms < 1_000 { format!("{ms} ms") } else { format!("{:.1} s", ms as f64 / 1000.) };
    format!("{at}  {tool}  {outcome}  {duration}  {args}  ->  {result}")
}
fn args(v: &Value) -> String {
    if v.is_null() { return "{}".into(); }
    cut(&redact(&v.to_string()), ARGS_CHARS)
}
/// The result's text parts (images as `[image]`), one line.
fn result(v: &Value) -> String {
    let text = match v["content"].as_array() {
        Some(parts) => parts.iter().map(|p| match p["type"].as_str() {
            Some("text") => p["text"].as_str().unwrap_or("").to_owned(),
            Some("image") => "[image]".into(),
            other => format!("[{}]", other.unwrap_or("?")),
        }).collect::<Vec<_>>().join(" "),
        None => v.as_str().map(str::to_owned).unwrap_or_else(|| if v.is_null() { String::new() } else { v.to_string() }),
    };
    let flat = text.trim().lines().map(str::trim_end).collect::<Vec<_>>().join(" / ");
    if flat.is_empty() { "(no output)".into() } else { cut(&redact(&flat), RESULT_CHARS) }
}

const MASK: &[u8] = b"[redacted]";
/// Prefixes of API keys and access tokens; the token after one (8 characters or more) is masked.
const KEY_PREFIXES: [&str; 12] = ["sk-", "sk_", "pk_live_", "rk_live_", "ghp_", "gho_", "ghs_", "github_pat_", "xox", "hf_", "glpat-", "aiza"];
/// Names whose value is a secret: an identifier ending in one of these, then `=` or `:`, masks its value.
const SECRET_NAMES: [&str; 11] = ["api_key", "apikey", "api-key", "token", "secret", "password", "passwd", "private_key", "access_key", "credential", "credentials"];
/// Secrets never reach the log (review 19 Oct): the text with every API key, `Bearer`/`Basic` credential and secret
/// `name=value` value replaced by `[redacted]`. Everything else is kept byte for byte.
pub(crate) fn redact(text: &str) -> String {
    let b = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let lower = lower.as_bytes();
    let token = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'/' | b'+' | b'=' | b'~');
    let ident = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-');
    let run = |from: usize, ok: &dyn Fn(u8) -> bool| (from..b.len()).find(|&j| !ok(b[j])).unwrap_or(b.len());
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if boundary {
            // `sk-…`, `ghp_…`: the whole token.
            if let Some(p) = KEY_PREFIXES.iter().find(|p| lower[i..].starts_with(p.as_bytes())) {
                let end = run(i + p.len(), &token);
                if end - i - p.len() >= 8 {
                    out.extend_from_slice(MASK);
                    i = end;
                    continue;
                }
            }
            // `Bearer …`, `Basic …`: the scheme stays, the credential goes.
            if let Some(scheme) = ["bearer ", "basic "].iter().find(|s| lower[i..].starts_with(s.as_bytes())) {
                let start = run(i + scheme.len(), &|c| c == b' ');
                let end = run(start, &token);
                if end - start >= 8 {
                    out.extend_from_slice(&b[i..start]);
                    out.extend_from_slice(MASK);
                    i = end;
                    continue;
                }
            }
            // `api_key=…`, `"token": "…"`, `OPENAI_API_KEY: …`: the value.
            let name_end = run(i, &ident);
            if name_end > i && SECRET_NAMES.iter().any(|n| lower[i..name_end].ends_with(n.as_bytes())) {
                let mut j = name_end;
                let skip = |j: &mut usize, set: &[u8]| while *j < b.len() && set.contains(&b[*j]) { *j += 1; };
                skip(&mut j, b"\\\"' ");
                if j < b.len() && (b[j] == b'=' || b[j] == b':') {
                    j += 1;
                    skip(&mut j, b"\\\"' ");
                    let end = (j..b.len()).find(|&k| b"\"'\\ \t&,;}])".contains(&b[k])).unwrap_or(b.len());
                    if end - j >= 4 {
                        out.extend_from_slice(&b[i..j]);
                        out.extend_from_slice(MASK);
                        i = end;
                        continue;
                    }
                }
                out.extend_from_slice(&b[i..name_end]);
                i = name_end;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}
fn cut(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn secrets_never_reach_the_log() {
        for (text, kept) in [
            ("curl -H 'Authorization: Bearer abcDEF123456.xyz' https://api.example.com", "curl -H 'Authorization: Bearer [redacted]' https://api.example.com"),
            ("export OPENAI_API_KEY=sk-proj-AbC123dEf456 && run", "export OPENAI_API_KEY=[redacted] && run"),
            ("key sk-ant-api03-AbCdEf123456 used", "key [redacted] used"),
            ("https://x.test/v1?api_key=Zx9Zx9Zx9&page=2", "https://x.test/v1?api_key=[redacted]&page=2"),
            (r#"{"command":"login","password":"hunter22","user":"owner"}"#, r#"{"command":"login","password":"[redacted]","user":"owner"}"#),
            (r#"{"command":"curl -d "token=abcd1234efgh" x"}"#, r#"{"command":"curl -d "token=[redacted]" x"}"#),
            ("GITHUB_TOKEN: ghp_0123456789abcdef", "GITHUB_TOKEN: [redacted]"),
            ("Authorization: Basic dXNlcjpodW50ZXIy", "Authorization: Basic [redacted]"),
            ("hf_abcdefghijk zażółć", "[redacted] zażółć"),
        ] {
            assert_eq!(redact(text), kept, "{text}");
        }
        // Ordinary words and numbers stay.
        for text in ["max_tokens: 4096 and tokens: 12", "task-list sk-8 skip", "Basic idea", "the token is fine", "zażółć gęślą jaźń 😀", "risk-free"] {
            assert_eq!(redact(text), text);
        }
        // In the log: arguments and results.
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        let mut log = ToolLog::new(p.clone());
        log.observe(&json!({"type":"tool_execution_start","toolCallId":"a","toolName":"bash","args":{"command":"curl -H 'Authorization: Bearer abcDEF123456' x"}}), 1_000);
        log.observe(&json!({"type":"tool_execution_end","toolCallId":"a","toolName":"bash","result":{"content":[{"type":"text","text":"OPENAI_API_KEY=sk-proj-AbC123dEf456"}]},"isError":false}), 1_200);
        let text = read(&p);
        assert!(!text.contains("abcDEF123456") && !text.contains("AbC123dEf456"), "{text}");
        assert!(text.contains("Bearer [redacted]") && text.contains("OPENAI_API_KEY=[redacted]"), "{text}");
    }

    #[test]
    fn every_finished_call_is_one_plain_line_with_its_outcome() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        let mut log = ToolLog::new(p.clone());
        log.observe(&json!({"type":"tool_execution_start","toolCallId":"a","toolName":"mcp__heimdall__speak","args":{"text":"Yes, I can hear you.","language":"en"}}), 1_000);
        log.observe(&json!({"type":"tool_execution_start","toolCallId":"b","toolName":"bash","args":{"command":"ls /nowhere"}}), 1_100);
        log.observe(&json!({"type":"tool_execution_end","toolCallId":"b","toolName":"bash","result":{"content":[{"type":"text","text":"ls: /nowhere: No such file\nexit 1"}]},"isError":true}), 3_600);
        log.observe(&json!({"type":"tool_execution_end","toolCallId":"a","toolName":"mcp__heimdall__speak","result":{"content":[{"type":"text","text":"Speech queued."}]},"isError":false,"durationMs":12}), 3_700);
        log.observe(&json!({"type":"tool_execution_end","toolCallId":"c","toolName":"mcp__heimdall__look","result":{"content":[{"type":"image","data":"AAAA"},{"type":"text","text":"Display 1: size 1728 1117"}]},"isError":false}), 4_000);
        let text = read(&p);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("  bash  ERROR  2.5 s  {\"command\":\"ls /nowhere\"}  ->  ls: /nowhere: No such file / exit 1"), "{}", lines[0]);
        assert!(lines[1].contains("  speak  ok  12 ms  ") && lines[1].contains("\"text\":\"Yes, I can hear you.\"") && lines[1].ends_with("->  Speech queued."), "{}", lines[1]);
        assert!(lines[2].contains("  look  ok  0 ms  {}  ->  [image] Display 1: size 1728 1117"), "{}", lines[2]);
        // Never the image bytes; never a second line for an update.
        assert!(!text.contains("AAAA"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "private like the session files");
    }

    #[test]
    fn long_arguments_and_results_are_cut_and_the_file_rotates() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        let mut log = ToolLog::with_limit(p.clone(), 2_000);
        let wall = "ł".repeat(5_000);
        for n in 0..10 {
            log.observe(&json!({"type":"tool_execution_start","toolCallId":format!("c{n}"),"toolName":"mcp__heimdall__t3_workers","args":{"catalog":true,"note":wall}}), n);
            log.observe(&json!({"type":"tool_execution_end","toolCallId":format!("c{n}"),"toolName":"mcp__heimdall__t3_workers","result":{"content":[{"type":"text","text":wall}]},"isError":false}), n);
        }
        let current = read(&p);
        let older = read(&p.with_extension("log.1"));
        assert!(!older.is_empty(), "the full file moved aside");
        assert!(!d.path().join("logs/tools.log.1.1").exists() && !d.path().join("logs/tools.log.2").exists(), "one older file only");
        for line in current.lines().chain(older.lines()) {
            assert!(line.chars().count() < 1_000, "a line stays short: {}", line.chars().count());
            assert!(line.contains('…'));
        }
        assert!(std::fs::metadata(&p).unwrap().len() < 4_000);
    }

    #[test]
    fn an_unwritable_log_never_fails_the_caller() {
        let mut log = ToolLog::new(PathBuf::from("/dev/null/cannot/tools.log"));
        log.observe(&json!({"type":"tool_execution_end","toolCallId":"x","toolName":"bash","result":null,"isError":false}), 0);
    }
    /// Night 20 Oct: a failed tool call is also one line in heimdall.log, masked and bounded.
    #[test]
    fn a_failed_call_is_one_masked_line_for_heimdall_log() {
        let output = result(&json!({"content":[{"type":"text","text":"401 from provider\nAuthorization: Bearer abcdefghijklmnopqrstu"}]}));
        let line = failure_line("mcp__heimdall__t3_call", 1530, &output);
        assert_eq!(line, "tool: t3_call failed after 1530 ms: 401 from provider / Authorization: Bearer [redacted]");
        assert!(failure_line("bash", 1, &"x".repeat(5000)).len() < 300);
    }
}
