//! `heimdall --lab-scenario FILE --data-dir DIR`: the post-install check's lab instance (lab/postinstall/README.md).
//!
//! The real UI without its menu-bar item or ⌃⌘M, and the real island at the real notch, driven by a scripted scenario:
//! the `UiUpdate`s the app sends for a workflow (agent health from the app's own `AgentHealth`), and taps, strokes and
//! Send through the island's and the overlay's own AppKit event path. Nothing else starts: no capture, no models, no
//! agent or Pi, no T3, no Keychain, no network. Drawing's Send composites onto a synthetic page, never the user's screen.
//! Honoured only with an explicit isolated `--data-dir` (main.rs refuses the user's profile).
//!
//! Writes into the data directory: `frames.jsonl` (the island's every event and painted frame), `steps.jsonl` (each
//! step, action and check on CLOCK_UPTIME_RAW), `result.json` (checks, CPU, frame cost), `drawing-sent-N.png`.
//! Waits up to 20 s for `<data>/go` (the recorder creates it once it captures) before the timeline starts.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use ui::agent_notice::{AgentNotice, Detail, NoticeAction, NoticeKind};
use ui::lab::Target;
use ui::reader::{FastRead, FastReadUpdate};

fn uptime() -> f64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn cpu_seconds() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    u.ru_utime.tv_sec as f64 + u.ru_utime.tv_usec as f64 / 1e6 + u.ru_stime.tv_sec as f64 + u.ru_stime.tv_usec as f64 / 1e6
}

/// Runs `f` on the main thread and waits for its answer.
fn on_main<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = crossbeam_channel::bounded(1);
    ui::Ui::handle().on_main(move || {
        let _ = tx.send(f());
    });
    rx.recv().expect("the main thread answers")
}

#[derive(Clone)]
struct Log(Arc<Mutex<std::fs::File>>);
impl Log {
    fn line(&self, v: Value) {
        if let Ok(mut f) = self.0.lock() {
            let _ = writeln!(f, "{v}");
        }
    }
}

struct Step {
    at: f64,
    op: Value,
}

fn steps(scenario: &Value) -> Result<Vec<Step>> {
    let mut out = vec![];
    for s in scenario["steps"].as_array().context("scenario needs steps")? {
        let at = s["at"].as_f64().context("step needs at")?;
        if s["op"] == "levels" {
            // A voice's loudness at `hz` for `dur` seconds: a syllable envelope, then 0 (the stream's end).
            let (dur, hz) = (s["dur"].as_f64().unwrap_or(2.0), s["hz"].as_f64().unwrap_or(30.0));
            let n = (dur * hz) as usize;
            for k in 0..n {
                let x = k as f64 / hz;
                let v = (0.5 + 0.5 * (x * 9.0).sin().abs()) * (0.6 + 0.4 * (x * 2.3).cos().abs());
                out.push(Step { at: at + x, op: json!({"op":"level","v":(v * 700.0) as u64}) });
            }
            out.push(Step { at: at + dur, op: json!({"op":"level","v":0}) });
            continue;
        }
        out.push(Step { at, op: s.clone() });
    }
    out.sort_by(|a, b| a.at.total_cmp(&b.at));
    Ok(out)
}

fn kind(v: &Value) -> NoticeKind {
    match v.as_str().unwrap_or("info") {
        "success" => NoticeKind::Success,
        "warning" => NoticeKind::Warning,
        "error" => NoticeKind::Error,
        "looking" => NoticeKind::Looking,
        "working" => NoticeKind::Working,
        _ => NoticeKind::Info,
    }
}

fn command_kind(v: &Value) -> ui::CommandKind {
    match v.as_str().unwrap_or("other") {
        "screenshot" => ui::CommandKind::Screenshot,
        "look_at_this" => ui::CommandKind::LookAtThis,
        "cancel" => ui::CommandKind::Cancel,
        "listening" => ui::CommandKind::Listening,
        _ => ui::CommandKind::Other,
    }
}

