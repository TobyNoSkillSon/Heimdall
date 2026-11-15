//! `islands_list` (bug note 2026-11-13T12-27-46: "it has no list tool, so it cannot see what is lingering, viewer
//! islands included"): what islands and viewers are up now, one plain line each, for the agent. Read-only.

use super::binding::{told_this_run, Bindings, RESULT_GRACE};
use super::files::Root;
use serde_json::{Map, Value};

/// What an island is, for the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Bound to a T3 thread: one task's job island.
    Job,
    /// A file the user or the agent opened (`islands_view`).
    Viewer,
    /// A finished job's report, kept after its job island left (`kept_report`).
    Report,
    /// Goals, Scheduled, Settled, the Day Timeline.
    Plan,
    /// Any other island the agent put up.
    Other,
}

impl Kind {
    fn word(self) -> &'static str {
        match self { Kind::Job => "job", Kind::Viewer => "viewer", Kind::Report => "report", Kind::Plan => "plan", Kind::Other => "island" }
    }
}

pub fn kind(id: &str, o: &Map<String, Value>, bound: bool) -> Kind {
    let preset = o.get("preset").and_then(Value::as_str);
    if matches!(preset, Some("todo" | "timeline")) || matches!(id, "goals" | "scheduled" | "settled" | "day") { return Kind::Plan }
    if preset == Some("viewer") { return Kind::Viewer }
    if super::kept_report::is_report(o) { return Kind::Report }
    if bound { return Kind::Job }
    Kind::Other
}

/// "under a minute", "12 min", "3 h 5 min".
pub fn span(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0).floor() as u64;
    match m {
        0 => "under a minute".into(),
        1..=89 => format!("{m} min"),
        _ if m % 60 == 0 => format!("{} h", m / 60),
        _ => format!("{} h {} min", m / 60, m % 60),
    }
}

/// The list, from the island files, the bindings, the helper's `state.json` (what is drawn, what is open) and the
/// global hide flag, at `now` (epoch seconds).
pub fn text(root: &Root, bindings: &Bindings, hidden: bool, now: f64) -> String {
    let screen = root.screen_state();
    let drawn = screen.get("islands").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut lines = vec![];
    let mut plans = vec![];
    for (id, o) in root.all() {
        let binding = bindings.get(&id);
        let k = kind(&id, &o, binding.is_some());
        let title = o.get("title").and_then(Value::as_str).unwrap_or(&id).to_string();
        if o.get("hidden").and_then(Value::as_bool) == Some(true) { continue }
        if k == Kind::Plan { plans.push(title); continue }
        let mut line = format!("{} '{title}' (id {id}", k.word());
        if let Some(b) = &binding { line.push_str(&format!(", task {}", b.thread)) }
        line.push_str("): ");
        let mut bits: Vec<String> = vec![];
        let state = o.get("state").and_then(Value::as_str);
        let at = |k: &str| o.get(k).and_then(Value::as_f64);
        match (k, state) {
            (Kind::Viewer | Kind::Report, _) => {}
            (_, Some(s @ ("done" | "failed"))) => bits.push(match at("ended") { Some(e) => format!("{s} {} ago", span(now - e)), None => s.to_string() }),
            (_, Some(s)) => bits.push(s.to_string()),
            (_, None) => bits.push("running".into()),
        }
        if let Some(s) = at("started") { bits.push(format!("up {}", span(now - s))) }
        match drawn.get(&id) {
            Some(d) if d.get("open").and_then(Value::as_bool) == Some(true) => bits.push("open on screen".into()),
            Some(_) => bits.push("closed on screen".into()),
            None => bits.push("not drawn yet".into()),
        }
        match k {
            Kind::Job => {
                let told = binding.as_ref().and_then(|b| bindings.told_at(&b.thread));
                if state == Some("done") && told_this_run(&o, told) {
                    let t = told.unwrap_or(now);
                    bits.push(format!("result told {} ago; leaves {} after that unless {} has it open", span(now - t), span(RESULT_GRACE.as_secs_f64()), super::user()));
                } else if state == Some("done") {
                    bits.push(format!("result not told yet in this run of Heimdall; leaves after {} sees it", super::user()));
                } else if state == Some("failed") {
                    bits.push("stays until removed".into());
                }
            }
            Kind::Viewer => bits.push(format!("stays until {} closes it", super::user())),
            Kind::Report => bits.push(format!("stays until {} opens or closes it", super::user())),
            _ => {}
        }
        line.push_str(&bits.join(", "));
        line.push('.');
        lines.push(line);
    }
    let mut out = match lines.len() {
        0 => "No job, viewer or other islands are up.".to_string(),
        1 => "1 island up:".to_string(),
        n => format!("{n} islands up:"),
    };
    if hidden { out.push_str(" Every island is hidden now (Show Islands brings them back).") }
    for l in lines { out.push('\n'); out.push_str(&l) }
    if !plans.is_empty() { out.push_str(&format!("\nPlan lists shown: {}.", plans.join(", "))) }
    out
}

