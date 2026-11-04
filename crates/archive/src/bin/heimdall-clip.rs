//! `heimdall-clip --from "2026-10-24 00:40" --to "00:47" [--out clip.mp4]`: one window of Heimdall's archive as a
//! movie (screen, one audio track per source, subtitles). Reads the archive read-only, safely beside the running app.
use anyhow::{bail, ensure, Context, Result};
use heimdall_archive::clip;
use std::path::PathBuf;

const USAGE: &str = "usage: heimdall-clip --from TIME --to TIME [--out FILE.mp4|.mov] [--display N] [--width PX] [--fps N]
                     [--hevc] [--no-subtitles] [--include-silent] [--force] [--json] [--archive DIR]
TIME: \"2026-10-24 00:40\", \"00:47\" (the --from day; a clock --from later than now is yesterday),
      \"2026-10-24T00:40:00+02:00\", now, -10m, +90s (after --from).
Default output: clips/heimdall-DATE-HHMM-HHMM.mp4 in Heimdall's data folder (<home>/.heimdall).
Each archived audio source gets its own track; the transcript is a soft subtitle track and FILE.srt.";

fn main() {
    if let Err(e) = run() {
        eprintln!("heimdall-clip: {e:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut from, mut to, mut out, mut archive) = (None, None, None, None);
    let mut options = clip::Options::new(0, 1);
    let (mut force, mut json) = (false, false);
    while let Some(a) = args.next() {
        let mut value = || args.next().with_context(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--from" => from = Some(value()?),
            "--to" => to = Some(value()?),
            "--out" | "-o" => out = Some(PathBuf::from(value()?)),
            "--display" => options.display = Some(value()?.parse().context("--display: a display number")?),
            "--width" => options.width = Some(value()?.parse().context("--width: pixels")?),
            "--fps" => options.fps = Some(value()?.parse().context("--fps: frames per second")?),
            "--archive" => archive = Some(PathBuf::from(value()?)),
            "--reference-dir" => options.reference_dir = Some(PathBuf::from(value()?)),
            "--hevc" => options.codec = clip::VideoCodec::Hevc,
            "--no-subtitles" => options.subtitles = false,
            "--include-silent" => options.include_silent = true,
            "--force" => force = true,
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    let (Some(from), Some(to)) = (from, to) else { bail!("--from and --to are required\n{USAGE}") };
    (options.from_ns, options.to_ns) = clip::window(&from, &to, heimdall_archive::wall_ns())?;
    let root = archive.unwrap_or_else(|| heimdall_home::data_dir().join("archive"));
    let data = root.parent().map(PathBuf::from).unwrap_or_default();
    ensure!(!clip::seeing_off(&data), "{}", clip::SEEING_OFF);
    let out = match out {
        Some(o) => o,
        None => clip::default_out(&data, options.from_ns, options.to_ns)?,
    };
    let srt = out.with_extension("srt");
    ensure!(force || !(out.exists() || srt.exists()), "{} or its .srt exists; pass --force to replace them", out.display());
    ensure!(!out.starts_with(&root), "write the clip outside the archive");
    if force {
        // The old subtitles belong to the movie being replaced; a clip without speech writes none.
        let _ = std::fs::remove_file(&srt);
    }
    // Seeing off during the build stops it too: the switch is read again every ten seconds of clip.
    let mut windows = 0u64;
    let mut seeing_off = false;
    let report = clip::build(&root, &options, &out, &mut || {
        windows += 1;
        if windows % 10 == 0 {
            seeing_off = clip::seeing_off(&data);
        }
        !seeing_off
    });
    ensure!(!seeing_off, "{}", clip::SEEING_OFF);
    let report = report?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.text());
    }
    Ok(())
}
