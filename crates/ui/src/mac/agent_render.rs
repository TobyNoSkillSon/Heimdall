//! `heimdall-ui-demo --agent <dir>`: the Agent window in every run state and the island's questions, drawn offscreen
//! (nothing reaches the screen), and their controls driven in-process: the window's buttons through accessibility
//! press (their real target/action), hit-testing at the buttons' centres, Return through the composer's command path,
//! Edit through the text view's link click; the island's answers through taps sent to its click target (AppKit's
//! dispatch, the island's own hit-testing). Every emitted `UiAction` is checked.

use std::path::Path;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{msg_send, sel, AllocAnyThread as _};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy, NSBitmapImageFileType,
    NSBitmapImageRep, NSButton, NSColor, NSGraphicsContext, NSView, NSWindow,
};
use objc2_foundation::{MainThreadMarker, NSDate, NSDictionary, NSPoint, NSRange, NSRunLoop, NSString};

use super::util::{ns, rect};
use super::{with_state_now, State, STATE};
use crate::agent_notice::{self, AgentNotice, AgentNoticeAction, NoticeAction, NoticeKind};
use crate::agent_view::{self, demo_live_entries, demo_state, LOOP_STOPPED};
use crate::island::object::{QLook, Scene, Target};
use crate::island::Part;
use crate::model::*;

type Actions = Arc<Mutex<Vec<UiAction>>>;

struct Run {
    dir: std::path::PathBuf,
    failures: Vec<String>,
    actions: Actions,
}

impl Run {
    fn check(&mut self, name: &str, ok: bool) {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            self.failures.push(name.to_string());
        }
    }
    fn take(&self) -> Vec<UiAction> {
        pump(0.05);
        std::mem::take(&mut *self.actions.lock().unwrap())
    }
    fn window(&mut self, name: &str) {
        let window = with_state_now(|s| s.agent_window.as_ref().map(|w| w.window.clone())).flatten();
        let Some(window) = window else { return self.check(&format!("{name}: window exists"), false) };
        let ok = capture_window(&window, &self.dir.join(format!("{name}.png")));
        self.check(&format!("{name}: rendered"), ok);
    }
}

/// Runs the main run loop briefly (main-queue blocks, image reads coming back), with nothing on screen.
fn pump(seconds: f64) {
    let until = NSDate::dateWithTimeIntervalSinceNow(seconds);
    NSRunLoop::currentRunLoop().runUntilDate(&until);
}

fn apply(update: UiUpdate) {
    with_state_now(|s| s.apply(update));
}

fn state(state: AgentWindowState) {
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::State(state)));
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("render_agent runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
    std::fs::create_dir_all(dir).ok();
    let screenshot = demo_screenshot();
    crate::set_attachment_loader(move |id| (id == "demo-screenshot").then(|| screenshot.clone()));

    let actions: Actions = Default::default();
    let sink = actions.clone();
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let options = crate::Options { fake_permissions: true, ..Default::default() };
    let mut s = State::new(mtm, controller, options, Box::new(move |a| sink.lock().unwrap().push(a)), None, menu, None);
    s.agent_autosave = false;
    STATE.with(|cell| *cell.borrow_mut() = Some(s));
    super::island::install_offscreen(mtm);
    let island_sink = actions.clone();
    super::island::set_sink(Box::new(move |a| island_sink.lock().unwrap().push(a)));
    let mut run = Run { dir: dir.into(), failures: vec![], actions };

    agent_window(&mut run, mtm);
    debug_window(&mut run);
    question_card(&mut run, mtm);
    streaming(&mut run);
    run.check("ACTIONS_RENDERED is on with working Yes/No", agent_notice::ACTIONS_RENDERED);
    run.failures
}

fn session() -> AgentSession {
    AgentSession { harness: Harness::Pi, model: Some("Qwen3.8 27B".into()), session_id: Some("019a-demo".into()), agent_on: true, can_open: false }
}

