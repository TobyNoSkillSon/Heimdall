//! The Goals file and the shadow's ops (plan, 31 Oct). `islands/goals.json` (preset "todo", list "goals"): `goals`,
//! each with its tasks (done | now | next) and a one-line `now`; `items`, the asks (what needs the user now: the To do item
//! shape plus `goal`). The model answers with ops, not the whole list (output was the plan refresher's main cost), and
//! every op quotes the words in the new lines that justify it: code drops an op whose quote is not there, so nothing
//! is invented. Hard caps live here, not in the prompt: at most `MAX_ACTIVE` active goals, titles `TITLE_CHARS`, the
//! now line `NOW_CHARS`, an ask's detail `DETAIL_CHARS`. The user's ticks and unknown keys survive; an ask he answered by
//! a click (chosen or done) leaves.

use crate::islands::files::{read_object, write_atomic, Root};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

pub const ID: &str = "goals";
pub const TITLE: &str = "Goals";
pub const ICON: &str = "flag";
pub const MAX_ACTIVE: usize = 6;
pub const TITLE_CHARS: usize = 40;
pub const NOW_CHARS: usize = 60;
pub const DETAIL_CHARS: usize = 60;
/// Open asks at most: they stay small and few.
pub const MAX_ASKS: usize = 8;
/// Ops one answer may carry (more are dropped, logged).
pub const MAX_OPS: usize = 12;
/// A quote shorter than this (normalised) proves nothing.
pub const QUOTE_MIN: usize = 4;
/// An op that takes something away (an ask answered or dropped, a goal finished or paused, a task dropped) needs a
/// quote of at least this many words and characters (review 31 Oct: "ok" and "done" were enough to clear an ask).
pub const REMOVE_WORDS: usize = 3;
pub const REMOVE_CHARS: usize = 15;
/// The user's own answer may be short ("Yeah, of course."): an `answered` quoted from his words needs this many words and
/// characters (climb v3, 31 Oct: the 15-character rule refused his plain yes to two approvals, so they stayed open).
pub const ANSWER_WORDS: usize = 2;
pub const ANSWER_CHARS: usize = 6;
/// A goal's `about`, at most: one line saying what work serves it (the model reads it; the pill does not draw it).
pub const ABOUT_CHARS: usize = 140;
/// Marks an ask the shadow raised (`by`): only those may be dropped on the evidence of anything but the user's words.
pub const BY: &str = "goals";
/// What an ask asks of the user (`kind`), drawn as its row's symbol: the To do kinds.
pub const KINDS: [&str; 9] = ["approve", "test", "pick", "send", "decide", "review", "write", "fix", "answer"];
/// Every op code knows. `ask` is refused:, "clear these orange goals and remember I didn't want them":
/// Goals hold the big goals only; what needs him comes as a card when it is needed, never raised by the shadow.
pub const OPS: [&str; 9] = ["add_goal", "progress", "decided", "finish_goal", "pause_goal", "ask", "answered", "drop_task", "drop_ask"];
/// The ops the model may write (the schema's enum).
/// Asks are answered or dropped through the answer's `checked` list, never by an op of the model's own.
pub const MODEL_OPS: [&str; 6] = ["progress", "decided", "add_goal", "finish_goal", "pause_goal", "drop_task"];
pub const TASK_STATES: [&str; 3] = ["done", "now", "next"];

/// The four seed goals (proposed; the user decides the final set): id, title, SF Symbol, what work serves it.
pub const SEED: [(&str, &str, &str, &str); 4] = [
    ("g-job", "Get a job", "briefcase", "CV, job search, applications, interviews."),
    ("g-arsenal", "Ship Arsenal", "shippingbox", "Arsenal, the Mac app that runs on-device AI models: its models, tests, MCP tools, site and release."),
    ("g-heimdall-public", "Ship Heimdall publicly", "eye", "Heimdall's public release (editions, licences, privacy, signing, updater, installer, setup wizard, first run, website and opening film) and product work on Heimdall, Vella and their parts that ships with it (menus, islands, relay, voice features, fixes)."),
    ("g-jarvis", "Make Jarvis efficient", "waveform", "The Jarvis voice (Drama 3) cheaper and faster: power, memory, latency, a Rust/MLX/Metal Drama engine, a tiny Jarvis voice trained to replace it (Kokoro or similar)."),
];

