//! Code-driven job islands ("Islands: code-driven first"): a pure reducer from a bound thread's
//! T3 activity items, its sub-agents and a TrainingLab run to what its island shows, with no model, within one T3 pass
//! (30 s). Before, the binding wrote only state and a stage on a state change and the plan refresher named stages
//! every 5–30 minutes, so an island sat on "Starting" for 25 minutes (Assistant/Plans/background-jobs/design.html).
//!
//! The rules are the prototype's (`Assistant/Plans/island-design/tools/extract_stages.py`, the design.html signal
//! table): each item maps to a signal; a stage changes when a signal arrives, and a weak stage (Reading, Editing) must
//! hold `WEAK_HOLD_S` before it replaces the current one, so the pill does not flicker. Training comes only from a live
//! trainlog run (round 1: command text alone called edits to `train_v5.py` training), Paused from a stopped one.
//! Counters (files changed, commits, tests passed, sub-agents, background commands), the agent's latest sentence and
//! the latest picture accumulate beside it. The compact history is round 2's (`key_phases`/`collapse` in
//! `tools/build_round2.py`), drawn as look A's rail of steps (the user has not picked a round-2 look; A is the default).
//!
//! Items are the activity view's (`t3_thread_read` `view: "activity"`): `{type, status, title, text, updatedAt, runId,
//! position, itemId}`; a command's text is `"$ cmd\n output"`, a file change's first line its path, a dynamic tool's
//! text the JSON of `toolName`, `input`, `output`. Incremental: one `Reducer` per run, fed only new items.
//! Its scenario tests replay recorded threads and stay in the private repository.

use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// A weak stage replaces the current one only once it has held this long.
pub const WEAK_HOLD_S: f64 = 45.0;
const WEAK: [&str; 2] = ["Reading", "Editing"];
/// Stages the compact history folds into one "Preparing" (round 2).
const PREPARING: [&str; 5] = ["Reading", "Checking", "Editing", "Committing", "Building"];
/// Stages the history always keeps visible.
const KEEP: [&str; 3] = ["Paused", "Your answer", "Reviewing"];
/// Phases that never fold into the one before, however short (round 1's `phases`).
const STRONG: [&str; 4] = ["Reviewing", "Sub-agents", "Paused", "Your answer"];
/// Most stages in the history; older ones fold into "Earlier".
pub const HISTORY: usize = 4;
/// A history segment under this share of the task gives its time to the one before.
pub const FOLD_SHARE: f64 = 0.06;
/// The summary line: the agent's first sentence, about this long (round 1: 400 pt wraps past 45 characters).
pub const SUMMARY_CHARS: usize = 44;
/// Counters on the island (`kv`).
pub const KV_CAP: usize = 3;
/// Pictures remembered per run, newest first.
const PICTURES: usize = 8;
const SOURCES: usize = 6;

// ---------------------------------------------------------------------------------------------------------
// Matching (the prototype's regular expressions, by hand: the app has no regex engine).

fn word(c: char) -> bool { c.is_alphanumeric() || c == '_' }

/// Every start of `pat` in `s` with a word boundary before it (`\bpat`) and, when `end`, after it (`pat\b`).
fn bounded(s: &str, pat: &str, end: bool) -> bool {
    let mut from = 0;
    while let Some(i) = s[from..].find(pat) {
        let at = from + i;
        let before = s[..at].chars().next_back();
        let after = s[at + pat.len()..].chars().next();
        let first = pat.chars().next().unwrap_or(' ');
        let last = pat.chars().next_back().unwrap_or(' ');
        let start_ok = !word(first) || before.is_none_or(|c| !word(c));
        let end_ok = !end || !word(last) || after.is_none_or(|c| !word(c));
        if start_ok && end_ok { return true }
        from = at + pat.len().max(1);
    }
    false
}

/// `pat` followed by a run of `class` characters that contains `tail` at least one character in.
fn followed(s: &str, pat: &str, class: impl Fn(char) -> bool, tails: &[&str]) -> bool {
    let mut from = 0;
    while let Some(i) = s[from..].find(pat) {
        let start = from + i + pat.len();
        let run: String = s[start..].chars().take_while(|c| class(*c)).collect();
        if tails.iter().any(|t| run.char_indices().any(|(k, _)| k >= 1 && run[k..].starts_with(t))) { return true }
        from = start;
    }
    false
}

/// `BUILD = \b(cargo (build|run)|xcodebuild|swift build|scripts/(build|bundle)[\w_]*\.sh|make\b|npm run build)`
pub fn is_build(c: &str) -> bool {
    ["cargo build", "cargo run", "xcodebuild", "swift build", "npm run build"].iter().any(|p| bounded(c, p, false))
        || bounded(c, "make", true)
        || ["scripts/build", "scripts/bundle"].iter().any(|p| {
            let mut from = 0;
            while let Some(i) = c[from..].find(p) {
                let at = from + i;
                let ok = c[..at].chars().next_back().is_none_or(|ch| !word(ch));
                let rest: String = c[at + p.len()..].chars().take_while(|ch| word(*ch)).collect();
                if ok && c[at + p.len() + rest.len()..].starts_with(".sh") { return true }
                from = at + p.len();
            }
            false
        })
}

/// `TEST = \b(cargo test|cargo nextest|pytest|swift test|unittest|npm test|vitest)\b`
pub fn is_test(c: &str) -> bool {
    ["cargo test", "cargo nextest", "pytest", "swift test", "unittest", "npm test", "vitest"].iter().any(|p| bounded(c, p, true))
}

/// `PROBE`: a built binary, a self-test, a render, a screenshot or a lab script.
pub fn is_probe(c: &str) -> bool {
    if ["target/release/", "target/debug/", ".app/Contents/MacOS/", "--selftest", "--render", "screencapture"].iter().any(|p| c.contains(p)) { return true }
    // build/[\w.]+\.app/
    let mut from = 0;
    while let Some(i) = c[from..].find("build/") {
        let start = from + i + 6;
        let run: String = c[start..].chars().take_while(|ch| word(*ch) || *ch == '.').collect();
        // Greedy then back off: some ".app" inside the run, one character in, followed by "/".
        if run.char_indices().any(|(k, _)| k >= 1 && run[k..].starts_with(".app") && c[start + k + 4..].starts_with('/')) { return true }
        from = start;
    }
    followed(c, "lab/", |ch| word(ch) || "/.-".contains(ch), &[".sh", ".py"])
}

/// `COMMIT = \bgit (commit|merge|rebase)(?![-\w])`
pub fn is_commit(c: &str) -> bool {
    ["git commit", "git merge", "git rebase"].iter().any(|p| {
        let mut from = 0;
        while let Some(i) = c[from..].find(p) {
            let at = from + i;
            let before = c[..at].chars().next_back().is_none_or(|ch| !word(ch));
            let after = c[at + p.len()..].chars().next().is_none_or(|ch| !(word(ch) || ch == '-'));
            if before && after { return true }
            from = at + p.len();
        }
        false
    })
}

