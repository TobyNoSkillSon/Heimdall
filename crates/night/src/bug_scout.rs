//! The bug scout ("whenever you file a bug note, a background agent like Haiku goes through the codebase
//! and tries to identify the problem and fill in the metadata … they're just scouting"). Filing a note starts one
//! scout for it: a cheap model reads what the note's time shows (Heimdall's log lines, the transcript the archive
//! holds, the repo lines that share words with the note) and writes a cause class, its reasoning, the suspected files
//! and functions, a repro hint and a confidence into the note (`bug_notes::set_cause`).
//!
//! Limits: one scout per note (a note that has a cause is skipped); one at a time (`enqueue` feeds a single worker
//! through a queue of `QUEUE` notes, and a full queue leaves the note for the night pass); `OUTPUT_TOKENS` of output,
//! `MAX_USD` a call and `TIMEOUT` of wall time. The model gets no tools (`claude -p --tools ""`), so it cannot read or
//! change files; the scout writes only the note's own two files. It never wakes the main agent: nothing here
//! delivers to a thread.
//!
//! The model call is `ask`, passed in, so the tests run with a canned answer and no model.
use crate::bug_notes::{self, CAUSE_CLASSES, DECIDER_LABELS};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Mutex, OnceLock},
    time::{Duration, Instant},
};

/// The model the scout runs on: the background-model default (Background Tasks), by its CLI name.
pub const MODEL: &str = "claude-haiku-5-5";
/// Output tokens a scout may write per reply (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`); its JSON is a few hundred.
pub const OUTPUT_TOKENS: u32 = 4096;
/// Dollars a scout may spend in one call (`--max-budget-usd`). A safety ceiling only: a real look costs cents.
pub const MAX_USD: &str = "0.50";
/// Wall time one scout may take before it is stopped (three minutes a note).
pub const TIMEOUT: Duration = Duration::from_secs(180);
/// Tool calls a scout may make reading the repo (the user: 15 to 20); the turns cap allows one more for each reply.
pub const MAX_TOOL_CALLS: u32 = 20;
/// The tools a scout may use: reading and searching the repo, nothing that writes or runs a command.
pub const READ_TOOLS: &str = "Read,Grep,Glob";
/// Version of the scout's answer format. A note whose cause came from an older scout is scouted again.
pub const SCOUT_VERSION: u64 = 2;
/// Notes that may wait for the worker; one more is left for the night pass.
pub const QUEUE: usize = 8;

/// Heimdall's log lines this far either side of the note's time are its context.
const LOG_WINDOW_S: u64 = 150;
const LOG_LINES: usize = 120;
/// Archive utterances this far either side of the note's time, at most `UTTERANCES`.
const TRANSCRIPT_WINDOW_MS: i64 = 90_000;
const UTTERANCES: usize = 80;
/// Repo excerpts: at most `FILES` files, `LINES_PER_FILE` lines each, `CODE_BYTES` in all.
const FILES: usize = 6;
const LINES_PER_FILE: usize = 10;
const CODE_BYTES: usize = 16_000;
const SKIP_DIRS: [&str; 6] = ["target", "third-party", "node_modules", "dist", "build", "worktrees"];
const STOP: [&str; 14] = ["heimdall", "owner", "about", "there", "which", "their", "would", "should", "expected", "really", "after", "before", "still", "being"];

/// The system prompt: the classes, what each means for Heimdall, and the one JSON object to reply with.
pub const SYSTEM: &str = "You scout one bug note for Heimdall, User's voice assistant on his Mac (a Rust and Swift app: a \
decider model decides from speech whether to wake the main agent; the main agent is an LLM with tools and instructions; \
TTS, ASR and other models produce voice, transcripts and answers). Your job: name the note's cause class and say why, \
from the evidence given. You can read the repo with Read, Grep and Glob, and nothing else: use them. Start from the
repo lines given, follow the call paths the log and transcript point to, open the functions you name and check them.
Make at most 20 tool calls, and stop earlier once you have the cause.

Classes:
- code: a defect in Heimdall's own code (state, timing, rendering, the echo guard's logic, a tool's behaviour).
- decider: the decider's judgement is wrong: it woke Heimdall when it should not have, did not wake it when it should, \
or misjudged a barge-in. Then set decider_label: send (User addressed Heimdall, so it should wake), wait (an unfinished \
thought), or not_for_heimdall (talk not for Heimdall, including a line that only tells Heimdall to be quiet). Also set \
decider_variant: plain or ack, if the log shows which question it was.
- agent-prompt: the main agent chose a wrong action or wording that its instructions or prompt could change.
- model-quality: a model's output is poor (speech sounds bad, recognition mishears, an answer is weak) with no code defect.
- environment: the OS, hardware, a device, a permission, a network or an account outside Heimdall's code.
- unknown: the evidence does not settle it. Say what would.