/// The answer's schema: every key required, "" when unused (each optional key roughly doubles part of a structured
/// output grammar), `quote` first so the evidence comes before the op.
pub fn schema() -> Value {
    let s = || json!({"type": "string"});
    let with_empty = |v: &[&str]| -> Vec<String> { v.iter().map(|x| x.to_string()).chain([String::new()]).collect() };
    let op = json!({
        "type": "object",
        "properties": {
            "quote": {"type": "string", "description": "Words copied from the new lines that justify this op."},
            "op": {"type": "string", "enum": MODEL_OPS},
            "goal": s(), "task": s(), "title": s(),
            "state": {"type": "string", "enum": with_empty(&TASK_STATES)},
            "now": s(),
            "kind": {"type": "string", "enum": with_empty(&KINDS)},
            "detail": s(), "ask": s(), "icon": s(), "about": s()
        },
        "required": ["quote", "op", "goal", "task", "title", "state", "now", "kind", "detail", "ask", "icon", "about"],
        "additionalProperties": false
    });
    let (finished, reported, checked) = crate::goals::ledger::schema_properties();
    json!({
        "type": "object",
        "properties": {"finished": finished, "reported": reported, "checked": checked, "ops": {"type": "array", "items": op}},
        "required": ["finished", "reported", "checked", "ops"],
        "additionalProperties": false
    })
}

/// One op as the model wrote it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Op {
    pub op: String,
    pub goal: String,
    pub task: String,
    pub title: String,
    pub state: String,
    pub now: String,
    pub kind: String,
    pub detail: String,
    pub ask: String,
    pub icon: String,
    pub about: String,
    pub quote: String,
}

impl Op {
    pub fn from_value(v: &Value) -> Op {
        let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_owned();
        Op { op: s("op"), goal: s("goal"), task: s("task"), title: s("title"), state: s("state"), now: s("now"), kind: s("kind"),
             detail: s("detail"), ask: s("ask"), icon: s("icon"), about: s("about"), quote: s("quote") }
    }
    pub fn to_value(&self) -> Value {
        json!({"op": self.op, "goal": self.goal, "task": self.task, "title": self.title, "state": self.state, "now": self.now,
               "kind": self.kind, "detail": self.detail, "ask": self.ask, "icon": self.icon, "about": self.about, "quote": self.quote})
    }
}

/// The answer's ops; an answer that is not `{"ops": [...]}` is an error (nothing is applied). A fence or prose
/// around the JSON is cut away.
pub fn parse(answer: &str) -> Result<Vec<Op>> {
    let v = parse_value(answer)?;
    let ops = v.get("ops").and_then(Value::as_array).context("no ops array")?;
    Ok(ops.iter().filter(|o| o.is_object()).map(Op::from_value).collect())
}

/// The answer's JSON object (a fence or prose around it dropped).
pub fn parse_value(answer: &str) -> Result<Value> {
    let t = answer.trim();
    let t = match (t.find('{'), t.rfind('}')) { (Some(a), Some(b)) if a < b => &t[a..=b], _ => t };
    serde_json::from_str(t).context("not JSON")
}

/// Lower case, letters and digits only, one space between words: quotes match whatever the case, spacing and
/// punctuation (a model drops a comma, curls a quote, joins two lines).
pub fn normalise(t: &str) -> String {
    let mapped: String = t.chars().flat_map(char::to_lowercase).map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The quote is in the new lines (normalised; at least `QUOTE_MIN` characters).
pub fn quoted(quote: &str, lines: &str) -> bool {
    let q = normalise(quote);
    q.chars().count() >= QUOTE_MIN && normalise(lines).contains(&q)
}

/// What a turn's ops are checked against: all its new lines, and the part that is the user's own (his words, and notices
/// of his clicks: "the user chose …", "the user ticked …").
/// `times`: each line's text and when it was written (epoch s), so a quote dates what it shows. `removed`: what the user
/// removed from Settled (`{title, quote}`): no task is made of it either.
#[derive(Clone, Debug, Default)]
/// `recorded`: proposal titles archived or declined before (`islands::proposals::recorded`): never proposed again.
pub struct Evidence { pub all: String, pub owner: String, pub times: Vec<(String, i64)>, pub removed: Vec<Value>, pub recorded: Vec<String> }

impl Evidence {
    /// Plain text: everything is the lines, and the user's (tests, callers without kinds); no times.
    pub fn text(t: &str) -> Evidence { Evidence { all: t.into(), owner: t.into(), ..Evidence::default() } }
}

/// An op that takes something away.
fn removes(op: &str) -> bool { matches!(op, "answered" | "drop_ask" | "finish_goal" | "pause_goal" | "drop_task") }

/// A quote strong enough to take something away: `REMOVE_WORDS` words and `REMOVE_CHARS` characters, or, for an ask
/// answered in the user's own words, `ANSWER_WORDS` and `ANSWER_CHARS`.
fn strong(op: &Op, lines: &Evidence) -> bool {
    let q = normalise(&op.quote);
    let (words, chars) = if op.op == "answered" && quoted(&op.quote, &lines.owner) { (ANSWER_WORDS, ANSWER_CHARS) } else { (REMOVE_WORDS, REMOVE_CHARS) };
    q.split(' ').count() >= words && q.chars().count() >= chars
}

/// One line of at most `n` characters, cut at a word with an ellipsis when long.
pub fn cap(s: &str, n: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n { return t }
    let cut: String = t.chars().take(n - 1).collect();
    let cut = match cut.rfind(' ') { Some(i) if i > n / 2 => cut[..i].trim_end_matches([',', ';', ':', '-']).to_owned(), _ => cut };
    format!("{cut}\u{2026}")
}

/// An SF Symbol name the app can draw (`islands::icon`'s symbol rule), else none.
pub fn symbol(s: &str) -> Option<String> {
    let t = s.trim();
    let ok = !t.is_empty() && t.len() <= 64 && t.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    ok.then(|| t.to_owned())
}

fn short_id(prefix: &str) -> String { format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]) }

