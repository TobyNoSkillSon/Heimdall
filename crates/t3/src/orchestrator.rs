//! T3 Code's orchestrator V2 surface beyond the tools the agent has by name (delegate_task, task_status, task_cancel,
//! t3_thread_list/read/send). `t3_tools` lists the live MCP catalog (about 80 tools: threads, projects, worktrees,
//! queues, schedules, pull requests, previews, models); `t3_call` calls any of them after checking the arguments
//! against the tool's live input schema, and answers in the compact form of `threads::compact` where T3's raw result
//! is a wall of metadata. The catalog is the authority, so a Nightly update that adds or changes a tool reaches the
//! agent without a Heimdall release.
//!
//! Safety: Heimdall may act on any thread. Destructive operations (deleting a project or a scheduled task,
//! changing environment settings, and interrupting, restarting, archiving or editing the queue of a thread
//! Heimdall did not start) need the user's answer to an exact `ask` question first, used once.
//! Archiving alone may cover a named set of his threads on one card (`archive_batch`).
use super::client::bounded_text;
use super::workers::Workers;
use agent::delivery::DeliveryPermit;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::time::Duration;

/// T3 marks the tools that act as a calling T3 thread; Heimdall is an outside client and has none.
const INSIDE_ONLY: &str = "Needs an agent running inside a T3 thread";
/// A read's result budget. Larger reads ask for a smaller page instead of being cut.
pub const RESULT_BYTES: usize = 48 * 1024;
const LIST_DESCRIPTION_CHARS: usize = 110;

fn inside_only(tool: &Value) -> bool {
    tool["description"].as_str().is_some_and(|d| d.starts_with(INSIDE_ONLY))
}
fn read_only(tool: &Value) -> bool {
    tool["annotations"]["readOnlyHint"] == true
}
/// T3's action tools (0.1.6; bug note 2026-11-02T14-56-51-3): `t3_queue`, `t3_pending_request` and
/// `t3_thread_transfer` took the place of one tool per operation. A call to one is judged as the operation its
/// `action` names, under the old tool's name (T3's own `ACTION_TOOLS` map, for its labels, names them the same way).
pub fn operation(tool: &str, args: &Value) -> String {
    let action = args["action"].as_str().unwrap_or("");
    match (tool, action) {
        ("t3_queue" | "t3_pending_request", a) if !a.is_empty() => format!("{tool}_{a}"),
        ("t3_thread_transfer", "fork") => "t3_thread_fork".into(),
        ("t3_thread_transfer", "merge_back") => "t3_thread_merge_back".into(),
        ("t3_thread_transfer", "status") => "t3_thread_transfers".into(),
        _ => tool.into(),
    }
}
const ACTION_TOOLS: &[&str] = &["t3_queue", "t3_pending_request", "t3_thread_transfer"];
/// The read-only operations of the action tools: direct, never confirmed, whatever the tool's annotations say. Their
/// other actions are writes: never direct (a replay record), whatever the tool's annotations say.
const READS: &[&str] = &["t3_queue_list", "t3_queue_read", "t3_pending_request_list", "t3_pending_request_read", "t3_thread_transfers"];
/// Reads and idempotent non-destructive calls (t3_thread_read is marked only idempotent) need no replay record.
/// Tools whose success Heimdall acts on (`Workers::effects`, the watch of a sent message) never go direct, whatever
/// their annotations say: the installed T3 (Nightly, 19 Oct build) marks send, launch and fork destructive and not
/// idempotent, but a later build marking send idempotent ("clientRequestId makes retries idempotent") must not
/// silently drop the watch and the adoption.
fn direct(tool: &Value, args: &Value) -> bool {
    let a = &tool["annotations"];
    let op = operation(tool["name"].as_str().unwrap_or(""), args);
    if READS.contains(&op.as_str()) {
        return true;
    }
    if ACTION_TOOLS.contains(&tool["name"].as_str().unwrap_or("")) {
        return false;
    }
    !EFFECTS.contains(&op.as_str()) && (a["readOnlyHint"] == true || (a["idempotentHint"] == true && a["destructiveHint"] != true))
}
/// The tools `Workers::effects` and the send watch act on after a successful call.
const EFFECTS: &[&str] = &["t3_thread_send", "t3_thread_launch", "t3_thread_fork", "t3_thread_interrupt", "t3_workflow_stop", "t3_queue_cancel", "t3_thread_organize"];
fn tools(catalog: &Value) -> impl Iterator<Item = &Value> {
    catalog["tools"].as_array().into_iter().flatten()
}
fn first_sentence(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat.find(". ").map(|i| i + 1).unwrap_or(flat.len());
    bounded_text(&flat[..end], LIST_DESCRIPTION_CHARS)
}

/// Destructive T3 tools without a dedicated question: they act on someone's thread or work in ways that are hard to
/// undo (answering for the user, merging, handing off a worktree, reconfiguring).
const DESTRUCTIVE: &[(&str, &str)] = &[
    ("t3_pending_request_respond", "Answer a pending question in T3 thread {}?"),
    ("t3_thread_merge_back", "Merge T3 thread {} back?"),
    ("t3_worktree_handoff", "Hand off the worktree of T3 thread {}?"),
    ("t3_thread_configure", "Change the model settings of T3 thread {}?"),
];
/// Tools T3 marks destructive that only add to a thread (a message, a pin, a fork, a new thread): the user asked for full
/// use of his threads (18 Oct), so these stay free; their harmful forms (restart, archive) have named rules above.
const ADDITIVE: &[&str] = &["t3_thread_send", "t3_thread_organize", "t3_thread_fork", "t3_thread_launch"];
fn destructive(tool: &Value, op: &str) -> bool {
    tool["annotations"]["destructiveHint"] == true && !ADDITIVE.contains(&op) && !READS.contains(&op)
}