/// `TRAIN = (train[\w_]*\.py|trainlog|mlx_lm\.lora|--max-steps)`: a hint only (no stage without a live run).
pub fn is_train(c: &str) -> bool {
    c.contains("trainlog") || c.contains("mlx_lm.lora") || c.contains("--max-steps") || {
        let mut from = 0;
        let mut hit = false;
        while let Some(i) = c[from..].find("train") {
            let start = from + i + 5;
            let run: String = c[start..].chars().take_while(|ch| word(*ch)).collect();
            if c[start + run.len()..].starts_with(".py") { hit = true; break }
            from = start;
        }
        hit
    }
}

/// `FETCH = \b(curl|wget|hf download|huggingface-cli)\b`
pub fn is_fetch(c: &str) -> bool { ["curl", "wget", "hf download", "huggingface-cli"].iter().any(|p| bounded(c, p, true)) }

/// Model and data files a download brings (a README or a JSON answer fetched with `curl -o` is research).
const DATA_EXT: [&str; 26] = [".safetensors", ".gguf", ".bin", ".pt", ".pth", ".ckpt", ".npz", ".npy", ".onnx", ".mlmodel", ".zip", ".tar",
    ".gz", ".tgz", ".xz", ".zst", ".parquet", ".arrow", ".wav", ".flac", ".mp3", ".mp4", ".mov", ".dmg", ".pkg", ".jsonl"];

/// Downloading (a download read "Reading"), on the command's first line: `hf download`,
/// `huggingface-cli download`, `git lfs pull`, `ollama pull`, or `curl`/`wget` of a model, data or archive file.
pub fn is_download(c: &str) -> bool {
    let c = c.lines().next().unwrap_or("");
    let data = |c: &str| c.split(|ch: char| ch.is_whitespace() || "'\"".contains(ch)).any(|t| { let l = t.to_ascii_lowercase(); DATA_EXT.iter().any(|e| l.ends_with(e)) });
    ["hf download", "huggingface-cli download", "git lfs pull", "ollama pull"].iter().any(|p| bounded(c, p, true))
        || ((bounded(c, "wget", true) || bounded(c, "curl", true)) && data(c))
}

/// Labelling (a 5 h labelling run read "Reading"), on the command's first line: a labelling or annotation script
/// (`label_v5.py`, `annotate.sh`), or a command that says labelling.
pub fn is_labelling(c: &str) -> bool {
    let l = c.lines().next().unwrap_or("").to_ascii_lowercase();
    if l.contains("labelling") || l.contains("labeling") { return true }
    l.split(|ch: char| !(word(ch) || ch == '-' || ch == '.')).any(|tok| {
        (tok.ends_with(".py") || tok.ends_with(".sh")) && (tok.contains("label") || tok.contains("annotat"))
    })
}

/// Drafting (drafting a page read "Reading"): a draft, brief or outline file, or a page or note a script writes into
/// Assistant/ (`python3 - <<EOF … open("…/Assistant/Research/x.html", "w")`, `> …/Assistant/Plans/x.md`).
pub fn is_draft_path(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    ["draft", "brief", "outline"].iter().any(|w| l.contains(w)) && (l.ends_with(".md") || l.ends_with(".txt") || l.ends_with(".html"))
}

/// A deliverable a script or a redirect writes in the command (not a heredoc `cat >`, which is Writing).
pub fn drafted(c: &str) -> Option<String> {
    let toks: Vec<&str> = c.split(|ch: char| ch.is_whitespace() || "'\"(),".contains(ch)).filter(|t| !t.is_empty()).collect();
    let writes = c.contains("open(") && (c.contains("'w'") || c.contains("\"w\"")) || c.contains(" > ") || c.contains(" tee ") || c.contains(".write_text(");
    if !writes { return None }
    toks.into_iter().find(|t| (t.ends_with(".html") || t.ends_with(".md")) && (t.contains("Assistant/") || is_draft_path(t)) && !t.starts_with('-')).map(str::to_owned)
}

/// `WRITE = cat > ([^\s<]+) <<`: the file a heredoc writes.
pub fn written(c: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = c[from..].find("cat > ") {
        let start = from + i + 6;
        let tok: String = c[start..].chars().take_while(|ch| !ch.is_whitespace() && *ch != '<').collect();
        if !tok.is_empty() && c[start + tok.len()..].starts_with(" <<") { return Some(tok) }
        from = start;
    }
    None
}

/// `DELIV`: a deliverable (research, a plan, a report, a page), not code.
pub fn deliverable(p: &str) -> bool {
    p.contains("Assistant/Research/") || p.contains("Assistant/Plans/") || p.contains("report") || p.contains("index.html") || p.ends_with(".html")
}

/// The machine-readable status a task may keep for its island (`islands/docs/model-html.md`): any file of this name.
pub const STATUS_FILE: &str = "island-status.json";

/// The last absolute (or `~/`) path to an `island-status.json` named in `s`.
pub fn status_in(s: &str) -> Option<String> {
    let class = |c: char| word(c) || "/.~-".contains(c);
    let mut found = None;
    let mut from = 0;
    while let Some(i) = s[from..].find(STATUS_FILE) {
        let end = from + i + STATUS_FILE.len();
        let start = s[..end].char_indices().rev().take_while(|(_, c)| class(*c)).last().map(|(k, _)| k).unwrap_or(end);
        let p = &s[start..end];
        if (p.starts_with('/') || p.starts_with("~/")) && s[end..].chars().next().is_none_or(|c| !word(c)) { found = Some(p.to_owned()) }
        from = end;
    }
    found
}

/// `IMG = [\w/.~-]+\.(png|jpg|jpeg|webp|gif)\b` (any case): the leftmost picture path, as `re.search` finds it.
pub fn image_in(s: &str) -> Option<String> {
    const EXT: [&str; 5] = [".png", ".jpg", ".jpeg", ".webp", ".gif"];
    let class = |c: char| word(c) || "/.~-".contains(c);
    let lower = s.to_ascii_lowercase();
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut k = 0;
    while k < chars.len() {
        if !class(chars[k].1) { k += 1; continue }
        let start = chars[k].0;
        let mut j = k;
        while j < chars.len() && class(chars[j].1) { j += 1 }
        let end = if j < chars.len() { chars[j].0 } else { s.len() };
        // The rightmost extension in the run followed by a word boundary, at least one character in.
        let mut best: Option<usize> = None;
        for ext in EXT {
            let mut from = start + 1;
            while from < end {
                let Some(i) = lower[from..end].find(ext) else { break };
                let e = from + i + ext.len();
                if s[e..].chars().next().is_none_or(|c| !word(c)) { best = Some(best.map_or(e, |b: usize| b.max(e))) }
                from = from + i + 1;
            }
        }
        if let Some(e) = best { return Some(s[start..e].to_owned()) }
        k = j;
    }
    None
}