/// A stable id from a title: `<prefix>-` and 8 hex of its FNV-1a hash.
fn title_id(prefix: &str, title: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in normalise(title).bytes() { h = (h ^ b as u32).wrapping_mul(0x0100_0193) }
    format!("{prefix}-{h:08x}")
}

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> &'a str { o.get(k).and_then(Value::as_str).unwrap_or("") }

fn arr<'a>(o: &'a mut Map<String, Value>, k: &str) -> &'a mut Vec<Value> {
    if !o.get(k).is_some_and(Value::is_array) { o.insert(k.into(), json!([])); }
    o.get_mut(k).and_then(Value::as_array_mut).unwrap()
}

/// A goal's state as the app reads it (`active` when missing).
pub fn goal_state(g: &Map<String, Value>) -> &str {
    match s(g, "state") { "done" => "done", "paused" => "paused", _ => "active" }
}

/// What a new Goals island starts as (docked left first, where To do was).
pub fn fresh() -> Map<String, Value> {
    json!({"id": ID, "preset": "todo", "list": ID, "title": TITLE, "icon": ICON, "side": "left", "order": 0, "items": [], "goals": []})
        .as_object().unwrap().clone()
}

/// The seed goals, no tasks.
pub fn seed_goals() -> Vec<Value> {
    SEED.iter().map(|(id, title, icon, about)| json!({"id": id, "title": title, "icon": icon, "about": about, "state": "active", "now": "", "tasks": [], "expanded": false})).collect()
}

/// The asks the user has answered by a click: ticked done, or a choice made (`chosen`, not "later": that one still waits).
fn answered_by_click(o: &Map<String, Value>) -> bool {
    o.get("done").and_then(Value::as_bool) == Some(true) || o.get("chosen").and_then(Value::as_str).is_some_and(|c| !c.is_empty() && c != "later")
}

/// Asks the user answered by a click leave; returns their titles.
pub fn settle_answered(file: &mut Map<String, Value>) -> Vec<String> {
    let items = arr(file, "items");
    let mut gone = Vec::new();
    items.retain(|v| match v.as_object() {
        Some(o) if answered_by_click(o) => { gone.push(s(o, "title").to_owned()); false }
        _ => true,
    });
    gone
}

/// The result of one op: kept or dropped, and why (the lab's `ops` and the live log).
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome { pub op: Op, pub kept: bool, pub reason: String }

impl Outcome {
    pub fn to_value(&self) -> Value {
        let mut v = self.op.to_value();
        v["kept"] = json!(self.kept);
        v["reason"] = json!(self.reason);
        v
    }
}

/// Applies `ops` to the Goals file in memory, each checked against the file as it is then and against `lines` (the
/// new stream lines: every quote must be in them). `now` is epoch seconds (`moved`, a task's `at`, an ask's `asked`).
pub fn apply(file: &mut Map<String, Value>, ops: &[Op], lines: &Evidence, now: i64) -> Vec<Outcome> {
    let mut out = Vec::new();
    for (n, op) in ops.iter().enumerate() {
        let r = if n >= MAX_OPS { Err(format!("over {MAX_OPS} ops in one answer")) }
            else if !quoted(&op.quote, &lines.all) { Err(if op.quote.trim().is_empty() { "no quote".into() } else { "quote not in the new lines".into() }) }
            else if removes(&op.op) && !strong(op, lines) { Err(format!("a quote of {REMOVE_WORDS} words takes something away")) }
            else if let Some(e) = needs_owner(file, op, &lines.owner) { Err(e) }
            // Removed stays removed: a new task the user removed from Settled, however it is worded (Goals' own `removed`
            // holds asks: it keeps asks out, never work; review 2 Nov).
            else if op.op == "progress" && op.task.is_empty()
                && crate::goals::settled::removed_in(&lines.removed, &op.title, &op.quote, now) {
                Err("User removed it".into())
            }
            // When it happened: the time of the line its quote comes from (a turn may carry an hour of lines).
            else { apply_one(file, op, lines, crate::goals::settled::when(&op.quote, lines).unwrap_or(now).min(now)) };
        out.push(match r {
            Ok(reason) => Outcome { op: op.clone(), kept: true, reason },
            Err(reason) => Outcome { op: op.clone(), kept: false, reason },
        });
    }
    out
}