fn target(v: &Value) -> Result<Target> {
    let s = v.as_str().context("target is a string")?;
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    Ok(match parts.as_slice() {
        // A card's option by its number (0 is the first): the lab taps a pick, not a label.
        ["answer", id, option] => Target::AskPick { id: id.to_string(), question: 0, option: option.parse().context("answer:<id>:<option number, from 0>")? },
        ["dismiss", id] => Target::Dismiss(id.to_string()),
        ["draw_send"] => Target::DrawSend,
        ["draw_close"] => Target::DrawClose,
        ["draw_undo"] => Target::DrawUndo,
        ["draw_clear"] => Target::DrawClear,
        ["reader_play"] => Target::ReaderPlay,
        ["reader_face"] => Target::ReaderFace,
        ["reader_band"] => Target::ReaderBand,
        // The pulled-down last turn (its reply and notes): its band, a note's X, the notch that opens it. A note no
        // longer has a band of its own; `note_band` is the turn's band.
        ["reply_band"] | ["note_band"] => Target::ReplyBand,
        ["note_close", id] => Target::NoteClose(id.to_string()),
        ["notch"] => Target::Token(ui::lab::Token::Reply),
        ["health_button"] => Target::HealthButton,
        _ => bail!("unknown target {s}"),
    })
}

fn window_state(state: &str) -> ui::AgentRunState {
    match state {
        "working" => ui::AgentRunState::Working,
        "compacting" => ui::AgentRunState::Compacting,
        "restarting" => ui::AgentRunState::Restarting,
        "off" => ui::AgentRunState::Off,
        "starting" => ui::AgentRunState::Starting,
        _ => ui::AgentRunState::Ready,
    }
}

