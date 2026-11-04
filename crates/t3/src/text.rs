//! What the T3 tools say to the agent: short plain lines (agent::plain has the rule and the user's words). Worker replies
//! and questions pass in full; ids the agent must pass back are written out whole.
use serde_json::Value;

/// A worker's title without Heimdall's launch marker (`Title [heimdall:…]`).
pub fn title(raw: &str) -> String {
    let t = raw.find(" [heimdall:").map_or(raw, |i| &raw[..i]).trim();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One model option's choices as T3 lists them, each with whether it is the default. Two shapes: the older one
/// (`type`, `options: [{id, isDefault}]`, `currentValue`) and the one since T3's update of 23 Oct (`values: [...]`,
/// `default`). Bug note 2026-10-23T01-29-13: the newer shape read as no choices at all, so `delegate_task` refused
/// `{"effort": "medium"}` with "effort takes one of" and an empty list.
pub fn option_choices(o: &Value) -> Vec<(Value, bool)> {
    if let Some(values) = o["values"].as_array() {
        return values.iter().map(|v| (v.clone(), o["default"] == *v)).collect();
    }
    if o["type"] == "boolean" {
        return vec![(Value::Bool(true), false), (Value::Bool(false), false)];
    }
    let list: Vec<&Value> = o["options"].as_array().into_iter().flatten().collect();
    let marked = list.iter().any(|c| c["isDefault"] == true);
    let current = o["currentValue"].as_str();
    list.iter()
        .filter_map(|c| {
            let id = c["id"].as_str()?;
            Some((Value::String(id.into()), c["isDefault"] == true || (!marked && current == Some(id))))
        })
        .collect()
}
/// A choice as the agent writes it: `medium`, `true`.
pub fn choice_text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// The model catalog in one line per usable model: provider, model id and each option's choices, `*` marking the
/// default. These are exactly the values `delegate_task`'s target takes.
pub fn catalog_line(row: &Value) -> String {
    let model = &row["model"];
    let mut line = format!("{} {}", row["provider"].as_str().unwrap_or("?"), model["id"].as_str().unwrap_or("?"));
    let options: Vec<String> = model["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let id = o["id"].as_str()?;
            let choices: Vec<String> = option_choices(o)
                .iter()
                .map(|(v, default)| if *default { format!("{}*", choice_text(v)) } else { choice_text(v) })
                .collect();
            (!choices.is_empty()).then(|| format!("{id} {}", choices.join("|")))
        })
        .collect();
    if !options.is_empty() {
        line.push_str(" — ");
        line.push_str(&options.join(", "));
    }
    line
}
/// `orchestrator_capabilities` as catalog lines: every model of every enabled provider that can run full access.
pub fn catalog(capabilities: &Value) -> String {
    let mut lines = vec![];
    let mut unusable = vec![];
    for p in capabilities["providers"].as_array().into_iter().flatten() {
        let models: Vec<&Value> = p["models"].as_array().into_iter().flatten().collect();
        let disabled = p["constraints"].as_array().into_iter().flatten().any(|c| c.as_str().is_some_and(|c| c.contains("disabled")));
        let full_access = p["runtimeModes"].is_null() || p["runtimeModes"].as_array().is_some_and(|m| m.iter().any(|x| x == "full-access"));
        if models.is_empty() || disabled {
            continue;
        }
        if !full_access {
            unusable.push(p["providerInstanceId"].as_str().unwrap_or("?").to_owned());
            continue;
        }
        for m in models {
            lines.push(catalog_line(&serde_json::json!({"provider":p["providerInstanceId"],"model":m})));
        }
    }
    let mut out = if lines.is_empty() { "No usable T3 models.".to_owned() } else { format!("provider model — options (* default):\n{}", lines.join("\n")) };
    if !unusable.is_empty() {
        out.push_str(&format!("\nNot usable (no full-access mode): {}.", unusable.join(", ")));
    }
    out
}