/// An ask is answered on the user's word (or a notice of his click); one the shadow did not raise (Scheduled, `plan_edit`,
/// To do's) is dropped only on his word too.
fn needs_owner(file: &Map<String, Value>, op: &Op, owner: &str) -> Option<String> {
    let own = || file.get("items").and_then(Value::as_array).into_iter().flatten().any(|i| i["id"] == op.ask.as_str() && i["by"] == BY);
    match op.op.as_str() {
        "answered" if !quoted(&op.quote, owner) => Some("an answer is User's: quote his words or his click".into()),
        "decided" if !quoted(&op.quote, owner) => Some("a decision is User's: quote his words or his click".into()),
        "drop_ask" if !quoted(&op.quote, owner) && !own() => Some("an ask the shadow did not raise leaves on User's word".into()),
        _ => None,
    }
}

fn find_goal<'a>(file: &'a mut Map<String, Value>, id: &str) -> Option<&'a mut Map<String, Value>> {
    arr(file, "goals").iter_mut().filter_map(Value::as_object_mut).find(|g| s(g, "id") == id)
}

fn active(file: &mut Map<String, Value>) -> usize {
    arr(file, "goals").iter().filter_map(Value::as_object).filter(|g| goal_state(g) == "active").count()
}

fn apply_one(file: &mut Map<String, Value>, op: &Op, lines: &Evidence, now: i64) -> std::result::Result<String, String> {
    let goal_known = |file: &mut Map<String, Value>| -> std::result::Result<(), String> {
        if op.goal.is_empty() { return Err("no goal".into()) }
        if find_goal(file, &op.goal).is_none() { return Err(format!("unknown goal {}", op.goal)) }
        Ok(())
    };
    match op.op.as_str() {
        "add_goal" => {
            // Nothing enters Goals without the user's yes (2 Nov, `docs/one-system.md`): a goal the job heard of is proposed.
            let key = normalise(&cap(&op.title, TITLE_CHARS));
            if arr(file, "goals").iter().filter_map(Value::as_object).any(|g| normalise(s(g, "title")) == key) {
                return Err("a goal of that title exists".into());
            }
            // Goals waiting for his yes count against the six: never more proposed than could be taken.
            let queued = crate::islands::proposals::queue(file).iter().filter(|p| s(p, "kind") == "goal").count();
            if active(file) + queued >= MAX_ACTIVE { return Err(format!("{MAX_ACTIVE} active goals already")) }
            let d = crate::islands::proposals::Draft { list: ID.into(), kind: "goal".into(), title: cap(&op.title, TITLE_CHARS), icon: op.icon.clone(),
                about: op.about.clone(), why: op.quote.clone(), by: BY.into(), ..Default::default() };
            let (id, _) = crate::islands::proposals::push_in(file, &d, &lines.recorded, now).map_err(|e| e.to_string())?;
            Ok(format!("proposed {id}"))
        }
        "progress" => {
            goal_known(file)?;
            let state = op.state.as_str();
            if !TASK_STATES.contains(&state) { return Err(format!("task state {state:?} is not done, now or next")) }
            // A task id that lives under another goal is not this goal's to move.
            if !op.task.is_empty() {
                let elsewhere = arr(file, "goals").iter().filter(|g| g["id"] != op.goal.as_str())
                    .any(|g| g["tasks"].as_array().into_iter().flatten().any(|t| t["id"] == op.task.as_str()));
                if elsewhere { return Err(format!("task {} is under another goal", op.task)) }
            }
            let resume = {
                let g = find_goal(file, &op.goal).unwrap();
                match goal_state(g) { "done" => return Err("goal finished".into()), "paused" => true, _ => false }
            };
            if resume && active(file) >= MAX_ACTIVE { return Err(format!("{MAX_ACTIVE} active goals already")) }
            let g = find_goal(file, &op.goal).unwrap();
            let tasks = arr(g, "tasks");
            let at = if op.task.is_empty() {
                let title = cap(&op.title, TITLE_CHARS);
                if title.is_empty() { return Err("a new task needs a title".into()) }
                let key = normalise(&title);
                // The same title, else one naming the same thing (a notice and Heimdall's report word it differently:
                // never two tasks for one piece of work); a done one first, so it is not done twice.
                // Only for a done op (review 2 Nov: a new task starting must never move a done one back to now).
                let fuzzy = || (state == "done").then(|| tasks.iter().position(|t| t["state"] == "done" && t["title"].as_str().is_some_and(|x| crate::goals::settled::same(x, &title)))
                    .or_else(|| tasks.iter().position(|t| t["title"].as_str().is_some_and(|x| crate::goals::settled::same(x, &title))))).flatten();
                match tasks.iter().position(|t| t["title"].as_str().is_some_and(|x| normalise(x) == key)).or_else(fuzzy) {
                    Some(i) => i,
                    // A new piece of work to do is proposed under its goal (the user's yes is the only door), unless the user
                    // himself just asked for it to start (a `now` op quoting his own words: that is his yes, by voice).
                    // One already finished is the goal's progress and settles on its proof.
                    None if state == "next" || (state == "now" && !quoted(&op.quote, &lines.owner)) => {
                        let d = crate::islands::proposals::Draft { list: ID.into(), kind: "step".into(), title, goal: op.goal.clone(), why: op.quote.clone(), by: BY.into(), ..Default::default() };
                        let (id, _) = crate::islands::proposals::push_in(file, &d, &lines.recorded, now).map_err(|e| e.to_string())?;
                        return Ok(format!("proposed {id}"));
                    }
                    None => {
                        let mut t = json!({"id": short_id("t"), "title": title, "state": "next"});
                        if state == "now" { t["yes"] = json!({"by": "voice", "at": now, "quote": cap(&op.quote, 200)}) }
                        tasks.push(t);
                        tasks.len() - 1
                    }
                }
            } else {
                tasks.iter().position(|t| t["id"] == op.task.as_str()).ok_or_else(|| format!("unknown task {}", op.task))?
            };
            let t = tasks[at].as_object_mut().unwrap();
            // The user's tick stands: only he unticks.
            if t.get("ticked").and_then(Value::as_bool) == Some(true) && state != "done" { return Err("User ticked it done".into()) }
            let changed = s(t, "state") != state || !t.contains_key("at");
            t.insert("state".into(), json!(state));
            if changed { t.insert("at".into(), json!(now)); }
            let id = s(t, "id").to_owned();
            if resume { g.insert("state".into(), json!("active")); }
            let line = cap(&op.now, NOW_CHARS);
            if !line.is_empty() { g.insert("now".into(), json!(line)); }
            g.insert("moved".into(), json!(now));
            Ok(format!("{id} {state}"))
        }
        "finish_goal" => {
            goal_known(file)?;
            let g = find_goal(file, &op.goal).unwrap();
            if goal_state(g) == "done" { return Err("goal finished already".into()) }
            g.insert("state".into(), json!("done"));
            let line = cap(&op.now, NOW_CHARS);
            if !line.is_empty() { g.insert("now".into(), json!(line)); }
            g.insert("moved".into(), json!(now));
            Ok("finished".into())
        }
        "pause_goal" => {
            goal_known(file)?;
            let g = find_goal(file, &op.goal).unwrap();
            if goal_state(g) != "active" { return Err(format!("goal is {}", goal_state(g))) }
            g.insert("state".into(), json!("paused"));
            Ok("paused".into())
        }
        "decided" => {
            goal_known(file)?;
            if cap(&op.title, TITLE_CHARS).is_empty() { return Err("no title".into()) }
            Ok("decided".into())
        }
        "ask" => Err("asks are not the shadow's: what needs User comes as a card".into()),
        "answered" | "drop_ask" => {
            if op.ask.is_empty() { return Err("no ask".into()) }
            let items = arr(file, "items");
            let before = items.len();
            items.retain(|i| i["id"] != op.ask.as_str());
            if items.len() == before { return Err(format!("unknown ask {:?}", op.ask)) }
            Ok("removed".into())
        }
        "drop_task" => {
            goal_known(file)?;
            let g = find_goal(file, &op.goal).unwrap();
            let tasks = arr(g, "tasks");
            let Some(i) = tasks.iter().position(|t| t["id"] == op.task.as_str()) else { return Err(format!("unknown task {:?}", op.task)) };
            if tasks[i]["state"] == "done" { return Err("finished tasks stay (they are the memory)".into()) }
            if tasks[i]["ticked"] == true { return Err("User ticked it".into()) }
            tasks.remove(i);
            Ok("dropped".into())
        }
        other => Err(format!("unknown op {other:?}")),
    }
}