/// A command item's text split into the command and its output: the first line, plus the lines a heredoc, a
/// trailing backslash or an open quote carry on to (the activity view joins them as `"$ cmd\noutput"`).
pub fn split_command(text: &str) -> (String, String) {
    let t = text.strip_prefix("$ ").unwrap_or(text);
    let lines: Vec<&str> = t.split('\n').collect();
    let mut n = 0;
    let mut markers: Vec<String> = Vec::new();
    let (mut single, mut double) = (false, false);
    while n < lines.len() {
        let line = lines[n];
        n += 1;
        if let Some(m) = markers.first() {
            if line.trim() == m { markers.remove(0); }
            if markers.is_empty() && !single && !double && !line.trim_end().ends_with('\\') && !opens_more(line) { break }
            continue;
        }
        // Quotes and heredoc markers on a command line.
        let cs: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i];
            if c == '\\' && !single { i += 2; continue }
            if c == '\'' && !double { single = !single }
            else if c == '"' && !single { double = !double }
            else if c == '<' && !single && !double && cs.get(i + 1) == Some(&'<') && cs.get(i + 2) != Some(&'<') {
                let mut j = i + 2;
                if cs.get(j) == Some(&'-') { j += 1 }
                while cs.get(j) == Some(&' ') { j += 1 }
                let q = cs.get(j).copied().filter(|q| *q == '\'' || *q == '"');
                if q.is_some() { j += 1 }
                let m: String = cs[j.min(cs.len())..].iter().take_while(|c| word(**c)).collect();
                if !m.is_empty() { markers.push(m.clone()); j += m.chars().count(); if q.is_some() { j += 1 } }
                i = j;
                continue;
            }
            i += 1;
        }
        if markers.is_empty() && !single && !double && !line.trim_end().ends_with('\\') { break }
    }
    let n = n.min(lines.len());
    (lines[..n].join("\n"), lines[n..].join("\n"))
}

/// A heredoc's end line that opens another (rare): carried on.
fn opens_more(line: &str) -> bool { line.contains("<<") && !line.contains("<<<") }

// ---------------------------------------------------------------------------------------------------------
// Signals.

#[derive(Clone, Debug, PartialEq)]
enum Sig {
    Owner,
    Question,
    Research,
    /// A file written: its path; a draft is Drafting, a deliverable Writing, else Editing.
    Edit(String),
    /// A page or draft a script writes.
    Draft(String),
    Download,
    Labelling,
    Image(String),
    /// A sub-agent brief: a review is Reviewing.
    Brief { review: bool },
    Read,
    Check(Option<String>),
    Test,
    Build,
    Commit,
    Say(String),
    None,
}

impl Sig {
    fn stage(&self) -> Option<&'static str> {
        Some(match self {
            Sig::Question => "Your answer",
            Sig::Research => "Researching",
            Sig::Edit(p) => if is_draft_path(p) { "Drafting" } else if deliverable(p) { "Writing" } else { "Editing" },
            Sig::Draft(_) => "Drafting",
            Sig::Download => "Downloading",
            Sig::Labelling => "Labelling",
            Sig::Brief { review: true } => "Reviewing",
            Sig::Read => "Reading",
            Sig::Check(_) => "Checking",
            Sig::Test => "Testing",
            Sig::Build => "Building",
            Sig::Commit => "Committing",
            _ => return None,
        })
    }
}

/// A dynamic tool's name and input from its text (the JSON may be cut off by `maxCharsPerItem`).
fn tool_parts(text: &str) -> (String, String) {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let input = if v["input"].is_null() { String::new() } else { v["input"].to_string() };
        return (v["toolName"].as_str().unwrap_or("").to_owned(), input);
    }
    let name = text.find("\"toolName\": \"").map(|at| text[at + 13..].split('"').next().unwrap_or("").to_owned()).unwrap_or_default();
    let input = match (text.find("\"input\""), text.find("\"output\"")) {
        (Some(a), Some(b)) if b > a => text[a..b].to_owned(),
        (Some(a), _) => text[a..].to_owned(),
        _ => String::new(),
    };
    (name, input)
}

fn classify(it: &Value) -> Sig {
    let text = it["text"].as_str().unwrap_or("");
    match it["type"].as_str().unwrap_or("") {
        "user_message" => Sig::Owner,
        "approval_request" | "user_input_request" => Sig::Question,
        "web_search" => Sig::Research,
        "file_change" => Sig::Edit(text.lines().next().unwrap_or("").trim().to_owned()),
        "dynamic_tool" => {
            let (n, i) = tool_parts(text);
            if n == "WebFetch" || n == "WebSearch" { return Sig::Research }
            if n.contains("html_preview") || n.contains("html_render") || n.to_lowercase().contains("screenshot") || n.contains("preview_snapshot") { return Sig::Check(None) }
            if n == "Read" { if let Some(p) = image_in(&i) { return Sig::Image(p) } }
            if n.to_lowercase().contains("download") { return Sig::Download }
            if n == "Agent" || n == "Task" { return Sig::Brief { review: i.to_lowercase().contains("review") } }
            Sig::Read
        }
        "command_execution" => {
            let (c, _) = split_command(text);
            if is_test(&c) { return Sig::Test }
            if is_build(&c) { return Sig::Build }
            if is_commit(&c) { return Sig::Commit }
            if is_labelling(&c) { return Sig::Labelling }
            if is_download(&c) { return Sig::Download }
            if let Some(w) = written(&c) { return Sig::Edit(w) }
            if let Some(d) = drafted(&c) { return Sig::Draft(d) }
            if is_probe(&c) { return Sig::Check(image_in(&c)) }
            if is_fetch(&c) { return Sig::Research }
            Sig::Read
        }
        "assistant_message" => Sig::Say(text.to_owned()),
        _ => Sig::None,
    }
}

/// The agent's first sentence, as the prototype cut it (at most 140 characters), without markdown marks.
pub fn first_sentence(text: &str) -> String {
    let t = text.trim().replace(['*', '`'], "");
    let t = t.trim_start_matches(['#', '-', ' ']);
    let mut end = t.len();
    for (i, c) in t.char_indices() {
        if c == '\n' { end = i; break }
        if ".!?".contains(c) && t[i + 1..].starts_with(char::is_whitespace) { end = i + 1; break }
    }
    t[..end].chars().take(140).collect::<String>().trim().to_owned()
}