/// Which operations need the user's confirmation, and whether only on threads Heimdall did not start. Named rules first;
/// then the listed destructive tools and any other tool T3 marks destructive, on a thread Heimdall did not start (a
/// listed tool without a thread argument always asks).
fn guarded(entry: &Value, args: &Value) -> Option<(String, Target, bool)> {
    let op = operation(entry["name"].as_str().unwrap_or(""), args);
    let tool = op.as_str();
    if let Some((template, target, others_only)) = named_guard(tool, args) {
        return Some((template.to_owned(), target, others_only));
    }
    let listed = DESTRUCTIVE.iter().find(|(name, _)| *name == tool).map(|(_, q)| *q);
    if listed.is_none() && !destructive(entry, tool) {
        return None;
    }
    Some(match args["threadId"].as_str() {
        Some(id) => (listed.map(str::to_owned).unwrap_or_else(|| format!("Run {tool} on T3 thread {{}}?")), Target::Thread(Some(id.to_owned())), true),
        // A listed tool without a thread to check ownership of always asks; other destructive tools act on no thread.
        None if listed.is_some() => (format!("Run {tool} in T3?"), Target::None, false),
        None => return None,
    })
}
fn named_guard(tool: &str, args: &Value) -> Option<(&'static str, Target, bool)> {
    let thread = || Target::Thread(args["threadId"].as_str().map(str::to_owned));
    Some(match tool {
        "t3_project_delete" => ("Delete T3 project {}?", Target::Project(args["projectId"].as_str().unwrap_or("").into()), false),
        "delete_scheduled_task" => ("Delete T3 scheduled task {}?", Target::Plain(args["scheduledTaskId"].as_str().unwrap_or("").into()), false),
        "t3_environment_preferences_update" => ("Change T3 environment settings?", Target::None, false),
        "t3_thread_interrupt" => ("Interrupt T3 thread {}?", thread(), true),
        "t3_workflow_stop" => ("Stop a workflow in T3 thread {}?", thread(), true),
        "t3_queue_cancel" => ("Cancel a queued message in T3 thread {}?", thread(), true),
        "t3_queue_edit" => ("Edit a queued message in T3 thread {}?", thread(), true),
        "t3_queue_reorder" => ("Reorder the queue of T3 thread {}?", thread(), true),
        "t3_queue_promote_to_steer" => ("Send a queued message in T3 thread {} now?", thread(), true),
        "t3_thread_send" if args["mode"] == "restart" => ("Restart the run in T3 thread {}?", thread(), true),
        "t3_thread_organize" if args["action"] == "archive" => ("Archive T3 thread {}?", thread(), true),
        _ => return None,
    })
}
enum Target {
    Thread(Option<String>),
    Project(String),
    Plain(String),
    None,
}
/// The listing marker for a tool's confirmation rule.
fn marker(entry: &Value) -> &'static str {
    let name = entry["name"].as_str().unwrap_or("");
    match name {
        "t3_project_delete" | "delete_scheduled_task" | "t3_environment_preferences_update" => " [confirm]",
        "t3_thread_interrupt" | "t3_workflow_stop" => " [confirm unless yours]",
        // The action tools: only their writing actions confirm.
        "t3_queue" => " [edit, cancel, reorder, promote_to_steer: confirm unless yours]",
        "t3_pending_request" => " [respond: confirm unless yours]",
        "t3_thread_transfer" => " [merge_back: confirm unless yours]",
        "t3_thread_send" => " [restart: confirm unless yours]",
        "t3_thread_organize" => " [archive: confirm unless yours]",
        _ if DESTRUCTIVE.iter().any(|(n, _)| *n == name) || destructive(entry, name) => " [confirm unless yours]",
        _ => "",
    }
}

/// `t3_tools` without `tool`: one line per callable tool, optionally filtered by words.
pub fn listing(catalog: &Value, query: Option<&str>) -> String {
    let words: Vec<String> = query.map(|q| q.to_lowercase().split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
    let mut lines = vec![];
    let mut unavailable = vec![];
    for t in tools(catalog) {
        let name = t["name"].as_str().unwrap_or("");
        let description = t["description"].as_str().unwrap_or("");
        if inside_only(t) {
            unavailable.push(name);
            continue;
        }
        let haystack = format!("{name} {description}").to_lowercase();
        if !words.is_empty() && !words.iter().any(|w| haystack.contains(w.as_str())) {
            continue;
        }
        let read = if read_only(t) { " [read]" } else { "" };
        lines.push(format!("{name}{read}{} - {}", marker(t), first_sentence(description)));
    }
    let mut out = format!(
        "{} T3 tools{}. Call one with t3_call {{tool, args}}; t3_tools {{tool}} gives its full description and input schema.\n{}",
        lines.len(),
        if words.is_empty() { String::new() } else { format!(" matching {:?}", query.unwrap_or("")) },
        lines.join("\n")
    );
    if words.is_empty() && !unavailable.is_empty() {
        out.push_str(&format!(
            "\nNot callable here (they act as a calling T3 thread): {}. Heimdall's own delegate_task, task_status and task_cancel do this for you.",
            unavailable.join(", ")
        ));
    }
    out
}

/// `t3_tools {tool}`: the tool's full live description and its arguments, one line each (tool-output: plain text).
pub fn detail(catalog: &Value, name: &str) -> Result<String> {
    let t = tools(catalog)
        .find(|t| t["name"] == name)
        .with_context(|| format!("T3_TOOL_UNKNOWN: {name} is not in the live catalog; list tools with t3_tools"))?;
    let read = if read_only(t) { " [read]" } else { "" };
    let mut out = format!("{name}{read}{}\n{}", marker(t), t["description"].as_str().unwrap_or("").trim());
    let args = crate::text::schema(&t["inputSchema"]);
    out.push_str(&if args.is_empty() { "\nArgs: none.".to_owned() } else { format!("\nArgs (* required):\n{args}") });
    if inside_only(t) {
        out.push_str("\nUnavailable through t3_call: Heimdall is an outside client with no calling thread. Use Heimdall's own delegate_task, task_status and task_cancel.");
    } else if !marker(t).is_empty() {
        out.push_str(&format!("\nConfirmation rule:{}. Confirmation is {}'s click on Yes.", marker(t), agent::identity::user()));
    }
    if t["inputSchema"]["properties"].get("clientRequestId").is_some() {
        out.push_str("\nclientRequestId is filled in for you.");
    }
    Ok(out)
}

/// JSON Schema check of the subset T3 uses (type, enum, const, anyOf/oneOf/allOf, required, properties,
/// additionalProperties, items, numeric and length bounds). Unknown keywords are ignored; T3 validates again.
pub fn check(schema: &Value, v: &Value, path: &str) -> Result<()> {
    let Some(s) = schema.as_object() else { return Ok(()) };
    for key in ["anyOf", "oneOf"] {
        if let Some(alternatives) = s.get(key).and_then(Value::as_array) {
            let errors: Vec<_> = alternatives.iter().map(|a| check(a, v, path)).collect();
            if !errors.iter().any(Result::is_ok) {
                // The alternative of the value's own JSON type explains the problem best.
                if let Some(Err(e)) = alternatives.iter().zip(errors).find(|(a, _)| types(a).iter().any(|t| matches_type(t, v))).map(|(_, e)| e) {
                    return Err(e);
                }
                let expected: Vec<String> = alternatives.iter().flat_map(types).collect();
                bail!("{path}: expected {}", if expected.is_empty() { "another shape".into() } else { expected.join(" or ") });
            }
        }
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        for a in all {
            check(a, v, path)?;
        }
    }
    if let Some(values) = s.get("enum").and_then(Value::as_array) {
        ensure!(values.contains(v), "{path}: use one of {}", Value::Array(values.clone()));
    }
    if let Some(c) = s.get("const") {
        ensure!(c == v, "{path}: must be {c}");
    }
    let declared = types(schema);
    if !declared.is_empty() {
        ensure!(declared.iter().any(|t| matches_type(t, v)), "{path}: expected {}", declared.join(" or "));
    }
    match v {
        Value::Object(map) => {
            if let Some(required) = s.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    // Lane S: a missing field with fixed values names them (4 Nov: t3_pending_request without action).
                    let values = s.get("properties").and_then(|p| p.get(key)).and_then(|p| p.get("enum")).cloned();
                    ensure!(map.contains_key(key), "{path}.{key}: required{}", values.map(|v| format!("; one of {v}")).unwrap_or_default());
                }
            }
            let properties = s.get("properties").and_then(Value::as_object);
            for (key, value) in map {
                match properties.and_then(|p| p.get(key)) {
                    Some(p) => check(p, value, &format!("{path}.{key}"))?,
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => bail!("{path}.{key}: unknown field; see t3_tools for the schema"),
                        Some(extra @ Value::Object(_)) => check(extra, value, &format!("{path}.{key}"))?,
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(n) = s.get("minItems").and_then(Value::as_u64) {
                ensure!(items.len() as u64 >= n, "{path}: at least {n} items");
            }
            if let Some(n) = s.get("maxItems").and_then(Value::as_u64) {
                ensure!(items.len() as u64 <= n, "{path}: at most {n} items");
            }
            if let Some(item) = s.get("items") {
                for (i, x) in items.iter().enumerate() {
                    check(item, x, &format!("{path}[{i}]"))?;
                }
            }
        }
        Value::String(text) => {
            let n = text.chars().count() as u64;
            if let Some(min) = s.get("minLength").and_then(Value::as_u64) {
                ensure!(n >= min, "{path}: at least {min} characters");
            }
            if let Some(max) = s.get("maxLength").and_then(Value::as_u64) {
                ensure!(n <= max, "{path}: at most {max} characters");
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
                ensure!(x >= min, "{path}: at least {min}");
            }
            if let Some(max) = s.get("maximum").and_then(Value::as_f64) {
                ensure!(x <= max, "{path}: at most {max}");
            }
        }
        _ => {}
    }
    Ok(())
}
fn types(schema: &Value) -> Vec<String> {
    match &schema["type"] {
        Value::String(t) => vec![t.clone()],
        Value::Array(list) => list.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect(),
        _ => vec![],
    }
}
fn matches_type(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some() || v.as_f64().is_some_and(|x| x.fract() == 0.0),
        _ => true,
    }
}