/// Settles answered asks and applies `ops` to `islands/goals.json` in one hold of the folder lock, against the file as
/// it is then (the user's clicks since the call stand). Unknown keys survive; a missing file starts fresh. Writes only
/// when something changed. Returns the outcomes and the file after.
pub fn apply_file(root: &Root, ops: &[Op], lines: &Evidence, now: i64) -> Result<(Vec<Outcome>, Map<String, Value>)> {
    root.locked(|| {
        let mut lines = lines.clone();
        if let Some(r) = current(root, crate::goals::settled::ID).and_then(|o| o.get("removed").and_then(Value::as_array).cloned()) { lines.removed.extend(r) }
        lines.recorded = crate::islands::proposals::recorded(root, ID);
        let lines = &lines;
        let mut file = current(root, ID).unwrap_or_else(fresh);
        let before = root.read(ID).unwrap_or_default();
        settle_answered(&mut file);
        let was = file.clone();
        let out = apply(&mut file, ops, lines, now);
        if file != before {
            file.insert("updated".into(), json!(now));
            write_list(root, ID, &file)?;
        }

        // Settled: each task newly done and each decision, dated by its line (one hold of the lock).
        let entries = crate::goals::settled::entries(&was, &file, &out, lines, now);
        for (e, why) in entries.iter().zip(crate::goals::settled::record_locked(root, &entries, now)?) {
            if why.is_empty() { eprintln!("Goals: settled {:?} ({}, {})", e.title, e.kind, e.goal) } else { eprintln!("Goals: not settled {:?}: {why}", e.title) }
        }
        Ok((out, file))
    }).inspect(|(_, file)| {
        // A proposal of this turn on screen now: Heimdall says it in one short line when it fits (told after the lock).
        let (front, _) = crate::islands::proposals::front(file, now);
        if let (Some(p), Some(h)) = (front.filter(|p| p["by"] == BY && p["proposed"] == now), crate::islands::host()) {
            h.deliver(crate::islands::Class::Context, &crate::islands::proposals::notice(ID, &p));
        }
    })
}