/// Round 1's `short`: the sentence's head before a semicolon, dash or colon, about `SUMMARY_CHARS`, cut at a word.
pub fn short(t: &str) -> Option<String> {
    // At a colon that starts a clause ("Bug 1: …"), not inside a time ("at 05:40").
    let t = t.split(';').next()?.split(" \u{2014} ").next()?.split(": ").next()?.trim().trim_end_matches(['.', ':']);
    if t.is_empty() { return None }
    if t.chars().count() <= SUMMARY_CHARS { return Some(t.to_owned()) }
    let cut: String = t.chars().take(SUMMARY_CHARS - 1).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a).trim_end_matches([',', ' ']);
    Some(format!("{cut}\u{2026}"))
}

/// The time an item happened (`updatedAt`), epoch seconds.
pub fn at_of(it: &Value) -> Option<f64> {
    let t = it["updatedAt"].as_str().or(it["startedAt"].as_str())?;
    chrono::DateTime::parse_from_rfc3339(t).ok().map(|t| t.timestamp_millis() as f64 / 1000.0)
}

// ---------------------------------------------------------------------------------------------------------
// The reducer.

/// One run of one bound thread, as code reads it.
#[derive(Clone, Debug, Default)]
pub struct Reducer {
    pub run: String,
    /// The run's start (epoch s): the island clock.
    pub started: f64,
    /// The stage items say (the prototype's `state["stage"]`).
    base: String,
    /// A training run's stage over it (Training, Paused), when one is live.
    pin: Option<String>,
    /// The stage shown and when it began.
    shown: String,
    since: f64,
    pending: Option<(f64, String)>,
    /// Every change of the shown stage: (at, stage), oldest first.
    changes: Vec<(f64, String)>,
    files: BTreeSet<String>,
    commits: u32,
    passed: Option<u64>,
    subs_open: Vec<String>,
    subs_done: u32,
    subs_total: u32,
    sources: Vec<String>,
    pictures: Vec<String>,
    say: Option<String>,
    /// Background commands started and not reported finished (when each started).
    background: Vec<f64>,
    /// When training commands ran, and the runs commands named (`train::find`).
    pub train_hints: Vec<f64>,
    pub runs_named: Vec<String>,
    train: Option<super::train::Train>,
    asked: Option<f64>,
    /// A labelling run or download running in the background: weak stages do not replace it while it runs.
    held: Option<String>,
    last_at: f64,
    /// The newest item position read.
    pub position: Option<u64>,
    /// Items fed (tests, logs).
    pub items: usize,
    /// For the island agent's signals: the user's brief (the run's first message), his latest message, the latest agent
    /// sentences and stage signals with their times, when the newest picture was named, the open approval, and when
    /// a helper last finished.
    brief: Option<String>,
    owner: Option<String>,
    recent: std::collections::VecDeque<(f64, String)>,
    picture_at: Option<f64>,
    approval: Option<String>,
    subs_closed_at: Option<f64>,
    subs_names: Vec<String>,
    /// The stage a question interrupted (`answered` goes back to it).
    before_question: Option<String>,
    /// The newest `island-status.json` the run named (`status_in`).
    status: Option<String>,
}

/// The stage as the compact history and the island agent's triggers name it: weak stages are one Preparing.
pub fn key_stage(stage: &str) -> &str {
    match stage { "Reading" | "Checking" | "Editing" | "Committing" | "Building" | "Starting" => "Preparing", s => s }
}

/// Lines kept for the island agent's `recent`.
pub const RECENT: usize = 12;
/// Characters of the user's brief the island agent gets.
pub const BRIEF_CHARS: usize = 900;

impl Reducer {
    pub fn new(run: &str, started: f64) -> Reducer {
        Reducer { run: run.to_owned(), started, base: "Starting".into(), shown: "Starting".into(), since: started, changes: vec![(started, "Starting".into())], last_at: started, ..Default::default() }
    }

    /// The stage the island shows: a question over everything, then a live training run, then the items' stage.
    fn display(&self) -> String {
        if self.base == "Your answer" { return self.base.clone() }
        self.pin.clone().unwrap_or_else(|| self.base.clone())
    }

    fn settle(&mut self, at: f64) {
        let now = self.display();
        if now != self.shown {
            self.shown = now.clone();
            self.since = at.max(self.since);
            self.changes.push((self.since, now));
        }
    }

    fn set_base(&mut self, stage: &str, at: f64) {
        self.base = stage.to_owned();
        self.settle(at);
    }