/// One of Heimdall's relay threads by its T3 shell: launched over MCP (Heimdall is T3's MCP client), top level, with a
/// relay title (`observer::RELAY_TITLES`, the same rule as the observer's thread record). A thread the user made and named
/// so in T3's window is created in the web client, not over MCP, and stays his.
fn relay_shell(t: &Value) -> bool {
    t["creationSource"] == "mcp" && t["parentThreadId"].is_null() && crate::observer::relay_title(t["title"].as_str().unwrap_or(""))
}

/// The clicked label approves only when it is exactly Yes (the question's options are Yes and No).
pub fn approves(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "yes" | "tak")
}

/// An MCP result envelope as plain text (tool-output): structured content as indented lines, text blocks verbatim
/// unless they are JSON, other blocks named. Nothing is shortened.
pub fn render(envelope: &Value) -> String {
    if let Some(v) = envelope.get("structuredContent").filter(|v| !v.is_null()) {
        return agent::plain::render(v);
    }
    let parts: Vec<String> = envelope["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| match c["type"].as_str() {
            Some("text") => agent::plain::text(c["text"].as_str().unwrap_or("")),
            Some("image") => format!("[image {} omitted; {} can see it in T3]", c["mimeType"].as_str().unwrap_or(""), agent::identity::user()),
            Some(other) => format!("[{other} content omitted]"),
            None => String::new(),
        })
        .collect();
    if parts.is_empty() { "Done; T3 returned nothing.".into() } else { parts.join("\n") }
}