fn agent_window(run: &mut Run, mtm: MainThreadMarker) {
    use AgentRunState as S;
    apply(UiUpdate::AgentSession(session()));
    // The first frame: Agent off, made before anything is clicked.
    state(demo_state(S::Off));
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::History(demo_live_entries())));
    with_state_now(|s| {
        s.ensure_agent_window();
    });
    pump(0.2);
    run.window("agent-off");
    let off = with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        (w.composer.isEditable(), w.send.isEnabled(), w.reload.isEnabled(), w.stop.isHidden())
    })
    .unwrap();
    run.check("first frame, Off: composer, Send and Reload disabled, no Stop", off == (false, false, false, true));
    run.check("Off: Send refuses an accessibility press", !press(&button(|w| w.send.clone())));
    run.check("Off: Reload refuses an accessibility press", !press(&button(|w| w.reload.clone())));
    run.check("Off: nothing emitted", run.take().is_empty());

    for (name, st) in [
        ("agent-starting", S::Starting),
        ("agent-compacting", S::Compacting),
        ("agent-waiting-engine", S::WaitingEngine),
        ("agent-waiting-provider", S::WaitingProvider),
        ("agent-reload-pending", S::ReloadPending),
        ("agent-restarting", S::Restarting),
        ("agent-failed", S::Failed),
    ] {
        state(demo_state(st.clone()));
        run.window(name);
        let hidden = with_state_now(|s| s.agent_window.as_ref().unwrap().reload.isHidden()).unwrap();
        run.check(&format!("{name}: header Reload only when failed"), hidden == (st != S::Failed));
        if st == S::Failed {
            run.check("Failed keeps Reload", press(&button(|w| w.reload.clone())));
            let a = run.take();
            run.check("Reload emits Reload without abort", matches!(a.as_slice(), [UiAction::AgentWindow(AgentWindowAction::Reload { abort: false, .. })]));
        }
        if st == S::WaitingEngine {
            type_text("Queue this for when the engine is back");
            run.check("WaitingEngine: Send is enabled (durable queue)", button(|w| w.send.clone()).isEnabled());
            run.window("agent-waiting-engine-typed");
            set_text("");
        }
        if st == S::WaitingProvider {
            type_text("Queue this for when the model can run");
            run.check("WaitingProvider: Send is enabled (durable queue)", button(|w| w.send.clone()).isEnabled());
            set_text("");
        }
    }

    // Ready: the typed message is editable; images load from the archive by id.
    state(demo_state(S::Ready));
    pump(0.3);
    run.window("agent-ready");
    let has_image = with_state_now(|s| storage_has_attachment(&s.agent_window.as_ref().unwrap().text)).unwrap();
    run.check("Ready: the screenshot attachment is drawn from the archive bytes", has_image);

    // Send by button (accessibility press and hit-testing), then by Return.
    type_text("Run the capture tests again");
    run.check("hit-test at Send's centre finds Send", hit(&button(|w| w.send.clone())));
    run.check("Send takes an accessibility press", press(&button(|w| w.send.clone())));
    let a = run.take();
    run.check(
        "Send emits the typed text, not steering",
        a == vec![UiAction::AgentWindow(AgentWindowAction::Send { text: "Run the capture tests again".into(), steer: false })],
    );
    run.check("Send clears the composer", composer_text().is_empty());
    type_text("Second line");
    with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        unsafe { objc2_app_kit::NSStandardKeyBindingResponding::doCommandBySelector(&*w.composer, sel!(insertNewline:)) };
    });
    let a = run.take();
    run.check("Return sends", a == vec![UiAction::AgentWindow(AgentWindowAction::Send { text: "Second line".into(), steer: false })]);
    // A refusal gives the text back.
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::Entries(vec![failure("message-error-1", "Agent is off", (3, 44))])));
    run.check("a refused message returns to the composer", composer_text() == "Second line");
    set_text("");

    // Edit and resend the typed message.
    let link = with_state_now(|s| edit_link(&s.agent_window.as_ref().unwrap().text)).flatten();
    run.check("Ready: the typed message has an Edit link", link.is_some());
    if let Some((index, value)) = link {
        with_state_now(|s| {
            let w = s.agent_window.as_ref().unwrap();
            let value: Retained<NSString> = ns(&value);
            let _: () = unsafe { msg_send![&*w.text, clickedOnLink: &*value, atIndex: index] };
        });
        pump(0.05);
        let (text, title) = with_state_now(|s| {
            let w = s.agent_window.as_ref().unwrap();
            (w.composer_text(), w.send.title().to_string())
        })
        .unwrap();
        run.check("Edit puts User's words in the composer and Send becomes Resend", text == "Can you check out the resampler branch and run its tests?" && title == "Resend");
        run.window("agent-edit");
        set_text("Can you check out the resampler branch and run only the capture tests?");
        press(&button(|w| w.send.clone()));
        let a = run.take();
        run.check(
            "Resend emits EditAndResend with the persisted entry id",
            a == vec![UiAction::AgentWindow(AgentWindowAction::EditAndResend {
                session_entry_id: "s-m1".into(),
                text: "Can you check out the resampler branch and run only the capture tests?".into(),
            })],
        );
    }

    // Working: a streaming reply replaces its own block; Stop and Interrupt.
    state(demo_state(S::Working));
    let mut reply = AgentLiveEntry {
        id: "stream-9-0".into(),
        session_entry_id: None,
        timestamp_ms: agent_view::demo_ms(14, 3, 50),
        kind: AgentLiveKind::Reply,
        text: String::new(),
        tool_name: None,
        attachments: vec![],
        finished: false,
        editable: false,
        error: false,
        sent: true,
        queued: false,
        owner_text: None,
        usage: None,
    };
    for text in ["Running", "Running the capture", "Running the capture tests on the resampler branch now"] {
        reply.text = text.into();
        apply(UiUpdate::AgentWindow(AgentWindowUpdate::Entries(vec![reply.clone()])));
    }
    let count = with_state_now(|s| s.agent_window.as_ref().unwrap().text.string().to_string().matches("Running").count()).unwrap();
    run.check("a streaming reply is one block, replaced in place", count == 1);
    let edit_now = with_state_now(|s| edit_link(&s.agent_window.as_ref().unwrap().text)).flatten();
    run.check("Working: Edit is greyed (no link)", edit_now.is_none());
    type_text("Skip the slow tests");
    run.window("agent-working");
    run.check("Working: Interrupt shows", !button(|w| w.interrupt.clone()).isHidden());
    press(&button(|w| w.interrupt.clone()));
    let a = run.take();
    run.check("Interrupt emits a steering message", a == vec![UiAction::AgentWindow(AgentWindowAction::Send { text: "Skip the slow tests".into(), steer: true })]);
    run.check("hit-test at Stop's centre finds Stop", hit(&button(|w| w.stop.clone())));
    press(&button(|w| w.stop.clone()));
    run.check("Stop emits Stop", run.take() == vec![UiAction::AgentWindow(AgentWindowAction::Stop)]);

    // The loop guard's stop: one finished line; the window stays usable.
    reply.finished = true;
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::Entries(vec![reply, failure("loop-guard-1-0", LOOP_STOPPED, (4, 2))])));
    state(demo_state(S::Ready));
    run.window("agent-loop-stopped");
    let text = with_state_now(|s| s.agent_window.as_ref().unwrap().text.string().to_string()).unwrap();
    run.check("the loop stop reads as one line", text.contains(&format!("{LOOP_STOPPED}\u{2029}")));
    type_text("Try a different approach");
    run.check("after the loop stop, Send still works", press(&button(|w| w.send.clone())) && run.take().len() == 1);

    // Needs review: the wake, what ran after it, Completed / Abandon (once).
    state(demo_state(S::NeedsReview));
    run.window("agent-needs-review");
    let review = with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        (w.review_completed.isHidden(), w.send.isHidden())
    })
    .unwrap();
    run.check("NeedsReview: the review replaces the composer", review == (false, true));
    run.check("hit-test at Completed's centre finds it", hit(&button(|w| w.review_completed.clone())));
    press(&button(|w| w.review_completed.clone()));
    let a = run.take();
    run.check("Completed resolves that delivery id", a == vec![UiAction::AgentWindow(AgentWindowAction::ResolveDelivery { id: "m1".into(), completed: true })]);
    press(&button(|w| w.review_abandon.clone()));
    run.check("a second answer is refused", run.take().is_empty());
    let mut missing = demo_state(S::NeedsReview);
    missing.unresolved = vec!["not-in-history".into()];
    state(missing);
    run.window("agent-needs-review-missing");
    press(&button(|w| w.review_abandon.clone()));
    run.check("Abandon resolves as not completed", run.take() == vec![UiAction::AgentWindow(AgentWindowAction::ResolveDelivery { id: "not-in-history".into(), completed: false })]);

    // The narrowest the window goes, and the empty first frame.
    state(demo_state(S::Ready));
    with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        w.window.setContentSize(objc2_foundation::NSSize::new(460.0, 520.0));
        w.layout();
    });
    run.window("agent-ready-narrow");
    with_state_now(|s| {
        s.agent_window = None;
        s.agent_view = Default::default();
    });
    state(demo_state(S::Ready));
    with_state_now(|s| {
        s.ensure_agent_window();
    });
    run.window("agent-empty");

    // Every other kind, with the engine away: compaction, a reload receipt, a voice wake with its screenshot (seen
    // at the top), live thinking, a failed and a running tool, and two typed messages queued for the engine.
    with_state_now(|s| {
        s.agent_window = None;
        s.agent_view = Default::default();
    });
    let mut waiting = demo_state(S::WaitingEngine);
    waiting.queued = 2;
    state(waiting);
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::History(kinds())));
    with_state_now(|s| {
        s.ensure_agent_window();
    });
    pump(0.3);
    run.window("agent-kinds");
    let text = with_state_now(|s| s.agent_window.as_ref().unwrap().text.string().to_string()).unwrap();
    for word in ["History compacted", "Reloaded", "Thinking…", "Screenshot · failed", "Bash · running", "User · queued", "Stopped"] {
        run.check(&format!("kinds: shows {word}"), text.contains(word));
    }
    with_state_now(|s| s.agent_window.as_ref().unwrap().scroll_to_top());
    run.window("agent-attachment");
    let _ = mtm;
}