    /// One activity item (any order of feeding within a read is the read's: by position).
    pub fn item(&mut self, it: &Value) {
        if let Some(p) = it["position"].as_u64() {
            if self.position.is_some_and(|q| p <= q) { return }
            self.position = Some(p);
        }
        if !self.run.is_empty() && it["runId"].as_str().is_some_and(|r| r != self.run) { return }
        self.items += 1;
        let at = at_of(it).unwrap_or(self.last_at).max(self.started);
        self.last_at = self.last_at.max(at);
        let text = it["text"].as_str().unwrap_or("");
        if let Some(p) = status_in(text) { self.status = Some(p) }
        let sig = classify(it);
        // Counters first, as the prototype's snapshot does.
        match &sig {
            Sig::Draft(p) => { self.files.insert(p.clone()); }
            Sig::Edit(p) => {
                if !p.is_empty() { self.files.insert(p.clone()); }
                if image_in(p).is_some_and(|i| i == *p) { self.picture(p) }
            }
            Sig::Image(p) => self.picture(p),
            Sig::Check(Some(p)) => self.picture(p),
            Sig::Commit => self.commits += 1,
            Sig::Test => {
                let (_, out) = split_command(text);
                let sum: u64 = passed_counts(&out).iter().sum();
                if !passed_counts(&out).is_empty() { self.passed = Some(sum) }
            }
            Sig::Research => {
                for u in urls(text).into_iter().take(3) { if !self.sources.contains(&u) { self.sources.push(u) } }
                if self.sources.len() > SOURCES { self.sources.remove(0); }
            }
            Sig::Say(t) => {
                let s = first_sentence(t);
                if !s.is_empty() { self.say = Some(s.clone()); self.note(at, format!("said: {s}")) }
            }
            Sig::Owner => {
                let t = text.trim();
                // Heimdall's task message carries its harness preamble first: the brief is what follows the rule.
                let brief = t.rsplit_once("\n---\n").map_or(t, |(_, b)| b).trim();
                if self.brief.is_none() { self.brief = Some(brief.chars().take(BRIEF_CHARS).collect()) }
                else { self.owner = Some(t.chars().take(400).collect()) }
            }
            _ => {}
        }
        // The stage signals the agent's log shows (`build: cargo build -p app`), and an open approval.
        let tag = match &sig {
            Sig::Build => Some("build"), Sig::Test => Some("test"), Sig::Check(_) => Some("check"), Sig::Research => Some("research"),
            Sig::Commit => Some("commit"), Sig::Download => Some("download"), Sig::Labelling => Some("labelling"), Sig::Draft(_) => Some("draft"),
            Sig::Edit(_) => Some("edit"), Sig::Question => Some("question"), _ => None,
        };
        if let Some(tag) = tag {
            let what = match it["type"].as_str() {
                Some("command_execution") => split_command(text).0.lines().next().unwrap_or("").chars().take(120).collect::<String>(),
                Some("file_change") => text.lines().next().unwrap_or("").to_owned(),
                _ => text.lines().next().unwrap_or("").chars().take(120).collect(),
            };
            self.note(at, format!("{tag}: {what}"));
        }
        if sig == Sig::Question { self.approval = Some(text.chars().take(600).collect()) }
        else if sig.stage().is_some() || matches!(sig, Sig::Say(_)) { self.approval = None }
        // Background commands and training hints.
        if it["type"] == "command_execution" {
            let (c, _) = split_command(text);
            if text.lines().skip(1).any(|l| l.contains("Command running in background with ID") || l.contains("was moved to the background")) {
                self.background.push(at);
                // A labelling run or a download left going in the background is the stage while the agent watches it.
                if matches!(sig, Sig::Labelling | Sig::Download) { self.held = sig.stage().map(str::to_owned) }
            }
            if is_train(&c) { self.train_hints.push(at) }
            for n in super::train::named_runs(&c) { if !self.runs_named.contains(&n) { self.runs_named.push(n) } }
        }
        let lower = text.to_ascii_lowercase();
        if matches!(it["type"].as_str(), Some("user_message" | "notification")) && lower.contains("background task")
            && ["finished", "completed", "failed", "killed"].iter().any(|w| lower.contains(w)) {
            self.background.clear();
        } else if it["type"] == "notification" && lower.starts_with("command ") && ["finished", "completed", "failed", "killed"].iter().any(|w| lower.contains(w)) {
            // One background command reported back (T3's notification: `Command "…" finished`).
            if !self.background.is_empty() { self.background.remove(0); }
        }
        if self.background.is_empty() { self.held = None }
        if sig == Sig::Question {
            if self.base != "Your answer" { self.before_question = Some(self.base.clone()) }
            self.asked.get_or_insert(at);
            self.set_base("Your answer", at);
            return;
        }
        let Some(stage) = sig.stage() else { return };
        // Answered: the next stage signal takes over at once (review: it waited for a strong stage or 45 s).
        if self.base == "Your answer" { self.asked = None; self.pending = None; self.set_base(stage, at); return }
        if (self.base == "Sub-agents" || self.base == "Reviewing" || self.held.as_deref() == Some(self.base.as_str())) && WEAK.contains(&stage) { return }
        if stage == self.base { self.pending = None; return }
        if WEAK.contains(&stage) {
            match &self.pending {
                Some((t, s)) if s == stage && at - t >= WEAK_HOLD_S => { self.pending = None; self.set_base(stage, at) }
                Some((_, s)) if s == stage => {}
                _ => self.pending = Some((at, stage.to_owned())),
            }
            return;
        }
        self.pending = None;
        self.set_base(stage, at);
    }

    /// A test command's whole output, read after it finished (it was still running when its item was read).
    pub fn test_result(&mut self, text: &str) {
        let n = passed_counts(&split_command(text).1);
        if !n.is_empty() { self.passed = Some(n.iter().sum()) }
    }

    fn note(&mut self, at: f64, line: String) {
        self.recent.push_back((at, line));
        while self.recent.len() > RECENT { self.recent.pop_front(); }
    }

    fn picture(&mut self, p: &str) {
        self.picture_at = Some(self.last_at);
        self.pictures.retain(|x| x != p);
        self.pictures.insert(0, p.to_owned());
        self.pictures.truncate(PICTURES);
    }

    /// The sub-agents at work now (their titles) and how many there were, at `at`. Opens and closes are what changed
    /// since the last look, as the prototype's sub-agent events.
    pub fn subagents(&mut self, open: &[String], total: usize, at: f64) {
        let before = self.subs_open.clone();
        let opened: Vec<&String> = open.iter().filter(|t| !before.contains(t)).collect();
        let closed = before.iter().filter(|t| !open.contains(t)).count();
        if opened.is_empty() && closed == 0 { self.subs_total = self.subs_total.max(total as u32); return }
        self.subs_done += closed as u32;
        if closed > 0 { self.subs_closed_at = Some(at.max(self.last_at)) }
        for t in &opened { if !self.subs_names.contains(t) { self.subs_names.push((*t).clone()) } }
        self.subs_total = (self.subs_total + opened.len() as u32).max(total as u32);
        self.subs_open = open.to_vec();
        let at = at.max(self.last_at);
        self.last_at = at;
        let new = if self.subs_open.is_empty() { None } else if self.subs_open.iter().any(|t| { let l = t.to_lowercase(); l.contains("review") || l.contains("check") }) { Some("Reviewing") } else { Some("Sub-agents") };
        match new {
            Some(n) if self.base != n => self.set_base(n, at),
            None if self.base == "Sub-agents" || self.base == "Reviewing" => self.set_base("Reading", at),
            _ => {}
        }
    }

    /// The training run the thread drives, as of `at` (`None`: none found).
    pub fn training(&mut self, t: Option<super::train::Train>, at: f64) {
        let pin = t.as_ref().and_then(|t| t.status).and_then(|s| s.stage()).map(str::to_owned);
        let since = t.as_ref().map_or(at, |t| t.since);
        self.train = t;
        if pin != self.pin {
            self.pin = pin;
            // The run's own clock: Training from when it started (or resumed), Paused from the stop.
            let when = if self.pin.is_some() { since.max(self.since).min(at.max(self.since)) } else { at.max(self.since) };
            self.settle(when);
        }
    }

    /// The thread asks the user (T3 waiting, a pending request) as of `at`.
    pub fn asking(&mut self, at: f64) { self.asked.get_or_insert(at.max(self.since)); }

    /// The thread no longer asks (the user answered, or the request went): a question stage goes back to the work it
    /// interrupted at once, not after the next strong signal (review, 30 Oct).
    pub fn answered(&mut self, at: f64) {
        self.asked = None;
        if self.base == "Your answer" {
            let back = self.before_question.take().filter(|b| b != "Your answer" && b != "Starting").unwrap_or_else(|| "Reading".into());
            self.set_base(&back, at.max(self.since));
        }
    }

    /// The latest picture the run named (not checked: the caller copies the first real one).
    pub fn pictures(&self) -> &[String] { &self.pictures }

