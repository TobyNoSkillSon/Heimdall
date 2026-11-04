//! Agent-operated voice-bank tool. Never plays audio.
//!
//! `heimdall-voicebank fill --models <supertonic-3 dir> --bank <lab archive dir> [--lines N] [--takes N] [--backend auto|cpu|metal]`
//!   generates up to N missing clips (default 3) into the bank of the archive store at <dir> and reads each back.
//! `heimdall-voicebank fill --backend drama3 --models <prepared Drama 3> --asr-probe <probe> --bank <dir> --set acks
//!   [--max-rejects N]` fills the instant-acknowledgement set (English, two takes each by default), each take
//!   checked by ASR in its own language. `--max-rejects N` gives a line up after N rejected takes and goes on (exit 3
//!   at the end when any line was given up); without it the first rejection stops the run.
//! `heimdall-voicebank stats --bank <dir>` prints clip counts.
//! `heimdall-voicebank import --from <lab bank> --into <Heimdall data> [--dry-run|--apply]` installs reply-bank and
//!   ack takes after validating every clip.
//! `heimdall-voicebank curate --from <lab bank> --into <new lab bank> --clips ID,ID,...` copies the chosen takes
//!   bit-exact into a new bank (the user's picks); the source keeps every generation.
//! It refuses Heimdall's own data directory; the app fills that bank itself when idle on AC power.
use anyhow::{bail, ensure, Context, Result};
use std::{collections::BTreeSet, path::PathBuf};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().context("usage: heimdall-voicebank fill|stats|import ...")?;
    if command == "import" { return import(args); }
    if command == "curate" { return curate(args); }
    let (mut models, mut bank, mut limit, mut takes, mut backend) =
        (None, None, 3usize, None::<usize>, "auto".to_string());
    let (mut set, mut max_rejects) = ("reply".to_string(), 0usize);
    let mut asr_probe=None;
    let mut splash_reserve = "cap".to_owned();
    while let Some(flag) = args.next() {
        let value = args.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--models" => models = Some(PathBuf::from(value)),
            "--bank" => bank = Some(PathBuf::from(value)),
            "--lines" => limit = value.parse()?,
            "--takes" => takes = Some(value.parse()?),
            "--set" => set = value,
            "--max-rejects" => max_rejects = value.parse()?,
            "--backend" => backend = value,
            "--asr-probe" => asr_probe=Some(PathBuf::from(value)),
            "--splash-reserve" => splash_reserve=value,
            _ => bail!("unknown flag {flag}"),
        }
    }
    let bank = bank.context("--bank DIR required")?;
    ensure!(["reply", "acks"].contains(&set.as_str()), "--set must be reply or acks");
    ensure!((set == "reply" && max_rejects == 0) || command == "fill", "--set and --max-rejects apply to fill");
    // The ack set gets its own takes, two for variety; the reply bank keeps its one-take default.
    let takes = takes.unwrap_or(if set == "acks" { voicebank::acks::FILL_TAKES } else { 1 });
    ensure!(["cap", "current"].contains(&splash_reserve.as_str()), "--splash-reserve must be cap or current");
    ensure!(splash_reserve == "cap" || (command == "fill" && backend == "drama3"),
        "--splash-reserve current is only supported for offline Drama 3 fills");
    voicebank::import::ensure_lab_path(&bank)?;
    let store = voicebank::Store::open(&bank)?;
    match command.as_str() {
        "stats" => println!("{}", serde_json::json!({"bank":bank,"clips":store.counts()?})),
        "fill" => {
            let models = models.context("--models DIR (the supertonic-3 root) required")?;
            let voice = if backend == "drama3" { speak::drama3::VOICE } else { speak::VOICE };
            let mut generator:Box<dyn voicebank::Generator> = if backend == "drama3" {
                Box::new(voicebank::drama3::Drama3::load_with_splash_reserve(models,asr_probe.context("--asr-probe required for Drama 3")?,&bank,&splash_reserve)?)
            } else {
            let model = match backend.as_str() {
                "cpu" => speak::Supertonic::load_with_backend(&models, speak::TtsBackend::Cpu)?,
                "metal" => speak::Supertonic::load_with_backend(&models, speak::TtsBackend::Metal)?,
                "auto" => speak::Supertonic::load(&models)?,
                other => bail!("unknown backend {other}"),
            };
                Box::new(voicebank::Supertonic { model, backend })
            };
            let (lines, languages, directions) = if set == "acks" {
                (voicebank::acks::lines(), voicebank::acks::languages(), voicebank::acks::directions())
            } else {
                (voicebank::lines::seed(), Default::default(), Default::default())
            };
            let generator_id = generator.id();
            let mut rejects = std::collections::BTreeMap::<String, usize>::new();
            let mut gave_up = vec![];
            let open = voicebank::Conditions {
                permit: true,
                on_ac: Some(true),
                user_idle_seconds: voicebank::fill::IDLE_SECONDS,
                speech_quiet_seconds: voicebank::fill::QUIET_SECONDS,
                heimdall_speaking: false,
            };
            let mut exhausted = BTreeSet::<(String, String, String)>::new();
            let mut complete = false;
            for _ in 0..limit {
                // The line this step generates (tick's own choice), to count its ASR rejections.
                let skip: BTreeSet<String> = exhausted.iter().filter(|(v, g, _)| v == voice && *g == generator_id).map(|(_, _, l)| l.clone()).collect();
                let target = voicebank::next_missing(&lines, &store.takes(voice, &generator_id)?, takes, &skip);
                match voicebank::tick_languages(open, &store, &lines, &languages, &directions, voice, generator.as_mut(), takes, &mut exhausted, &|| false) {
                    voicebank::Tick::Filled { missing, stored, synthesis_ms } => {
                        let back = store.audio(stored.clip_id)?;
                        let meta = store.describe(stored.clip_id)?;
                        println!(
                            "{}",
                            serde_json::json!({"line":missing.line.id,"take":missing.take,"clip_id":stored.clip_id,
                                "sha256":&stored.audio_sha256[..16],"seconds":back.duration_s(),"rate":back.sample_rate,
                                "synthesis_ms":synthesis_ms,"samples_match":meta["samples"].as_i64()==Some(back.samples.len() as i64),
                                "generator":meta["generator"],"settings":meta["settings"]})
                        );
                    }
                    voicebank::Tick::Duplicate { missing } => println!("{}", serde_json::json!({"line":missing.line.id,"duplicate":true})),
                    voicebank::Tick::Complete => {
                        complete = true;
                        break;
                    }
                    voicebank::Tick::Closed(r) => bail!("closed: {r}"),
                    voicebank::Tick::Failed(e) if max_rejects > 0 && e.contains("ASR rejected") && target.is_some() => {
                        let line = target.unwrap().line.id;
                        let count = rejects.entry(line.clone()).or_default();
                        *count += 1;
                        println!("{}", serde_json::json!({"line":line,"rejected":*count,"error":e}));
                        if *count >= max_rejects {
                            exhausted.insert((voice.into(), generator_id.clone(), line.clone()));
                            gave_up.push(line);
                        }
                    }
                    voicebank::Tick::Failed(e) => bail!("{e}"),
                }
            }
            println!("{}", serde_json::json!({"bank":bank,"set":set,"complete":complete && gave_up.is_empty(),
                "gave_up":gave_up,"clips":store.counts()?}));
            if !gave_up.is_empty() {
                eprintln!("gave up after {max_rejects} rejected takes each: {}", gave_up.join(", "));
                std::process::exit(3);
            }
        }
        other => bail!("unknown command {other}"),
    }
    Ok(())
}