/// The Debug Agent window: empty; a busy stretch of the long conversation (a voice wake whose edit fails and is
/// retried, a NO_REPLY wake, a typed request that fails, compactions, the rate wait) over the session's totals, at
/// its default, wide and narrow sizes; and a pending reload with Reload Now.
fn debug_window(run: &mut Run) {
    use AgentRunState as S;
    let fresh = |size: (f64, f64)| {
        with_state_now(|s| {
            s.agent_window = None;
            s.agent_view = Default::default();
            let w = s.ensure_agent_window();
            w.window.setContentSize(objc2_foundation::NSSize::new(size.0, size.1));
            w.layout();
        });
    };
    apply(UiUpdate::AgentSession(AgentSession { model: Some("claude-opus-5-5".into()), ..session() }));
    fresh((720.0, 640.0));
    state(demo_state(S::Ready));
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::History(vec![])));
    pump(0.1);
    run.window("debug-empty");
    let title = with_state_now(|s| s.agent_window.as_ref().unwrap().window.title().to_string()).unwrap();
    run.check("debug: the window is titled Debug Agent", title == "Debug Agent");
    let tiles = with_state_now(|s| s.agent_window.as_ref().unwrap().strip.tiles()).unwrap();
    run.check("debug-empty: six quiet tiles", tiles.len() == 6 && tiles.iter().all(|t| t.value == "—" || t.value == "0"));
    // [n23-menu] A ready agent has nothing to reload: no pending line and no header Reload.
    run.check("debug-empty: no pending reload line, no Reload while ready", with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        w.reload_now.isHiddenOrHasHiddenAncestor()
    }).unwrap() && button(|w| w.reload.clone()).isHidden());

    for (name, size) in [("debug-busy", (760.0, 760.0)), ("debug-busy-wide", (1240.0, 780.0)), ("debug-busy-narrow", (460.0, 700.0))] {
        fresh(size);
        state(demo_state(S::Working));
        apply(UiUpdate::AgentWindow(AgentWindowUpdate::Session(busy_stats())));
        apply(UiUpdate::AgentWindow(AgentWindowUpdate::History(busy())));
        pump(0.2);
        run.window(name);
    }
    let text = with_state_now(|s| s.agent_window.as_ref().unwrap().text.string().to_string()).unwrap();
    for word in ["Wake", "Spoke", "No reply", "Failed", "Running", "Edit · failed", "History compacted", "request resumed"] {
        run.check(&format!("debug-busy: shows {word}"), text.contains(word));
    }
    let tiles = with_state_now(|s| s.agent_window.as_ref().unwrap().strip.tiles()).unwrap();
    let values: Vec<&str> = tiles.iter().map(|t| t.value.as_str()).collect();
    run.check(&format!("debug-busy: telemetry adds the stretch to the session totals {values:?}"), values.first() == Some(&"9.9K") && values.get(2) == Some(&"1,315") && values.get(4) == Some(&"4") && values.get(5) == Some(&"2"));
    // Scrolled to the top: the first turn's heading and its failed edit.
    with_state_now(|s| s.agent_window.as_ref().unwrap().scroll_to_top());
    run.window("debug-busy-narrow-top");

    // A pending reload: its reason on one line; Reload Now stops the turn and reloads.
    fresh((760.0, 640.0));
    let mut pending = demo_state(S::ReloadPending);
    pending.reload = Some(AgentReloadPending { reason: "Prompt updated: the Arsenal section and the model routing rule changed.".into(), receipt_ids: vec!["r1".into()] });
    state(pending);
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::Session(busy_stats())));
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::History(busy())));
    pump(0.2);
    run.window("debug-reload-pending");
    run.check("reload pending: the header Reload steps aside", button(|w| w.reload.clone()).isHidden());
    run.take();
    run.check("reload pending: Reload Now takes a press", press(&button(|w| w.reload_now.clone())));
    let emitted = run.take();
    run.check(
        &format!("reload pending: Reload Now emits an aborting reload {emitted:?}"),
        matches!(emitted.as_slice(), [UiAction::AgentWindow(AgentWindowAction::Reload { abort: true, .. })]),
    );
    // The next scenario starts from a fresh window at the default size.
    fresh((660.0, 600.0));
    apply(UiUpdate::AgentSession(session()));
}

fn busy_stats() -> AgentSessionStats {
    AgentSessionStats {
        since_ms: agent_view::demo_ms(9, 0, 0) - 7 * 86_400_000,
        through_ms: agent_view::demo_ms(13, 59, 0),
        requests: 4_806,
        input: 3_100,
        output: 1_204_000,
        cache_read: 409_000_000,
        cache_write: 880_000,
        last_prompt: 176_000,
        compactions: 3,
        last_compaction_ms: Some(agent_view::demo_ms(11, 20, 0)),
        tool_calls: 1_311,
        tool_failures: 8,
        failed_edits: 1,
        errors: 1,
        last_error_ms: Some(agent_view::demo_ms(12, 2, 0)),
        window_tokens: Some(200_000),
    }
}