    /// What the island shows now.
    pub fn signals(&self, now: f64) -> Signals {
        let open_bg = self.background.iter().filter(|t| now - **t < 3600.0).count();
        let progress = self.train.as_ref().and_then(|t| Some((t.step? as f64, t.total? as f64))).filter(|(d, t)| *t > 0.0 && *d <= *t)
            .or_else(|| self.say.as_deref().and_then(n_of_m));
        Signals {
            run: self.run.clone(),
            started: self.started,
            now,
            stage: self.shown.clone(),
            stage_since: self.since,
            changes: self.changes.clone(),
            asked_since: self.asked,
            say: self.say.clone(),
            summary: self.say.as_deref().and_then(short),
            files: self.files.len(),
            commits: self.commits,
            passed: self.passed,
            subs_open: self.subs_open.clone(),
            subs_done: self.subs_done,
            subs_total: self.subs_total,
            background_open: open_bg,
            pictures: self.pictures.clone(),
            sources: self.sources.clone(),
            progress,
            train: self.train.clone(),
            position: self.position,
            brief: self.brief.clone(),
            owner: self.owner.clone(),
            recent: self.recent.iter().cloned().collect(),
            picture_at: self.picture_at,
            approval: self.approval.clone(),
            subs_closed_at: self.subs_closed_at,
            subs_names: self.subs_names.clone(),
            status: self.status.clone(),
        }
    }
}

/// Every "N passed" in a test command's output (`cargo test`'s `test result: ok. 12 passed`, pytest's `3 passed`).
pub fn passed_counts(out: &str) -> Vec<u64> {
    let mut v = Vec::new();
    let mut from = 0;
    while let Some(i) = out[from..].find(" passed") {
        let at = from + i;
        let digits: String = out[..at].chars().rev().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() { if let Ok(n) = digits.chars().rev().collect::<String>().parse() { v.push(n) } }
        from = at + 7;
    }
    v
}

/// "3 of 8" in a sentence: a measurable share (at most 1000 of anything).
fn n_of_m(s: &str) -> Option<(f64, f64)> {
    let w: Vec<&str> = s.split_whitespace().collect();
    w.windows(3).find_map(|x| {
        let (a, b) = (x[0].trim_matches(|c: char| !c.is_ascii_digit()).parse::<f64>().ok()?, x[2].trim_matches(|c: char| !c.is_ascii_digit()).parse::<f64>().ok()?);
        (x[1] == "of" && x[0].chars().all(|c| c.is_ascii_digit()) && b > 0.0 && b <= 1000.0 && a <= b).then_some((a, b))
    })
}

fn urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| c.is_whitespace() || "\"'<>()[]{},".contains(c)) {
        if (tok.starts_with("https://") || tok.starts_with("http://")) && tok.len() > 10 && !out.iter().any(|u: &String| u == tok) { out.push(tok.to_owned()) }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// What the island shows.

/// The reducer's output at one moment: the island's stage, clock, history, counters, line and picture.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Signals {
    pub run: String,
    pub started: f64,
    pub now: f64,
    pub stage: String,
    pub stage_since: f64,
    #[serde(skip)]
    pub changes: Vec<(f64, String)>,
    pub asked_since: Option<f64>,
    /// The agent's latest first sentence, and its short form for the island.
    pub say: Option<String>,
    pub summary: Option<String>,
    pub files: usize,
    pub commits: u32,
    pub passed: Option<u64>,
    pub subs_open: Vec<String>,
    pub subs_done: u32,
    pub subs_total: u32,
    pub background_open: usize,
    pub pictures: Vec<String>,
    pub sources: Vec<String>,
    /// (done, total) where it is measurable.
    pub progress: Option<(f64, f64)>,
    pub train: Option<super::train::Train>,
    pub position: Option<u64>,
    pub brief: Option<String>,
    pub owner: Option<String>,
    /// (when, "said: …" or "build: …"), oldest first.
    pub recent: Vec<(f64, String)>,
    pub picture_at: Option<f64>,
    /// The latest approval or question item's text, while nothing came after it.
    pub approval: Option<String>,
    pub subs_closed_at: Option<f64>,
    /// Every sub-agent title seen this run, in the order they opened.
    pub subs_names: Vec<String>,
    /// The task's status file, when the run named one (`STATUS_FILE`).
    pub status: Option<String>,
}

/// How the island stands for its history: still at work, asking the user, or over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown { Running, Asking, Ended }

impl Signals {
    /// The stage pill while running.
    pub fn pill(&self) -> &str { &self.stage }

    /// The compact history as look A's rail (round 2): at most three steps, the earlier stages merged into one row,
    /// the last finished stage, and the current one with its length (state `now`, or `done` when it is over).
    pub fn steps(&self, shown: Shown) -> Vec<Value> {
        let kp = self.history(shown);
        let Some((cur, earlier)) = kp.split_last() else { return Vec::new() };
        let mut steps = Vec::new();
        if earlier.len() > 2 {
            let n = earlier.len() - 1;
            steps.push(json!({"title": format!("{n} earlier stages  {}", fmt(earlier[n - 1].1 - earlier[0].0)), "state": "done"}));
            steps.extend(earlier[n..].iter().map(|p| json!({"title": format!("{}  {}", p.2, fmt(p.1 - p.0)), "state": "done"})));
        } else {
            steps.extend(earlier.iter().map(|p| json!({"title": format!("{}  {}", p.2, fmt(p.1 - p.0)), "state": "done"})));
        }
        let state = if shown == Shown::Ended { "done" } else { "now" };
        steps.push(json!({"title": format!("{}  {}", cur.2, fmt(cur.1 - cur.0)), "state": state}));
        steps
    }

    /// The compact history: (start, end, stage), ending in the stage the pill shows.
    pub fn history(&self, shown: Shown) -> Vec<(f64, f64, String)> {
        let now = self.now.max(self.started);
        let ph = phases(&self.changes, now);
        let kp = key_phases(&ph);
        let (cur, since) = match shown {
            Shown::Asking => ("Your answer".to_owned(), self.asked_since.unwrap_or(now - 30.0)),
            Shown::Ended => match kp.last() { Some(k) => (k.2.clone(), k.0), None => return Vec::new() },
            Shown::Running if matches!(self.stage.as_str(), "Starting" | "Done") => match kp.last() { Some(k) => (k.2.clone(), k.0), None => ("Preparing".into(), self.started) },
            Shown::Running => (self.stage.clone(), self.stage_since),
        };
        with_current(kp, now, &cur, since)
    }