fn curate(mut args: impl Iterator<Item=String>) -> Result<()> {
    let (mut from, mut into, mut clips) = (None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--from" => from = Some(PathBuf::from(value)),
            "--into" => into = Some(PathBuf::from(value)),
            "--clips" => clips = Some(value.split(',').map(|c| c.trim().parse::<i64>()).collect::<Result<Vec<_>, _>>()?),
            _ => bail!("unknown curate flag {flag}"),
        }
    }
    let (from, into, clips) = (from.context("--from LAB_BANK required")?, into.context("--into NEW_BANK required")?,
        clips.context("--clips ID,ID,... required")?);
    voicebank::import::ensure_lab_path(&from)?;
    voicebank::import::ensure_lab_path(&into)?;
    ensure!(!into.exists(), "{} exists; curate writes a new bank", into.display());
    let (source, target) = (voicebank::Store::open(&from)?, voicebank::Store::open(&into)?);
    let unique: BTreeSet<i64> = clips.iter().copied().collect();
    ensure!(unique.len() == clips.len(), "a clip is listed twice");
    for clip in &clips {
        source.copy_clip(*clip, &target)?;
    }
    println!("{}", serde_json::json!({"from":from,"into":into,"copied":clips.len(),"clips":target.counts()?}));
    Ok(())
}
fn import(mut args: impl Iterator<Item=String>) -> Result<()> {
    let (mut from, mut into, mut apply) = (None, None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--from" => from = Some(PathBuf::from(args.next().context("--from needs a directory")?)),
            "--into" => into = Some(PathBuf::from(args.next().context("--into needs a data directory")?)),
            "--apply" | "--dry-run" => {
                ensure!(apply.is_none(), "choose --apply or --dry-run once");
                apply = Some(flag == "--apply");
            }
            _ => bail!("unknown import flag {flag}"),
        }
    }
    let report = voicebank::import::run(&from.context("--from LAB_BANK required")?,
        &into.context("--into HEIMDALL_DATA required")?, apply.unwrap_or(false))?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
