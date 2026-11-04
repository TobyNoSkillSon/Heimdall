//! The suggester's offline replay and its label export (lab, "a replay over 3 real archived conversations,
//! using the archive CLI read-only: show which suggestions would have fired and the card text").
//!
//! The replay reads archived talk as the archive CLI prints it (`heimdall-archive timeline --kinds utterance`, one JSON
//! line per utterance, read-only) and runs the app's own stub, pacing and job on it, one model call at a time, under
//! `BUDGET_USD` and `MAX_CALLS`. Nothing is shown or sent: each decision goes to `replay.json`, and the whole run to a
//! dark `replay.html`. Its input is real talk, so its output stays in the user's Vault, never in the repository.
//! The model is Haiku whatever the background choice is (the replay is the weekly quota's spend).

use crate::background_call;
use crate::plan_refresher::Endpoint;
use crate::background_call::Prompt;
use crate::suggester as sg;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The most a replay may spend in all, US dollars (the user's limit for the replay is $3; this keeps a margin).
pub const BUDGET_USD: f64 = 2.50;
/// The most model calls a replay makes in all.
pub const MAX_CALLS: usize = 12;

/// What the replay has spent so far, across every conversation it reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Budget {
    pub calls: usize,
    pub spent: f64,
}
/// The replay's model.
pub const MODEL: &str = "claude-haiku-5-5";

/// One line of the archive CLI's utterance timeline, as a line of talk. None when it carries no words, or when it is
/// Heimdall's own speech.
pub fn parse_line(json: &str) -> Option<sg::Line> {
    let v: Value = serde_json::from_str(json).ok()?;
    let speaker = v["payload"]["speaker"].as_str()?.to_owned();
    // Heimdall's own spoken replies are not talk with anyone (the app never gives them to the window either).
    if speaker == "Heimdall" {
        return None;
    }
    let text = v["payload"]["text"].as_str()?.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    let at_ms = v["wall_ns"].as_u64()? / 1_000_000;
    let owner = speaker == "User";
    Some(sg::Line { at_ms, speaker, text, owner })
}

/// The talk of one timeline file, in time order.
pub fn parse(timeline: &str) -> Vec<sg::Line> {
    let mut lines: Vec<sg::Line> = timeline.lines().filter_map(parse_line).collect();
    lines.sort_by_key(|l| l.at_ms);
    lines
}

/// What the replay did at one moment the user's line could have started a pass.
#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub at_ms: u64,
    pub time: String,
    /// The user's line that the stub acted on.
    pub said: String,
    /// What the model saw: the window's talk, oldest first.
    pub transcript: String,
    /// "card", "silent", "pacing", "budget" or "error".
    pub outcome: String,
    pub reason: String,
    pub card: Option<sg::Suggestion>,
    /// Whether a model call was made for this moment, and what it cost (dollars, as the ledger counted it).
    pub called: bool,
    pub usd: f64,
}

/// Runs the stub, the pacing and the job over `lines` in order. `ask` makes one model call for a window and time and
/// returns the verdict with the call's cost; `budget` is the replay's running total, shared by every conversation. A
/// card the replay would have shown holds the pacer closed for its life (`sg::CARD_MS`), as an unanswered card does in
/// the app. Calls stop once the budget reaches `BUDGET_USD` or `MAX_CALLS`. With no `ask` (`--triggers-only`) the run
/// only lists the moments the stub fires on: no call, no pacing, no card.
pub fn run(lines: &[sg::Line], mut ask: Option<&mut dyn FnMut(Prompt) -> (Result<sg::Verdict>, f64)>, budget: &mut Budget) -> Vec<Decision> {
    let mut window = sg::Window::default();
    let mut pacer = sg::Pacer::default();
    let mut open_until: Option<u64> = None;
    let mut out = vec![];
    for line in lines {
        if open_until.is_some_and(|t| line.at_ms >= t) {
            pacer.closed();
            open_until = None;
        }
        window.push(line.clone());
        let now = line.at_ms;
        if !(line.owner && sg::stub_suggest_now(&window)) {
            continue;
        }
        let decision = |outcome: &str, reason: String, card: Option<sg::Suggestion>, called: bool, usd: f64| Decision {
            at_ms: now,
            time: agent::clock::local_hms(now),
            said: line.text.clone(),
            transcript: window.transcript(),
            outcome: outcome.into(),
            reason,
            card,
            called,
            usd,
        };
        let Some(ask) = ask.as_mut() else {
            out.push(decision("trigger", "the stub fires here; no model call in this run".into(), None, false, 0.0));
            continue;
        };
        if !pacer.may_call(now) {
            out.push(decision("pacing", "a card is up, the minute since the last call, or a pause".into(), None, false, 0.0));
            continue;
        }
        if budget.calls >= MAX_CALLS || budget.spent >= BUDGET_USD {
            out.push(decision("budget", format!("not run: {} calls, ${:.2} spent", budget.calls, budget.spent), None, false, 0.0));
            continue;
        }
        pacer.called(now);
        budget.calls += 1;
        let (verdict, usd) = ask(sg::prompt(&window, now, &window.people()));
        budget.spent += usd;
        out.push(match verdict {
            Ok(sg::Verdict::Silent(reason)) => decision("silent", reason, None, true, usd),
            Ok(sg::Verdict::Card(s)) if pacer.may_show(now, &s.topic) => {
                pacer.shown(now, &s.topic);
                open_until = Some(now + sg::CARD_MS);
                let topic = s.topic.clone();
                decision("card", topic, Some(s), true, usd)
            }
            Ok(sg::Verdict::Card(s)) => decision("pacing", format!("would repeat or crowd the gap: {}", s.topic), None, true, usd),
            Err(err) => decision("error", format!("{err:#}"), None, true, usd),
        });
    }
    out
}