    /// Counters worth a glance, the stage's own first: tests passed, sub-agents done of all (while one works, or at
    /// the end), files changed, commits, background commands open; a training run's step only when there is no
    /// progress bar to say it. Only what is there; at most `KV_CAP`; none while the island asks (the question is the
    /// moment). The loss is the island agent's curve, not a counter (the renders showed the two disagreeing).
    pub fn kv(&self, shown: Shown) -> Vec<Value> {
        if shown == Shown::Asking { return Vec::new() }
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(t) = self.train.as_ref().filter(|_| self.progress.is_none() || shown == Shown::Ended) {
            if let (Some(s), Some(n)) = (t.step, t.total) { out.push(("Step".into(), format!("{}/{}", thousands(s), thousands(n)))) }
        }
        let mut rest: Vec<(&str, (String, String))> = Vec::new();
        if let Some(p) = self.passed.filter(|p| *p > 0) { rest.push(("Testing", ("Tests".into(), format!("{p} passed")))) }
        let subs_now = !self.subs_open.is_empty() || matches!(self.stage.as_str(), "Sub-agents" | "Reviewing") || shown == Shown::Ended;
        if self.subs_total > 0 && subs_now { rest.push(("Sub-agents", ("Sub-agents".into(), format!("{}/{}", self.subs_done, self.subs_total)))) }
        if self.files > 0 { rest.push(("Editing", ("Files".into(), self.files.to_string()))) }
        if self.commits > 0 { rest.push(("Committing", ("Commits".into(), self.commits.to_string()))) }
        if self.background_open > 0 { rest.push(("", ("Background".into(), format!("{} open", self.background_open)))) }
        let stage = if self.stage == "Reviewing" { "Sub-agents" } else if self.stage == "Writing" { "Editing" } else { self.stage.as_str() };
        rest.sort_by_key(|(s, _)| *s != stage);
        out.extend(rest.into_iter().map(|(_, kv)| kv));
        out.truncate(KV_CAP);
        out.into_iter().map(|(l, v)| json!({"label": l, "value": v})).collect()
    }

    /// The island keys code owns on a bound island: the clock, the summary until the island agent writes the now line
    /// (`summary_ours`: false once it has, this run), progress where it is measurable (`null` clears). Round 3: no
    /// steps rail and no counters (bookkeeping is not progress); the stage history is the band (`band`).
    pub fn island_keys(&self, shown: Shown, summary_ours: bool) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("started".into(), json!((self.started * 10.0).round() / 10.0));
        if summary_ours { m.insert("summary".into(), self.summary.clone().map_or(Value::Null, Value::String)); }
        let progress = self.progress.filter(|_| shown == Shown::Running).map(|(d, t)| ((d / t) * 1000.0).round() / 1000.0);
        m.insert("progress".into(), progress.map_or(Value::Null, |p| json!(p)));
        m
    }

    /// The band's stage segments (round 3): the history ending in the current stage; a failed job ends in a red Failed
    /// stage; a done one has no current stage (the renderer dims it and ends it in a green check).
    pub fn segments(&self, shown: Shown, state: &str) -> Vec<(f64, f64, String)> {
        let now = self.now.max(self.started);
        let cur = match shown {
            Shown::Asking => "Your answer".to_owned(),
            Shown::Running if self.stage == "Starting" => "Reading".to_owned(),
            Shown::Running => self.stage.clone(),
            Shown::Ended => {
                let last = self.changes.iter().rev().map(|(_, n)| n.as_str()).find(|n| !matches!(*n, "Done" | "Failed" | "Stopped")).unwrap_or("Reading");
                if last == "Starting" { "Reading".to_owned() } else { last.to_owned() }
            }
        };
        let mut h = band_history_max(&self.changes, now, &cur, if state == "failed" { HISTORY - 1 } else { HISTORY });
        if state == "failed" { h.push((now, now + 1.0, "Failed".into())) }
        h
    }

    /// The band part code writes on a bound island (`"code": true`): helper rows (names only; `agent` is the island
    /// agent's short names, used only while their count matches code's rows; the now/done state is always code's)
    /// while a helper works or one finished within `HELPERS_LINGER`, and the stage segments with their lengths.
    pub fn band(&self, shown: Shown, state: &str, agent: Option<&Value>) -> Value {
        let segs: Vec<Value> = self.segments(shown, state).into_iter().map(|(a, b, n)| json!({"title": n, "secs": (b - a).round().max(0.0)})).collect();
        let lingering = self.subs_closed_at.is_some_and(|t| self.now - t < HELPERS_LINGER);
        let mut rows: Vec<Value> = Vec::new();
        if shown != Shown::Ended && (!self.subs_open.is_empty() || lingering) {
            // Code's rows: the working ones first, then the finished ones, newest first; their state is always code's.
            let mut r: Vec<(String, &str)> = self.subs_open.iter().map(|t| (helper_name(t), "now")).collect();
            r.extend(self.subs_names.iter().rev().filter(|t| !self.subs_open.contains(t)).map(|t| (helper_name(t), "done")));
            r.truncate(4);
            // The island agent's short names, in the same order, only while its list matches code's in count.
            let agent_names: Vec<String> = agent.and_then(Value::as_array).into_iter().flatten()
                .filter_map(|h| h["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned)).take(4).collect();
            let named = agent_names.len() == r.len();
            rows = r.into_iter().enumerate().map(|(i, (t, st))| json!({"title": if named { agent_names[i].clone() } else { t }, "state": st})).collect();
        }
        json!({"type": "band", "code": true, "state": state, "segments": segs, "helpers": rows})
    }

    /// The stage list the island agent reads (`history`): [stage, minutes].
    pub fn history_minutes(&self, shown: Shown, state: &str) -> Vec<Value> {
        self.segments(shown, state).into_iter().map(|(a, b, n)| json!([n, ((b - a) / 60.0).round()])).collect()
    }
}

/// How long finished helpers stay on the band after the last one finished.
pub const HELPERS_LINGER: f64 = 5.0 * 60.0;

/// A helper's name as code shortens it when the island agent has not: the part after a colon, three words.
pub fn helper_name(title: &str) -> String {
    let t = super::digest::without_tag(title);
    let t = t.split_once(": ").map_or(t, |(_, b)| b);
    t.split_whitespace().take(3).collect::<Vec<_>>().join(" ").trim_end_matches([',', ';', ':']).to_owned()
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 { out.push(',') }
        out.push(c);
    }
    out
}

/// A length as the rail shows it: "1 h 21" from an hour, else "N min" (at least 1).
pub fn fmt(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{} h {:02}", s / 3600, s % 3600 / 60) } else { format!("{} min", (s / 60).max(1)) }
}

/// Round 1's `phases`: the stage changes up to `upto` as (start, end, stage), Starting read as Reading, repeats
/// merged, and a flicker shorter than 40 s (or 2 % of the task) folded into the phase before, except the strong ones.
pub fn phases(changes: &[(f64, String)], upto: f64) -> Vec<(f64, f64, String)> {
    let mut ph: Vec<(f64, f64, String)> = Vec::new();
    for (at, st) in changes {
        if *at > upto { break }
        let st = if st == "Starting" { "Reading" } else { st.as_str() };
        if st == "Done" || ph.last().is_some_and(|p| p.2 == st) { continue }
        ph.push((*at, upto, st.to_owned()));
    }
    for i in 0..ph.len().saturating_sub(1) { ph[i].1 = ph[i + 1].0 }
    let dwell = ph.first().map_or(40.0, |p| (40f64).max((upto - p.0) * 0.02));
    let mut out: Vec<(f64, f64, String)> = Vec::new();
    for p in ph {
        let strong = STRONG.contains(&p.2.as_str());
        if let Some(last) = out.last_mut() {
            if (p.1 - p.0 < dwell && !strong) || last.2 == p.2 { last.1 = p.1; continue }
        }
        out.push(p);
    }
    out
}

