//! Drawing mode and pointing, the app's side (the UI is `ui::drawing`).
//!
//! - Drawing mode's Send captures through Heimdall's own Screen Recording path (`install_grabber`; no new
//!   permission) and the marked PNG comes back as `UiAction::DrawingSent`, which the pipeline attaches to the user's
//!   pending utterance (`Pipeline::attach_marked`): it never wakes the agent, the decider is never told, and the image
//!   goes with the turn only when the decider sends it ("decider doesn't need to know … when he decides
//!   to send it to the agent, then only then the images are actually attached").
//! - While the overlay is up, a take-a-look screenshot of that display gets his current strokes (`mark_shots`); the
//!   overlay itself is excluded from every capture by title.
//! - The agent's tools: `point` (display-only marks), `look` (bring an app forward, return the screen as an image)
//!   and `drawing_mode` (open or close the overlay). No voice command opens it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::bridge::Tools;

/// Main-thread work for a tool call: the UI's queue, with a bound so a stopped UI never hangs the agent.
fn on_main<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    ui::Ui::handle().on_main(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(3)).context("UI_UNAVAILABLE: the UI did not answer; try again")
}

/// How drawing mode's Send captures a display: Heimdall's native full-resolution grab (its windows excluded). Seeing
/// off never captures. Errors are short: the island shows them.
pub(crate) fn install_grabber(tools: Arc<Tools>) {
    let seeing = tools.clone();
    ui::drawing::set_grabber(move |display| {
        if tools.screen_off.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Seeing is off".into());
        }
        let grab = grabber(&tools).ok_or_else(|| "Screenshot unavailable".to_string())?;
        match grab(display) {
            Ok(frame) => Ok((frame.width, frame.height, frame.rgb.to_vec())),
            Err(e) => {
                eprintln!("Drawing mode: capture failed: {e:#}");
                Err("Screenshot unavailable".into())
            }
        }
    });
    // The still drawing mode holds under its overlay while a menu is open; Seeing off never captures, and a still
    // taken before Seeing went off is neither held nor sent.
    let still = seeing.clone();
    ui::drawing::set_still_grabber(move |display| {
        if still.screen_off.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Seeing is off".into());
        }
        screen::grab_still(display).map(|s| (s.width, s.height, s.bgra)).map_err(|e| format!("{e:#}"))
    });
    ui::drawing::set_seeing(move || !seeing.screen_off.load(std::sync::atomic::Ordering::Acquire));
}

fn grabber(tools: &Tools) -> Option<screen::Grabber> {
    #[cfg(test)]
    if let Some(g) = tools.test_grabber.lock().unwrap().clone() {
        return Some(g);
    }
    let _ = tools;
    screen::native_grabber()
}

/// A take-a-look screenshot while drawing mode is up: shots of the overlay's display get the user's current strokes,
/// rewritten in place before they are archived and attached. Other displays are untouched.
pub(crate) fn mark_shots(action: &screen::Action) {
    let Some(marks) = ui::drawing::current_marks() else { return };
    for shot in action.shots.iter().filter(|s| s.display_id == marks.display) {
        let marked = std::fs::read(&shot.path).map_err(|e| e.to_string()).and_then(|png| ui::drawing::mark_png(&png, &marks));
        match marked {
            Ok(png) => {
                if let Err(e) = std::fs::write(&shot.path, png) {
                    eprintln!("Drawing mode: marked screenshot not saved: {e}");
                }
            }
            Err(e) => eprintln!("Drawing mode: strokes not composited into the screenshot: {e}"),
        }
    }
}

fn journal(tools: &Tools) -> Option<heimdall_archive::Journal> {
    let session = tools.archive_session.lock().unwrap().clone()?;
    heimdall_archive::Journal::open(&session).ok()
}

/// `point`: the marks on the screen, one line back (or the first mistake, for the model to fix).
pub(crate) fn point(tools: &Tools, args: &Value, permit: &agent::delivery::DeliveryPermit) -> Result<String> {
    permit.check()?;
    let spec = args["spec"].as_str().context("spec required")?.to_owned();
    let shown = spec.clone();
    let result = on_main(move || ui::drawing::point(&shown))?;
    if let Some(j) = journal(tools) {
        let _ = j.event("agent_point", "agent", &[], capture::host_now_ns(), json!({"spec":spec,"result":result.as_ref().map_or_else(|e| e.clone(), |l| l.clone()),"ok":result.is_ok()}));
    }
    result.map_err(|e| anyhow::anyhow!("INVALID_ARGUMENT: {e}"))
}