/// The archive's read-only index, where the replay's timeline search looks (`archive::store_root` in the app).
pub fn archive_index(root: &Path) -> PathBuf {
    root.join(heimdall_archive::store::INDEX)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The replay page: the answer first (how many cards, what they asked), then each conversation's moments.
pub fn html(conversations: &[(String, Vec<Decision>)], usd: f64, calls: usize, triggers_only: bool) -> String {
    let cards: usize = conversations.iter().flat_map(|(_, d)| d).filter(|d| d.outcome == "card").count();
    let triggers: usize = conversations.iter().flat_map(|(_, d)| d).filter(|d| d.outcome == "trigger").count();
    let mut body = String::new();
    for (title, decisions) in conversations {
        body.push_str(&format!("<section><h2>{}</h2>", esc(title)));
        if decisions.is_empty() {
            body.push_str("<p class=\"quiet\">User's lines in this talk never met the stub.</p>");
        }
        for d in decisions {
            let said = format!("<div class=\"said\"><span class=\"time\">{}</span> User: “{}”</div>", esc(&d.time), esc(&d.said));
            let verdict = match (&d.outcome[..], &d.card) {
                ("card", Some(s)) => {
                    let questions: String = s
                        .questions
                        .iter()
                        .map(|q| {
                            let options: String = q.options.iter().map(|o| format!("<span class=\"opt\">{}</span>", esc(o))).collect();
                            format!("<div class=\"q\"><div class=\"qt\">{}</div><div class=\"opts\">{options}</div></div>", esc(&q.text))
                        })
                        .collect();
                    let found: String = s
                        .found
                        .iter()
                        .map(|f| format!("<li><code>{}</code> {}</li>", esc(&f.path), esc(&f.note)))
                        .collect();
                    let found = if found.is_empty() { String::new() } else { format!("<ul class=\"found\">{found}</ul>") };
                    format!("<div class=\"card\"><div class=\"topic\">{}</div>{questions}{found}</div>", esc(&s.topic))
                }
                ("card", None) => String::new(),
                ("trigger", _) => "<div class=\"silent\">The stub fires here: the model would be asked, and no call was made.</div>".to_owned(),
                _ => format!("<div class=\"silent\">No card: {}</div>", esc(&d.reason)),
            };
            let talk = format!("<details><summary>the talk before</summary><pre>{}</pre></details>", esc(&d.transcript));
            let cost = if d.usd > 0.0 { format!(" <span class=\"cost\">${:.4}</span>", d.usd) } else { String::new() };
            body.push_str(&format!("<div class=\"moment\">{said}{verdict}{talk}{cost}</div>"));
        }
        body.push_str("</section>");
    }
    let lede = if triggers_only {
        format!("{triggers} moment(s) across {} conversation(s) where the stub would ask the model. No model call was made, so there is no card text yet.", conversations.len())
    } else {
        format!("{cards} card(s) would have shown across {} conversation(s), from {calls} model call(s); measured spend ${usd:.4} on {MODEL}.", conversations.len())
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>In-conversation suggestions: replay</title><style>\
body{{background:#0b0b0d;color:#f2f2f7;font:15px -apple-system,system-ui,sans-serif;margin:0;padding:32px;}}\
h1{{font-size:22px;margin:0 0 6px}} h2{{font-size:17px;margin:28px 0 10px;color:#c7c7cc}}\
.lede{{font-size:16px;margin:0 0 22px}} .quiet,.silent{{color:#8e8e93}}\
.moment{{background:#1c1c1e;border-radius:14px;padding:14px 16px;margin:0 0 12px}}\
.said{{margin:0 0 10px}} .time{{color:#8e8e93;font-variant-numeric:tabular-nums;margin-right:6px}}\
.card{{background:#2c2c2e;border-radius:12px;padding:12px 14px;margin:8px 0}}\
.topic{{font-weight:600;margin-bottom:8px;color:#64d2ff}} .q{{margin:6px 0}} .qt{{margin-bottom:6px}}\
.opt{{display:inline-block;background:#3a3a3c;border-radius:999px;padding:4px 11px;margin:0 6px 6px 0;font-size:14px}}\
.found{{margin:8px 0 0;padding-left:18px;color:#c7c7cc;font-size:13px}} code{{color:#ffd60a}}\
details{{margin-top:8px;color:#8e8e93;font-size:13px}} pre{{white-space:pre-wrap;color:#aeaeb2}}\
.cost{{color:#8e8e93;font-size:12px}}</style></head><body>\
<h1>In-conversation suggestions: replay</h1>\
<p class=\"lede\">{lede}</p>\
{body}</body></html>",
    )
}

/// `--suggest-replay [--triggers-only] OUT_DIR TIMELINE.jsonl...`: each timeline file is one conversation (its name is
/// its title). Writes `OUT_DIR/replay.json` and `OUT_DIR/replay.html`, prints one line. `--triggers-only` makes no model
/// call and lists the stub's moments only.
pub fn cli(args: &[String]) -> Result<String> {
    let (triggers_only, args) = match args.split_first() {
        Some((first, rest)) if first == "--triggers-only" => (true, rest),
        _ => (false, args),
    };
    let (out, files) = args.split_first().context("--suggest-replay [--triggers-only] OUT_DIR TIMELINE.jsonl...")?;
    ensure_files(files)?;
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out)?;
    // Every paid run is counted in the ledger; the replay's spend is the ledger's delta around each call.
    let total = Arc::new(Mutex::new(0.0_f64));
    let ledger = total.clone();
    background_call::set_ledger(Arc::new(move |r: &background_call::Run| *ledger.lock().unwrap() += r.usd));
    let mut endpoint = Endpoint::anthropic();
    endpoint.model = Some(MODEL.into());
    if !triggers_only && endpoint.key.is_none() {
        bail!("no Anthropic key for the replay: set HEIMDALL_ANTHROPIC_API_KEY (lab runs read only that variable), or pass --triggers-only");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let archive = heimdall_home::data_dir().join("archive");
    let paths = sg::Paths { home, data: out.join("data"), archive: Some(archive_index(&archive)) };
    let mut conversations = vec![];
    let mut budget = Budget::default();
    for file in files {
        let title = Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("conversation").to_owned();
        let lines = parse(&std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?);
        let mut ask = |prompt: Prompt| {
            let before = *total.lock().unwrap();
            let verdict = sg::ask(&endpoint, prompt, &paths);
            (verdict, *total.lock().unwrap() - before)
        };
        let decisions = run(&lines, if triggers_only { None } else { Some(&mut ask) }, &mut budget);
        conversations.push((title, decisions));
    }
    let usd = *total.lock().unwrap();
    let calls = budget.calls;
    let cards = conversations.iter().flat_map(|(_, d)| d).filter(|d| d.outcome == "card").count();
    let triggers = conversations.iter().flat_map(|(_, d)| d).filter(|d| d.outcome == "trigger").count();
    let json: Vec<Value> = conversations.iter().map(|(t, d)| serde_json::json!({"conversation": t, "decisions": d})).collect();
    std::fs::write(out.join("replay.json"), serde_json::to_vec_pretty(&json)?)?;
    std::fs::write(out.join("replay.html"), html(&conversations, usd, calls, triggers_only))?;
    if triggers_only {
        return Ok(format!("triggers only: {triggers} moment(s) the stub fires on, no model call; {}", out.join("replay.html").display()));
    }
    Ok(format!("replay: {cards} card(s), {calls} call(s), ${usd:.4} measured; {}", out.join("replay.html").display()))
}

fn ensure_files(files: &[String]) -> Result<()> {
    anyhow::ensure!(!files.is_empty(), "give at least one timeline file");
    Ok(())
}

/// `--suggest-export [--log PATH] [--out PATH]`: the decider's labels from the suggestion log (`sg::export`). The log
/// defaults to Heimdall's data folder; the export is written to `--out` (default: next to the log).
pub fn export_cli(args: &[String], default_data: impl FnOnce() -> Result<PathBuf>) -> Result<String> {
    let mut log = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--log" => log = Some(PathBuf::from(it.next().context("--log PATH")?)),
            "--out" => out = Some(PathBuf::from(it.next().context("--out PATH")?)),
            other => bail!("unknown argument {other}; --suggest-export [--log PATH] [--out PATH]"),
        }
    }
    let log = match log {
        Some(p) => p,
        None => sg::log_path(&default_data()?),
    };
    let text = std::fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;
    let rows = sg::export(&text);
    let out = out.unwrap_or_else(|| log.with_file_name("labels.jsonl"));
    let mut body = String::new();
    for row in &rows {
        body.push_str(&row.to_string());
        body.push('\n');
    }
    std::fs::write(&out, body)?;
    Ok(format!("{} label row(s) written to {}", rows.len(), out.display()))
}

#[cfg(test)]
#[path = "suggester_replay_tests.rs"]
mod tests;