impl Workers {
    /// Whether Heimdall started this thread: a task (`delegate_task`, or launched or forked through `t3_call`). A
    /// thread it only sent a message into stays the user's for every confirmation rule.
    pub(super) fn owns(&self, thread: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM workers WHERE thread=?1 AND kind='task') OR EXISTS(SELECT 1 FROM owned_threads WHERE thread=?1)",
            [thread],
            |r| r.get(0),
        )?)
    }
    /// Whether a thread is Heimdall's own in every relay session, for the confirmation rules (bug notes
    /// 2026-11-03T22-29-14, 22-29-20: the user clicked once per leftover relay thread to archive it, "you don't need this
    /// click"): a task or launch (`owns`), a thread the T3 client recorded as Heimdall's (`Client::recorded_thread`: the
    /// relay threads and tasks it launched, and the relay and task threads each pass saw active), or a T3
    /// `delegate_task` child of one of those, at any depth. The identity is Heimdall's, not the
    /// current relay thread's. A relay thread from before the record is recognised by its shell (`relay_shell`).
    /// Threads Heimdall only sent a message into, and the user's own, still ask.
    pub(super) fn heimdall_s(&self, thread: &str) -> Result<bool> {
        let mut id = thread.to_owned();
        for _ in 0..8 {
            // Not `relay_parents`: the observer fills it by title alone, so a thread the user named "Heimdall relay …" in
            // T3's window would pass (fresh review, 4 Nov).
            if self.owns(&id)? || self.client.recorded_thread(&id) {
                return Ok(true);
            }
            match heimdall_emergency::approvals::delegate_parent(&id) {
                Some(parent) => id = parent,
                None => return Ok(false),
            }
        }
        Ok(false)
    }
    pub(super) async fn t3_tools(&self, a: &Value) -> Result<String> {
        let catalog = self.client.catalog().await?;
        match a["tool"].as_str() {
            Some(name) => detail(&catalog, name),
            None => Ok(listing(&catalog, a["query"].as_str())),
        }
    }
    /// The exact confirmation question for a guarded call, or None.
    async fn confirmation(&self, entry: &Value, args: &Value) -> Result<Option<String>> {
        let tool = entry["name"].as_str().unwrap_or("");
        let Some((template, target, others_only)) = guarded(entry, args) else { return Ok(None) };
        let name = match target {
            Target::Thread(None) => bail!("args.threadId: required; Heimdall has no current T3 thread"),
            Target::Thread(Some(id)) => {
                // A merge changes its target: free only when Heimdall started both threads (review 2 Nov: T3 0.1.6's
                // `threadId` is the source, `targetThreadId` the thread merged into).
                let target_owned = match (operation(tool, args).as_str(), args["targetThreadId"].as_str()) {
                    ("t3_thread_merge_back", Some(target)) => self.heimdall_s(target)?,
                    // No target: T3 picks the related thread, which may be the user's, so it asks (fresh review, 2 Nov).
                    ("t3_thread_merge_back", None) => false,
                    _ => true,
                };
                if others_only && target_owned && self.heimdall_s(&id)? {
                    return Ok(None);
                }
                let shell = self.client.call("t3_thread_read", json!({"threadId":id,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await.ok();
                // A relay thread of an earlier relay session, from before the thread record (bug note 2026-11-03T22-29-20).
                if others_only && target_owned && shell.as_ref().is_some_and(|r| relay_shell(&r["thread"])) {
                    return Ok(None);
                }
                shell.and_then(|r| r["thread"]["title"].as_str().map(str::to_owned)).unwrap_or(id)
            }
            Target::Project(id) => self
                .client
                .call("t3_project_read", json!({"projectId":id}))
                .await
                .ok()
                .and_then(|r| r["title"].as_str().map(str::to_owned))
                .unwrap_or(id),
            Target::Plain(id) => id,
            Target::None => String::new(),
        };
        let name = bounded_text(&name.replace(['\'', '"', '\n'], " "), 60);
        let mut question = template.replace("{}", &format!("'{name}'"));
        if tool == "t3_project_delete" && args["force"] == true {
            question = question.replace('?', ", with its threads?");
        }
        Ok(Some(question))
    }
    fn verify_confirmation(&self, id: &str, question: &str) -> Result<()> {
        let consumed: bool = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM consumed_confirms WHERE id=?)", [id], |r| r.get(0))?;
        ensure!(!consumed, "CONFIRM_USED: that click already approved an earlier call, and each approves one call; ask again with this call's question");
        // Only the user's click counts: a spoken answer reaches this as the agent's own words (ask_close), which must
        // never approve a destructive call.
        let (asked, answer) = match self.actions.ask_click(id)? {
            Some(clicked) => clicked,
            None if self.actions.ask_answer(id)?.is_some() => bail!("CONFIRM_NEEDS_CLICK: a spoken answer cannot approve this; ask again and wait for {}'s click on Yes", agent::identity::user()),
            None => bail!("CONFIRM_PENDING: {} has not answered that question yet; wait for their click", agent::identity::user_start()),
        };
        ensure!(asked.trim() == question, "CONFIRM_MISMATCH: ask exactly {question:?} for this call");
        ensure!(approves(&answer), "CONFIRM_DECLINED: {} answered {answer:?}; do not do it", agent::identity::user_start());
        Ok(())
    }
    pub(super) async fn t3_call(&self, a: Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        permit.check()?;
        let name = a["tool"].as_str().context("tool missing")?.to_owned();
        let mut args = a.get("args").cloned().unwrap_or_else(|| json!({}));
        let catalog = self.client.catalog().await?;
        let tool = tools(&catalog)
            .find(|t| t["name"] == name.as_str())
            .with_context(|| format!("T3_TOOL_UNKNOWN: {name} is not in the live catalog; list tools with t3_tools"))?
            .clone();
        ensure!(
            !inside_only(&tool),
            "OUTSIDE_CALLER: {name} acts as a calling T3 thread and Heimdall has none. Use Heimdall's own delegate_task, task_status or task_cancel tool instead"
        );
        // Bug note 2026-11-13T18-04-30: several threads archived on one card.
        if a.get("threads").is_some() {
            return self.archive_batch(&a, &tool, op, permit).await;
        }
        let properties = &tool["inputSchema"]["properties"];
        // The durable action identity makes a replayed send or interrupt idempotent in T3 too.
        if properties.get("clientRequestId").is_some() && args.get("clientRequestId").is_none() {
            args["clientRequestId"] = json!(op);
        }
        let mut deadline = Duration::from_secs(30);
        if name == "t3_thread_wait" {
            // Heimdall wakes on completion; a wait never holds a turn for long.
            let ms = args["timeoutMs"].as_f64().unwrap_or(55_000.0).clamp(0.0, 55_000.0);
            args["timeoutMs"] = json!(ms as u64);
            deadline = Duration::from_secs(65);
        }
        check(&tool["inputSchema"], &args, "args").map_err(|e| anyhow::anyhow!("INVALID_ARGUMENT: {e}"))?;
        if direct(&tool, &args) && guarded(&tool, &args).is_none() {
            let envelope = permit.run(self.client.call_raw(&name, args.clone(), deadline)).await?;
            let mut text = crate::threads::compact(&name, &args, &envelope["structuredContent"]).unwrap_or_else(|| render(&envelope));
            if operation(&name, &args) == "t3_pending_request_list" {
                if let Some(thread) = args["threadId"].as_str() {
                    text.push_str(&self.unlisted_approval(thread, &envelope["structuredContent"]).await);
                }
            }
            ensure!(
                text.len() <= RESULT_BYTES,
                "OUTPUT_TOO_LARGE: {name} returned {} KB; ask for less (limit, maxCharsPerItem) or page with its cursor",
                text.len() / 1024
            );
            return Ok(text);
        }
        let body = json!({"tool":name,"args":args});
        if let Some(done) = self.prepare(op, "t3_call", &body, permit)? {
            return Ok(done.as_str().map(agent::plain::text).unwrap_or_else(|| agent::plain::render(&done)));
        }
        let confirm = match self.confirmation(&tool, &args).await? {
            None => None,
            Some(question) => match a["confirm"].as_str() {
                None => {
                    // Only a click approves it, so the user's voice does not close this card (night-ask, 20 Oct).
                    self.actions.require_click(&question);
                    let user = agent::identity::user();
                    bail!(
                        "CONFIRM_REQUIRED: call ask with question {question:?} and options [\"Yes\",\"No\"]. When {user} clicks Yes, repeat this call with confirm set to that ask's request_id. A spoken yes does not count"
                    )
                }
                Some(id) => {
                    self.verify_confirmation(id, &question)?;
                    Some(id.to_owned())
                }
            },
        };
        let state: String = self.db.lock().unwrap().query_row("SELECT state FROM operations WHERE id=?", [op], |r| r.get(0))?;
        ensure!(
            state != "calling" || args.get("clientRequestId").is_some(),
            "T3_UNCERTAIN: an earlier attempt of this call may have been applied. Read the current state in T3 before deciding to repeat it"
        );
        permit.with(|| {
            self.db.lock().unwrap().execute("UPDATE operations SET state='calling' WHERE id=?", [op])?;
            Ok(())
        })?;
        // A message into a thread Heimdall did not start: follow that run from here (day review T1). The activity
        // position before the send makes only what follows count as its sub-agents and background work.
        // The thread itself too: a T3 `delegate_task` child of Heimdall's relay is a task run for Heimdall.
        let watch_from = match (name.as_str(), args["threadId"].as_str()) {
            ("t3_thread_send", Some(thread)) if !self.owns(thread)? => match self.latest(thread, "activity", 1, 1).await {
                Ok(l) => Some((l.items.last().and_then(|i| i["position"].as_u64()), l.thread["title"].as_str().map(str::to_owned), l.thread)),
                Err(_) => Some((None, None, Value::Null)),
            },
            _ => None,
        };
        permit.check()?;
        let sent = match self.client.call_raw(&name, args.clone(), deadline).await {
            Err(e) if name == "t3_thread_send" => self.land(&args, e, deadline, permit).await,
            Err(e) if name == "t3_thread_organize" && already_so(&args, &e.to_string()) => {
                // ---- Lane S (night 10–11 Nov), t3_call archive failures: T3 refuses to unarchive a thread that is not
                // archived (3 and 4 Nov, twice). The thread is already as asked: a plain answer, not a failure. ----
                let text = format!("Thread {} is already {}d; nothing to do.", args["threadId"].as_str().unwrap_or("?"), args["action"].as_str().unwrap_or(""));
                // Its record follows as if the call had gone through (an archive ends following a task).
                self.effects(&operation(&name, &args), &args, &Value::Null)?;
                // Review 1111 S5: the user's click approved this call; it is spent here as on any other path.
                if let Some(id) = &confirm {
                    self.db.lock().unwrap().execute("INSERT OR IGNORE INTO consumed_confirms VALUES(?)", [id])?;
                }
                self.finish(op, &Value::String(text.clone()))?;
                return Ok(text);
            }
            other => other,
        };
        let envelope = match sent {
            Ok(v) => v,
            Err(e) => {
                // A definite rejection applied nothing; only a lost response leaves the call uncertain.
                let text = e.to_string();
                if text.starts_with("T3_TOOL_FAILED") || text.starts_with("T3_RPC_ERROR") || text.starts_with("T3_TOOL_UNKNOWN") {
                    self.db.lock().unwrap().execute("UPDATE operations SET state='rejected' WHERE id=?", [op])?;
                }
                return Err(e);
            }
        };
        let result = envelope.get("structuredContent").cloned().unwrap_or(Value::Null);
        let mut note = self.effects(&operation(&name, &args), &args, &result)?;
        // New work for a task whose island the user cleared: its run shows on an island again, so it is not archived.
        if let ("t3_thread_send", Some(thread)) = (name.as_str(), args["threadId"].as_str()) {
            self.db.lock().unwrap().execute("DELETE FROM cleared WHERE thread=?", [thread])?;
        }
        if let (Some((cursor, title, row)), Some(thread), Some(run)) = (watch_from, args["threadId"].as_str(), result["runId"].as_str()) {
            self.watch_run(thread, run, result["status"].as_str().unwrap_or("running"), cursor, title.as_deref())?;
            // Bug note 2026-10-30T21-56-20: the three climbs the user saw run without islands were T3 `delegate_task`
            // children of the relay, each woken by a send here; a send into a thread that is not one of them (the user's
            // own) still gets none.
            if self.relay_task(&row).await.unwrap_or(false) {
                let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Task".into());
                note += &self.run_started(thread, Some(run), &title, args["message"].as_str().unwrap_or(""))?;
            }
        }
        if let Some(id) = confirm {
            self.db.lock().unwrap().execute("INSERT OR IGNORE INTO consumed_confirms VALUES(?)", [id])?;
        }
        let mut text = crate::threads::compact(&name, &args, &result).unwrap_or_else(|| render(&envelope));
        // Lane S (proposal 10): the reply promise once per session; afterwards the send's answer ends at its state.
        if name == "t3_thread_send" && text.contains(crate::threads::REPLY_PROMISE) && !self.once("reply-promise") {
            text = text.replacen(crate::threads::REPLY_PROMISE, ".", 1);
        }
        let text = text + &note;
        self.finish(op, &Value::String(text.clone()))?;
        self.changed.notify_one();
        Ok(text)
    }
    // ---- Lane S (night 10–11 Nov): a send always lands. Bug note 2026-11-10T09-03-14. ----
    // T3 refuses a steer into a thread with no running turn ("has no running turn that can be steered": idle, or its
    // run just finished) and any send into an archived thread. Steers failed 20 times in two weeks, each retried by the
    // relay with `auto`; three of them said "unarchiving it failed too … not archived", because the archived matcher
    // took the steer refusal for an archive. Now a steer with no turn to steer starts one (what `auto` does), and an
    // archived Heimdall thread is unarchived and gets the message as a new turn. The user's archived threads keep T3's
    // refusal.
    async fn land(&self, args: &Value, refused: anyhow::Error, deadline: Duration, permit: &DeliveryPermit) -> Result<Value> {
        let text = refused.to_string();
        if archived_refusal(&text) {
            return self.resend_archived(args, refused, deadline, permit).await;
        }
        if idle_refusal(&text) {
            // Review 1111 S4: only Heimdall's own threads get the steer as a new turn; a thread of the user's keeps T3's
            // refusal, saying how to start a turn there on purpose.
            if !args["threadId"].as_str().map(|t| self.heimdall_s(t)).transpose()?.unwrap_or(false) {
                return Err(anyhow::anyhow!("{text} Nothing was sent: this is a thread of {user}'s with no running turn to steer. Starting a turn in their own thread needs their say-so: ask {user} first, then send with mode auto.", user = agent::identity::user()));
            }
            permit.check()?;
            return self.client.call_raw("t3_thread_send", as_new_turn(args, "started"), deadline).await;
        }
        Err(refused)
    }
    // ---- end lane S ----
    // ---- Night 8 Nov log sweep, finding 1: a message for an archived Heimdall worker still reaches it. ----
    // The relay archives a finished task with task_cancel, and the user's words or island click for it often follow
    // seconds later ("Try int8 once", 8 Nov 02:08:50); T3 refuses a send into an archived thread
    // (`thread_not_sendable`). For Heimdall's own threads the send unarchives the thread and goes out once more, so
    // the click lands in the thread that holds its context; the user's threads keep T3's refusal.
    async fn resend_archived(&self, args: &Value, refused: anyhow::Error, deadline: Duration, permit: &DeliveryPermit) -> Result<Value> {
        let Some(thread) = args["threadId"].as_str() else { return Err(refused) };
        if !self.heimdall_s(thread)? {
            return Err(refused);
        }
        permit.check()?;
        // Lane S: a thread unarchived meanwhile (by the user, or a send racing this one) is fine; the message still goes.
        match self.client.call("t3_thread_organize", json!({"threadId":thread,"action":"unarchive"})).await {
            Err(e) if !e.to_string().contains("is not archived") => return Err(refused.context(format!("unarchiving it failed too: {e:#}"))),
            _ => {}
        }
        eprintln!("t3: {thread} was archived; unarchived it to deliver a message");
        // Its archive no longer stands: the run this message starts is followed and wakes as any task's does.
        self.db.lock().unwrap().execute("DELETE FROM quiet WHERE thread=?", [thread])?;
        // T3 applied nothing the first time; a fresh retry key keeps its own dedupe from answering the refusal. An
        // archived thread has no turn to steer, so a steer goes as a new turn (lane S).
        permit.check()?;
        self.client.call_raw("t3_thread_send", as_new_turn(args, "unarchived"), deadline).await
    }
    // ---- end finding 1 ----
    /// Bug note 2026-11-06T11-57-54: T3's `t3_pending_request` list names questions only ("approvals are not listed"),
    /// so the relay never saw a thread wait on an approval. The thread's live approval wait (`crate::waits_on_approval`,
    /// the hook's answered cards left out) as one more line; nothing when there is none or the read fails.
    async fn unlisted_approval(&self, thread: &str, listed: &Value) -> String {
        let questions = listed["requestIds"].as_array().map_or(0, Vec::len) as u64;
        let Ok(r) = self.client.call("t3_thread_read", json!({"threadId":thread,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await else { return String::new() };
        let t = &r["thread"];
        let counted = crate::threads::live_requests(t).saturating_sub(questions);
        let counted = if counted == 0 && questions == 0 && t["status"] == "waiting" { 1 } else { counted };
        if counted == 0 {
            return String::new();
        }
        match crate::waits_on_approval(&self.client, thread, t, counted).await {
            Ok(Some(a)) => {
                let asks = a.text.map(|t| format!(": {t}")).unwrap_or_default();
                let open = t["link"].as_str().map(|l| l.trim_matches(['<', '>'])).filter(|l| !l.is_empty()).map(|l| format!(" Open it: {l}")).unwrap_or_default();
                format!("\nApproval waiting (T3 lists no approvals; only {} can give it, in T3){asks}.{open}", agent::identity::user())
            }
            _ => String::new(),
        }
    }
    /// Ownership and quiet periods that follow from a successful call.
    /// Returns a note for the call's answer: the job island a launched or forked thread got.
    fn effects(&self, tool: &str, args: &Value, result: &Value) -> Result<String> {
        let mut note = String::new();
        // A thread launched or forked here is a task like delegate_task's: same wake, same done rule.
        match tool {
            "t3_thread_launch" => {
                if let Some(thread) = result["threadId"].as_str() {
                    let selection = if result["modelSelection"].is_object() { &result["modelSelection"] } else { &args["modelSelection"] };
                    note = self.adopt(thread, result["runId"].as_str(), result["status"].as_str().unwrap_or("preparing"), selection, result["projectId"].as_str(), args["title"].as_str(), args["message"].as_str().unwrap_or(""))?;
                }
            }
            "t3_thread_fork" => {
                if let Some(thread) = result["targetThreadId"].as_str() {
                    note = self.adopt(thread, None, "idle", &Value::Null, None, args["title"].as_str().or(Some("Fork")), "")?;
                }
            }
            // A follow-up to Heimdall's own task: follow its new run.
            "t3_thread_send" => {
                // Heimdall's message waits in T3's queue: a later send joins it (`join_queued`).
                if let (Some(thread), Some(run), Some("queued")) = (args["threadId"].as_str(), result["runId"].as_str(), result["delivery"].as_str()) {
                    self.db.lock().unwrap().execute("INSERT OR REPLACE INTO queued_sends(thread,run,text) VALUES(?,?,?)", params![thread, run, args["message"].as_str().unwrap_or("")])?;
                }
                if let (Some(thread), Some(run)) = (args["threadId"].as_str(), result["runId"].as_str()) {
                    // Heimdall's own message: if T3 drops this run before it starts, the thread reads cancelled.
                    self.sent_run(thread, run)?;
                    let task = self.db.lock().unwrap().execute(
                        "UPDATE workers SET run=?,status=?,archived=0 WHERE thread=? AND kind='task'",
                        params![run, result["status"].as_str().unwrap_or("running"), thread],
                    )? > 0;
                    // The run a send starts gets its job island again (bugs 2026-10-28T19-22-44, 18-55-56): a done
                    // island leaves once the user saw it and its binding goes, so a woken task ran without one. The hook
                    // keeps an island still bound to the thread.
                    if task {
                        let title: Option<String> = self.db.lock().unwrap().query_row("SELECT title FROM workers WHERE thread=?", [thread], |r| r.get(0))?;
                        let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Task".into());
                        note = self.run_started(thread, Some(run), &title, args["message"].as_str().unwrap_or(""))?;
                    }
                }
            }
            _ => {}
        }
        let db = self.db.lock().unwrap();
        if let Some(thread) = match tool {
            "t3_thread_launch" => result["threadId"].as_str(),
            "t3_thread_fork" => result["targetThreadId"].as_str(),
            _ => None,
        } {
            db.execute("INSERT OR IGNORE INTO owned_threads VALUES(?)", [thread])?;
        }
        // Heimdall's own stop or archive is not news to Heimdall: no wake for its result.
        let own_stop = matches!(tool, "t3_thread_interrupt" | "t3_workflow_stop" | "t3_queue_cancel")
            || (tool == "t3_thread_send" && args["mode"] == "restart")
            || (tool == "t3_thread_organize" && args["action"] == "archive");
        if let (true, Some(thread)) = (own_stop, args["threadId"].as_str()) {
            db.execute(
                "INSERT OR REPLACE INTO quiet VALUES(?,?)",
                params![thread, (crate::now_ms() + 10 * 60 * 1000) as i64],
            )?;
        }
        // Archiving one of Heimdall's tasks ends following it.
        if let (true, Some(thread)) = (tool == "t3_thread_organize" && args["action"] == "archive", args["threadId"].as_str()) {
            db.execute("UPDATE workers SET archived=1 WHERE thread=?", [thread])?;
        }
        Ok(note)
    }
    /// True while a thread's terminal or archive news would only echo Heimdall's own action.
    pub(super) fn quiet(&self, thread: &str) -> Result<bool> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT until_ms FROM quiet WHERE thread=?", [thread], |r| r.get::<_, i64>(0))
            .optional()?
            .is_some_and(|until| until as u64 > crate::now_ms()))
    }
}

// ---- Batch archive on one card (bug note 2026-11-13T18-04-30) ----
// The user asked to clean up his T3 threads and clicked "Archive them too" on one card covering 46 of his finished
// threads; the guard then wanted one exact card per thread. Archiving keeps a thread's history and is undone by
// unarchive, so archive alone (never a delete, interrupt or any other guarded action) may cover a named set on one
// card: `t3_call {tool: t3_thread_organize, args: {action: archive}, threads: [ids]}`. Heimdall's own threads in the
// set archive at once; his are covered by one question naming their count, when they were last active and a tag of
// their exact ids. His click on Yes approves archiving exactly that set, once, within 30 minutes of the question.
/// The most threads one batch archives.
pub const BATCH_MAX: usize = 100;
/// How long a batch question stays good for its click.
const BATCH_LIFETIME_MS: u64 = 30 * 60 * 1000;
/// One of the user's threads in a batch: id, title, last active (`updatedAt`).
pub struct BatchThread {
    pub id: String,
    pub title: String,
    pub updated: Option<String>,
}
/// The tag of an exact set of thread ids shown on the card: six hex digits of the SHA-256 of the sorted ids, so any
/// other set (one thread more or less) almost always asks a different question. The binding itself is the full hash
/// (`batch_key`), the card's titles and the order of question and card.
pub fn batch_tag(ids: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let digest = Sha256::digest(sorted.join("\n").as_bytes());
    digest.iter().take(3).map(|b| format!("{b:02X}")).collect()
}
/// The key a batch question is kept under: the full SHA-256 of the sorted ids.
fn batch_key(ids: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    Sha256::digest(sorted.join("\n").as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}
/// "12 Oct" from an ISO time.
fn day_month(iso: &str) -> Option<(String, String)> {
    let date = iso.get(..10)?;
    let month: usize = date.get(5..7)?.parse().ok()?;
    let day: u32 = date.get(8..10)?.parse().ok()?;
    let name = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].get(month.checked_sub(1)?)?;
    Some((date.to_owned(), format!("{day} {name}")))
}
/// The batch card's exact question and the Yes option's description (the first titles): count, when they were last
/// active, and the set's tag.
pub fn batch_question(threads: &[BatchThread]) -> (String, String) {
    let ids: Vec<String> = threads.iter().map(|t| t.id.clone()).collect();
    let mut days: Vec<(String, String)> = threads.iter().filter_map(|t| t.updated.as_deref().and_then(day_month)).collect();
    days.sort();
    let active = match (days.first(), days.last()) {
        (Some(a), Some(b)) if a.0 == b.0 => format!(", last active {}", a.1),
        (Some(a), Some(b)) => format!(", last active {} to {}", a.1, b.1),
        _ => String::new(),
    };
    let what = if threads.len() == 1 { "this T3 thread of yours".to_owned() } else { format!("these {} T3 threads of yours", threads.len()) };
    let question = format!("Archive {what}{active} (set {})?", batch_tag(&ids));
    // The first titles that fit the option's description, then how many more.
    let titles: Vec<String> = threads.iter().map(|t| bounded_text(&t.title.replace(['\'', '"', '\n'], " "), 30)).collect();
    let more = |shown: usize| if shown < titles.len() { format!(" and {} more", titles.len() - shown) } else { String::new() };
    let mut shown = 1;
    while shown < titles.len() && titles[..=shown].join(", ").chars().count() + more(shown + 1).chars().count() <= agent::mcp_contract::DESCRIPTION_CHARS {
        shown += 1;
    }
    let description = format!("{}{}", titles[..shown].join(", "), more(shown));
    (question, bounded_text(&description, agent::mcp_contract::DESCRIPTION_CHARS))
}

impl Workers {
    /// `t3_call` with `threads`: archive every listed thread, Heimdall's own at once and the user's on one click.
    async fn archive_batch(&self, a: &Value, tool: &Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        let name = tool["name"].as_str().unwrap_or("");
        let args = a.get("args").cloned().unwrap_or_else(|| json!({}));
        ensure!(
            name == "t3_thread_organize" && args["action"] == "archive",
            "INVALID_ARGUMENT: threads covers archiving only (t3_thread_organize with action archive); every other call, deleting included, names one threadId and gets its own question"
        );
        ensure!(args.get("threadId").is_none(), "INVALID_ARGUMENT: pass the threads to archive in threads, not args.threadId");
        let ids: Vec<String> = a["threads"]
            .as_array()
            .context("INVALID_ARGUMENT: threads: a list of thread ids")?
            .iter()
            .map(|v| v.as_str().map(str::to_owned).context("INVALID_ARGUMENT: threads: each is a thread id"))
            .collect::<Result<_>>()?;
        ensure!((1..=BATCH_MAX).contains(&ids.len()), "INVALID_ARGUMENT: threads: 1 to {BATCH_MAX} thread ids; split a larger clean-up");
        ensure!(ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id)), "INVALID_ARGUMENT: threads: name each thread once");
        let properties = &tool["inputSchema"]["properties"];
        let call_args = |id: &str| {
            let mut c = args.clone();
            c["threadId"] = json!(id);
            if properties.get("clientRequestId").is_some() {
                c["clientRequestId"] = json!(format!("{op}:{id}"));
            }
            c
        };
        for id in &ids {
            check(&tool["inputSchema"], &call_args(id), "args").map_err(|e| anyhow::anyhow!("INVALID_ARGUMENT: {e}"))?;
        }
        let body = json!({"tool":name,"args":args,"threads":ids});
        if let Some(done) = self.prepare(op, "t3_call", &body, permit)? {
            return Ok(done.as_str().map(agent::plain::text).unwrap_or_else(|| agent::plain::render(&done)));
        }
        // Whose each thread is, as the one-thread rule judges it (`confirmation`).
        let mut his = vec![];
        for id in &ids {
            if self.heimdall_s(id)? {
                continue;
            }
            let shell = self
                .client
                .call("t3_thread_read", json!({"threadId":id,"limit":1,"maxCharsPerItem":1,"runLimit":1}))
                .await
                .with_context(|| format!("T3_TOOL_FAILED: reading thread {id} for the card failed; check the id or leave it out of threads"))?;
            let t = &shell["thread"];
            if relay_shell(t) {
                continue;
            }
            his.push(BatchThread { id: id.clone(), title: t["title"].as_str().unwrap_or(id).to_owned(), updated: t["updatedAt"].as_str().map(str::to_owned) });
        }
        let mut confirm = None;
        if !his.is_empty() {
            let his_ids: Vec<String> = his.iter().map(|t| t.id.clone()).collect();
            let key = batch_key(&his_ids);
            let set = serde_json::to_string(&{ let mut s = his_ids.clone(); s.sort(); s })?;
            match a["confirm"].as_str() {
                None => {
                    let (question, description) = batch_question(&his);
                    // One live set per question (review: a set chosen to share another's short tag and days would
                    // otherwise take over its click); its own key is the full hash of the exact set. The question and
                    // titles are kept with it: a thread active again before the click keeps its card.
                    {
                        let db = self.db.lock().unwrap();
                        db.execute("DELETE FROM batch_confirms WHERE question=? AND key!=?", params![question, key])?;
                        db.execute("INSERT OR REPLACE INTO batch_confirms(key,ids,question,description,created_ms) VALUES(?,?,?,?,?)", params![key, set, question, description, crate::now_ms() as i64])?;
                    }
                    self.actions.require_click(&question);
                    let user = agent::identity::user();
                    bail!(
                        "CONFIRM_REQUIRED: call ask with questions [{{question: {question:?}, options: [{{label: \"Yes\", description: {description:?}}}, \"No\"]}}], exactly so. When {user} clicks Yes, repeat this call with the same threads and confirm set to that ask's request_id. Their click archives exactly these {} threads of theirs, once. A spoken yes does not count",
                        his.len()
                    )
                }
                Some(id) => {
                    let row: Option<(String, String, String, i64)> = self
                        .db
                        .lock()
                        .unwrap()
                        .query_row("SELECT ids,question,description,created_ms FROM batch_confirms WHERE key=?", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                        .optional()?;
                    let Some((_, question, description, created)) = row.filter(|r| r.0 == set) else {
                        let consumed: bool = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM consumed_confirms WHERE id=?)", [id], |r| r.get(0))?;
                        ensure!(!consumed, "CONFIRM_USED: that click already approved an earlier call, and each approves one call; call again without confirm and ask again");
                        bail!("CONFIRM_MISMATCH: that card was not asked for these threads; call again without confirm and ask its question")
                    };
                    self.verify_confirmation(id, &question)?;
                    // The card he clicked: asked after this question was issued (an older click never revives with a
                    // later question of the same words), and showing these threads' titles under Yes.
                    let (card, asked_ms) = self.actions.ask_card(id)?.context("CONFIRM_MISMATCH: that card's contents are not on record; ask again")?;
                    ensure!(asked_ms >= created as u64, "CONFIRM_MISMATCH: that card was asked before this question; ask again with it");
                    let yes = card.iter().flat_map(|q| &q.options).find(|o| approves(&o.label));
                    ensure!(
                        card.len() == 1 && yes.and_then(|o| o.description.as_deref()) == Some(description.as_str()),
                        "CONFIRM_MISMATCH: the card must show Yes with the description {description:?}; ask again exactly as CONFIRM_REQUIRED said"
                    );
                    ensure!(crate::now_ms().saturating_sub(created as u64) <= BATCH_LIFETIME_MS, "CONFIRM_EXPIRED: that question is more than 30 minutes old; call again without confirm and ask again");
                    confirm = Some((id.to_owned(), key));
                }
            }
        }
        permit.with(|| {
            self.db.lock().unwrap().execute("UPDATE operations SET state='calling' WHERE id=?", [op])?;
            Ok(())
        })?;
        // His click approves this one batch: spent before the first archive, so a second call racing this one finds it
        // used (review), and the question's set goes with it.
        if let Some((id, key)) = &confirm {
            let db = self.db.lock().unwrap();
            ensure!(db.execute("INSERT OR IGNORE INTO consumed_confirms VALUES(?)", [id])? == 1, "CONFIRM_USED: that click already approved an earlier call, and each approves one call; call again without confirm and ask again");
            db.execute("DELETE FROM batch_confirms WHERE key=?", [key])?;
        }
        let (mut archived, mut failed) = (0usize, vec![]);
        for id in &ids {
            permit.check()?;
            let c = call_args(id);
            match self.client.call_raw(name, c.clone(), Duration::from_secs(30)).await {
                Ok(_) => archived += 1,
                Err(e) if already_so(&c, &e.to_string()) => archived += 1,
                Err(e) => {
                    failed.push(format!("{id}: {e}"));
                    continue;
                }
            }
            self.effects("t3_thread_organize", &c, &Value::Null)?;
        }
        let mut text = format!("Archived {archived} of {} threads.", ids.len());
        if !failed.is_empty() {
            text.push_str(&format!(" Not archived: {}", failed.join("; ")));
        }
        self.finish(op, &Value::String(text.clone()))?;
        self.changed.notify_one();
        Ok(text)
    }
}
// ---- end batch archive ----