Reply with ONE JSON object and nothing else:
{\"class\": one class above, \"reasoning\": one to three sentences that name the evidence, \"confidence\": a number from 0 to 1, \
\"evidence\": up to 4 short lines copied from the log, transcript or excerpts, \"suspected_sites\": up to 4 objects {\"file\": repo path, \"function\": name as written in the code, \"lines\": \"start-end\"}, each a function you read at those lines (do not name a site you did not read), \
\"repro_hint\": one line on how to reproduce or check it, \"decider_label\": send, \
wait, not_for_heimdall or null, \"decider_variant\": plain, ack or null}
Quote only lines you were given or read. Use confidence below 0.5 when the code you read does not show the cause.";

/// Where a scout reads: Heimdall's log and archive (the installed app's data folder) and the repo its excerpts come from.
#[derive(Clone, Debug)]
pub struct Sources {
    pub log: PathBuf,
    pub archive: PathBuf,
    pub code: Option<PathBuf>,
}

impl Sources {
    /// Heimdall's data folder (`HEIMDALL_DATA`, else `<home>/.heimdall`), and `HEIMDALL_CODE_ROOT` (else the project
    /// that holds the inbox, when it has a `crates` folder).
    pub fn for_inbox(inbox: &Path) -> Sources {
        let data = heimdall_home::data_dir();
        let project = inbox.parent().and_then(Path::parent).map(Path::to_path_buf);
        let code = std::env::var_os("HEIMDALL_CODE_ROOT").map(PathBuf::from).or(project).filter(|p| p.join("crates").is_dir());
        Sources { log: data.join("logs/heimdall.log"), archive: data.join("archive/heimdall.sqlite"), code }
    }
}

/// What the scout reads for one note.
#[derive(Debug, Default)]
pub struct Reading {
    pub log: Vec<String>,
    pub transcript: Vec<Value>,
    /// (repo path, `line: text` excerpts), the files whose lines share the most words with the note.
    pub code: Vec<(String, Vec<String>)>,
}

/// One model answer: its text and what it cost.
pub struct Answer {
    pub text: String,
    pub cost_usd: f64,
}

/// Gathers the context of `note` from `src`. A missing log, archive or repo leaves its part empty.
pub fn gather(note: &Value, src: &Sources) -> Reading {
    let at_ms = note["created_unix_ms"].as_u64().unwrap_or_default();
    let text = format!("{} {}", note["note"]["text"].as_str().unwrap_or_default(), note["evidence"].as_str().unwrap_or_default());
    let code = src.code.as_deref().map(|root| code_excerpts(root, &terms(&text))).unwrap_or_default();
    Reading { log: log_lines(&src.log, at_ms), transcript: transcript(&src.archive, at_ms as i64), code }
}

/// Heimdall's log lines within `LOG_WINDOW_S` of `at_ms`, the last `LOG_LINES`, each cut to 240 characters.
pub fn log_lines(log: &Path, at_ms: u64) -> Vec<String> {
    let Ok(bytes) = std::fs::read(log) else { return Vec::new() };
    let secs = at_ms / 1000;
    let (lo, hi) = (bug_notes::local_stamp(secs.saturating_sub(LOG_WINDOW_S), true), bug_notes::local_stamp(secs + LOG_WINDOW_S, true));
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| l.get(..19).is_some_and(|stamp| stamp >= lo.as_str() && stamp <= hi.as_str()))
        .map(|l| l.chars().take(240).collect())
        .collect();
    let keep = lines.len().saturating_sub(LOG_LINES);
    lines.drain(..keep);
    lines
}

/// The archive's utterances around `at_ms`, oldest first. Read-only: the app keeps writing the archive.
pub fn transcript(archive: &Path, at_ms: i64) -> Vec<Value> {
    use rusqlite::{Connection, OpenFlags};
    if !archive.is_file() {
        return Vec::new();
    }
    let Ok(db) = Connection::open_with_flags(archive, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) else {
        return Vec::new();
    };
    let Ok(mut stmt) = db.prepare("SELECT wall_ms, speaker, text FROM utterances WHERE wall_ms BETWEEN ?1 AND ?2 ORDER BY wall_ms LIMIT ?3") else {
        return Vec::new();
    };
    let rows = stmt.query_map(
        rusqlite::params![at_ms - TRANSCRIPT_WINDOW_MS, at_ms + TRANSCRIPT_WINDOW_MS, UTTERANCES as i64],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)),
    );
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .map(|(ms, speaker, text)| {
            json!({"time": bug_notes::local_stamp(ms.max(0) as u64 / 1000, true), "speaker": speaker.unwrap_or_default(),
                "text": text.unwrap_or_default().chars().take(300).collect::<String>()})
        })
        .collect()
}