/// The Goals as the model reads them each turn: goals with ids, state and the latest move; the task in hand and the
/// next ones with ids; finished tasks only counted (with the newest one) unless `all` (a new segment's header: there
/// the finished tasks are the memory); the open asks.
pub fn compact(file: &Map<String, Value>, all: bool) -> String {
    let mut out = String::new();
    for g in file.get("goals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
        let mut head = format!("- {} · {} · {}", s(g, "id"), s(g, "title"), goal_state(g));
        if !s(g, "about").is_empty() { head.push_str(&format!(" · about: {}", s(g, "about"))) }
        if !s(g, "now").is_empty() { head.push_str(&format!(" · now: {}", s(g, "now"))) }
        out.push_str(&head);
        out.push('\n');
        let tasks: Vec<&Map<String, Value>> = g.get("tasks").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object).collect();
        let done: Vec<&&Map<String, Value>> = tasks.iter().filter(|t| s(t, "state") == "done").collect();
        if all {
            for t in &done { out.push_str(&format!("  done {} {}\n", s(t, "id"), s(t, "title"))) }
        } else if let Some(last) = done.iter().max_by_key(|t| t.get("at").and_then(Value::as_i64).unwrap_or(0)) {
            out.push_str(&format!("  {} done, latest: {}\n", done.len(), s(last, "title")));
        }
        for st in ["now", "next"] {
            for t in tasks.iter().filter(|t| s(t, "state") == st) { out.push_str(&format!("  {st} {} {}\n", s(t, "id"), s(t, "title"))) }
        }
    }
    let asks: Vec<String> = file.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object)
        .map(|i| {
            let mut l = format!("- {} · {}", s(i, "id"), s(i, "title"));
            for k in ["kind", "goal", "detail"] { if !s(i, k).is_empty() { l.push_str(&format!(" · {k}: {}", s(i, k))) } }
            l
        }).collect();
    // What waits for the user's yes (code proposes; the model never proposes the same again).
    let props: Vec<String> = crate::islands::proposals::queue(file).iter().map(|p| {
        let goal = p.get("goal").and_then(Value::as_str).filter(|g| !g.is_empty()).map(|g| format!(" under {g}")).unwrap_or_default();
        format!("- {} {}{goal}", p.get("kind").and_then(Value::as_str).unwrap_or(""), s(p, "title"))
    }).collect();
    if !props.is_empty() { out.push_str("Proposed, waiting for User's yes:\n"); for p in props { out.push_str(&p); out.push('\n'); } }
    out.push_str(if asks.is_empty() { "Asks: none\n" } else { "Asks:\n" });
    for a in asks { out.push_str(&a); out.push('\n'); }
    out
}

