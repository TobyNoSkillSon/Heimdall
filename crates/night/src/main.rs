use anyhow::{bail, ensure, Context, Result};
use heimdall_night::{gate, job, metrics, separator};
use listen::Recognizer;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::Instant,
};
fn path(args: &mut impl Iterator<Item = String>, name: &str) -> Result<PathBuf> {
    Ok(args
        .next()
        .with_context(|| format!("missing {name}"))?
        .into())
}
fn main() -> Result<()> {
    std::env::set_var("HEIMDALL_SPEAKERS_METAL", "0");
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
  // The night fixing round's inbox (bug_notes.rs): list what is open, resolve what was handled.
  Some("bugs")=>{
   let inbox=match std::env::var_os("HEIMDALL_BUG_INBOX") { Some(d)=>PathBuf::from(d), None=>heimdall_night::bug_notes::default_inbox().unwrap_or_else(|| heimdall_night::bug_notes::data_inbox(&heimdall_home::data_dir())) };
   let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
   match args.next().as_deref() {
    Some("list")=>{
     let notes=heimdall_night::bug_notes::list(&inbox);
     println!("{} open bug note(s) in {}",notes.len(),inbox.display());
     for n in notes { println!("{} · [{}] · {}",n["id"].as_str().unwrap_or_default(),n["cause"]["class"].as_str().unwrap_or("unclassified"),n["note"]["text"].as_str().unwrap_or_default().replace('\n'," ")); }
    }
    // The bug scout (bug_scout.rs): `scout ID` scouts one open note now, `scout --open` every open note without a cause.
    Some("scout")=>{
     let arg=args.next().context("bugs scout ID | bugs scout --open")?;
     let src=heimdall_night::bug_scout::Sources::for_inbox(&inbox);
     let bin=heimdall_night::bug_scout::claude_bin();
     let mut ask=|system:&str,prompt:&str| heimdall_night::bug_scout::ask_claude(&bin,system,prompt,src.code.as_deref());
     if arg=="--open" {
      let mut open=heimdall_night::bug_notes::list(&inbox);
      open.reverse();
      for n in open {
       let id=n["id"].as_str().unwrap_or_default();
       if !heimdall_night::bug_scout::needs_scout(&n) { continue; }
       if let Err(e)=heimdall_night::bug_scout::scout_note(&inbox,id,&src,&mut ask) { eprintln!("bug scout {id}: {e:#}"); }
      }
     } else {
      match heimdall_night::bug_scout::scout_note(&inbox,&arg,&src,&mut ask)? {
       Some(cause)=>println!("{}",serde_json::to_string_pretty(&cause)?),
       None=>println!("{}",serde_json::to_string_pretty(&heimdall_night::bug_notes::get(&inbox,&arg)?["cause"])?),
      }
     }
    }
    // The coordinator's cause for a note, by hand: `set-cause ID '<json>'` (class, reasoning, confidence, by:"coordinator").
    Some("set-cause")=>{
     let id=args.next().context("bugs set-cause ID JSON")?;let json=args.collect::<Vec<_>>().join(" ");
     let cause:Value=serde_json::from_str(&json).context("the cause is not JSON")?;
     let saved=heimdall_night::bug_notes::set_cause(&inbox,&id,&cause)?;
     println!("{}",serde_json::to_string_pretty(&saved["cause"])?);
    }
    // Decider cases for the next decider build: one dataset row per labelled decider note (bug_notes::decider_row).
    Some("export-decider-cases")=>{
     let out=path(&mut args,"output file")?;
     let rows:Vec<Value>=heimdall_night::bug_notes::decider_cases(&inbox).iter().map(heimdall_night::bug_notes::decider_row).collect();
     let mut bytes=Vec::new();
     for r in &rows { bytes.extend(serde_json::to_vec(r)?); bytes.push(b'\n'); }
     if let Some(dir)=out.parent() { std::fs::create_dir_all(dir)?; }
     heimdall_archive::atomic_write(&out,&bytes)?;
     println!("{} decider case(s) to {}",rows.len(),out.display());
    }
    Some("resolve")=>{
     let id=args.next().context("missing bug-note id")?;let done=args.collect::<Vec<_>>().join(" ");
     let out=heimdall_night::bug_notes::resolve(&inbox,&id,&done,now)?;
     println!("Resolved {id}: {}; {} still open.",out.display(),heimdall_night::bug_notes::list(&inbox).len());
    }
    // A commit that names an open note's ID fixed it: the note must be resolved before the pass ends (docs/nightly-build.md §2).
    Some("check")=>{
     let range=args.next().context("bugs check BASE..HEAD")?;
     let log=std::process::Command::new("git").args(["log","--format=%B",&range]).output().context("git log")?;
     ensure!(log.status.success(), "git log {range} failed: {}",String::from_utf8_lossy(&log.stderr).trim());
     let stale=heimdall_night::bug_notes::cited(&inbox,&String::from_utf8_lossy(&log.stdout));
     if stale.is_empty() { println!("No open bug note is named by {range}."); }
     else { bail!("{} open bug note(s) named by {range} are still in the inbox: {}; resolve each with bugs resolve",stale.len(),stale.join(", ")); }
    }
    // A note whose fix is already on main closes with that commit (docs/bugs.md, "Closing notes"). Run from a checkout.
    Some("reconcile")=>{
     let rev=args.next().unwrap_or_else(||"main".into());
     let log=std::process::Command::new("git").args(["log","--since=60 days ago","--format=%h%x1f%s%x1f%B%x1e",&rev]).output().context("git log")?;
     ensure!(log.status.success(), "git log {rev} failed: {}",String::from_utf8_lossy(&log.stderr).trim());
     let commits:Vec<(String,String,String)>=String::from_utf8_lossy(&log.stdout).split('\x1e').filter_map(|c|{
      let mut f=c.trim_start_matches('\n').splitn(3,'\x1f');
      Some((f.next()?.to_owned(),f.next()?.to_owned(),f.next()?.to_owned()))
     }).collect();
     let done=heimdall_night::bug_notes::reconcile(&inbox,&commits,now)?;
     for (id,text) in &done { println!("Resolved {id}: {text}"); }
     println!("{} note(s) resolved from {rev}; {} still open.",done.len(),heimdall_night::bug_notes::list(&inbox).len());
    }
    _=>bail!("bugs list | bugs resolve ID WHAT_WAS_DONE | bugs check BASE..HEAD | bugs reconcile [REV] | bugs scout ID|--open | bugs set-cause ID JSON | bugs export-decider-cases OUT.jsonl"),
   }
  }
  Some("gate")=> {
   let state=gate::state()?;
   println!("{}",json!({"eligible":state.eligible(),"on_ac":state.on_ac,"idle_seconds":state.idle_seconds,"required_idle_seconds":gate::IDLE_SECONDS}));
  }
  Some("--speech-split-helper")=>heimdall_night::correction::helper(path(&mut args,"models")?)?,
  Some("worker")=>job::worker(&path(&mut args,"request")?)?,
  Some("process")=>{
   let session=path(&mut args,"completed archive session")?;
   let models=path(&mut args,"models")?;
   let output=path(&mut args,"output")?;
   let enroll=args.next().map(PathBuf::from);
   println!("completed_jobs={}",job::process(&session,&models,&output,enroll.as_deref())?);
  }
  Some("run")=>{
   let dataset=path(&mut args,"dataset")?;let models=path(&mut args,"models")?;let output=path(&mut args,"output")?;
   let enroll=args.next().map(PathBuf::from);
   println!("completed_jobs={}",job::run(&dataset,&models,enroll.as_deref(),&output)?);
  }
  Some("export")=>{
   let snapshot=path(&mut args,"archive snapshot")?;let output=path(&mut args,"output")?;let additional=args.next().map(PathBuf::from);
   ensure!(snapshot.join("NIGHT_SNAPSHOT").is_file(),"export requires an isolated night snapshot");
   println!("utterances={}",heimdall_archive::export::export_with_additional(&snapshot,&output,additional.as_deref())?);
  }
  Some("snapshot")=>{
   let archive=path(&mut args,"completed archive session")?;let destination=path(&mut args,"snapshot destination")?;
   heimdall_archive::export::snapshot_archive(&archive,&destination)?;println!("snapshot={}",destination.display());
  }
  Some("enroll-fixture")=>{
   let voices=path(&mut args,"public fixture manifest")?;let models=path(&mut args,"models")?;let output=path(&mut args,"new enrollment file")?;
   ensure!(!output.exists(),"fixture enrollment exists");
   let voices:Value=serde_json::from_slice(&std::fs::read(voices)?)?;
   use speakers::embedding::VoiceEncoder;
   let mut encoder=speakers::embedding::ReDimNet::load(&models.join("speakers/ReDimNet2B6.onnx"))?;
   let mut store=speakers::identity::EnrollmentStore::default();
   for voice in voices["voices"].as_array().context("fixture voices")? {
    let pcm=listen::file::read_wav(&PathBuf::from(voice["audio"].as_str().context("fixture audio")?))?;
    store.enroll(voice["name"].as_str().context("fixture name")?,encoder.embed(&pcm)?)?;
   }
   store.save(&output)?;println!("fixture_voices={}",store.names.len());
  }
  Some("evaluate")=>{
   let mixture=path(&mut args,"mixed wav")?;let references=path(&mut args,"reference json")?;let models=path(&mut args,"models")?;let output=path(&mut args,"new lab output")?;let enroll=args.next().map(PathBuf::from);
   evaluate(mixture,references,models,output,enroll)?;
  }
  _=>bail!("Agent CLI: bugs list | bugs resolve ID WHAT_WAS_DONE | gate | process SESSION MODELS OUTPUT [ENROLLMENTS] | snapshot SESSION DEST | export SNAPSHOT DEST [NIGHT_OUTPUT] | run DATASET MODELS OUTPUT [ENROLLMENTS] | evaluate MIX REFERENCES MODELS LAB_OUTPUT [ENROLLMENTS] | enroll-fixture VOICES MODELS NEW_ENROLLMENTS"),
 }
    Ok(())
}
fn evaluate(
    mixture: PathBuf,
    references: PathBuf,
    models: PathBuf,
    output: PathBuf,
    enroll: Option<PathBuf>,
) -> Result<()> {
    ensure!(!output.exists(), "evaluation output exists");
    heimdall_archive::private_dir(&output)?;
    let audio = listen::file::read_wav(&mixture)?;
    ensure!(
        audio.len() <= separator::MAX_SAMPLES,
        "evaluate at most 8 seconds per clip"
    );
    let refs: Value = serde_json::from_slice(&std::fs::read(references)?)?;
    ensure!(
        refs["reference_status"] == "official-labels-clipped-to-mixture",
        "evaluation requires verified independent reference labels"
    );
    let reference = [
        refs["speakers"][0]
            .as_str()
            .context("speaker 0 reference")?,
        refs["speakers"][1]
            .as_str()
            .context("speaker 1 reference")?,
    ];
    // Explicit bounded diagnostic: same model path as run; does not alter real profiles.
    let start = Instant::now();
    let region = heimdall_night::dataset::Region {
        session: mixture
            .file_stem()
            .context("fixture session")?
            .to_string_lossy()
            .to_string(),
        source: "public-call-mixture".into(),
        start_ns: 0,
        end_ns: audio.len() as u64 * 62500,
        speakers: 2,
        utterance_ids: vec!["public-fixture".into()],
        audio: vec![],
    };
    let configuration = job::configuration(&models, enroll.as_deref())?;
    let mut recovered = job::infer(
        &audio,
        0,
        &models,
        enroll.as_deref(),
        &output,
        &region,
        &configuration,
    )?;
    #[cfg(target_os = "macos")]
    {
        let mut asr = listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), "metal")?;
        let time = Instant::now();
        let baseline = asr.transcribe(&audio, 0)?;
        let first = time.elapsed().as_secs_f64();
        let time = Instant::now();
        let repeat = asr.transcribe(&audio, 0)?;
        let second = time.elapsed().as_secs_f64();
        let tracks = &recovered["tracks"];
        let hyp = [
            tracks[0]["window_transcript"]["text"]
                .as_str()
                .context("track0")?,
            tracks[1]["window_transcript"]["text"]
                .as_str()
                .context("track1")?,
        ];
        let before = metrics::cpwer(reference, [&baseline.text, ""]);
        let after = metrics::cpwer(reference, hyp);
        let concatenated = format!("{} {}", reference[0], reference[1]);
        let wer_before = metrics::wer(&concatenated, &baseline.text);
        let order = if metrics::wer(reference[0], hyp[0]).errors
            + metrics::wer(reference[1], hyp[1]).errors
            <= metrics::wer(reference[0], hyp[1]).errors + metrics::wer(reference[1], hyp[0]).errors
        {
            [0, 1]
        } else {
            [1, 0]
        };
        let wer_after = metrics::wer(
            &concatenated,
            &format!("{} {}", hyp[order[0]], hyp[order[1]]),
        );
        let seconds = recovered["timings_seconds"]["worker_total"]
            .as_f64()
            .unwrap();
        recovered["evaluation"] = json!({"reference":refs,"mixture":mixture,"baseline":baseline,"baseline_repeat":repeat,"baseline_decode_seconds":[first,second],"baseline_text_repeat_equal":baseline.text==repeat.text,"cpwer_before":before,"cpwer_after":after,"wer_before":wer_before,"wer_after":wer_after,"before_percent":before.rate()*100.,"after_percent":after.rate()*100.,"derived_seconds_per_audio_hour":seconds/(audio.len() as f64/16000.)*3600.,"evaluation_wall_seconds":start.elapsed().as_secs_f64(),"hardware":"Apple M5 Max, 128 GiB; concurrent Vault work; not a quiet energy measurement"});
    }
    heimdall_archive::atomic_write(
        &output.join("result.json"),
        &serde_json::to_vec_pretty(&recovered)?,
    )?;
    println!("{}", recovered["evaluation"]);
    Ok(())
}