/// Words of `text` worth searching the repo for: at least four letters, not a stop word, longest first, ten at most.
pub fn terms(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for w in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let w = w.to_lowercase();
        if w.chars().count() >= 4 && !STOP.contains(&w.as_str()) && !words.contains(&w) {
            words.push(w);
        }
    }
    words.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    words.truncate(10);
    words
}

/// Repo lines that share the note's words: the files with the most distinct words, their matching lines, `CODE_BYTES`.
pub fn code_excerpts(root: &Path, terms: &[String]) -> Vec<(String, Vec<String>)> {
    if terms.is_empty() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for dir in ["crates", "docs"] {
        collect_sources(&root.join(dir), &mut files);
    }
    let mut scored: Vec<(usize, PathBuf, String)> = files
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            let lower = text.to_lowercase();
            let score = terms.iter().filter(|t| lower.contains(t.as_str())).count();
            (score >= 2 || (terms.len() < 2 && score >= 1)).then_some((score, p, text))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = Vec::new();
    let mut bytes = 0;
    for (_, path, text) in scored.into_iter().take(FILES) {
        let lines: Vec<String> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let lower = l.to_lowercase();
                terms.iter().any(|t| lower.contains(t.as_str()))
            })
            .take(LINES_PER_FILE)
            .map(|(i, l)| format!("{}: {}", i + 1, l.trim().chars().take(200).collect::<String>()))
            .collect();
        let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        bytes += rel.len() + lines.iter().map(String::len).sum::<usize>();
        if bytes > CODE_BYTES {
            break;
        }
        out.push((rel, lines));
    }
    out
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let (path, name) = (entry.path(), entry.file_name().to_string_lossy().into_owned());
        if path.is_dir() {
            if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                collect_sources(&path, out);
            }
        } else if (name.ends_with(".rs") || name.ends_with(".md")) && path.metadata().is_ok_and(|m| m.len() <= 512 * 1024) {
            out.push(path);
        }
    }
}

/// The user message: the note, then each part of the context under its own heading.
pub fn prompt(id: &str, note: &Value, ctx: &Reading) -> String {
    let mut p = format!(
        "BUG NOTE {id}, filed {}:\n{}\n",
        note["created_local"].as_str().unwrap_or(id),
        note["note"]["text"].as_str().unwrap_or_default()
    );
    if let Some(e) = note["evidence"].as_str() {
        p += &format!("\nEVIDENCE filed with the note:\n{e}\n");
    }
    p += "\nHEIMDALL LOG, about 2.5 minutes around the note (decider, agent, voice and other lines, oldest first):\n";
    p += &if ctx.log.is_empty() { "(none)\n".to_owned() } else { ctx.log.join("\n") + "\n" };
    p += "\nTRANSCRIPT, archive utterances in the 3 minutes around the note (speaker, time, text):\n";
    for u in &ctx.transcript {
        p += &format!("[{} {}] {}\n", u["time"].as_str().unwrap_or_default(), u["speaker"].as_str().unwrap_or_default(), u["text"].as_str().unwrap_or_default());
    }
    if ctx.transcript.is_empty() {
        p += "(none)\n";
    }
    p += "\nCODE, repo lines that share words with the note (path, then line: text):\n";
    for (path, lines) in &ctx.code {
        p += &format!("{path}\n{}\n", lines.join("\n"));
    }
    if ctx.code.is_empty() {
        p += "(none)\n";
    }
    p
}

/// The JSON object in a model's answer (fenced or bare), as a value.
pub fn parse_answer(text: &str) -> Result<Value> {
    let start = text.find('{').context("the scout's answer has no JSON object")?;
    let end = text.rfind('}').filter(|e| *e >= start).context("the scout's answer has no closing brace")?;
    serde_json::from_str(&text[start..=end]).context("the scout's answer is not JSON")
}