/// First contact (every start, cheap). A truly first run (no Goals file, closed or kept copy, `seeded` mark or
/// migrated To do anywhere) gets the seed goals (none in the public edition: a blank first run); a Goals file that lost
/// its goals while a copy has them gets them back (`current`; review 31 Oct: a reseed over them lost everything); seed
/// goals without `about` get theirs, once. An old `todo.json` is migrated once, after a one-time backup of the
/// islands folder: its open items with a time (`due`, `when`) go to Scheduled, its open waiting items become asks
/// (goal "": the shadow files them); its running and discussed items are not shown to the shadow and stay in the kept
/// file, `islands/.todo-migrated.json`; `todo.json` is removed so the dock shows Goals in its
/// place. Returns what it did.
pub fn ensure(root: &Root, now: i64, seed: bool) -> Result<Vec<String>> {
    let todo = root.file("todo");
    let migrated = root.islands().join(".todo-migrated.json");
    let copies = || [root.read(ID), read_object(&closed_file(root, ID)), kept(root, ID)];
    let first_run = !migrated.exists() && !copies().iter().flatten().any(|o| o.contains_key("seeded") || has_goals(o));
    let cur = current(root, ID);
    let disk = root.read(ID);
    let about_missing = |o: &Map<String, Value>| seed && o.get("goals").and_then(Value::as_array).into_iter().flatten()
        .any(|g| g.get("about").is_none() && SEED.iter().any(|x| g["id"] == x.0));
    let rescue = disk.is_some() && cur != disk;
    if !first_run && !rescue && !cur.as_ref().is_some_and(about_missing) && (!todo.exists() || migrated.exists()) { return Ok(Vec::new()) }
    root.locked(|| {
        let mut did = Vec::new();
        let mut file = current(root, ID).unwrap_or_else(fresh);
        let before = root.read(ID).unwrap_or_default();
        if first_run {
            file.insert("goals".into(), Value::Array(if seed { seed_goals() } else { vec![] }));
            file.insert("seeded".into(), json!(true));
            did.push(if seed { "seeded the four goals".to_owned() } else { "started an empty Goals list".to_owned() });
        } else if root.read(ID).is_some_and(|d| d != file) {
            did.push("the Goals file had lost its goals; they are back from the kept copy".into());
        }
        if seed {
            for g in arr(&mut file, "goals").iter_mut().filter_map(Value::as_object_mut) {
                if g.contains_key("about") { continue }
                if let Some(x) = SEED.iter().find(|x| g.get("id").and_then(Value::as_str) == Some(x.0)) {
                    g.insert("about".into(), json!(x.3));
                }
            }
        }
        // A To do that does not parse is left where it is (never replaced by nothing).
        let readable = read_object(&todo);
        if todo.exists() && readable.is_none() { did.push("todo.json does not parse; left as it is".into()) }
        // Its timed items go to Scheduled: one that is there but does not parse is never replaced from a copy; the
        // migration waits for a readable one (the next start; review 31 Oct).
        let sched_broken = root.file("scheduled").exists() && root.read("scheduled").is_none();
        if sched_broken && readable.is_some() && !migrated.exists() { did.push("scheduled.json does not parse; the old list waits for the next start".into()) }
        let readable = readable.filter(|_| !sched_broken);
        let mut timed: Vec<Value> = Vec::new();
        let moved_todo = if let (Some(old), false) = (readable, migrated.exists()) {
            backup_once(root)?;
            let items = arr(&mut file, "items");
            let mut n = 0;
            for v in old.get("items").and_then(Value::as_array).into_iter().flatten() {
                let Some(o) = v.as_object() else { continue };
                let open = o.get("done").and_then(Value::as_bool) != Some(true);
                if !open || s(o, "title").trim().is_empty() { continue }
                let mut a = o.clone();
                a.remove("group");
                // An obligation with a time is Scheduled's ("we have to do this because it's
                // scheduled"), whatever its group.
                if o.get("due").is_some_and(|d| !d.is_null()) || !s(o, "when").is_empty() {
                    if a.get("id").and_then(Value::as_str).is_none_or(str::is_empty) { a.insert("id".into(), json!(title_id("s", s(o, "title")))); }
                    timed.push(Value::Object(a));
                    continue;
                }
                if s(o, "group") != "waiting" { continue }
                // From its title, so a migration that stopped halfway and ran again finds it (never a duplicate).
                if a.get("id").and_then(Value::as_str).is_none_or(str::is_empty) { a.insert("id".into(), json!(title_id("a", s(o, "title")))); }
                if items.iter().any(|i| i["id"] == a["id"]) { continue }
                a.entry("goal").or_insert(json!(""));
                a.entry("asked").or_insert(json!(now));
                items.push(Value::Object(a));
                n += 1;
            }
            did.push(format!("moved {n} waiting To do item(s) into Goals as asks"));
            Some(old)
        } else { None };
        if file != before {
            file.insert("updated".into(), json!(now));
            write_list(root, ID, &file)?;
        }
        if !timed.is_empty() {
            let mut sched = current(root, "scheduled").unwrap_or_else(|| json!({"id": "scheduled", "preset": "todo", "list": "scheduled",
                "title": "Scheduled", "side": "left", "order": 1, "items": []}).as_object().unwrap().clone());
            let n = timed.len();
            let its = arr(&mut sched, "items");
            for t in timed { if !its.iter().any(|i| i["id"] == t["id"]) { its.push(t) } }
            write_list(root, "scheduled", &sched)?;
            did.push(format!("moved {n} To do item(s) with a time into Scheduled"));
        }
        if let Some(old) = moved_todo {
            // Goals and Scheduled are written first: a failure here leaves To do where it was, never neither.
            write_atomic(&migrated, &Value::Object(old))?;
            std::fs::remove_file(&todo)?;
            did.push("kept todo.json as islands/.todo-migrated.json".into());
        }
        Ok(did)
    })
}

