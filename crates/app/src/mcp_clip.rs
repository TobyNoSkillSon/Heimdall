//! `archive_clip`: one time window of the archive as a movie (`heimdall_archive::clip`), read-only beside the live
//! capture. Seeing off refuses it like `screen_frames`, without naming the switch.
//!
//! The installed app runs the bundled `heimdall-clip` beside its own executable as a child process: an
//! AVFoundation exception or a large window's memory stays out of Heimdall, and Agent off kills it. A development
//! build without the helper builds in process.
use agent::{
    delivery::DeliveryPermit,
    mcp_contract::{utc, wall_ns},
};
use anyhow::{bail, ensure, Context, Result};
use heimdall_archive::clip;
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

/// The MCP connection closes after 75 s; a longer build keeps going and its file appears when complete.
const ANSWER_WITHIN: Duration = Duration::from_secs(55);
/// Builds at once; each holds the archive's shared lock and a hardware encoder.
const MOST_AT_ONCE: usize = 2;
static RUNNING: AtomicUsize = AtomicUsize::new(0);
struct Running;
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
    }
}

pub async fn clip(tools: &crate::bridge::Tools, args: Value, permit: &DeliveryPermit) -> Result<String> {
    permit.check()?;
    ensure!(!tools.screen_off.load(Ordering::Acquire), "{}", crate::mcp_frames::SCREEN_FRAMES_UNAVAILABLE);
    let session = tools.archive_session.lock().unwrap().clone().context("ARCHIVE_UNAVAILABLE: the archive is not attached")?;
    let root = crate::archive::store_root(&session);
    let data = root.parent().map(PathBuf::from).context("ARCHIVE_UNAVAILABLE: archive folder")?;
    let mut options = clip::Options::new(wall_ns(args["start"].as_str().unwrap())?, wall_ns(args["end"].as_str().unwrap())?);
    options.display = args["display_id"].as_str().map(str::parse).transpose()?;
    options.width = args["width"].as_u64().map(|w| w as u32);
    options.fps = args["fps"].as_u64().map(|f| f as u32);
    let out = match args["path"].as_str() {
        Some(p) => PathBuf::from(p),
        None => clip::default_out(&data, options.from_ns, options.to_ns)?,
    };
    ensure!(!clip::taken(&out), "PATH_EXISTS: {} or its .srt exists or is being written; pass another path", out.display());
    ensure!(!out.starts_with(&root), "path: write the clip outside the archive");
    if RUNNING.fetch_add(1, Ordering::AcqRel) >= MOST_AT_ONCE {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
        bail!("CLIP_BUSY: {MOST_AT_ONCE} clips are being written; try again when one is done");
    }
    let running = Running;
    let helper = std::env::current_exe().ok().map(|e| e.with_file_name("heimdall-clip")).filter(|p| p.is_file());
    let shown = out.display().to_string();
    let permit = permit.clone();
    let job = tokio::task::spawn_blocking(move || {
        let _running = running;
        match helper {
            Some(exe) => child(&exe, &root, &options, &out, &permit),
            None => {
                let mut windows = 0u64;
                clip::build(&root, &options, &out, &mut || {
                    windows += 1;
                    permit.check().is_ok() && (windows % 10 != 0 || !clip::seeing_off(&data))
                })
                .map(|r| r.text())
            }
        }
    });
    match tokio::time::timeout(ANSWER_WITHIN, job).await {
        Ok(done) => done?.map_err(|e| anyhow::anyhow!("CLIP_FAILED: {e:#}")),
        Err(_) => Ok(format!(
            "Still writing {shown}: a long window. The file appears there only when complete; check for it in a minute or two. For windows this long, a task can run heimdall-clip itself."
        )),
    }
}

/// Runs the bundled CLI, killed when the agent's permit ends (Agent off, server stop). It stops itself on Seeing off.
fn child(exe: &Path, root: &Path, options: &clip::Options, out: &Path, permit: &DeliveryPermit) -> Result<String> {
    let mut command = Command::new(exe);
    command.args(["--from", &utc(options.from_ns), "--to", &utc(options.to_ns), "--json", "--archive"]).arg(root).arg("--out").arg(out);
    for (flag, value) in [("--display", options.display), ("--width", options.width), ("--fps", options.fps)] {
        if let Some(v) = value {
            command.args([flag, &v.to_string()]);
        }
    }
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("heimdall-clip did not start")?;
    let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let reading = std::thread::spawn(move || {
        let (mut o, mut e) = (String::new(), String::new());
        let _ = stdout.read_to_string(&mut o);
        let _ = stderr.read_to_string(&mut e);
        (o, e)
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if permit.check().is_err() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("clip cancelled");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let (stdout, stderr) = reading.join().map_err(|_| anyhow::anyhow!("heimdall-clip output"))?;
    if !status.success() {
        let message = stderr.trim().trim_start_matches("heimdall-clip: ");
        bail!("{}", if message.is_empty() { format!("heimdall-clip exited with {status}") } else { message.to_owned() });
    }
    let report: clip::Report = serde_json::from_str(&stdout).context("heimdall-clip report")?;
    Ok(report.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seeing_off_refuses_archived_footage_without_naming_the_switch() {
        let t = tempfile::tempdir().unwrap();
        let tools = crate::bridge::Tools::new(t.path(), t.path());
        tools.screen_off.store(true, Ordering::Release);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let args = serde_json::json!({"start":"2026-10-23T22:40:00Z","end":"2026-10-23T22:47:00Z"});
        let error = rt.block_on(clip(&tools, args, &tools.agent_enabled.permit().unwrap())).unwrap_err();
        assert_eq!(error.to_string(), crate::mcp_frames::SCREEN_FRAMES_UNAVAILABLE);
        assert_eq!(clip::SEEING_OFF, crate::mcp_frames::SCREEN_FRAMES_UNAVAILABLE, "the CLI refuses with the same words");
    }
}