/// Checks each suspected site against the repo: the file must exist under the repo root, the cited lines must hold the
/// named function (within three lines of the range), and the answer keeps only the sites that pass, with the line the
/// function really starts on (`at_line`). `suspected_files` and `suspected_functions` are derived from those sites. With no
/// repo to check, no site survives.
pub fn verify_sites(answer: &mut Value, src: &Sources) {
    let sites: Vec<Value> = match src.code.as_deref() {
        Some(root) => answer["suspected_sites"].as_array().into_iter().flatten().filter_map(|site| checked_site(root, site)).collect(),
        None => Vec::new(),
    };
    let mut files: Vec<String> = sites.iter().filter_map(|s| s["file"].as_str().map(str::to_owned)).collect();
    files.dedup();
    let functions: Vec<String> = sites.iter().filter_map(|s| s["function"].as_str().map(str::to_owned)).collect();
    answer["suspected_sites"] = json!(sites);
    answer["suspected_files"] = json!(files);
    answer["suspected_functions"] = json!(functions);
}

fn checked_site(root: &Path, site: &Value) -> Option<Value> {
    let (file, function, lines) = (site["file"].as_str()?, site["function"].as_str()?, site["lines"].as_str()?);
    let rel = Path::new(file);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let (first, last) = parse_range(lines)?;
    let name = function.rsplit("::").next().filter(|n| !n.is_empty())?;
    let text = std::fs::read_to_string(root.join(rel)).ok()?;
    let at_line = text.lines().enumerate().map(|(i, l)| (i + 1, l)).find(|(n, l)| {
        *n + 3 >= first && *n <= last + 3 && defines_fn(l, name)
    })?.0;
    Some(json!({"file": file, "function": function, "lines": lines, "at_line": at_line}))
}

/// `a-b` or `a`, one-based, `a <= b`.
fn parse_range(lines: &str) -> Option<(usize, usize)> {
    let (a, b) = lines.split_once('-').unwrap_or((lines, lines));
    let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a >= 1 && a <= b).then_some((a, b))
}