/// The `UiUpdate`s one step sends (the app's own shapes; delivery.rs publishes the window state and the menu state
/// together).
fn updates(op: &Value, health: &mut crate::bridge::LabAgentHealth) -> Result<Vec<ui::UiUpdate>> {
    use ui::UiUpdate as U;
    let s = |k: &str| op[k].as_str().unwrap_or_default().to_string();
    Ok(match op["op"].as_str().unwrap_or_default() {
        "listen" => vec![U::Listen(if op["on"] == false { ui::ListenState::Paused } else { ui::ListenState::Listening })],
        "sent" => vec![U::AgentSent],
        "state" => {
            let state = window_state(&s("state"));
            let session = match state {
                ui::AgentRunState::Off => ui::SessionState::Stopped,
                ui::AgentRunState::Working | ui::AgentRunState::Compacting => ui::SessionState::Working,
                ui::AgentRunState::Ready => ui::SessionState::Running { since: "lab".into() },
                _ => ui::SessionState::Starting,
            };
            let working = matches!(state, ui::AgentRunState::Working | ui::AgentRunState::Compacting);
            vec![
                U::AgentWindow(ui::AgentWindowUpdate::State(ui::AgentWindowState {
                    state,
                    can_send: true,
                    can_stop: working,
                    can_reload: true,
                    problem: None,
                    unresolved: vec![],
                    queued: 0,
                    reviews: vec![],
                    reload: None,
                })),
                U::Agent(ui::AgentState { harness: ui::Harness::Pi, session }),
            ]
        }
        "speaking" => vec![U::HeimdallSpeaking(op["on"] != false)],
        // The audio cut by an interrupt (the app sends it from its interrupt path, before the speaking flag falls).
        "speech_cut" => vec![U::HeimdallSpeechCut],
        "spoken" => vec![U::SpokenText(op["text"].as_str().map(str::to_owned))],
        "level" => vec![U::SpeechLevel(op["v"].as_u64().unwrap_or(0).min(1000) as u16)],
        "notice" => {
            let mut n = AgentNotice::line(s("id"), s("text"), kind(&op["kind"]));
            if let Some(d) = op["detail"].as_str() {
                n.detail = Some(Detail::Text(d.into()));
            }
            // A table detail: {"table":{"headers":[…],"rows":[[…],…]}}.
            let strings = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().map(|x| x.as_str().unwrap_or_default().to_owned()).collect()).unwrap_or_default() };
            if op["table"].is_object() {
                let rows = op["table"]["rows"].as_array().map(|r| r.iter().map(strings).collect()).unwrap_or_default();
                n.detail = Some(Detail::Table { headers: strings(&op["table"]["headers"]), rows });
            }
            vec![U::AgentNotice(n)]
        }
        // The turn's reply as written (the pull-down's words; it ends the agent's turn).
        "last_reply" => vec![U::LastReply(s("text"))],
        "ask" => {
            let options = op["options"].as_array().map(|a| a.iter().filter_map(|o| o.as_str().map(|label| ui::agent_notice::AskOption { label: label.to_owned(), description: None, grant: vec![] })).collect()).unwrap_or_default();
            let mut n = AgentNotice::line(s("id"), s("text"), NoticeKind::Info);
            let questions = vec![ui::agent_notice::AskQuestion { id: "q1".into(), header: None, text: s("text"), multi_select: false, allow_other: true, field: None, options }];
            n.action = Some(NoticeAction::Ask { request_id: s("id"), questions, expires_ms: u64::MAX / 4, countdown: false });
            vec![U::AgentNotice(n)]
        }
        "clear_notices" => vec![U::ClearAgentNotices],
        "dismiss" => vec![U::DismissAgentNotice(s("id"))],
        "close_question" => vec![U::CloseQuestion { id: s("id"), answer: s("answer") }],
        "command" => {
            let id = op["id"].as_u64();
            let k = command_kind(&op["kind"]);
            let cue = match op["cue"].as_str().unwrap_or("done") {
                "heard" => ui::CommandCue::Heard { id: id.unwrap_or(1), kind: k },
                "failed" => ui::CommandCue::Failed { id, kind: k, reason: s("label") },
                "dropped" => ui::CommandCue::Dropped { id: id.unwrap_or(1) },
                _ => ui::CommandCue::Done { id, kind: k, label: s("label") },
            };
            vec![U::Command(cue)]
        }
        // The app's own agent-health copy (bridge::health) for a scripted status.
        "agent_health" => health
            .step(&s("status"), op["failures"].as_u64().unwrap_or(0) as u32, op["restarting"] == true)
            .into_iter()
            .map(U::Health)
            .collect(),
        "fast_read" => vec![U::FastRead(FastReadUpdate::Show(FastRead {
            id: s("id"),
            text: s("text"),
            title: op["title"].as_str().map(str::to_owned),
            wpm: op["wpm"].as_u64().unwrap_or(240) as u32,
        }))],
        _ => vec![],
    })
}

/// Stand-in for drawing's capture: a light page with a dark header at the display's pixel size (never the screen).
fn stand_in_grabber() {
    let (w, h) = ui::display_sizes().first().copied().unwrap_or((3456, 2234));
    let mut rgb = vec![236u8; (w * h * 3) as usize];
    for y in 0..(h / 16) {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&[24, 24, 28]);
        }
    }
    let rgb = Arc::new(rgb);
    ui::drawing::set_grabber(move |_| Ok((w, h, (*rgb).clone())));
}