/// Shaped like the live session's rows (22 Oct, read-only copy): a voice wake that edits a file (the first edit
/// fails), runs the tests and speaks; a NO_REPLY wake; context; a typed request whose provider fails; a compaction;
/// the rate wait; and a wake still running.
fn busy() -> Vec<AgentLiveEntry> {
    let e = |id: &str, (m, s): (u64, u64), kind: AgentLiveKind, text: &str| AgentLiveEntry {
        id: id.into(),
        session_entry_id: Some(format!("s-{id}")),
        timestamp_ms: agent_view::demo_ms(14, m, s),
        kind,
        text: text.into(),
        tool_name: None,
        attachments: vec![],
        finished: true,
        editable: false,
        error: false,
        sent: true,
        queued: false,
        owner_text: None,
        usage: None,
    };
    let tool = |id: &str, at: (u64, u64), kind: AgentLiveKind, name: &str, text: &str, error: bool| {
        let mut x = e(id, at, kind, text);
        x.tool_name = Some(name.into());
        x.error = error;
        x
    };
    let used = |mut x: AgentLiveEntry, cached: u64, output: u64, started: (u64, u64), first: Option<u64>| {
        let start = agent_view::demo_ms(14, started.0, started.1);
        x.usage = Some(AgentUsage { input: 3, output, cache_read: cached, cache_write: 900, started_ms: start, first_token_ms: first.map(|f| start + f) });
        x
    };
    let wake = |id: &str, at: (u64, u64), said: &str| {
        e(id, at, AgentLiveKind::Prompt, &format!("[Heimdall delivery: {id}]\nTranscript:\n{said}\n\nOwner's wake instruction:\n{}", agent_view::WAKE_INSTRUCTION))
    };
    let mut typed = e("m1", (6, 10), AgentLiveKind::Prompt, "[Heimdall delivery: m1]\n[Heimdall typed message]\nCan you summarise what the night pass changed?");
    typed.owner_text = Some("Can you summarise what the night pass changed?".into());
    typed.editable = true;
    let mut rate = e("provider-rate-wait", (6, 12), AgentLiveKind::Compaction, "Anthropic rate limit · retrying in 6 seconds");
    rate.finished = false;
    let mut settled_rate = rate.clone();
    settled_rate.finished = true;
    settled_rate.text = "Anthropic request resumed".into();
    vec![
        wake("d1", (1, 5), "[14:01:03 Mic · User] Heimdall, the resampler test name is wrong, fix it and run the tests"),
        used(e("t1", (1, 9), AgentLiveKind::Thinking, "The test is named resampler_handles_44k but it checks 48 kHz. Rename it in crates/capture/src/resample.rs."), 171_000, 120, (1, 6), Some(1_840)),
        used(tool("c1", (1, 10), AgentLiveKind::ToolCall, "edit", "{\"path\":\"crates/capture/src/resample.rs\",\"oldText\":\"fn resampler_handles_44k()\",\"newText\":\"fn resampler_handles_48k()\"}", false), 171_400, 90, (1, 9), None),
        tool("c1r", (1, 10), AgentLiveKind::ToolResult, "edit", "Could not find the exact text in crates/capture/src/resample.rs. The old text must match exactly including all whitespace and newlines.", true),
        used(tool("c2", (1, 14), AgentLiveKind::ToolCall, "edit", "{\"path\":\"crates/capture/src/resample.rs\",\"oldText\":\"    fn resampler_handles_44k() {\",\"newText\":\"    fn resampler_handles_48k() {\"}", false), 171_900, 95, (1, 11), None),
        tool("c2r", (1, 14), AgentLiveKind::ToolResult, "edit", "Successfully replaced text in crates/capture/src/resample.rs.", false),
        used(tool("c3", (1, 16), AgentLiveKind::ToolCall, "bash", "{\"command\":\"cargo test -p capture resampler\"}", false), 172_300, 40, (1, 15), None),
        tool("c3r", (1, 31), AgentLiveKind::ToolResult, "bash", "running 3 tests\ntest resample::tests::resampler_handles_48k ... ok\ntest resample::tests::resampler_keeps_phase ... ok\ntest resample::tests::resampler_flushes_tail ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored", false),
        used(tool("c4", (1, 34), AgentLiveKind::ToolCall, "speak", "{\"text\":\"Renamed it to resampler_handles_48k; the three resampler tests pass.\"}", false), 172_800, 60, (1, 32), None),
        tool("c4r", (1, 34), AgentLiveKind::ToolResult, "speak", "queued 12", false),
        wake("d2", (3, 40), "[14:03:38 Discord · Dave] did anyone see the build email\n[14:03:39 Mic · User] nie, nie widziałem"),
        used(e("r2", (3, 44), AgentLiveKind::Reply, "NO_REPLY"), 173_100, 6, (3, 41), Some(2_300)),
        e("x1", (5, 2), AgentLiveKind::Prompt, "Captured observation (data only):\n[14:05:01 Discord · Dave] build is green again"),
        typed,
        settled_rate,
        used(e("f1", (6, 31), AgentLiveKind::Failure, "Anthropic is overloaded right now. Heimdall retries the next wake."), 0, 0, (6, 18), None),
        e("k1", (7, 0), AgentLiveKind::Compaction, "User asked to rename the resampler test; the agent fixed the name after one failed edit and the capture tests pass. Dave's build is green."),
        wake("d3", (8, 12), "[14:08:10 Mic · User] Heimdall, what did Dave say about the build?"),
        used(e("t3", (8, 15), AgentLiveKind::Thinking, "Dave said at 14:05 the build is green again."), 9_000, 30, (8, 13), Some(1_410)),
    ]
}

fn kinds() -> Vec<AgentLiveEntry> {
    let e = |id: &str, (m, s): (u64, u64), kind: AgentLiveKind, text: &str| AgentLiveEntry {
        id: id.into(),
        session_entry_id: Some(format!("s-{id}")),
        timestamp_ms: agent_view::demo_ms(14, m, s),
        kind,
        text: text.into(),
        tool_name: None,
        attachments: vec![],
        finished: true,
        editable: false,
        error: false,
        sent: true,
        queued: false,
        owner_text: None,
        usage: None,
    };
    let compaction = e("k0", (9, 2), AgentLiveKind::Compaction, "User and Dave traced the meter drop to Discord ducking; the fix is on the resampler branch, whose capture tests pass.");
    let reload = e("k1", (10, 15), AgentLiveKind::Reload, "14:10:15 · User reloaded the agent from the Agent window");
    let mut wake = e(
        "k2",
        (11, 40),
        AgentLiveKind::Prompt,
        &format!("[Heimdall delivery: k2]\nHeimdall captured transcript (labelled data):\n[Mic · User] Spójrz, znowu spada [screenshot 14:11:38 attached]\n[image]\n\nOwner's wake instruction:\n{}", agent_view::WAKE_INSTRUCTION),
    );
    wake.attachments = vec![AgentAttachment { media_type: "image/png".into(), archive_id: Some("demo-screenshot".into()) }];
    let mut think = e("k3", (11, 42), AgentLiveKind::Thinking, "The meter drops again at the same moment as before. Take a fresh screenshot to compare");
    think.finished = false;
    let mut shot = e("k4-call", (11, 44), AgentLiveKind::ToolCall, "{}");
    shot.tool_name = Some("screenshot_now".into());
    let mut shot_result = e("k4-result", (11, 44), AgentLiveKind::ToolResult, "Screen Recording permission is off");
    shot_result.tool_name = Some("screenshot_now".into());
    shot_result.error = true;
    let mut bash = e("k5-call", (11, 47), AgentLiveKind::ToolCall, "{\"command\":\"cargo test -p capture resampler\"}");
    bash.tool_name = Some("bash".into());
    let mut running = e("k5-result", (11, 49), AgentLiveKind::ToolResult, "Compiling capture v0.1.0\nRunning 12 tests");
    running.tool_name = Some("bash".into());
    running.finished = false;
    let queued = |id: &str, at, text: &str| {
        let mut q = e(id, at, AgentLiveKind::Prompt, &format!("[Heimdall typed message]\n{text}"));
        q.session_entry_id = None;
        q.sent = false;
        q.queued = true;
        q.editable = true;
        q.owner_text = Some(text.into());
        q
    };
    // The user's words steered the turn mid-request (audit 27 Oct, F4): a calm stop row, not an orange failure.
    let stop = e("k3-stop", (11, 43), AgentLiveKind::Failure, "Turn stopped");
    vec![
        compaction,
        reload,
        wake,
        think,
        stop,
        shot,
        shot_result,
        bash,
        running,
        queued("k6", (12, 5), "When that's done, push the branch"),
        queued("k7", (12, 20), "And tell Dave"),
    ]
}

/// Every text line the island draws now, in its elements' order.
fn with_host_frame_texts() -> Vec<String> {
    super::island::host::with_host(|h| {
        h.island.frame(h.clock.unwrap_or(0.0)).slots.iter().filter_map(|s| match &*s.content {
            crate::island::object::Content::Text { lines, .. } => Some(lines.clone()),
            _ => None,
        }).flatten().collect()
    })
    .unwrap_or_default()
}

/// The island's scene now.
fn scene() -> Scene {
    super::island::host::with_host(|h| h.island.scene().clone()).unwrap_or(Scene::Rest)
}

/// A tap on the island's `target` through its click target; whether the target was there.
fn tap_island(target: &Target) -> bool {
    let Some(control) = super::island::host::with_host(|h| h.panels().1) else { return false };
    let ok = super::island::checks::tap(&control, target);
    pump(0.05);
    ok
}