/// A task's state in words, from its T3 status and the sub-agents and background commands it still waits on.
pub fn task_state(status: &str, agents: usize, commands: usize) -> String {
    let waiting = match (agents, commands) {
        (0, 0) => None,
        (a, 0) => Some(format!("waiting on {a} sub-agent{}", if a == 1 { "" } else { "s" })),
        (0, c) => Some(format!("waiting on {c} background command{}", if c == 1 { "" } else { "s" })),
        (a, c) => Some(format!("waiting on {a} sub-agent{} and {c} background command{}", if a == 1 { "" } else { "s" }, if c == 1 { "" } else { "s" })),
    };
    match status {
        "preparing" | "queued" | "starting" | "running" => "working".into(),
        "waiting" => "waiting for an answer or approval".into(),
        "completed" => waiting.unwrap_or_else(|| "done".into()),
        "idle" => waiting.unwrap_or_else(|| "idle".into()),
        other => other.into(),
    }
}
/// One task of `task_status`: the id to pass back, then what tells tasks apart.
pub fn task_line(id: &str, title: Option<&str>, state: &str, model: &str) -> String {
    let mut parts = vec![id.to_owned()];
    if let Some(t) = title.map(self::title).filter(|t| !t.is_empty()) {
        parts.push(format!("'{t}'"));
    }
    parts.push(state.to_owned());
    if !model.is_empty() {
        parts.push(model.to_owned());
    }
    parts.join(" · ")
}