pub(super) fn run(file: &Path, data: PathBuf) -> Result<()> {
    let scenario: Value = serde_json::from_str(&std::fs::read_to_string(file).context("read scenario")?).context("parse scenario")?;
    let plan = steps(&scenario)?;
    let log = Log(Arc::new(Mutex::new(std::fs::File::create(data.join("steps.jsonl"))?)));
    let actions: Arc<Mutex<Vec<String>>> = Default::default();
    stand_in_grabber();
    let (act_log, act_store, shot_dir) = (log.clone(), actions.clone(), data.clone());
    let shots = std::sync::atomic::AtomicUsize::new(0);
    let on_action = move |a: ui::UiAction| {
        let text = match &a {
            ui::UiAction::DrawingSent(shot) => {
                let n = shots.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = std::fs::write(shot_dir.join(format!("drawing-sent-{n}.png")), &shot.png);
                format!("DrawingSent {{ strokes: {}, width: {}, height: {}, png_bytes: {} }}", shot.strokes, shot.width, shot.height, shot.png.len())
            }
            other => format!("{other:?}"),
        };
        act_log.line(json!({"kind":"action","uptime":uptime(),"action":text}));
        act_store.lock().unwrap().push(text);
    };
    let runner_data = data.clone();
    std::thread::Builder::new().name("lab-scenario".into()).spawn(move || {
        if let Err(e) = timeline(&scenario, plan, &runner_data, &log, &actions) {
            log.line(json!({"kind":"error","error":format!("{e:#}")}));
            eprintln!("lab scenario failed: {e:#}");
        }
        on_main(|| ui::lab::flush_log());
        ui::Ui::handle().on_main(|| std::process::exit(0));
    })?;
    ui::run(ui::Options { fake_permissions: true, lab: true, system_profile: Some(data.join("system.json")), ..Default::default() }, on_action);
}