/// The line declares `fn name` (not `fn names`, not a call).
fn defines_fn(line: &str, name: &str) -> bool {
    let pat = format!("fn {name}");
    line.match_indices(&pat).any(|(at, _)| {
        let after = line[at + pat.len()..].chars().next();
        !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// The cause a scout's answer makes, with the note's transcript as a `decider_case` when the class is `decider`.
/// The answer is verified first (`verify_sites`). `bug_notes::set_cause` checks it.
pub fn cause_from(answer: &Value, ctx: &Reading, cost_usd: f64, now_ms: u64) -> Result<Value> {
    let class = answer["class"].as_str().unwrap_or_default();
    anyhow::ensure!(CAUSE_CLASSES.contains(&class), "the scout named class {class:?}");
    let mut cause = json!({
        "class": class,
        "reasoning": bug_notes::first_sentences(answer["reasoning"].as_str().unwrap_or_default(), 3),
        "confidence": answer["confidence"],
        "evidence": answer["evidence"],
        "suspected_sites": answer["suspected_sites"],
        "suspected_files": answer["suspected_files"],
        "suspected_functions": answer["suspected_functions"],
        "repro_hint": answer["repro_hint"],
        "by": "scout",
        "scout_version": SCOUT_VERSION,
        "model": MODEL,
        "at_unix_ms": now_ms,
        "cost_usd": cost_usd,
    });
    if class == "decider" {
        if let Some(label) = answer["decider_label"].as_str().filter(|l| DECIDER_LABELS.contains(l)) {
            cause["decider_case"] = json!({"label": label, "variant": answer["decider_variant"].as_str().unwrap_or("unknown"), "window": ctx.transcript});
        }
    }
    Ok(cause)
}

/// A note still needs a scout: it has no cause, or its cause came from an older scout (`SCOUT_VERSION`).
pub fn needs_scout(note: &Value) -> bool {
    let cause = &note["cause"];
    !cause.is_object() || (cause["by"] == "scout" && cause["scout_version"] != SCOUT_VERSION)
}

/// Scouts open note `id` (if it `needs_scout`): `Ok(None)` when it already has a current cause, else the cause written
/// into the note.
pub fn scout_note(inbox: &Path, id: &str, src: &Sources, ask: &mut dyn FnMut(&str, &str) -> Result<Answer>) -> Result<Option<Value>> {
    let note = bug_notes::get(inbox, id)?;
    if !needs_scout(&note) {
        return Ok(None);
    }
    let ctx = gather(&note, src);
    let answer = ask(SYSTEM, &prompt(id, &note, &ctx))?;
    let answer_text = || answer.text.chars().take(600).collect::<String>();
    let mut parsed = parse_answer(&answer.text).with_context(answer_text)?;
    verify_sites(&mut parsed, src);
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let cause = cause_from(&parsed, &ctx, answer.cost_usd, now_ms).with_context(answer_text)?;
    let saved = bug_notes::set_cause(inbox, id, &cause).with_context(answer_text)?;
    eprintln!(
        "bug scout {id}: {} ({:.2}), {} sites checked, ${:.4}",
        cause["class"].as_str().unwrap_or_default(),
        cause["confidence"].as_f64().unwrap_or_default(),
        cause["suspected_sites"].as_array().map_or(0, Vec::len),
        answer.cost_usd
    );
    Ok(saved["cause"].is_object().then(|| saved["cause"].clone()))
}

/// The `claude` CLI: `HEIMDALL_CLAUDE_BIN`, else the usual install places, else whatever `PATH` has.
pub fn claude_bin() -> PathBuf {
    if let Some(bin) = std::env::var_os("HEIMDALL_CLAUDE_BIN") {
        return bin.into();
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    [home.join(".local/bin/claude"), PathBuf::from("/opt/homebrew/bin/claude"), PathBuf::from("/usr/local/bin/claude")]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("claude"))
}

/// The arguments of one scout call. With a repo, the model reads it with `READ_TOOLS` only (no Bash, no writes) and may make
/// `MAX_TOOL_CALLS` tool calls; without one it has no tools at all.
pub fn claude_args(system: &str, repo: bool) -> Vec<String> {
    let max_turns = (MAX_TOOL_CALLS + 2).to_string();
    let tools: Vec<&str> = if repo {
        vec!["--tools", READ_TOOLS, "--allowedTools", READ_TOOLS, "--disallowedTools", "Bash,Write,Edit,NotebookEdit,WebFetch,WebSearch", "--max-turns", &max_turns]
    } else {
        vec!["--tools", ""]
    };
    let mut args = vec!["-p", "--model", MODEL, "--effort", "low"];
    args.extend(tools);
    args.extend(["--setting-sources", "", "--strict-mcp-config", "--no-session-persistence", "--output-format", "json", "--max-budget-usd", MAX_USD, "--system-prompt", system]);
    args.into_iter().map(str::to_owned).collect()
}

/// One call on the scout's model. With `repo`, the call runs from the repo's folder so the reads are relative to it;
/// without, from an empty folder (so no CLAUDE.md is read). The prompt goes on stdin; the call is stopped after `TIMEOUT`.
pub fn ask_claude(bin: &Path, system: &str, prompt: &str, repo: Option<&Path>) -> Result<Answer> {
    let scratch = std::env::temp_dir().join("heimdall-bug-scout");
    std::fs::create_dir_all(&scratch)?;
    let mut child = Command::new(bin)
        .args(claude_args(system, repo.is_some()))
        .current_dir(repo.unwrap_or(&scratch))
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", OUTPUT_TOKENS.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    let mut stdin = child.stdin.take().context("no stdin for the scout")?;
    let input = prompt.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the scout ran past {}s and was stopped", TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = writer.join();
    let out = child.wait_with_output()?;
    let v: Value = serde_json::from_slice(&out.stdout).with_context(|| format!("the scout's output: {}", String::from_utf8_lossy(&out.stderr).trim()))?;
    if v["is_error"] == true {
        bail!("the scout failed: {}", v["result"].as_str().unwrap_or("no reason given"));
    }
    Ok(Answer { text: v["result"].as_str().unwrap_or_default().to_owned(), cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0) })
}

/// One model call, made only while `allowed` says yes.
fn gated(allowed: fn() -> bool, ask: impl FnOnce() -> Result<Answer>) -> Result<Answer> {
    if !allowed() { anyhow::bail!("Agent is off: the scout asks no model; the night pass classifies it"); }
    ask()
}

struct Job {
    inbox: PathBuf,
    id: String,
    allowed: fn() -> bool,
}

static SENDER: OnceLock<mpsc::SyncSender<Job>> = OnceLock::new();
/// Serialises the queue's worker: a burst of notes runs one scout at a time.
static WORKER: Mutex<()> = Mutex::new(());

/// Queues a scout for note `id` on the one background worker. False when the scout is off (`HEIMDALL_BUG_SCOUT=off`),
/// `allowed` says no (the app's Agent switch is off), or the queue is full (the note then waits for the night pass).
/// `allowed` is asked again before every model call, so a scout queued before Agent off never asks one after it.
pub fn enqueue(inbox: &Path, id: &str, allowed: fn() -> bool) -> bool {
    if std::env::var_os("HEIMDALL_BUG_SCOUT").is_some_and(|v| v == "off") || !allowed() {
        return false;
    }
    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
        std::thread::Builder::new().name("bug-scout".into()).spawn(move || {
            for job in rx {
                let _one = WORKER.lock().unwrap_or_else(|e| e.into_inner());
                let src = Sources::for_inbox(&job.inbox);
                let bin = claude_bin();
                let result = scout_note(&job.inbox, &job.id, &src, &mut |system, prompt| gated(job.allowed, || ask_claude(&bin, system, prompt, src.code.as_deref())));
                if let Err(e) = result {
                    eprintln!("bug scout {}: {e:#}; the night pass classifies it", job.id);
                }
            }
        }).expect("bug scout thread");
        tx
    });
    match sender.try_send(Job { inbox: inbox.to_path_buf(), id: id.to_owned(), allowed }) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("bug scout: {id} not queued ({e}); the night pass classifies it");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn with_agent_off_the_scout_queues_nothing_and_asks_no_model() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!enqueue(dir.path(), "2026-10-26T00-52-29", || false), "nothing is queued");
        let mut asked = false;
        assert!(gated(|| false, || { asked = true; Ok(Answer { text: String::new(), cost_usd: 0.0 }) }).is_err());
        assert!(!asked, "no model call");
        assert!(gated(|| true, || Ok(Answer { text: "ok".into(), cost_usd: 0.0 })).is_ok());
    }
    use super::*;

    const NOW: u64 = 1_792_788_356_000;

    fn canned(text: &str) -> impl FnMut(&str, &str) -> Result<Answer> + '_ {
        move |_, _| Ok(Answer { text: text.to_owned(), cost_usd: 0.0021 })
    }

    fn inbox_with_note(text: &str) -> (tempfile::TempDir, PathBuf, String) {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let id = bug_notes::write(&inbox, text, Some("evidence line"), NOW).unwrap()["id"].as_str().unwrap().to_owned();
        (t, inbox, id)
    }

    #[test]
    fn a_scout_writes_its_cause_into_the_note_and_only_once() {
        let (_t, inbox, id) = inbox_with_note("Island froze on Working after the menu closed.");
        let src = Sources { log: inbox.join("none.log"), archive: inbox.join("none.sqlite"), code: None };
        let answer = r#"Here you go: {"class":"code","reasoning":"The island state never clears on close. The log shows no end event.","confidence":0.7,"evidence":["island stuck"],"suspected_files":["crates/ui/src/mac/mod.rs"],"suspected_functions":["show_bug_notes"],"repro_hint":"open the menu twice","decider_label":null,"decider_variant":null}"#;
        let cause = scout_note(&inbox, &id, &src, &mut canned(answer)).unwrap().expect("a cause");
        assert_eq!(cause["class"], "code");
        assert_eq!(cause["by"], "scout");
        // No repo to check against here, so no suspected file survives (verify_suspects).
        assert_eq!(cause["suspected_files"], json!([]));
        let note = bug_notes::get(&inbox, &id).unwrap();
        assert_eq!(note["cause"]["reasoning"], "The island state never clears on close. The log shows no end event.");
        let md = std::fs::read_to_string(inbox.join(format!("{id}.md"))).unwrap();
        assert!(md.contains("## Cause: code (confidence 0.70, by scout)") && md.contains("Repro: open the menu twice") && !md.contains("Files:"));
        // One scout per note: the second call does not ask the model.
        let mut calls = 0;
        let again = scout_note(&inbox, &id, &src, &mut |_, _| {
            calls += 1;
            Ok(Answer { text: String::new(), cost_usd: 0.0 })
        })
        .unwrap();
        assert!(again.is_none() && calls == 0);
    }

    #[test]
    fn a_decider_answer_carries_its_label_and_the_transcript_window() {
        let (_t, inbox, id) = inbox_with_note("User said 'I'm not talking to you, shut up' and Heimdall still acked.");
        let src = Sources { log: inbox.join("none.log"), archive: inbox.join("none.sqlite"), code: None };
        let answer = r#"{"class":"decider","reasoning":"Heimdall was named only to be told to be quiet. The decider sent an ack.","confidence":0.9,"evidence":[],"suspected_files":[],"suspected_functions":[],"repro_hint":"replay the line","decider_label":"not_for_heimdall","decider_variant":"ack"}"#;
        let cause = scout_note(&inbox, &id, &src, &mut canned(answer)).unwrap().unwrap();
        assert_eq!(cause["decider_case"]["label"], "not_for_heimdall");
        assert_eq!(cause["decider_case"]["variant"], "ack");
        assert!(cause["decider_case"]["window"].as_array().unwrap().is_empty());
        assert_eq!(bug_notes::decider_cases(&inbox).len(), 1);
        // A label outside the decider's answers is not kept as a case; the class still stands for the coordinator.
        let (_t2, inbox2, id2) = inbox_with_note("x");
        let bad = answer.replace("not_for_heimdall", "maybe");
        let cause = scout_note(&inbox2, &id2, &src, &mut canned(&bad)).unwrap().unwrap();
        assert_eq!(cause["class"], "decider");
        assert!(cause["decider_case"].is_null());
    }

    #[test]
    fn an_answer_that_is_not_a_cause_is_refused_and_leaves_the_note_open() {
        let (_t, inbox, id) = inbox_with_note("x");
        let src = Sources { log: inbox.join("none.log"), archive: inbox.join("none.sqlite"), code: None };
        assert!(scout_note(&inbox, &id, &src, &mut canned("no json here")).is_err());
        assert!(scout_note(&inbox, &id, &src, &mut canned(r#"{"class":"mystery","reasoning":"x","confidence":1}"#)).is_err());
        assert!(bug_notes::get(&inbox, &id).unwrap()["cause"].is_null());
        // A reasoning of four sentences is cut to three, not refused.
        let long = scout_note(&inbox, &id, &src, &mut canned(r#"{"class":"code","reasoning":"One. Two. Three. Four.","confidence":0.5}"#)).unwrap().unwrap();
        assert_eq!(long["reasoning"], "One. Two. Three.");
    }

    #[test]
    fn the_log_window_takes_only_lines_near_the_note_and_the_newest_of_them() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("heimdall.log");
        let at = NOW;
        let mut text = String::from("2026-10-23 10:00:00.000 agent: Ready\n");
        text += &format!("{} decider: woke the agent (trigger d1)\n", bug_notes::local_stamp(at / 1000 - 60, true));
        for i in 0..200 {
            text += &format!("{} voice: line {i}\n", bug_notes::local_stamp(at / 1000 - 30 + i / 10, true));
        }
        std::fs::write(&log, text).unwrap();
        // 201 lines fall in the window; the newest `LOG_LINES` are kept, so the oldest (the decider's) is not.
        let lines = log_lines(&log, at);
        assert_eq!(lines.len(), LOG_LINES);
        assert!(lines.last().unwrap().ends_with("voice: line 199"));
        assert!(lines.first().unwrap().ends_with("voice: line 80"));
        assert!(!lines.iter().any(|l| l.contains("10:00:00")));
    }

    #[test]
    fn repo_excerpts_name_the_files_that_share_the_notes_words() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("crates/ui/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("menu.rs"), "fn bug_notes_title() {}\nlet count = bug_notes.len();\n").unwrap();
        std::fs::write(src.join("other.rs"), "fn unrelated() {}\n").unwrap();
        std::fs::create_dir_all(t.path().join("target/debug")).unwrap();
        std::fs::write(t.path().join("target/debug/bug_notes.rs"), "bug_notes bug_notes").unwrap();
        let found = code_excerpts(t.path(), &terms("the bug_notes count is wrong"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "crates/ui/src/menu.rs");
        assert_eq!(found[0].1.len(), 2);
    }

    #[test]
    fn a_suspected_site_is_kept_only_when_the_repo_holds_that_function_at_those_lines() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("crates/x/src")).unwrap();
        // `real_name` starts on line 3.
        std::fs::write(t.path().join("crates/x/src/lib.rs"), "// header\n\n\npub fn real_name() {}\nfn other() {}\n").unwrap();
        let src = Sources { log: t.path().join("l"), archive: t.path().join("a"), code: Some(t.path().to_path_buf()) };
        let mut a = json!({"suspected_sites": [
            {"file": "crates/x/src/lib.rs", "function": "x::real_name", "lines": "1-5"},
            {"file": "crates/x/src/lib.rs", "function": "real_name", "lines": "40-50"},
            {"file": "crates/x/src/lib.rs", "function": "invented_fn", "lines": "1-5"},
            {"file": "crates/x/src/made_up.rs", "function": "real_name", "lines": "1-5"},
            {"file": "../outside.rs", "function": "real_name", "lines": "1-5"},
            {"file": "crates/x/src/lib.rs", "function": "other", "lines": "5-5"},
        ]});
        verify_sites(&mut a, &src);
        let sites = a["suspected_sites"].as_array().unwrap();
        assert_eq!(sites.len(), 2, "{sites:?}");
        assert_eq!(sites[0]["at_line"], 4);
        assert_eq!(sites[1]["function"], "other");
        assert_eq!(a["suspected_files"], json!(["crates/x/src/lib.rs"]));
        assert_eq!(a["suspected_functions"], json!(["x::real_name", "other"]));
        // No repo, no survivors.
        verify_sites(&mut a, &Sources { code: None, ..src });
        assert_eq!(a["suspected_sites"], json!([]));
    }

    #[test]
    fn the_scout_runs_read_only_over_the_repo_and_without_tools_when_there_is_none() {
        let repo = claude_args("sys", true);
        let flag = |args: &[String], f: &str| args.iter().position(|a| a == f).map(|i| args[i + 1].clone());
        assert_eq!(flag(&repo, "--tools").as_deref(), Some("Read,Grep,Glob"));
        assert_eq!(flag(&repo, "--allowedTools").as_deref(), Some("Read,Grep,Glob"));
        assert!(flag(&repo, "--disallowedTools").unwrap().contains("Bash") && flag(&repo, "--disallowedTools").unwrap().contains("Write"));
        assert_eq!(flag(&repo, "--max-turns").unwrap(), (MAX_TOOL_CALLS + 2).to_string());
        assert_eq!(flag(&repo, "--max-budget-usd").as_deref(), Some(MAX_USD));
        let bare = claude_args("sys", false);
        assert_eq!(flag(&bare, "--tools").as_deref(), Some(""));
        assert!(flag(&bare, "--allowedTools").is_none());
        assert!(TIMEOUT.as_secs() == 180 && MAX_TOOL_CALLS == 20);
    }

    #[test]
    fn a_note_from_an_older_scout_is_scouted_again_and_a_coordinators_cause_is_kept() {
        let old = json!({"cause": {"by": "scout", "class": "code"}});
        let current = json!({"cause": {"by": "scout", "class": "code", "scout_version": SCOUT_VERSION}});
        let by_hand = json!({"cause": {"by": "coordinator", "class": "code"}});
        assert!(needs_scout(&json!({"cause": null})));
        assert!(needs_scout(&old));
        assert!(!needs_scout(&current));
        assert!(!needs_scout(&by_hand));
    }

    #[test]
    fn terms_drop_stop_words_and_short_words() {
        assert_eq!(terms("Heimdall, the echo guard and the owner's island!"), ["island", "guard", "echo"]);
    }

    #[test]
    fn the_prompt_names_each_part_of_the_context() {
        let note = json!({"created_local": "2026-10-23 22:45:56", "note": {"text": "false wake"}, "evidence": "[22:45:44] line"});
        let ctx = Reading { log: vec!["2026-10-23 22:45:44 decider: woke".into()], transcript: vec![json!({"time": "22:45:44", "speaker": "User", "text": "Hi"})], code: vec![("crates/x.rs".into(), vec!["1: fn a()".into()])] };
        let p = prompt("n1", &note, &ctx);
        for part in ["BUG NOTE n1", "EVIDENCE filed", "HEIMDALL LOG", "TRANSCRIPT", "[22:45:44 User] Hi", "CODE, repo lines", "crates/x.rs"] {
            assert!(p.contains(part), "{part}");
        }
    }

    #[test]
    fn a_fenced_answer_parses_and_a_missing_claude_is_an_error_not_a_hang() {
        let v = parse_answer("```json\n{\"class\":\"unknown\",\"reasoning\":\"x\",\"confidence\":0.1}\n```").unwrap();
        assert_eq!(v["class"], "unknown");
        let missing = Path::new("/nonexistent/claude-for-the-test");
        assert!(ask_claude(missing, "s", "p", None).is_err());
    }

    #[test]
    fn the_labels_and_classes_the_scout_may_name_are_the_ones_the_notes_accept() {
        for class in CAUSE_CLASSES {
            let cause = json!({"class": class, "reasoning": "A reason.", "confidence": 0.5, "by": "scout"});
            assert!(bug_notes::check_cause(&cause).is_ok(), "{class}");
        }
        for label in DECIDER_LABELS {
            let cause = json!({"class": "decider", "reasoning": "A reason.", "confidence": 0.5, "decider_case": {"label": label}});
            assert!(bug_notes::check_cause(&cause).is_ok(), "{label}");
        }
    }
}