/// How long ago an RFC 3339 time was, in words ("just now", "5 min ago", "3 h ago", "2 days ago").
pub fn ago(iso: &str, now_ms: u64) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(iso).ok()?.timestamp_millis().max(0) as u64;
    let s = now_ms.saturating_sub(at) / 1000;
    Some(match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=172_799 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    })
}
/// An RFC 3339 time as local `HH:MM`.
pub fn clock(iso: &str) -> Option<String> {
    Some(chrono::DateTime::parse_from_rfc3339(iso).ok()?.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// The notice a finished task sends: who, how it ended, and its reply in full when the notice stays within `budget`
/// bytes (`notice_reply_budget`), else a pointer to `task_status`. `budget` 0 is the short form. `still` says what it
/// still waits on (`still`: a held turn delivered at its hold limit).
pub fn finished(task: &str, title: Option<&str>, state: &str, reply: Option<&str>, budget: usize) -> String {
    finished_with(task, title, state, reply, budget, None)
}
pub fn finished_with(task: &str, title: Option<&str>, state: &str, reply: Option<&str>, budget: usize, still: Option<&str>) -> String {
    let who = match title.map(self::title).filter(|t| !t.is_empty()) {
        Some(t) => format!("Task '{t}' ({task})"),
        None => format!("Task {task}"),
    };
    ended(&who, &format!("task_status {{taskId: \"{task}\"}}"), state, reply, budget, still)
}
/// The notice for a run of someone else's thread that Heimdall sent a message into (day review T1, 
/// "if I have to wake you up and then tell you to read it and then tell you to answer it like that's just stupid").
pub fn watched(thread: &str, title: Option<&str>, state: &str, reply: Option<&str>, budget: usize, still: Option<&str>) -> String {
    let who = match title.map(self::title).filter(|t| !t.is_empty()) {
        Some(t) => format!("Thread '{t}' ({thread}), which you sent a message,"),
        None => format!("Thread {thread}, which you sent a message,"),
    };
    ended(&who, &format!("t3_thread_read {{threadId: \"{thread}\", last: 1}}"), state, reply, budget, still)
}
fn ended(who: &str, read: &str, state: &str, reply: Option<&str>, budget: usize, still: Option<&str>) -> String {
    let still = still.map(|s| format!(" {s}")).unwrap_or_default();
    let notice = match reply.map(str::trim).filter(|r| !r.is_empty()) {
        None => format!("{who} {state} with no reply text.{still}"),
        Some(r) if r.len() > notice_reply_budget(budget) => format!(
            "{who} {state}.{still} Its reply is {} KB, too long for a notice; read it with {read}.",
            r.len().div_ceil(1024)
        ),
        Some(r) => format!("{who} {state}.{still} {}:\n\n{r}", if state == "completed" { "Final reply" } else { "Last reply" }),
    };
    format!("{notice}\n\n{SILENCE_RULE}")
}
/// What a result delivered at its hold limit still waits on: sub-agents still working (or waiting on a question) and
/// background work left running. None when nothing is left.
pub fn still(agents: &[String], background: &[String]) -> Option<String> {
    still_with(agents, background, &[])
}
/// `still`, and background commands nothing can check (`unchecked`: no output file known), named without claiming
/// they run (night pass 3, 21 Oct 04:00: four commands that had finished were reported as still running).
pub fn still_with(agents: &[String], background: &[String], unchecked: &[String]) -> Option<String> {
    let mut parts = vec![];
    if !agents.is_empty() {
        let names: Vec<String> = agents.iter().map(|a| format!("'{}'", a.replace(['\'', '\n'], " ").trim())).collect();
        let (one, more) = (names.len() == 1, names.join(", "));
        parts.push(format!("It is still waiting on sub-agent{} {more}; a later result may follow.", if one { "" } else { "s" }));
    }
    if !background.is_empty() {
        parts.push(format!("It still has background work running: {}.", background.join("; ")));
    }
    if !unchecked.is_empty() {
        parts.push(format!("Heimdall cannot check whether its background work still runs: {}.", unchecked.join("; ")));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}
/// Bug note 2026-10-24T23-34-21: a turn's last words when it failed on its provider's sign-in (Claude's "Failed to
/// authenticate: OAuth session expired and could not be refreshed", an expired or revoked token, a 401): that text
/// in one line, at most 160 characters. Anything else, including a spent quota, is None.
pub fn auth_failure(reply: &str) -> Option<String> {
    const SIGNS: [&str; 8] = [
        "failed to authenticate",
        "oauth session expired",
        "could not be refreshed",
        "oauth token has expired",
        "authentication_error",
        "invalid api key",
        "please run /login",
        "401 unauthorized",
    ];
    let low = reply.to_lowercase();
    if !SIGNS.iter().any(|s| low.contains(s)) {
        return None;
    }
    let line = reply.lines().map(str::trim).find(|l| SIGNS.iter().any(|s| l.to_lowercase().contains(s))).unwrap_or(reply.trim());
    Some(crate::client::bounded_text(line, 160))
}
/// The last line of a notice whose news may need nothing from the user (a worker finished, a T3 update).
pub const SILENCE_RULE: &str = "Tell User only what he needs to know; if nothing here needs him, your whole reply is NO_REPLY.";
/// The reply bytes a notice of `notice_budget` bytes (`Actions::notice_budget`, from the live agent message limit)
/// may paste: the budget less room for the notice's own lines. A larger reply is pointed to instead: Heimdall's
/// harness rejects a message above half its context window in bytes (64 KB at the smallest context setting,
/// bridge/startup.rs). Reading it pages instead.
pub fn notice_reply_budget(notice_budget: usize) -> usize {
    notice_budget.saturating_sub(1024)
}

/// A pending T3 question in full: each question with its options, and the ids an answer needs.
pub fn question(q: &Value) -> String {
    let parts: Vec<String> = q["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            let mut s = String::new();
            if let Some(h) = x["header"].as_str().filter(|h| !h.trim().is_empty()) {
                s.push_str(&format!("{h}: "));
            }
            s.push_str(x["question"].as_str().unwrap_or("").trim());
            let options: Vec<String> = x["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    let label = o["label"].as_str().or_else(|| o.as_str())?;
                    Some(match o["description"].as_str().filter(|d| !d.trim().is_empty()) {
                        Some(d) => format!("{label} ({d})"),
                        None => label.to_owned(),
                    })
                })
                .collect();
            if !options.is_empty() {
                s.push_str(&format!(" Options: {}.", options.join(" / ")));
            }
            if x["multiSelect"] == true {
                s.push_str(" Several options may be picked.");
            }
            if x["allowCustomAnswer"] == true {
                s.push_str(" A custom answer is allowed.");
            }
            if let Some(id) = x["id"].as_str() {
                s.push_str(&format!(" [question {id}]"));
            }
            s
        })
        .collect();
    if parts.is_empty() {
        return agent::plain::render(q);
    }
    parts.join("\n")
}

/// A JSON Schema as one line per argument (`*` = required), nested objects indented. For `t3_tools {tool}`.
pub fn schema(s: &Value) -> String {
    let mut out = vec![];
    properties(s, 1, &mut out);
    out.join("\n")
}
fn properties(s: &Value, depth: usize, out: &mut Vec<String>) {
    let Some(props) = s["properties"].as_object() else { return };
    let required: Vec<&str> = s["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    for (name, p) in props {
        let star = if required.contains(&name.as_str()) { "*" } else { "" };
        let mut line = format!("{}{name}{star}: {}", "  ".repeat(depth), kind(p));
        if let Some(d) = p["description"].as_str().filter(|d| !d.trim().is_empty()) {
            line.push_str(&format!(" — {}", d.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        out.push(line);
        let object = object_of(p);
        if let Some(o) = object {
            properties(o, depth + 1, out);
        }
    }
}
/// The object schema whose fields belong under a property: itself, its array items, or its non-null alternative.
fn object_of(p: &Value) -> Option<&Value> {
    if p["properties"].is_object() {
        return Some(p);
    }
    if p["items"]["properties"].is_object() {
        return Some(&p["items"]);
    }
    for key in ["anyOf", "oneOf"] {
        let objects: Vec<&Value> = p[key].as_array().into_iter().flatten().filter(|a| a["properties"].is_object() || a["items"]["properties"].is_object()).collect();
        if objects.len() == 1 {
            return object_of(objects[0]);
        }
    }
    None
}
fn kind(p: &Value) -> String {
    if let Some(values) = p["enum"].as_array() {
        return values.iter().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).collect::<Vec<_>>().join("|");
    }
    if let Some(c) = p.get("const") {
        return format!("exactly {}", c.as_str().map(str::to_owned).unwrap_or_else(|| c.to_string()));
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(alternatives) = p[key].as_array() {
            let mut kinds: Vec<String> = alternatives.iter().filter(|a| a["type"] != "null").map(kind).collect();
            kinds.dedup();
            let nullable = alternatives.iter().any(|a| a["type"] == "null");
            return format!("{}{}", kinds.join(" or "), if nullable { " or null" } else { "" });
        }
    }
    let types: Vec<&str> = match &p["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let nullable = types.contains(&"null");
    let mut base = match types.iter().find(|t| **t != "null").copied() {
        Some("array") => format!("list of {}", if p["items"].is_object() { kind(&p["items"]) } else { "values".into() }),
        Some("object") if !p["properties"].is_object() => match &p["additionalProperties"] {
            Value::Object(_) => format!("map of {}", kind(&p["additionalProperties"])),
            _ => "object".into(),
        },
        Some(t) => t.to_owned(),
        None => "any".into(),
    };
    let bound = |lo: &str, hi: &str| match (p[lo].as_f64(), p[hi].as_f64()) {
        (Some(a), Some(b)) => Some(format!("{a}–{b}")),
        (Some(a), None) => Some(format!("≥{a}")),
        (None, Some(b)) => Some(format!("≤{b}")),
        _ => None,
    };
    if let Some(b) = bound("minimum", "maximum").or_else(|| bound("minLength", "maxLength").map(|b| format!("{b} chars"))) {
        base.push_str(&format!(" {b}"));
    }
    if nullable {
        base.push_str(" or null");
    }
    base
}

/// Of the approval cards still waiting in a thread's activity, how many wait on a tool call that has not run; None when
/// no waiting card is in `items`. A card's id ends `native-request%3A<tool call id>%3A<uuid>`, the tool call's own
/// item `native-item:<tool call id>`: a completed or failed item means the prompt was answered (by Heimdall's
/// permission hook) and T3 just never closed the card.
pub fn open_approvals(items: &[serde_json::Value]) -> Option<usize> {
    let tool_call = |id: &str| id.rsplit_once("native-request%3A").and_then(|(_, rest)| rest.split("%3A").next()).map(str::to_owned);
    let ran = |call: &str| items.iter().any(|i| {
        i["type"] != "approval_request"
            && i["itemId"].as_str().is_some_and(|id| id.ends_with(&format!("native-item:{call}")))
            && matches!(i["status"].as_str(), Some("completed" | "failed"))
    });
    let waiting: Vec<&serde_json::Value> = items.iter().filter(|i| i["type"] == "approval_request" && i["status"] == "waiting").collect();
    if waiting.is_empty() { return None; }
    Some(waiting.iter().filter(|i| i["itemId"].as_str().and_then(tool_call).is_none_or(|c| !ran(&c))).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn catalog_lines_golden() {
        let codex = json!({"provider":"codex","family":"codex","runtime_modes":null,"model":{"id":"gpt-6-luna","label":"GPT-6-Luna","options":[{"currentValue":"medium","id":"reasoningEffort","label":"Reasoning","options":[{"id":"low","label":"Low"},{"id":"medium","isDefault":true,"label":"Medium"},{"id":"high","label":"High"}],"type":"select"}]}});
        assert_eq!(catalog_line(&codex), "codex gpt-6-luna — reasoningEffort low|medium*|high");
        let claude = json!({"provider":"claudeAgent","model":{"id":"claude-opus-5-5","options":[{"id":"effort","options":[{"id":"low"},{"id":"medium","isDefault":true},{"id":"max"}],"type":"select"},{"id":"fastMode","label":"Fast","type":"boolean"}]}});
        assert_eq!(catalog_line(&claude), "claudeAgent claude-opus-5-5 — effort low|medium*|max, fastMode true|false");
        assert_eq!(catalog_line(&json!({"provider":"grok","model":{"id":"grok-build","options":[]}})), "grok grok-build");
        // Without isDefault, the current value marks the default.
        let current = json!({"provider":"p","model":{"id":"m","options":[{"id":"e","currentValue":"b","type":"select","options":[{"id":"a"},{"id":"b"}]}]}});
        assert_eq!(catalog_line(&current), "p m — e a|b*");
        // T3's shape since 23 Oct (bug note 2026-10-23T01-29-13), as orchestrator_capabilities returns it live.
        let live = json!({"provider":"claudeAgent","model":{"id":"claude-opus-5-5","label":"Claude Opus 5.5","options":[
            {"id":"effort","values":["low","medium","high","xhigh","max","ultracode","ultrathink"],"default":"medium"},
            {"id":"fastMode","values":[false,true]}]}});
        assert_eq!(catalog_line(&live), "claudeAgent claude-opus-5-5 — effort low|medium*|high|xhigh|max|ultracode|ultrathink, fastMode false|true");
    }
    #[test]
    fn task_lines_and_states_golden() {
        assert_eq!(
            task_line("mcp:9c59", Some("Haiku reply test [heimdall:abc123]"), "done", "claude-haiku-5-5"),
            "mcp:9c59 · 'Haiku reply test' · done · claude-haiku-5-5"
        );
        assert_eq!(task_state("running", 2, 1), "working");
        assert_eq!(task_state("completed", 0, 0), "done");
        assert_eq!(task_state("completed", 3, 0), "waiting on 3 sub-agents");
        assert_eq!(task_state("completed", 1, 1), "waiting on 1 sub-agent and 1 background command");
        assert_eq!(task_state("completed", 0, 2), "waiting on 2 background commands");
        assert_eq!(task_state("waiting", 0, 0), "waiting for an answer or approval");
        assert_eq!(task_state("failed", 1, 0), "failed");
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-19T16:30:00Z").unwrap().timestamp_millis() as u64;
        assert_eq!(ago("2026-10-19T16:29:31Z", now).unwrap(), "just now");
        assert_eq!(ago("2026-10-19T16:25:00Z", now).unwrap(), "5 min ago");
        assert_eq!(ago("2026-10-19T13:30:00Z", now).unwrap(), "3 h ago");
        assert_eq!(ago("2026-10-16T16:30:00Z", now).unwrap(), "3 days ago");
        assert!(ago("not a time", now).is_none());
    }
    #[test]
    fn finished_notice_carries_the_whole_reply() {
        let reply = format!("{}\n\nSecond paragraph: zażółć 😀.\n\n{}", "Line of a long report. ".repeat(1500), "Last paragraph.");
        let n = finished("mcp:w", Some("Haiku reply test [heimdall:ff]"), "completed", Some(&reply), 64 * 1024);
        assert!(n.starts_with("Task 'Haiku reply test' (mcp:w) completed. Final reply:\n\n"), "{}", &n[..80]);
        assert!(n.ends_with(&format!("{}\n\n{SILENCE_RULE}", reply.trim())));
        assert_eq!(finished("w", None, "failed", None, 64 * 1024), format!("Task w failed with no reply text.\n\n{SILENCE_RULE}"));
        assert_eq!(finished("w", None, "interrupted", Some("half"), 64 * 1024), format!("Task w interrupted. Last reply:\n\nhalf\n\n{SILENCE_RULE}"));
        assert_eq!(
            finished_with("w", None, "completed", Some("Serving at :4321."), 64 * 1024, still(&[], &["npm run dev".into()]).as_deref()),
            format!("Task w completed. It still has background work running: npm run dev. Final reply:\n\nServing at :4321.\n\n{SILENCE_RULE}")
        );
        assert_eq!(
            watched("t-qwen", Some("Compare Qwen 27B"), "completed", Some("DFlash2 wins on all three."), 64 * 1024, None),
            format!("Thread 'Compare Qwen 27B' (t-qwen), which you sent a message, completed. Final reply:\n\nDFlash2 wins on all three.\n\n{SILENCE_RULE}")
        );
        assert!(watched("t", None, "completed", Some("x"), 0, None).contains("read it with t3_thread_read {threadId: \"t\", last: 1}"));
        // A result delivered at the hold limit names what it still waits on.
        assert_eq!(still(&[], &[]), None);
        assert_eq!(still(&["Research oMLX".into()], &[]).unwrap(), "It is still waiting on sub-agent 'Research oMLX'; a later result may follow.");
        assert_eq!(
            still(&["A".into(), "B's check".into()], &["npm run dev".into()]).unwrap(),
            "It is still waiting on sub-agents 'A', 'B s check'; a later result may follow. It still has background work running: npm run dev."
        );
        assert!(SILENCE_RULE.contains(agent::reply::SILENT), "the harness's silent token");
        // The live budget decides (`Actions::notice_budget`): every notice fits its budget, a longer reply is pointed to.
        for limit in [64 * 1024, 100 * 1024, 200 * 1024] {
            let budget = limit - limit / 8;
            let fits = "y".repeat(notice_reply_budget(budget));
            let n = finished("w", Some("t"), "completed", Some(&fits), budget);
            assert!(n.contains(&fits) && n.len() <= budget, "{limit}: {}", n.len());
            let huge = "x".repeat(notice_reply_budget(budget) + 1);
            let n = finished("w", None, "completed", Some(&huge), budget);
            assert!(n.contains("too long for a notice; read it with task_status") && n.len() < 300, "{n}");
        }
        assert!(finished("w", None, "completed", Some("short"), 0).contains("read it with task_status {taskId: \"w\"}"), "budget 0: the short form");
    }
    #[test]
    fn every_question_of_a_request_is_rendered_with_its_own_picks() {
        let q = json!({"requestId":"q-2","questions":[
            {"id":"q1","header":"Voice","question":"Which voice?","options":["Warm","Bright"]},
            {"id":"q2","header":"Tones","question":"Which tones?","multiSelect":true,"options":[{"label":"Soft","description":"low"},"Loud"]}
        ]});
        assert_eq!(question(&q), "Voice: Which voice? Options: Warm / Bright. [question q1]\nTones: Which tones? Options: Soft (low) / Loud. Several options may be picked. [question q2]");
    }
    #[test]
    fn question_and_schema_golden() {
        let q = json!({"requestId":"q-1","questions":[{"id":"q","header":"Plan","question":"Which way?","options":[{"label":"Fast","description":"skip tests"},{"label":"Safe","description":""}],"allowCustomAnswer":true}]});
        assert_eq!(question(&q), "Plan: Which way? Options: Fast (skip tests) / Safe. A custom answer is allowed. [question q]");
        let s = json!({"type":"object","required":["threadId"],"properties":{
            "threadId":{"type":"string","description":"The thread."},
            "view":{"anyOf":[{"type":"string","enum":["messages","activity"]},{"type":"null"}]},
            "limit":{"type":["integer","null"],"minimum":1,"maximum":100},
            "answers":{"type":"object","additionalProperties":{}},
            "workspaceStrategy":{"type":"object","properties":{"type":{"const":"worktree"},"branch":{"type":"string","maxLength":200}},"required":["type"]}
        }});
        assert_eq!(schema(&s), "  answers: map of any\n  limit: integer 1–100 or null\n  threadId*: string — The thread.\n  view: messages|activity or null\n  workspaceStrategy: object\n    branch: string ≤200 chars\n    type*: exactly worktree");
    }

    /// The live T3 activity of 28 Oct 19:21: the hook approved the rm, the command ran, the card stayed "waiting".
    #[test]
    fn an_approval_card_whose_command_ran_is_not_a_wait() {
        let card = |call: &str, status: &str| serde_json::json!({"type":"approval_request","status":status,"itemId":format!("turn-item:runtime-request:runtime-request%3Aprovider%3AclaudeAgent%3Aprovider-turn%3Aprovider-turn%253A...%3Anative-request%3A{call}%3Ad55543bb-4f05-4292-bac7-4be82e69433b")});
        let command = |call: &str, status: &str| serde_json::json!({"type":"command_execution","status":status,"itemId":format!("turn-item:provider:claudeAgent:native-item:{call}")});
        assert_eq!(open_approvals(&[command("toolu_A", "completed"), card("toolu_A", "waiting")]), Some(0));
        assert_eq!(open_approvals(&[command("toolu_A", "running"), card("toolu_A", "waiting")]), Some(1), "not run yet: a real wait");
        assert_eq!(open_approvals(&[card("toolu_B", "waiting")]), Some(1));
        assert_eq!(open_approvals(&[command("toolu_A", "completed"), card("toolu_A", "waiting"), card("toolu_B", "waiting")]), Some(1));
        assert_eq!(open_approvals(&[card("toolu_A", "completed")]), None, "no waiting card in view: cannot tell");
        assert_eq!(open_approvals(&[]), None);
    }
}
