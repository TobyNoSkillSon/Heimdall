//! What each turn must account for, checked in code (climb, 1 Nov: on held-out days the job missed finished work and
//! left answered asks open; 6 of the 10 missed tasks were in a worker's `completed` notice). The turn numbers every
//! completed task notice in its new lines and lists every open ask. The answer carries one
//! `finished` entry per number (the goal it serves, or none) and one `checked` entry per ask (open, answered, moot);
//! code turns them into ops (a done task quoted from the notice itself; an answer or a drop with the model's quote,
//! still checked as any op) and logs any number or ask the answer left out.

use crate::goals::ops::{normalise, Op};
use crate::goals::stream::{Kind, Line};
use serde_json::{json, Map, Value};

/// One completed task notice of the turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub n: usize,
    /// The first words of its result: the quote of the done task it becomes (verbatim, one line).
    pub quote: String,
    /// What the turn shows: the task's name and the start of its result.
    pub show: String,
}

/// What the turn must account for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ledger {
    pub done: Vec<Notice>,
    /// Heimdall's own lines (its words, the relay's replies) that say work is finished (Goals, 1 Nov: finished work
    /// told only by the relay was missed): each one is accounted for as a notice is.
    pub reported: Vec<Notice>,
    /// Open asks (id, title), listed every turn.
    pub asks: Vec<(String, String)>,
}

/// Words of a notice's body kept as its quote: enough to tell it apart, short enough to read.
const QUOTE_WORDS: usize = 14;

/// Words shown per notice: enough for a lane's summary to name the pieces it finished.
const SHOW_WORDS: usize = 60;

/// A line that only frames the result (`Earlier turn:`, the `Task '…' completed. … Final reply:` line).
fn framing(t: &str) -> bool {
    // Also a thread's notice (`Thread '…' (…), which you sent a message, completed. Final reply:`; held-out 2 Nov: its
    // framing became the quote of the done task).
    matches!(t, "Earlier turn:" | "Last turn:" | "Final reply:") || ((t.starts_with("Task '") || t.starts_with("Thread '")) && t.contains(" completed"))
}

/// A completed task notice's quote and what the turn shows of it: the task's name (from `Task '…'`) and the first
/// lines of its result, past the framing (night 1 Nov: a long-running lane's every notice opened with the same
/// "Task '…' completed. It still has background work running…", so the model could not tell what had finished).
fn body(l: &Line) -> Option<(String, String)> {
    let first = l.lines.first()?;
    let at = first.find("[Heimdall notice: terminal:")?;
    let end = first[at..].find(']')? + at;
    if !first[at..end].ends_with(":completed") { return None }
    let mut texts: Vec<String> = vec![first[end + 1..].trim().to_owned()];
    texts.extend(l.lines.iter().skip(1).map(|t| t.trim().to_owned()));
    let name = texts.iter().find_map(|t| t.strip_prefix("Task '").or_else(|| t.strip_prefix("Thread '")).and_then(|r| r.split_once("' (")).map(|(n, _)| n.to_owned()));
    let content: Vec<&String> = texts.iter().filter(|t| !t.is_empty() && !framing(t)).collect();
    // The first content line with words enough to quote (a reply may open with "## Result" or "Done.").
    let lead = content.iter().find(|t| t.split_whitespace().count() >= 3)?;
    let words: Vec<&str> = lead.split_whitespace().take(QUOTE_WORDS).collect();
    let shown: Vec<&str> = content.iter().flat_map(|t| t.split_whitespace()).take(SHOW_WORDS).collect();
    // What the framing said, kept as a plain mark (Settled climb, 2 Nov: steps of a lane still running were settled as
    // finished work). "Earlier turn:" only means the last turn had no words of its own: not a repeat, so no mark.
    let mut marks = String::new();
    if texts.iter().any(|t| t.contains("still has background work")) { marks.push_str("(the task is still running) ") }
    let show = match name { Some(n) => format!("[{n}] {marks}{}", shown.join(" ")), None => format!("{marks}{}", shown.join(" ")) };
    Some((words.join(" "), show))
}

/// Words that say something is finished (whole words, lower case).
const FINISHED: [&str; 24] = ["done", "finished", "merged", "installed", "built", "fixed", "shipped", "committed", "landed", "passes",
    "passed", "complete", "completed", "works", "ready", "published", "deployed", "measured", "trained", "ported", "rendered",
    "settled", "wrote", "added"];

/// A line of Heimdall's that says work finished: the sentence that says it (verbatim, at most `QUOTE_WORDS` words from
/// its start: the quote) and the line as the turn shows it.
fn report(l: &Line) -> Option<(String, String)> {
    if l.kind != Kind::Heimdall { return None }
    let text = l.lines.iter().map(|t| t.trim()).collect::<Vec<_>>().join(" ");
    let text = text.strip_prefix("Heimdall: ").unwrap_or(&text).to_owned();
    let sentence = text.split_inclusive(['.', '!', '?', ';', '\n']).map(str::trim)
        .find(|s| s.split(|c: char| !c.is_alphanumeric()).any(|w| FINISHED.contains(&w.to_lowercase().as_str())) && s.split_whitespace().count() >= 3)?;
    let quote: Vec<&str> = sentence.split_whitespace().take(QUOTE_WORDS).collect();
    let shown: Vec<&str> = text.split_whitespace().take(SHOW_WORDS).collect();
    Some((quote.join(" "), shown.join(" ")))
}