/// The actions about notices (the island's diagnostics reports go to the app too, and are not checked here).
fn notice_actions(a: Vec<UiAction>) -> Vec<UiAction> {
    a.into_iter().filter(|a| matches!(a, UiAction::AgentNotice(_))).collect()
}

/// A tap on option `option` (index) of question 0 on card `id`.
fn answer(id: &str, option: usize) -> Target {
    Target::AskPick { id: id.into(), question: 0, option }
}

/// The answer a card sends for one single-select question.
fn one(question: &str, label: &str) -> Vec<crate::agent_notice::QuestionAnswer> {
    vec![crate::agent_notice::QuestionAnswer { question_id: question.into(), labels: vec![label.into()], other: None }]
}

/// The island's questions: pictures of each state, then the real event path.
fn question_card(run: &mut Run, _mtm: MainThreadMarker) {
    use super::island::checks::advance;
    let asking = |id: &str, text: &str, options: &[&str], expires_ms: u64| AgentNotice {
        action: Some(NoticeAction::single(id, text, options, expires_ms)),
        ..AgentNotice::line(id, text, NoticeKind::Info)
    };
    let question = |id: &str, text: &str, expires_ms: u64| asking(id, text, &["Yes", "No"], expires_ms);
    let connect = |id: &str| AgentNotice { action: Some(NoticeAction::ConnectT3), ..AgentNotice::line(id, "Connect T3 · click", NoticeKind::Info) };
    let later = super::now_ms() + 86_400_000;
    // The user's own example (18 Oct): "I finished the new Heimdall build. Do you want me to install it? Yes, no?"
    let short = question("q1", "Install the new Heimdall build?", later);
    let four = asking("q2", "Which build should I install tonight?", &["Main", "Island", "Reader", "None"], later);
    let longest = asking("q3", &"Restart the speech engine now, or wait until the call with Dave ends in the next hour?".chars().take(120).collect::<String>(), &["Restart now", "After the call", "Tomorrow morning", "Never mind"], later);

    // Pictures: each question state as the island draws it.
    let picture = |run: &mut Run, name: &str| {
        advance(1.0);
        let ok = super::island::checks::snapshot(&run.dir.join(format!("{name}.png")));
        run.check(&format!("{name}: rendered"), ok);
        let (w, h) = super::island::host::with_host(|h| h.island.target_size()).unwrap_or((0.0, 0.0));
        // A question card is the 580 pt card (`layout::ASK_W`); four options with a two-line question stay under 540 pt.
        run.check(&format!("{name}: compact ({w:.0} × {h:.0} pt)"), w <= 580.0 && h <= 540.0);
    };
    apply(UiUpdate::AgentNotice(short.clone()));
    picture(run, "island-question");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(four.clone()));
    picture(run, "island-question-four");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(longest.clone()));
    picture(run, "island-question-longest");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(short.clone()));
    apply(UiUpdate::Health(crate::island::HealthCue::Restarting { key: "agent".into(), part: Part::Agent, label: "Restarting the agent".into() }));
    picture(run, "island-question-under-status");
    apply(UiUpdate::Health(crate::island::HealthCue::Clear { key: "agent".into() }));
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(question("q4", "Run the full test suite now?", super::now_ms() - 1)));
    picture(run, "island-question-expired");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(connect("t3-connect-401:key")));
    picture(run, "island-connect-t3");
    apply(UiUpdate::ClearAgentNotices);
    // Approval grants (4 Nov evening): an option that grants draws every path and login item whole under it, and a
    // tap made in this process (`sendEvent`, judged like an event another process posts) picks no grant.
    {
        use crate::agent_notice::{AskOption, AskQuestion, GrantItem};
        let grant = AgentNotice {
            action: Some(NoticeAction::Ask { request_id: "grant".into(), expires_ms: later, countdown: false, questions: vec![AskQuestion {
                id: "q1".into(), header: None, text: "Delete the old T3 Code copies and the nightly helper?".into(), multi_select: false, allow_other: false, field: None,
                options: vec![
                    AskOption { label: "Delete them".into(), description: Some("Frees 1.6 GB".into()), grant: vec![
                        GrantItem::Delete("~/Backups/T3 Code (Nightly).app".into()),
                        GrantItem::Delete("~/.t3-backup-before-nightly-20261005".into()),
                        GrantItem::Delete("~/Library/Application Support/t3code-nightly/sessions/2026-10-05 before the nightly migration/state.sqlite".into()),
                        GrantItem::LoginItem("T3 Code Nightly Helper".into()),
                    ] },
                    AskOption::label("Keep them"),
                ],
            }] }),
            ..AgentNotice::line("grant", "Delete the old T3 Code copies and the nightly helper?", NoticeKind::Info)
        };
        apply(UiUpdate::AgentNotice(grant));
        advance(1.0);
        let ok = super::island::checks::snapshot(&run.dir.join("island-question-grant.png"));
        run.check("island-question-grant: rendered", ok);
        let whole = with_host_frame_texts().iter().any(|t| t == "~/Backups/T3 Code (Nightly).app")
            && with_host_frame_texts().concat().contains("~/Library/Application Support/t3code-nightly/sessions/2026-10-05 before the nightly migration/state.sqlite")
            && with_host_frame_texts().iter().any(|t| t == "T3 Code Nightly Helper");
        run.check("island-question-grant: every path and item drawn whole", whole);
        run.take();
        // Through the real click target: an NSEvent made in this process carries this pid, so the grant takes no pick.
        run.check("island-question-grant: the granting option has a click target", tap_island(&answer("grant", 0)));
        let a = notice_actions(run.take());
        run.check(&format!("island-question-grant: a tap from this process picks no grant ({a:?})"), a.is_empty());
        run.check("island-question-grant: Keep takes a tap", tap_island(&answer("grant", 1)));
        let a = notice_actions(run.take());
        run.check(&format!("island-question-grant: Keep answers by no hand ({a:?})"), matches!(&a[..], [UiAction::AgentNotice(AgentNoticeAction::Answer { request_id, answers, by_hand })] if request_id == "grant" && answers[0].labels == ["Keep them"] && by_hand.is_empty()));
        apply(UiUpdate::ClearAgentNotices);
        advance(1.0);
    }
    // The adaptive card (24 Oct): several picks, three tabs, Other typed, and the composer in the pulled-down
    // conversation. Each is a card as the app sends it (`ask` with `questions`); its picks are set as a voice answer.
    let ask_opt = |label: &str, description: &str| crate::agent_notice::AskOption { label: label.into(), description: (!description.is_empty()).then(|| description.into()), grant: vec![] };
    let ask_q = |id: &str, header: Option<&str>, text: &str, multi: bool, options: Vec<crate::agent_notice::AskOption>| crate::agent_notice::AskQuestion {
        id: id.into(),
        header: header.map(str::to_owned),
        text: text.into(),
        multi_select: multi,
        allow_other: true, field: None,
        options,
    };
    let ask_card = |id: &str, questions: Vec<crate::agent_notice::AskQuestion>| AgentNotice {
        action: Some(NoticeAction::Ask { request_id: id.into(), questions, expires_ms: later, countdown: false }),
        ..AgentNotice::line(id, "Questions", NoticeKind::Info)
    };
    let ask_pick = |q: &str, labels: &[&str]| crate::agent_notice::QuestionAnswer { question_id: q.into(), labels: labels.iter().map(|l| (*l).into()).collect(), other: None };
    apply(UiUpdate::AgentNotice(ask_card("multi", vec![ask_q("q1", Some("Branches"), "Which stale branches should the worker delete?", true, vec![ask_opt("resampler-old", "merged in September"), ask_opt("resampler-v2", "merged in October"), ask_opt("meter-fix", "two commits not in main"), ask_opt("stash-2024", "untouched since March")])])));
    apply(UiUpdate::AskSet { id: "multi".into(), answer: ask_pick("q1", &["resampler-old", "resampler-v2"]) });
    advance(1.5);
    picture(run, "island-question-multi");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(ask_card("tabs", vec![
        ask_q("q1", Some("Branches"), "Which stale branches should the worker delete?", false, vec![ask_opt("resampler-old", "merged in September"), ask_opt("resampler-v2", "merged in October")]),
        ask_q("q2", Some("Push"), "Push when?", false, vec![ask_opt("Yes", "push now"), ask_opt("Tomorrow", "after the call"), ask_opt("Never", "keep local")]),
        ask_q("q3", Some("Voice"), "Which voice reads it?", false, vec![ask_opt("Dave", ""), ask_opt("Default", ""), ask_opt("None", "")]),
    ])));
    apply(UiUpdate::AskSet { id: "tabs".into(), answer: ask_pick("q1", &["resampler-v2"]) });
    apply(UiUpdate::AskSet { id: "tabs".into(), answer: ask_pick("q2", &["Yes"]) });
    advance(1.5);
    picture(run, "island-question-tabs");
    apply(UiUpdate::ClearAgentNotices);
    // Bug note 2026-11-02T16-27-21: two questions, the second ticked, the first still open. Send after the chips is lit;
    // the open question's chip carries the accent dot. Then the same card before anything is picked: Send dim.
    let partial = || ask_card("partial", vec![
        ask_q("q1", Some("IDOL"), "Keep the IDOL models in the catalogue?", false, vec![ask_opt("Keep them", ""), ask_opt("Drop them", "")]),
        ask_q("q2", Some("Meta models"), "What should happen to the Meta models?", true, vec![ask_opt("Bring them back", "all four sizes"), ask_opt("Keep them hidden", ""), ask_opt("Only the small one", "")]),
    ]);
    apply(UiUpdate::AgentNotice(partial()));
    advance(1.5);
    picture(run, "island-question-partial-none");
    apply(UiUpdate::AskSet { id: "partial".into(), answer: ask_pick("q2", &["Bring them back"]) });
    super::island::apply(crate::island::object::Event::AskTab { id: "partial".into(), question: 1 });
    advance(1.5);
    picture(run, "island-question-partial");
    super::island::apply(crate::island::object::Event::AskTab { id: "partial".into(), question: 0 });
    advance(1.5);
    picture(run, "island-question-partial-open-tab");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(ask_card("typed", vec![ask_q("q1", None, "Which build should I install tonight?", false, vec![ask_opt("Main", ""), ask_opt("Island", "")])])));
    advance(1.0);
    super::island::apply(crate::island::object::Event::AskOther { id: "typed".into(), question: 0, text: "keep the old one".into() });
    super::island::host::with_host(|h| h.fields.type_for_check("typed", "keep the old one"));
    advance(1.0);
    picture(run, "island-question-other-typed");
    apply(UiUpdate::ClearAgentNotices);
    // The in-conversation suggester's card (crates/app/src/suggester.rs, 24 Oct): the same ask card with the topic as its
    // line; each question carries "Not now" and Other stays open. Three states: one question, three tabs, the longest
    // the app lets through (160 characters of question, options of 40).
    let suggestion = |id: &str, topic: &str, questions: Vec<crate::agent_notice::AskQuestion>| AgentNotice {
        action: Some(NoticeAction::Ask { request_id: id.into(), questions, expires_ms: later, countdown: false }),
        ..AgentNotice::line(id, topic, NoticeKind::Info)
    };
    apply(UiUpdate::AgentNotice(suggestion("suggest-one", "CAD file from the old laptop", vec![ask_q("q1", None, "Should I look for the CAD file from your old laptop?", false, vec![ask_opt("Find the file", ""), ask_opt("Investigate it", ""), ask_opt("Not now", "")])])));
    advance(1.0);
    picture(run, "island-suggestion-one");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::AgentNotice(suggestion("suggest-three", "Sam's drawings and the old laptop", vec![
        ask_q("q1", Some("Find"), "Which drawings should I look for?", false, vec![ask_opt("CAD files", ""), ask_opt("Photos", ""), ask_opt("Not now", "")]),
        ask_q("q2", Some("Look"), "Look up what the archive says about Sam?", false, vec![ask_opt("Look up Sam", ""), ask_opt("Not now", "")]),
        ask_q("q3", Some("Next"), "Investigate the laptop's backups too?", false, vec![ask_opt("Investigate it", ""), ask_opt("Not now", "")]),
    ])));
    advance(1.0);
    picture(run, "island-suggestion-three");
    apply(UiUpdate::ClearAgentNotices);
    let long_q: &str = "Should I look for the drawings from the laptop that Sam mentioned last week, check the backups too, and find the photos and videos from that holiday trip?";
    apply(UiUpdate::AgentNotice(suggestion("suggest-long", "Sam's drawings from the laptop", vec![ask_q("q1", None, &long_q, false, vec![
        ask_opt("Find the drawings from last week's talk", ""), ask_opt("Investigate the backups as well", ""), ask_opt("Look up what Sam said about it", ""), ask_opt("Not now", ""),
    ])])));
    advance(1.0);
    picture(run, "island-suggestion-long");
    apply(UiUpdate::ClearAgentNotices);
    apply(UiUpdate::Chat(crate::island::object::gallery::chat_day()));
    advance(1.0);
    super::island::apply(crate::island::object::Event::Click(Target::Token(crate::island::object::Token::Reply)));
    advance(1.5);
    let composer_ok = super::island::checks::snapshot(&run.dir.join("island-composer.png"));
    run.check("island-composer: rendered", composer_ok);
    // Pull the conversation up again: the live question checks below start from the notch at rest.
    super::island::apply(crate::island::object::Event::Click(Target::ReplyBand));
    apply(UiUpdate::Chat(vec![]));
    apply(UiUpdate::ClearAgentNotices);
    advance(2.0);
    advance(1.0);
    run.take();

    // The live path: one question at a time, oldest first, answered through taps on the click target.
    apply(UiUpdate::AgentNotice(short.clone()));
    apply(UiUpdate::AgentNotice(four.clone()));
    apply(UiUpdate::AgentNotice(short.clone()));
    advance(1.0);
    run.check("island: one question at a time, the oldest first", matches!(scene(), Scene::Question { ref id, .. } if id == "q1"));
    let notices_in_window = with_state_now(|s| s.agent_view.rows.iter().filter(|r| matches!(r, agent_view::Row::Notice { id, .. } if id == "q1" || id == "q2")).count()).unwrap();
    run.check("Agent window keeps each question's full text once", notices_in_window == 2);
    run.check("island: No on the first question takes a tap", tap_island(&answer("q1", 1)));
    let a = notice_actions(run.take());
    run.check("island: No answers its own request id", a == vec![UiAction::AgentNotice(AgentNoticeAction::Answer { request_id: "q1".into(), answers: one("q1", "No"), by_hand: vec![] })]);
    // No chosen-mark stage ("animations should be simple"): the answered question leaves at once.
    run.check("island: the answered question leaves at once", !matches!(scene(), Scene::Question { ref id, .. } if id == "q1"));
    run.check("island: an answered question takes no second answer", !tap_island(&answer("q1", 0)) && notice_actions(run.take()).is_empty());
    advance(3.0);
    let four_answers = matches!(scene(), Scene::Question { ref id, ref card, .. } if id == "q2" && card.questions[0].options.len() == 4);
    run.check("island: the next question follows with its own four answers", four_answers);
    run.check("island: the third answer takes a tap", tap_island(&answer("q2", 2)));
    let taken = notice_actions(run.take());
    run.check(
        &format!("island: the answer resumes the agent with its label and request id ({taken:?})"),
        taken == vec![UiAction::AgentNotice(AgentNoticeAction::Answer { request_id: "q2".into(), answers: one("q1", "Reader"), by_hand: vec![] })],
    );
    advance(1.0);
    let sc = scene();
    run.check(&format!("island: answered questions retract by themselves ({sc:?})"), matches!(sc, Scene::Rest));

    // Dismissal by the agent removes only that request.
    apply(UiUpdate::AgentNotice(question("d1", "First?", later)));
    apply(UiUpdate::AgentNotice(question("d2", "Second?", later)));
    apply(UiUpdate::DismissAgentNotice("d1".into()));
    advance(1.0);
    let sc = scene();
    run.check(&format!("island: DismissAgentNotice removes only that request ({sc:?})"), matches!(sc, Scene::Question { ref id, .. } if id == "d2"));
    apply(UiUpdate::ClearAgentNotices);
    advance(1.0);
    run.check("island: ClearAgentNotices removes every question", matches!(scene(), Scene::Rest));

    // Expired: no answers, one Dismiss; it leaves by itself.
    apply(UiUpdate::AgentNotice(question("x1", "Expired already?", super::now_ms() - 10)));
    advance(1.0);
    run.check("island: an expired question has no answers, one Dismiss", matches!(scene(), Scene::Question { ref card, .. } if card.look == QLook::Expired) && !tap_island(&answer("x1", 0)));
    run.check("island: Dismiss takes a tap", tap_island(&Target::Dismiss("x1".into())));
    advance(1.0);
    let (sc, left) = (scene(), notice_actions(run.take()));
    run.check(&format!("island: dismissed, the question is gone ({sc:?}, {left:?})"), matches!(sc, Scene::Rest) && left.is_empty());
    apply(UiUpdate::AgentNotice(question("x2", "Expired too?", super::now_ms() - 10)));
    advance(31.0);
    run.check("island: an expired question leaves by itself", matches!(scene(), Scene::Rest));

    // Closed by voice: it leaves at once (Heimdall's voice confirms the answer), nothing takes a click.
    apply(UiUpdate::AgentNotice(question("v1", "Install it now?", later)));
    advance(0.5);
    apply(UiUpdate::CloseQuestion { id: "v1".into(), answer: "yes".into() });
    run.check("island: ask_close retracts the question at once", !matches!(scene(), Scene::Question { .. }));
    run.check("island: a closed question takes no click", !tap_island(&answer("v1", 0)) && notice_actions(run.take()).is_empty());
    advance(1.0);
    run.check("island: a closed question leaves after its hold", matches!(scene(), Scene::Rest));

    // Connect T3.
    apply(UiUpdate::AgentNotice(connect("t3-connect-401:key")));
    advance(1.0);
    run.check("island: Connect T3 takes a tap", tap_island(&Target::Connect("t3-connect-401:key".into())));
    let emitted = notice_actions(run.take());
    run.check(&format!("island: Connect T3 emits ConnectT3 once ({emitted:?})"), emitted == vec![UiAction::AgentNotice(AgentNoticeAction::ConnectT3)]);
    advance(0.5);
    run.check("island: while it connects, Connect is not offered", !tap_island(&Target::Connect("t3-connect-401:key".into())));
    apply(UiUpdate::Health(crate::island::HealthCue::Notice { part: Part::Agent, label: agent_notice::T3_CONNECT_FAILED.into() }));
    advance(1.0);
    run.check("island: a failed connection offers Connect again", super::island::host::with_host(|h| h.island.targets().iter().any(|(t, _)| matches!(t, Target::Connect(_)))).unwrap_or(false));
    apply(UiUpdate::Health(crate::island::HealthCue::Notice { part: Part::Agent, label: agent_notice::T3_CONNECTED.into() }));
    advance(4.0);
    run.check("island: T3 connected removes the notice", !super::island::host::with_host(|h| h.island.targets().iter().any(|(t, _)| matches!(t, Target::Connect(_)))).unwrap_or(true));
}