/// Night 8 Nov log sweep, finding 1: T3's refusal of a message into an archived thread (0.1.x: `thread_not_sendable`,
/// "is archived and cannot receive messages"; older: "is archived; unarchive it first").
/// Lane S: only the archive refusal itself. `thread_not_sendable` alone also covers a steer with no running turn (bug
/// note 2026-11-10T09-03-14), which unarchiving cannot help.
pub fn archived_refusal(error: &str) -> bool {
    error.starts_with("T3_TOOL_FAILED") && error.contains(" is archived")
}
/// Lane S: T3's refusal of a steer into a thread with no running turn (T3 0.1.x: `thread_not_sendable`, "Thread … has
/// no running turn that can be steered.").
pub fn idle_refusal(error: &str) -> bool {
    error.starts_with("T3_TOOL_FAILED") && error.contains("no running turn")
}
/// Lane S: the same send as a new turn (`auto`: T3 starts a turn on an idle thread, steers a running one), under a
/// fresh retry key so T3's dedupe does not answer with the first refusal.
fn as_new_turn(args: &Value, why: &str) -> Value {
    let mut again = args.clone();
    if again["mode"] == "steer" {
        again["mode"] = json!("auto");
    }
    if let Some(key) = args["clientRequestId"].as_str() {
        again["clientRequestId"] = json!(format!("{key}-{why}"));
    }
    again
}
/// Lane S, t3_call archive failures: T3's refusal to unarchive a thread that is not archived, or to archive one that
/// already is: the thread is already as asked.
fn already_so(args: &Value, error: &str) -> bool {
    error.starts_with("T3_TOOL_FAILED")
        && match args["action"].as_str() {
            Some("unarchive") => error.contains("is not archived"),
            Some("archive") => error.contains("already archived"),
            _ => false,
        }
}