/// `islands_list` on the live host.
pub fn list(h: &super::Host) -> anyhow::Result<String> {
    Ok(text(&h.root, &h.bindings, super::visibility::hidden(), super::tools::now_s()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }

    #[test]
    fn spans_read_as_minutes_and_hours() {
        assert_eq!(span(20.0), "under a minute");
        assert_eq!(span(23.0 * 60.0 + 5.0), "23 min");
        assert_eq!(span(120.0 * 60.0), "2 h");
        assert_eq!(span(185.0 * 60.0), "3 h 5 min");
        assert_eq!(span(-5.0), "under a minute");
    }

    /// Bug note 2026-11-13T12-27-46: the relay sees every island that is up, with its kind, task, age and time since
    /// it finished; plan lists in one line, hidden ones left out.
    #[test]
    fn lists_jobs_viewers_reports_and_plan_lists_with_their_task_age_and_finish() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        let b = Bindings::load(None);
        let now = 10_000.0;
        root.put("task-voice-isolation", &obj(json!({"title": "Voice isolation", "state": "done", "started": now - 41.0 * 60.0, "ended": now - 23.0 * 60.0}))).unwrap();
        b.bind("task-voice-isolation", "mcp:abc");
        b.told("mcp:abc", now - 22.0 * 60.0);
        root.put("task-build", &obj(json!({"title": "App build", "state": "running", "started": now - 300.0}))).unwrap();
        b.bind("task-build", "mcp:def");
        root.put("view-report.html", &obj(json!({"title": "Report", "preset": "viewer", "path": "/tmp/report.html", "started": now - 90.0, "hidden": false}))).unwrap();
        root.put("goals", &obj(json!({"title": "Goals", "preset": "todo", "list": "goals"}))).unwrap();
        root.put("settled", &obj(json!({"title": "Settled", "preset": "todo", "list": "settled", "hidden": true}))).unwrap();
        super::super::files::write_atomic(&root.state(), &json!({"islands": {"task-voice-isolation": {"open": false}, "view-report.html": {"open": true}}})).unwrap();
        let t = text(&root, &b, false, now);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "3 islands up:");
        assert!(lines.contains(&"job 'App build' (id task-build, task mcp:def): running, up 5 min, not drawn yet."), "{t}");
        assert!(lines.contains(&"job 'Voice isolation' (id task-voice-isolation, task mcp:abc): done 23 min ago, up 41 min, closed on screen, result told 22 min ago; leaves 1 min after that unless the user has it open."), "{t}");
        assert!(lines.contains(&"viewer 'Report' (id view-report.html): up 1 min, open on screen, stays until the user closes it."), "{t}");
        assert_eq!(*lines.last().unwrap(), "Plan lists shown: Goals.");
        assert_eq!(lines.len(), 5);
        let hidden = text(&root, &b, true, now);
        assert!(hidden.starts_with("3 islands up: Every island is hidden now"), "{hidden}");
    }

    #[test]
    fn nothing_up_says_so() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        assert_eq!(text(&root, &Bindings::load(None), false, 0.0), "No job, viewer or other islands are up.");
    }
}