fn failure(id: &str, text: &str, at: (u64, u64)) -> AgentLiveEntry {
    AgentLiveEntry {
        id: id.into(),
        session_entry_id: None,
        timestamp_ms: agent_view::demo_ms(14, at.0, at.1),
        kind: AgentLiveKind::Failure,
        text: text.into(),
        tool_name: None,
        attachments: vec![],
        finished: true,
        editable: false,
        error: true,
        sent: false,
        queued: false,
        owner_text: None,
        usage: None,
    }
}

fn button(f: impl FnOnce(&super::agent::AgentWindow) -> Retained<NSButton>) -> Retained<NSButton> {
    with_state_now(|s| f(s.agent_window.as_ref().unwrap())).unwrap()
}

/// An accessibility press: the button's own target/action, refused when it is disabled or hidden.
fn press(button: &NSButton) -> bool {
    if button.isHidden() {
        return false;
    }
    // The result says whether the press reached an enabled control; ordered-out windows report NO either way, so
    // the checks read what the press emitted.
    let enabled = button.isEnabled();
    let _: Bool = unsafe { msg_send![button, accessibilityPerformPress] };
    pump(0.05);
    enabled
}

/// Hit-testing the window at the button's centre finds that button.
fn hit(button: &NSButton) -> bool {
    let Some(window) = button.window() else { return false };
    let Some(frame_view) = window.contentView().and_then(|c| unsafe { c.superview() }) else { return false };
    let b = button.bounds();
    let center = button.convertPoint_toView(NSPoint::new(b.size.width / 2.0, b.size.height / 2.0), None);
    let Some(found) = frame_view.hitTest(center) else { return false };
    let target: &NSView = button;
    std::ptr::eq(&*found, target) || found.isDescendantOf(target)
}