/// Before the migration writes: the islands folder's files copied once to `<root>/backup-goals-<YYYYMMDD-HHMMSS>/`
/// (beside `islands/`, so the helper never shows it; `islands::plan::backup_once`'s rule, its own prefix because that
/// one runs once ever and the user's folder has had one since 22 Oct). Nothing when one exists. Call holding the lock.
pub fn backup_once(root: &Root) -> Result<Option<std::path::PathBuf>> {
    let taken = std::fs::read_dir(&root.0).into_iter().flatten().flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("backup-goals-") && e.path().is_dir());
    if taken { return Ok(None) }
    let dir = root.0.join(format!("backup-goals-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    std::fs::create_dir_all(&dir)?;
    for e in std::fs::read_dir(root.islands())?.flatten() {
        let name = e.file_name();
        if !e.path().is_file() || name.to_string_lossy().ends_with(".tmp") || name.to_string_lossy() == ".write.lock" { continue }
        std::fs::copy(e.path(), dir.join(&name))?;
    }
    eprintln!("Goals: To do migrated to Goals; the islands folder was backed up to {}", dir.display());
    Ok(Some(dir))
}

/// The copy of a plan list kept at its last write (`islands/.kept-<list>.json`, hidden from the helper): what a close
/// that arrives without the island falls back to (bug note 2026-10-31T15-57-16, `islands::plan::keep_closed`).
pub fn kept_file(root: &Root, list: &str) -> std::path::PathBuf { root.islands().join(format!(".kept-{list}.json")) }
pub fn kept(root: &Root, list: &str) -> Option<Map<String, Value>> { read_object(&kept_file(root, list)) }
/// Where a closed plan list's content waits for `islands::plan::reopen_closed` (`.closed-<list>.json`).
pub fn closed_file(root: &Root, list: &str) -> std::path::PathBuf { root.islands().join(format!(".closed-{list}.json")) }

/// A plan list as a writer must start from (call holding the lock): the file; else, when the user closed it and an app
/// deleted the file, the closed copy, still hidden (it stays closed, and `reopen_closed` brings it back with what was
/// written since); else the copy kept at its last write (a file that went away or no longer parses). `None`: the
/// list never existed. Review 31 Oct: a writer starting from a fresh list here overwrote both copies and lost Goals.
///
/// Goals: a file on disk that has no goals while a closed or kept copy has them (an app wrote a fresh list holding
/// only a due ask after the file went away) is merged: the copy's goals, the copy's asks and the disk's (the disk's win
/// on the same id), the disk's other keys; hidden when the copy was the closed one (review 31 Oct).
pub fn current(root: &Root, list: &str) -> Option<Map<String, Value>> {
    let disk = root.read(list);
    let closed = read_object(&closed_file(root, list)).map(|mut o| { o.insert("hidden".into(), json!(true)); o });
    if list != ID { return disk.or(closed).or_else(|| kept(root, list)) }
    if disk.as_ref().is_some_and(has_goals) { return disk }
    let copy = closed.clone().filter(has_goals).or_else(|| kept(root, list).filter(has_goals));
    match (disk, copy) {
        (Some(d), Some(c)) => {
            let mut out = d.clone();
            out.insert("goals".into(), c["goals"].clone());
            let mut items: Vec<Value> = c.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            for i in d.get("items").and_then(Value::as_array).into_iter().flatten() {
                match items.iter_mut().find(|x| x.get("id").is_some() && x["id"] == i["id"]) { Some(x) => *x = i.clone(), None => items.push(i.clone()) }
            }
            out.insert("items".into(), Value::Array(items));
            for k in ["seeded", "hidden"] { if let Some(v) = c.get(k) { out.entry(k).or_insert(v.clone()); } }
            Some(out)
        }
        (Some(d), None) => Some(d),
        (None, c) => c.or(closed).or_else(|| kept(root, list)),
    }
}

/// The file holds at least one goal.
pub fn has_goals(o: &Map<String, Value>) -> bool { o.get("goals").and_then(Value::as_array).is_some_and(|g| !g.is_empty()) }

/// A plan list written, and its kept copy (call holding the folder lock).
pub fn write_list(root: &Root, list: &str, o: &Map<String, Value>) -> Result<()> {
    write_atomic(&root.file(list), &Value::Object(o.clone()))?;
    write_atomic(&kept_file(root, list), &Value::Object(o.clone()))
}