fn timeline(scenario: &Value, plan: Vec<Step>, data: &Path, log: &Log, actions: &Arc<Mutex<Vec<String>>>) -> Result<()> {
    // The island exists once `ui::run` has installed it.
    let frames = data.join("frames.jsonl");
    let started = Instant::now();
    let zero = loop {
        let path = frames.clone();
        match on_main(move || ui::lab::frame_log(&path)) {
            Ok(z) => break z,
            Err(_) if started.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => bail!("frame log: {e}"),
        }
    };
    let go = data.join("go");
    let waited = Instant::now();
    while !go.exists() && waited.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let recorder = go.exists();
    // The recorder's window-sharing set-up, paid before the timeline (ui::lab::capture_prime), then a moment of rest
    // before the first step: the recorder's first frames show nothing.
    on_main(|| ui::lab::capture_prime(true));
    std::thread::sleep(Duration::from_millis(300));
    on_main(|| ui::lab::capture_prime(false));
    std::thread::sleep(Duration::from_millis(400));
    let name = scenario["name"].as_str().unwrap_or("scenario").to_string();
    log.line(json!({"kind":"start","name":name,"uptime":uptime(),"island_zero":zero,"recorder":recorder}));
    let mut checks: Vec<Value> = vec![];
    let mut health = crate::bridge::LabAgentHealth::default();
    let u = ui::Ui::handle();
    let (t0, c0) = (Instant::now(), cpu_seconds());
    for step in &plan {
        let due = Duration::from_secs_f64(step.at.max(0.0));
        if let Some(wait) = due.checked_sub(t0.elapsed()) {
            std::thread::sleep(wait);
        }
        let op = step.op["op"].as_str().unwrap_or_default().to_string();
        if op != "level" {
            log.line(json!({"kind":"step","at":step.at,"uptime":uptime(),"op":step.op}));
        }
        match op.as_str() {
            "tap" => {
                let t = target(&step.op["target"])?;
                let label = format!("{t:?}");
                let ok = on_main(move || ui::lab::tap(&t));
                checks.push(check(log, step.at, &format!("tap {label} lands on the island's click target"), ok, json!(on_main(ui::lab::targets))));
            }
            "draw_open" => {
                let r = on_main(ui::lab::draw_open);
                checks.push(check(log, step.at, "drawing mode opens", r.is_ok(), json!(r.err())));
            }
            "draw_stroke" => {
                let pts: Vec<(f64, f64)> = step.op["pts"].as_array().map(|a| a.iter().filter_map(|p| Some((p[0].as_f64()?, p[1].as_f64()?))).collect()).unwrap_or_default();
                let ok = on_main(move || ui::lab::draw_stroke(&pts));
                checks.push(check(log, step.at, "a stroke draws through the overlay's event path", ok, json!(on_main(ui::lab::draw_strokes))));
            }
            "expect" => {
                let e = step.op.clone();
                let (class, idle, targets, windows, draw_rest) =
                    on_main(|| (ui::lab::class(), ui::lab::idle(), ui::lab::targets(), ui::lab::windows(), ui::lab::draw_at_rest()));
                let acts = actions.lock().unwrap().clone();
                let state = json!({"class":class,"idle":idle,"targets":targets,"windows":windows,"draw_at_rest":draw_rest});
                if let Some(want) = e["class"].as_str() {
                    checks.push(check(log, step.at, &format!("island shows {want}"), class == want, state.clone()));
                }
                if let Some(not) = e["not_class"].as_str() {
                    checks.push(check(log, step.at, &format!("island is not {not}"), class != not, state.clone()));
                }
                if let Some(a) = e["action"].as_str() {
                    checks.push(check(log, step.at, &format!("action {a}"), acts.iter().any(|x| x.contains(a)), json!(acts)));
                }
                if let Some(a) = e["no_action"].as_str() {
                    checks.push(check(log, step.at, &format!("no action {a}"), !acts.iter().any(|x| x.contains(a)), json!(acts)));
                }
                if let Some(t) = e["target"].as_str() {
                    checks.push(check(log, step.at, &format!("target {t} offered"), targets.iter().any(|x| x.contains(t)), state.clone()));
                }
                if let Some(n) = e["targets_max"].as_u64() {
                    checks.push(check(log, step.at, &format!("at most {n} targets"), targets.len() as u64 <= n, state.clone()));
                }
                if e["draw_at_rest"] == true {
                    checks.push(check(log, step.at, "drawing mode fully at rest", draw_rest, state.clone()));
                }
            }
            _ => {
                for update in updates(&step.op, &mut health)? {
                    u.send(update);
                }
            }
        }
    }
    let end = scenario["end"].as_f64().unwrap_or(0.0);
    if let Some(wait) = Duration::from_secs_f64(end).checked_sub(t0.elapsed()) {
        std::thread::sleep(wait);
    }
    // Rest: the island back in the notch, no display link or loop timer (one pending one-shot deadline is allowed),
    // no window of this process on screen, drawing mode gone.
    let settle = scenario["settle"].as_f64().unwrap_or(8.0);
    let settle_start = Instant::now();
    let rested = loop {
        let (class, animating, windows, draw_rest) = on_main(|| (ui::lab::class(), ui::lab::animating(), ui::lab::windows(), ui::lab::draw_at_rest()));
        if class == "Rest" && !animating && windows.is_empty() && draw_rest {
            break true;
        }
        if settle_start.elapsed().as_secs_f64() > settle {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let active = t0.elapsed().as_secs_f64();
    let active_cpu = cpu_seconds() - c0;
    let (class, idle, windows, targets, clock) = on_main(|| (ui::lab::class(), ui::lab::idle(), ui::lab::windows(), ui::lab::targets(), ui::lab::clock()));
    let at = t0.elapsed().as_secs_f64();
    checks.push(check(
        log,
        at,
        &format!("back to rest within {settle:.0} s after the scenario's end (took {:.2} s)", settle_start.elapsed().as_secs_f64()),
        rested,
        json!({"class":class,"idle":idle,"windows":windows,"targets":targets,"clock":clock}),
    ));
    log.line(json!({"kind":"rest_begin","uptime":uptime()}));
    // The cost at rest: 3 s with nothing to show.
    let (_, ticks0, _) = on_main(ui::lab::stats);
    let (r0, rc0) = (Instant::now(), cpu_seconds());
    std::thread::sleep(Duration::from_secs(3));
    let (rest_s, rest_cpu) = (r0.elapsed().as_secs_f64(), cpu_seconds() - rc0);
    let (frames, ticks1, draw) = on_main(ui::lab::stats);
    let rest_pct = rest_cpu / rest_s * 100.0;
    checks.push(check(log, at + rest_s, &format!("at rest: {rest_pct:.3} % CPU, {} island ticks in 3 s", ticks1 - ticks0), rest_pct < 0.5 && ticks1 == ticks0, json!({})));
    let windows_after = on_main(ui::lab::windows);
    checks.push(check(log, at + rest_s, "no window of the lab instance on screen after the scenario", windows_after.is_empty(), json!(windows_after)));
    let mut sorted = draw.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let pick = |q: f64| sorted.get(((sorted.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0.0) * 1000.0;
    let failed = checks.iter().filter(|c| c["ok"] == false).count();
    let result = json!({
        "name": name, "recorder": recorder, "island_zero": zero, "failed": failed, "checks": checks,
        "cpu": {"active_seconds": active, "active_cpu_seconds": active_cpu, "active_percent_one_core": active_cpu / active * 100.0,
                "rest_seconds": rest_s, "rest_cpu_seconds": rest_cpu, "rest_percent_one_core": rest_pct},
        "frames": {"painted": frames, "ticks": ticks1, "draw_ms_p50": pick(0.5), "draw_ms_p95": pick(0.95), "draw_ms_max": pick(1.0)},
    });
    std::fs::write(data.join("result.json"), serde_json::to_vec_pretty(&result)?)?;
    log.line(json!({"kind":"end","uptime":uptime(),"failed":failed}));
    Ok(())
}

fn check(log: &Log, at: f64, what: &str, ok: bool, detail: Value) -> Value {
    let v = json!({"at":at,"check":what,"ok":ok,"detail":detail});
    log.line(json!({"kind":"check","uptime":uptime(),"check":what,"ok":ok}));
    v
}

#[cfg(test)]
mod scenario_tests {
    /// Every post-install scenario parses, each tap names a known target and each step makes its updates (a typo
    /// would only show as a failed check in a lab run).
    #[test]
    #[ignore = "needs the lab/ folder (post-install scenarios), which is not in the public repository"]
    fn every_postinstall_scenario_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lab/postinstall/scenarios");
        let mut n = 0;
        for entry in std::fs::read_dir(&dir).expect("scenarios") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let scenario: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            let mut health = crate::bridge::LabAgentHealth::default();
            for step in super::steps(&scenario).unwrap_or_else(|e| panic!("{path:?}: {e}")) {
                if step.op["op"] == "tap" {
                    super::target(&step.op["target"]).unwrap_or_else(|e| panic!("{path:?}: {e}"));
                }
                super::updates(&step.op, &mut health).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            }
            n += 1;
        }
        assert!(n >= 10, "{n} scenarios");
    }

    /// The table scenario's notice carries its table, and its reply ends the turn.
    #[test]
    fn a_table_notice_and_a_last_reply_become_their_updates() {
        let mut health = crate::bridge::LabAgentHealth::default();
        let table = serde_json::json!({"op":"notice","id":"t","text":"Jobs","kind":"info","table":{"headers":["A","B"],"rows":[["1","2"]]}});
        let ups = super::updates(&table, &mut health).unwrap();
        assert!(matches!(&ups[..], [ui::UiUpdate::AgentNotice(n)] if n.detail == Some(ui::agent_notice::Detail::Table { headers: vec!["A".into(), "B".into()], rows: vec![vec!["1".into(), "2".into()]] })));
        let reply = super::updates(&serde_json::json!({"op":"last_reply","text":"Done."}), &mut health).unwrap();
        assert!(matches!(&reply[..], [ui::UiUpdate::LastReply(t)] if t == "Done."));
        assert_eq!(super::target(&serde_json::json!("note_close:t")).unwrap(), ui::lab::Target::NoteClose("t".into()));
        assert_eq!(super::target(&serde_json::json!("notch")).unwrap(), ui::lab::Target::Token(ui::lab::Token::Reply));
    }
}