fn type_text(text: &str) {
    with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        // The input system's own call, as a key press delivers typed text (textDidChange follows).
        let end = NSRange::new(w.composer.string().length(), 0);
        w.composer.setSelectedRange(end);
        unsafe { objc2_app_kit::NSTextInputClient::insertText_replacementRange(&*w.composer, &*ns(text) as &AnyObject, NSRange::new(usize::MAX >> 1, 0)) };
    });
    pump(0.05);
}

fn set_text(text: &str) {
    with_state_now(|s| s.agent_window.as_ref().unwrap().set_composer_text(text));
}

fn composer_text() -> String {
    with_state_now(|s| s.agent_window.as_ref().unwrap().composer_text()).unwrap_or_default()
}

/// The first Edit link in the transcript: its character index and value.
fn edit_link(text: &objc2_app_kit::NSTextView) -> Option<(usize, String)> {
    let storage = unsafe { text.textStorage() }?;
    let len = storage.length();
    let mut i = 0;
    while i < len {
        let mut range = NSRange::new(0, 0);
        let value = unsafe { storage.attribute_atIndex_effectiveRange(objc2_app_kit::NSLinkAttributeName, i, &mut range) };
        if let Some(v) = value.and_then(|v| v.downcast::<NSString>().ok()) {
            let v = v.to_string();
            if v.starts_with("heimdall-edit:") {
                return Some((i, v));
            }
        }
        i = (range.location + range.length).max(i + 1);
    }
    None
}