/// Round 2's `key_phases`: weak stages fold into Preparing, repeats merge, a first phase under a minute joins the next.
pub fn key_phases(ph: &[(f64, f64, String)]) -> Vec<(f64, f64, String)> {
    let mut out: Vec<(f64, f64, String)> = Vec::new();
    for (a, b, n) in ph {
        let n = if PREPARING.contains(&n.as_str()) { "Preparing".to_owned() } else { n.clone() };
        if let Some(last) = out.last_mut().filter(|l| l.2 == n) { last.1 = *b; continue }
        out.push((*a, *b, n));
    }
    if out.len() > 1 && out[0].1 - out[0].0 < 60.0 { out[1].0 = out[0].0; out.remove(0); }
    out
}

fn merge_same(kp: Vec<(f64, f64, String)>) -> Vec<(f64, f64, String)> {
    let mut out: Vec<(f64, f64, String)> = Vec::new();
    for p in kp {
        match out.last_mut() { Some(l) if l.2 == p.2 => l.1 = p.1, _ => out.push(p) }
    }
    out
}

/// Round 2's `collapse`: a segment under `FOLD_SHARE` of the task gives its time to the one before (never forward,
/// never into a kept one), except the current stage and the kept ones; then at most `HISTORY` stages, the oldest
/// folded into "Earlier".
pub fn collapse(kp: Vec<(f64, f64, String)>) -> Vec<(f64, f64, String)> {
    let mut kp = merge_same(kp);
    if kp.is_empty() { return kp }
    let total = (kp[kp.len() - 1].1 - kp[0].0).max(1.0);
    let mut changed = true;
    while changed && kp.len() > 2 {
        changed = false;
        for i in 0..kp.len() - 1 {
            let (a, b) = (kp[i].0, kp[i].1);
            if b - a >= FOLD_SHARE * total || KEEP.contains(&kp[i].2.as_str()) { continue }
            if i > 0 && !KEEP.contains(&kp[i - 1].2.as_str()) { kp[i - 1].1 = b } else { continue }
            kp.remove(i);
            kp = merge_same(kp);
            changed = true;
            break;
        }
    }
    while kp.len() > HISTORY {
        kp[1].0 = kp[0].0;
        kp[1].2 = "Earlier".into();
        kp.remove(0);
    }
    kp
}

/// Round 3's band history (`tools/island_band.py` `history`): every segment keeps the name it had live; repeats merge;
/// flickers under 20 s join the segment before; the current stage closes it; beyond four, the shortest past stage joins
/// its longer neighbour (Paused, Your answer, Reviewing, Sub-agents and Training last), and a merged run of weak stages
/// is called Preparing. (start, end, name).
pub fn band_history(changes: &[(f64, String)], at: f64, cur: &str) -> Vec<(f64, f64, String)> { band_history_max(changes, at, cur, HISTORY) }

/// `band_history` with at most `max` segments (a failed job keeps one place for its red Failed stage).
pub fn band_history_max(changes: &[(f64, String)], at: f64, cur: &str, max: usize) -> Vec<(f64, f64, String)> {
    const KEEP3: [&str; 5] = ["Paused", "Your answer", "Reviewing", "Sub-agents", "Training"];
    let mut out: Vec<(f64, f64, String)> = Vec::new();
    for (a, n) in changes.iter().filter(|(a, _)| *a <= at) {
        let n = if n == "Starting" { "Reading" } else { n.as_str() };
        if n == "Done" { continue }
        if let Some(l) = out.last_mut() { l.1 = *a; if l.2 == n { continue } }
        out.push((*a, at, n.to_owned()));
    }
    if out.last().is_some_and(|l| l.2 != cur) || out.is_empty() {
        let t = out.last().map_or(at - 30.0, |l| (l.0 + 1.0).max(at - 30.0)).min(at);
        if let Some(l) = out.last_mut() { l.1 = t }
        out.push((t, at, cur.to_owned()));
    }
    if let Some(l) = out.last_mut() { l.1 = at }
    let mut i = 1;
    while i + 1 < out.len() {
        if out[i].1 - out[i].0 < 20.0 {
            out[i - 1].1 = out[i].1;
            out.remove(i);
            if i < out.len() && out[i - 1].2 == out[i].2 { out[i - 1].1 = out[i].1; out.remove(i); }
            continue;
        }
        i += 1;
    }
    let weak = |n: &str| PREPARING.contains(&n) || n == "Writing";
    while out.len() > max.max(2) {
        let past = out.len() - 1;
        let mut cand: Vec<usize> = (0..past).filter(|i| !KEEP3.contains(&out[*i].2.as_str())).collect();
        if cand.is_empty() { cand = (0..past).collect() }
        let i = *cand.iter().min_by(|a, b| (out[**a].1 - out[**a].0).partial_cmp(&(out[**b].1 - out[**b].0)).unwrap_or(std::cmp::Ordering::Equal)).unwrap();
        let mut j = if i > 0 { i - 1 } else { i + 1 };
        if j == out.len() - 1 && i > 0 { j = i - 1 }
        let (lo, hi) = (i.min(j), i.max(j));
        let (di, dj) = (out[i].1 - out[i].0, out[j].1 - out[j].0);
        let keep = if di > dj && j != out.len() - 1 { out[i].2.clone() } else { out[j].2.clone() };
        let name = if weak(&keep) { "Preparing".to_owned() } else { keep };
        let merged = (out[lo].0, out[hi].1, name);
        out.splice(lo..=hi, [merged]);
        let mut k = 1;
        while k < out.len() {
            if out[k].2 == out[k - 1].2 { out[k - 1].1 = out[k].1; out.remove(k); } else { k += 1 }
        }
    }
    out
}

/// A length as the band labels it: "<1 min", "N min", "1 h 03".
pub fn band_fmt(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{} h {:02}", s / 3600, s % 3600 / 60) } else if s >= 60 { format!("{} min", s / 60) } else { "<1 min".into() }
}

/// Round 2's `with_current`: the history ends in the stage the island shows, from when it last began.
pub fn with_current(mut kp: Vec<(f64, f64, String)>, at: f64, cur: &str, since: f64) -> Vec<(f64, f64, String)> {
    if kp.last().is_some_and(|k| k.2 == cur) { return collapse(kp) }
    let t = since.max(kp.last().map_or(0.0, |k| k.0 + 1.0)).min(at);
    if let Some(k) = kp.last_mut() { k.1 = t }
    kp.push((t, at, cur.to_owned()));
    collapse(kp)
}