/// `drawing_mode`: opens or closes the user's drawing overlay.
pub(crate) fn drawing_mode(tools: &Tools, args: &Value, permit: &agent::delivery::DeliveryPermit) -> Result<String> {
    permit.check()?;
    let open = args["open"].as_bool().context("open required")?;
    let line = if open {
        on_main(ui::drawing::open)?.map_err(|e| anyhow::anyhow!("DRAWING_UNAVAILABLE: {e}"))?
    } else {
        match on_main(ui::drawing::close)? {
            Ok(true) => "Drawing mode closed; the strokes were discarded.".into(),
            Ok(false) => "Drawing mode was not open.".into(),
            Err(e) => anyhow::bail!("DRAWING_UNAVAILABLE: {e}"),
        }
    };
    if let Some(j) = journal(tools) {
        let _ = j.event("agent_drawing_mode", "agent", &[], capture::host_now_ns(), json!({"open":open,"result":line}));
    }
    Ok(line)
}

/// `look`: brings `app` forward (waiting for macOS's Space change, not a fixed delay), captures the display under the
/// pointer and returns it at its point size (a quarter of a Retina capture's pixels: what the agent reads) as image
/// content, with the `size` line `point` needs. The user's strokes are on it while drawing mode is up.
pub(crate) async fn look(tools: &Tools, app: Option<&str>, permit: &agent::delivery::DeliveryPermit) -> Result<Value> {
    permit.check()?;
    if tools.screen_off.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(json!({"content":[{"type":"text","text":crate::bridge::SCREEN_OFF}]}));
    }
    let grab = grabber(tools).context("CAPTURE_UNAVAILABLE: screen capture is unavailable")?;
    let app = app.map(str::to_owned);
    let work = permit.clone();
    let (png, w, h, index, switched) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, u32, u32, usize, Option<Duration>)> {
        let switched = match &app {
            Some(app) => Some(ui::drawing::bring_forward(app).map_err(|e| anyhow::anyhow!("APP_UNAVAILABLE: {e}"))?),
            None => None,
        };
        work.check()?;
        let display = ui::drawing::pointer_display().context("no display")?;
        let frame = grab(display.id)?;
        let mut rgb = frame.rgb.to_vec();
        if let Some(marks) = ui::drawing::current_marks().filter(|m| m.display == display.id) {
            rgb = ui::drawing::composite_rgb(&rgb, frame.width, frame.height, marks.view, &marks.strokes).map_err(anyhow::Error::msg)?;
        }
        let (w, h) = (display.width.round().max(1.0) as u32, display.height.round().max(1.0) as u32);
        let small = ui::drawing::resize_rgb(&rgb, frame.width, frame.height, (w, h)).map_err(anyhow::Error::msg)?;
        let png = ui::drawing::encode_png(w, h, &small).map_err(anyhow::Error::msg)?;
        Ok((png, w, h, display.index, switched))
    })
    .await??;
    permit.check()?;
    if let Some(j) = journal(tools) {
        if let Ok(id) = j.image(&png, "image/png") {
            let _ = j.event("agent_look", "agent", &[], capture::host_now_ns(), json!({"image_id":id,"display":index,"width":w,"height":h,"switched_ms":switched.map(|d| d.as_millis() as u64)}));
        }
    }
    let block = crate::bridge::context_attachment(png)?.block()?;
    let mut line = format!("Display {index}: size {w} {h}");
    if let Some(d) = switched {
        line.push_str(&format!(" (switched in {} ms)", d.as_millis()));
    }
    Ok(look_result(&block, &line))
}
/// The `look` result: the screenshot as the agent receives it, labelled with the type its bytes are. 20 Oct 19:20:
/// the label was always PNG while a busy screen (the YouTube home page) went as JPEG, and the provider refused every
/// request after it.
pub(crate) fn look_result(block: &Value, line: &str) -> Value {
    json!({"content":[
        {"type":"image","data":block["source"]["data"],"mimeType":block["source"]["media_type"]},
        {"type":"text","text":line}
    ]})
}

impl crate::pipeline::Pipeline {
    /// Drawing mode's Send: archived, then attached to the user's pending utterance. Never a wake, never decider
    /// context; the next dispatch through the current conversation takes it (`dispatch_pending`).
    pub(crate) fn attach_marked(&mut self, shot: ui::drawing::MarkedShot) -> Result<()> {
        let kind = ui::CommandKind::Screenshot;
        if !self.attachment_room() {
            self.island(ui::CommandCue::Failed { id: None, kind, reason: "Too many screenshots waiting".into() });
            return Ok(());
        }
        let id = self.evidence().image(&shot.png, "image/png")?;
        self.evidence().event(
            "marked_screenshot",
            "drawing",
            &[],
            capture::host_now_ns(),
            json!({"image_id":id,"display_id":shot.display,"width":shot.width,"height":shot.height,"strokes":shot.strokes,"attached":"pending_utterance"}),
        )?;
        self.attach_pending(crate::bridge::context_attachment(shot.png)?);
        Ok(())
    }
}