fn storage_has_attachment(text: &objc2_app_kit::NSTextView) -> bool {
    let Some(storage) = (unsafe { text.textStorage() }) else { return false };
    storage.string().to_string().contains('\u{FFFC}')
}

/// The whole window (title bar and content) drawn through `cacheDisplayInRect` while it stays ordered out.
pub(crate) fn capture_window(window: &NSWindow, path: &Path) -> bool {
    let Some(view) = window.contentView().and_then(|c| unsafe { c.superview() }) else { return false };
    view.layoutSubtreeIfNeeded();
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else { return false };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // Over the window's own background: an ordered-out window's frame view leaves its fill transparent.
    let scale = rep.pixelsWide() as f64 / bounds.size.width;
    let Some(out) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), rep.pixelsWide(), rep.pixelsHigh(), 8, 4, true, false,
            objc2_app_kit::NSDeviceRGBColorSpace, 0, 0,
        )
    }) else {
        return false;
    };
    let Some(context) = NSGraphicsContext::graphicsContextWithBitmapImageRep(&out) else { return false };
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let full = rect(0.0, 0.0, rep.pixelsWide() as f64, rep.pixelsHigh() as f64);
    NSColor::colorWithSRGBRed_green_blue_alpha(30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 1.0).setFill();
    objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(full, 10.0 * scale, 10.0 * scale).fill();
    let image = objc2_app_kit::NSImage::initWithSize(objc2_app_kit::NSImage::alloc(), full.size);
    image.addRepresentation(&rep);
    image.drawInRect(full);
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    let Some(data) = (unsafe { out.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }) else { return false };
    let ok = data.writeToFile_atomically(&ns(&path.to_string_lossy()), true);
    eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
    ok
}

/// A stand-in screenshot (PNG bytes): a dark app window with a level meter that drops, for the attachment.
fn demo_screenshot() -> Vec<u8> {
    let (w, h) = (1200isize, 750isize);
    let Some(rep) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), w, h, 8, 4, true, false, objc2_app_kit::NSDeviceRGBColorSpace, 0, 0,
        )
    }) else {
        return vec![];
    };
    let Some(context) = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep) else { return vec![] };
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let fill = |r: f64, g: f64, b: f64, x: f64, y: f64, ww: f64, hh: f64| {
        NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0).setFill();
        objc2_app_kit::NSRectFill(rect(x, y, ww, hh));
    };
    fill(0.12, 0.13, 0.15, 0.0, 0.0, w as f64, h as f64);
    fill(0.18, 0.19, 0.22, 0.0, h as f64 - 44.0, w as f64, 44.0);
    fill(0.16, 0.17, 0.20, 40.0, 60.0, 340.0, 590.0);
    for i in 0..48 {
        let level = if (20..30).contains(&i) { 0.25 } else { 0.75 + 0.15 * ((i as f64) * 0.7).sin() };
        let (r, g) = if level > 0.5 { (0.30, 0.78) } else { (0.95, 0.55) };
        fill(r, g, 0.35, 440.0 + i as f64 * 15.0, 120.0, 10.0, 420.0 * level);
    }
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }.map(|d| d.to_vec()).unwrap_or_default()
}

/// A reply streaming into a full window (the kept 3000 entries): each update's time on the main thread, the view
/// following the newest text while the user is at the bottom, and standing still while he reads further up.
fn streaming(run: &mut Run) {
    let entry = |id: String, kind: AgentLiveKind, text: String, finished: bool| AgentLiveEntry {
        id,
        session_entry_id: None,
        timestamp_ms: 1_760_450_000_000,
        kind,
        text,
        tool_name: None,
        attachments: vec![],
        finished,
        editable: false,
        error: false,
        sent: false,
        queued: false,
        owner_text: None,
        usage: None,
    };
    let sentence = "The capture clock and the transcript agree on this span, so the audio range covers exactly what was said. ";
    let history: Vec<AgentLiveEntry> = (0..agent_view::KEEP)
        .map(|i| entry(format!("h{i}"), if i % 3 == 0 { AgentLiveKind::Prompt } else { AgentLiveKind::Reply }, sentence.repeat(1 + i % 4), true))
        .collect();
    state(demo_state(AgentRunState::Working));
    apply(UiUpdate::AgentWindow(AgentWindowUpdate::History(history)));
    pump(0.1);
    let at_bottom = || with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        let v = w.scroll.contentView().bounds();
        (v.origin.y, v.origin.y + v.size.height >= w.text.frame().size.height - 2.0)
    }).unwrap();
    let mut times = vec![];
    let mut text = String::new();
    for k in 0..120 {
        text.push_str(&format!("word{k} "));
        let started = std::time::Instant::now();
        apply(UiUpdate::AgentWindow(AgentWindowUpdate::Entries(vec![entry("streaming".into(), AgentLiveKind::Reply, text.clone(), false)])));
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let following = at_bottom().1;
    // The user scrolls up to read; the stream keeps coming.
    with_state_now(|s| {
        let w = s.agent_window.as_ref().unwrap();
        let clip = w.scroll.contentView();
        clip.scrollToPoint(objc2_foundation::NSPoint::new(0.0, 2000.0));
        w.scroll.reflectScrolledClipView(&clip);
    });
    let before = at_bottom().0;
    for k in 0..30 {
        text.push_str(&format!("more{k} "));
        apply(UiUpdate::AgentWindow(AgentWindowUpdate::Entries(vec![entry("streaming".into(), AgentLiveKind::Reply, text.clone(), false)])));
    }
    let still = at_bottom().0;
    times.sort_by(f64::total_cmp);
    let p = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    eprintln!("streaming into {} entries: update ms p50 {:.2} p95 {:.2} max {:.2}", agent_view::KEEP, p(0.5), p(0.95), p(1.0));
    run.check("streaming: the view follows the newest text while at the bottom", following);
    run.check("streaming: the view stays where User scrolled to read", (still - before).abs() < 0.5);
    run.check("streaming: an update into a full window fits in a 60 Hz frame (p95 under 8 ms)", p(0.95) < 8.0);
    state(demo_state(AgentRunState::Ready));
}