/// The ledger of `lines` against the Goals file as it is.
pub fn build(lines: &[Line], goals: &Map<String, Value>) -> Ledger {
    let done = lines.iter().filter(|l| l.kind == Kind::Task).filter_map(body).enumerate()
        .map(|(i, (quote, show))| Notice { n: i + 1, quote, show }).collect();
    let reported = lines.iter().filter_map(report).enumerate().map(|(i, (quote, show))| Notice { n: i + 1, quote, show }).collect();
    // Every turn, not only when the user spoke (climb night 2: asks Heimdall settled itself, or that the user answered a turn
    // earlier than the model saw it, stayed open).
    let asks = {
        goals.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object)
            // "Later" keeps an ask waiting (as `answered_by_click` does): it is checked again.
            .filter(|i| i.get("done") != Some(&Value::Bool(true)) && matches!(i.get("chosen").and_then(Value::as_str).unwrap_or(""), "" | "later"))
            .filter_map(|i| Some((i.get("id")?.as_str()?.to_owned(), i.get("title").and_then(Value::as_str).unwrap_or("").to_owned())))
            .collect()
    };
    Ledger { done, reported, asks }
}

/// The ledger as the turn shows it (after the Goals now; empty when there is nothing to account for).
pub fn text(l: &Ledger) -> String {
    let mut t = String::new();
    if !l.done.is_empty() {
        t.push_str("<finished_tasks note=\"one `finished` entry for each\">\n");
        for d in &l.done { t.push_str(&format!("#{} {}\n", d.n, d.show)) }
        t.push_str("</finished_tasks>\n\n");
    }
    if !l.reported.is_empty() {
        t.push_str("<heimdall_reports note=\"one `reported` entry for each\">\n");
        for d in &l.reported { t.push_str(&format!("#{} {}\n", d.n, d.show)) }
        t.push_str("</heimdall_reports>\n\n");
    }
    if !l.asks.is_empty() {
        t.push_str("<asks_to_check note=\"one `checked` entry for each\">\n");
        for (id, title) in &l.asks { t.push_str(&format!("{id} · {title}\n")) }
        t.push_str("</asks_to_check>\n\n");
    }
    t
}

/// The schema's two ledger arrays (required in every answer; empty when the turn listed nothing).
pub fn schema_properties() -> (Value, Value, Value) {
    let s = || json!({"type": "string"});
    let finished = json!({"type": "array", "items": {"type": "object",
        "properties": {"n": {"type": "integer"}, "goal": s(), "task": s(), "title": s(), "now": s()},
        "required": ["n", "goal", "task", "title", "now"], "additionalProperties": false}});
    let checked = json!({"type": "array", "items": {"type": "object",
        "properties": {"ask": s(), "status": {"type": "string", "enum": ["open", "answered", "moot"]}, "quote": s()},
        "required": ["ask", "status", "quote"], "additionalProperties": false}});
    (finished.clone(), finished, checked)
}

/// The model's ops without those the ledger's ops already cover: a `progress` op on a task the ledger marks done
/// (it would add the task twice, or set it back to now), an op on an ask the ledger answers or drops (review 1 Nov:
/// the model's copy, with a paraphrased quote, used to displace the ledger's and was then dropped by the quote check).
pub fn drop_duplicates(model: Vec<Op>, ledger: &[Op]) -> Vec<Op> {
    model.into_iter().filter(|o| !ledger.iter().any(|l| {
        if !l.ask.is_empty() { return o.ask == l.ask }
        o.op == "progress" && o.goal == l.goal && ((!l.task.is_empty() && o.task == l.task) || (!l.title.is_empty() && normalise(&o.title) == normalise(&l.title)))
    })).collect()
}

/// What the ledger entries of `answer` add before the model's own `ops`, and what the answer left out (logged).
pub fn ops(answer: &Value, l: &Ledger) -> (Vec<Op>, Vec<String>) {
    let mut out = Vec::new();
    let mut missing = Vec::new();
    let arr = |k: &str| answer.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    for (key, notices) in [("finished", &l.done), ("reported", &l.reported)] {
    let finished = arr(key);
    for d in notices {
        // One notice may report several finished pieces (a lane's summary): one entry each, the same number.
        let entries: Vec<&Value> = finished.iter().filter(|e| e.get("n").and_then(Value::as_u64) == Some(d.n as u64)).collect();
        if entries.is_empty() { missing.push(format!("{key} #{}", d.n)); continue }
        for e in entries {
            let (goal, task, title) = (s(e, "goal"), s(e, "task"), s(e, "title"));
            if goal.is_empty() || (task.is_empty() && title.is_empty()) { continue }
            // Named twice in `finished`: one task. (The model's own op for the same task gives way to this one, which
            // carries the notice's own quote: `drop_duplicates`; review 1 Nov.)
            let same = |o: &Op| o.goal == goal && ((!task.is_empty() && o.task == task) || (!title.is_empty() && normalise(&o.title) == normalise(&title)));
            if out.iter().any(same) { continue }
            out.push(Op { op: "progress".into(), goal, task, title, state: "done".into(), now: s(e, "now"), quote: d.quote.clone(), ..Op::default() });
        }
    }
    }
    let checked = arr("checked");
    for (id, _) in &l.asks {
        let Some(e) = checked.iter().find(|e| s(e, "ask") == *id) else {
            missing.push(format!("checked {id}"));
            continue;
        };
        let op = match s(e, "status").as_str() { "answered" => "answered", "moot" => "drop_ask", _ => continue };
        out.push(Op { op: op.into(), ask: id.clone(), quote: s(e, "quote"), ..Op::default() });
    }
    (out, missing)
}
