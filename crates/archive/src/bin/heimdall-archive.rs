use anyhow::{Context, Result, ensure};
use heimdall_archive::{audio, compact, migrate, store, timeline};
use std::path::Path;
fn crash_flag(args: &[String], at: usize) -> Result<()> {
    if args.get(at).is_some_and(|x| x == "--crash-after") {
        compact::set_crash_test(args.get(at + 1).context("crash stage")?);
    }
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("timeline") => return heimdall_archive::event_timeline::cli(&args[1..]),
        Some("embedding-fixtures") => {
            ensure!(args.len()==4,"embedding-fixtures LAB_ARCHIVE_ROOT OUTPUT_DIR ITEMS_PER_KIND_ROLE");
            let root=Path::new(&args[1]).canonicalize()?;
            lab_archive(&root)?;
            let output=Path::new(&args[2]).canonicalize().or_else(|_| {
                std::fs::create_dir_all(&args[2])?;Path::new(&args[2]).canonicalize()
            })?;
            let explicit=if args[3].parse::<usize>().is_err() {Some(args[3].clone())}else{None};
            let n:usize=if explicit.is_some(){1}else{args[3].parse()?};ensure!((1..=32).contains(&n),"fixture sample bound");
            let db=store::open(&root)?;
            let groups:Vec<(String,String,i64)>=db.prepare("SELECT kind,role,count(*) FROM timeline_chunks GROUP BY kind,role ORDER BY kind,role")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            let _reader=compact::reader_lock(&root)?;
            let mut cases=vec![];let mut failures=vec![];
            for (kind,role,count) in groups {
                let ids:Vec<String>=db.prepare("SELECT id FROM timeline_chunks WHERE kind=?1 AND role=?2 AND (?3 IS NULL OR id=?3) ORDER BY id")?.query_map(rusqlite::params![kind,role,explicit],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
                // Fixed evenly spaced samples over the actual archive, not a handpicked hit.
                for i in 0..n.min(ids.len()) {
                    let id=&ids[i*ids.len()/n.min(ids.len())];let c=timeline::load(&db,id)?;
                    let destination=output.join(format!("case-{}",cases.len()+failures.len()));
                    let start=std::time::Instant::now();
                    match heimdall_archive::embedding::prepare_chunk(&root,&c,&destination) {
                        Ok(inputs)=>cases.push(serde_json::json!({"id":id,"kind":kind,"role":role,"group_count":count,"text":c.text,"inputs":inputs,"archive_prepare_s":start.elapsed().as_secs_f64()})),
                        Err(e)=>failures.push(serde_json::json!({"id":id,"kind":kind,"role":role,"error":format!("{e:#}")})),
                    }
                }
            }
            std::fs::write(output.join("fixtures.json"),serde_json::to_vec_pretty(&serde_json::json!({"archive":root,"cases":cases,"failures":failures}))?)?;
            println!("{}",serde_json::json!({"cases":cases.len(),"failures":failures,"output":output.join("fixtures.json")}));
        }
        Some("embedding-index") => {
            ensure!(args.len()==3,"embedding-index LAB_ARCHIVE_ROOT MODEL_ROOT");
            let root=Path::new(&args[1]).canonicalize()?;lab_archive(&root)?;
            let mut provider=heimdall_archive::embedding::MlxEmbedder::new(Path::new(&args[2]));
            let start=std::time::Instant::now();let mut total=0;
            loop {
                let n=timeline::embed_batch(&root,&mut provider,16,&mut ||timeline::BackfillGate{on_ac:start.elapsed().as_secs()<540,idle_seconds:300.})?;
                total+=n;println!("{}",serde_json::json!({"embedded":total,"batch":n,"wall_s":start.elapsed().as_secs_f64()}));
                if n==0 {break;}
            }
        }
        Some("embedding-search") => {
            ensure!(args.len()==4,"embedding-search ARCHIVE_ROOT MODEL_ROOT SEARCH_JSON_WITH_VECTOR_QUERY");
            let mut q:timeline::Search=serde_json::from_str(&args[3])?;
            let text=q.vector.as_ref().and_then(|v|v["query"].as_str()).context("vector.query required")?;
            let mut p=heimdall_archive::embedding::MlxEmbedder::new(Path::new(&args[2]));
            q.vector=Some(serde_json::to_value(p.query(text,&mut ||true)?)?);
            let db=rusqlite::Connection::open_with_flags(Path::new(&args[1]).join(store::INDEX),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            println!("{}",serde_json::to_string(&timeline::search(&db,&q)?)?);
        }
        Some("chunk-live-cost") => {
            ensure!(args.len()==2,"chunk-live-cost LAB_ARCHIVE_ROOT");
            chunk_live_cost(Path::new(&args[1]))?;
        }
        Some("chunk-backfill") => {
            ensure!(args.len()==2,"chunk-backfill LAB_ARCHIVE_ROOT");
            chunk_backfill(Path::new(&args[1]))?;
        }
        Some("chunk-search") => {
            ensure!(args.len()==3,"chunk-search ARCHIVE_ROOT SEARCH_JSON");
            let db=rusqlite::Connection::open_with_flags(Path::new(&args[1]).join(store::INDEX),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let q:timeline::Search=serde_json::from_str(&args[2])?;
            println!("{}",serde_json::to_string(&timeline::search(&db,&q)?)?);
        }
        Some("migrate") => {
            ensure!(args.len() >= 2, "migrate ARCHIVE_ROOT [--keep] [--crash-after STAGE]");
            let keep = args.iter().any(|a| a == "--keep");
            if let Some(i) = args.iter().position(|a| a == "--crash-after") {
                crash_flag(&args, i)?;
            }
            let root = Path::new(&args[1]);
            let before = std::time::Instant::now();
            let reports = migrate::migrate_all_with(root, &migrate::Options { keep }, |r| {
                println!("{}", serde_json::to_string(r).unwrap_or_default());
            })?;
            let failed = reports.iter().filter(|r| !r.verified).count();
            // Blobs a failed attempt left behind are queued; collect them while nothing else runs.
            compact::collect_now(root)?;
            println!(
                "{}",
                serde_json::json!({"sessions":reports.len(),"verified":reports.len()-failed,"failed":failed,"store_bytes":store::bytes_on_disk(root)?,"seconds":before.elapsed().as_secs_f64()})
            );
            ensure!(failed == 0, "{failed} legacy folders were not migrated; they are untouched");
        }
        Some("pack-file") => {
            ensure!(args.len() >= 4, "pack-file ARCHIVE_ROOT NOW_NS audio|records [--crash-after STAGE]");
            crash_flag(&args, 4)?;
            println!("{}", compact::compact_metadata(Path::new(&args[1]), args[2].parse()?, args[3] == "audio")?);
        }
        Some("compact") => {
            ensure!(args.len() >= 2, "compact ARCHIVE_ROOT");
            println!("{:?}", compact::compact_once(Path::new(&args[1]), heimdall_archive::wall_ns())?);
        }
        Some("bench") => {
            ensure!(args.len() >= 3, "bench LAB_ARCHIVE_ROOT GROUPS");
            bench(Path::new(&args[1]), args[2].parse()?)?;
        }
        Some("compact-file") => {
            ensure!(args.len() >= 3, "compact-file ARCHIVE_ROOT NOW_NS [--crash-after verified|committed]");
            crash_flag(&args, 3)?;
            println!("{:?}", compact::compact_audio(Path::new(&args[1]), args[2].parse()?, compact::COLD_BITRATE, usize::MAX)?);
        }
        Some("codec") | Some("codec-speech") | Some("codec-flac") | Some("normalize") => {
            ensure!(
                args.len() == 5,
                "codec INPUT_WAV BITRATE OUTPUT_HAR OUTPUT_WAV"
            );
            let mut wav = hound::WavReader::open(&args[1])?;
            let spec = wav.spec();
            let pcm: Vec<f32> = if spec.sample_format == hound::SampleFormat::Float {
                wav.samples::<f32>()
                    .collect::<std::result::Result<_, _>>()?
            } else {
                let scale = 2_f32.powi(spec.bits_per_sample as i32 - 1);
                wav.samples::<i32>()
                    .map(|x| x.map(|s| s as f32 / scale))
                    .collect::<std::result::Result<_, _>>()?
            };
            let mono = audio::mono_16k(&pcm, spec.sample_rate, spec.channels)?;
            let flac = args[0] == "codec-flac";
            if args[0] == "normalize" {
                let mut w = hound::WavWriter::create(
                    &args[4],
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: 16000,
                        bits_per_sample: 32,
                        sample_format: hound::SampleFormat::Float,
                    },
                )?;
                for x in mono {
                    w.write_sample(x)?;
                }
                w.finalize()?;
                return Ok(());
            }
            let bytes = if flac {
                audio::encode_flac(&mono)?
            } else {
                audio::encode_opus_mode(&mono, args[2].parse()?, args[0] == "codec-speech")?
            };
            let decoded = if flac {
                audio::decode_flac(&bytes, mono.len())?
            } else {
                audio::decode_opus(&bytes)?
            };
            std::fs::write(&args[3], &bytes)?;
            let mut w = hound::WavWriter::create(
                &args[4],
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 16000,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )?;
            for x in decoded {
                w.write_sample(x)?;
            }
            w.finalize()?;
            let hot = zstd::bulk::compress(&audio::pcm_bytes(&pcm), 1)?.len();
            println!(
                "{}",
                serde_json::json!({"seconds":mono.len() as f64/16000.,"hot_bytes":hot,"cold_bytes":bytes.len(),"bitrate":args[2]})
            );
        }
        _ => anyhow::bail!("usage: heimdall-archive migrate|compact|compact-file|pack-file|bench|codec ..."),
    }
    Ok(())
}

fn lab_archive(root:&Path)->Result<()> {
    let project=Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    ensure!(root.starts_with(project.join("target")),"embedding lab commands only accept this worktree's target/ archive copies");
    Ok(())
}

/// Lab-only catch-up benchmark: CPU seconds of this process per compacted audio hour.
/// Runs the background compactor's pass on an isolated store until GROUPS audio groups move.
fn bench(root: &Path, groups: usize) -> Result<()> {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
    }
    let db = store::open(root)?;
    let before: i64 = db.query_row("SELECT COALESCE(max(rowid),0) FROM audio_spans", [], |r| r.get(0))?;
    let cpu = || {
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
        let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
        (t(u.ru_utime), t(u.ru_stime))
    };
    let (u0, s0) = cpu();
    let wall = std::time::Instant::now();
    let mut done = 0usize;
    let mut passes = 0usize;
    while done < groups {
        let r = compact::compact_once(root, heimdall_archive::wall_ns())?;
        passes += 1;
        ensure!(r.errors.is_empty(), "compaction errors: {:?}", r.errors);
        if r.audio_groups == 0 {
            break;
        }
        done += r.audio_groups;
    }
    let (u1, s1) = cpu();
    let elapsed = wall.elapsed().as_secs_f64();
    let audio_ns: i64 = db.query_row("SELECT COALESCE(sum(end_ns-start_ns),0) FROM audio_spans WHERE rowid>?1", [before], |r| r.get(0))?;
    let hours = audio_ns as f64 / 3.6e12;
    let cpu_s = (u1 - u0) + (s1 - s0);
    println!(
        "{}",
        serde_json::json!({"groups":done,"passes":passes,"audio_hours":hours,"user_s":u1-u0,"sys_s":s1-s0,"wall_s":elapsed,"cpu_s_per_audio_hour":cpu_s/hours})
    );
    Ok(())
}

/// Agent-only metadata benchmark on an isolated consistent archive copy. CPU covers model-free
/// migration, not capture/model inference. Never opens or launches the installed application.
fn chunk_backfill(root:&Path)->Result<()> {
    let cpu=|| {
        let mut u:libc::rusage=unsafe{std::mem::zeroed()};unsafe{libc::getrusage(libc::RUSAGE_SELF,&mut u)};
        u.ru_utime.tv_sec as f64+u.ru_utime.tv_usec as f64/1e6+u.ru_stime.tv_sec as f64+u.ru_stime.tv_usec as f64/1e6
    };
    let db=store::open(root)?;
    let before=store::bytes_on_disk(root)?;
    let cpu0=cpu();let wall=std::time::Instant::now();
    let mut batches=0;let mut processed=0;
    loop {
        let mut budget=512usize;
        let n=timeline::backfill(root,&mut ||{if budget==0{return false;}budget-=1;true})?;
        batches+=1;processed+=n;if n==0 {break;}
        ensure!(wall.elapsed().as_secs()<540,"chunk backfill benchmark reached its nine-minute limit; resume from checkpoints");
    }
    let cpu_s=cpu()-cpu0;let elapsed=wall.elapsed().as_secs_f64();
    let after=store::bytes_on_disk(root)?;
    let counts:std::collections::BTreeMap<String,i64>=db.prepare("SELECT kind||':'||role,count(*) FROM timeline_chunks GROUP BY kind,role")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let rows:i64=db.query_row("SELECT (SELECT count(*) FROM timeline_chunks)+(SELECT count(*) FROM chunk_tags)+(SELECT count(*) FROM chunk_links)+(SELECT count(*) FROM timeline_state)+(SELECT count(*) FROM timeline_finals)+(SELECT count(*) FROM timeline_backfill)+(SELECT count(*) FROM timeline_speakers)+(SELECT count(*) FROM timeline_audio_backfill)+(SELECT count(*) FROM video_screen_events)+(SELECT count(*) FROM timeline_video_backfill)+(SELECT count(*) FROM timeline_screen_detector)",[],|r|r.get(0))?;
    // Union audio/video coverage per session, not the sum across simultaneous sources. Cold
    // callback rows are restored by View; no waveform is loaded into memory.
    let mut duration_ns=0u64;
    for name in store::sessions(root)? {
        let view=heimdall_archive::View::session(&root.join(name))?;view.capture()?;
        let mut stmt=view.db.prepare("SELECT host_ns,host_ns+frames*1000000000/rate FROM temp.blocks UNION ALL SELECT v.host_ns,v.end_ns FROM main.video_samples v JOIN main.video_segments s ON s.id=v.segment WHERE s.session=?1 ORDER BY 1")?;
        let mut rs=stmt.query([view.session_id()])?;let mut end=0u64;
        while let Some(row)=rs.next()? {let a=row.get::<_,i64>(0)?.max(0) as u64;let b=row.get::<_,i64>(1)?.max(0) as u64;if b>end {duration_ns+=b-a.max(end);end=b;}}
    }
    let hours=duration_ns as f64/3.6e12;
    let embeddings:i64=db.query_row("SELECT count(*) FROM chunk_embeddings",[],|r|r.get(0))?;
    println!("{}",serde_json::json!({"batches":batches,"progress_units":processed,"chunk_counts":counts,"index_rows":rows,"embeddings":embeddings,"recorded_hours_union":hours,"cpu_s":cpu_s,"wall_s":elapsed,"index_growth_bytes":after.saturating_sub(before),"cpu_s_per_recorded_hour":cpu_s/hours,"rows_per_recorded_hour":rows as f64/hours,"bytes_per_recorded_hour":after.saturating_sub(before) as f64/hours}));
    Ok(())
}

/// Marginal model-free live projection cost, interleaved with its existing transcript SQL
/// baseline. Only an isolated archive copy is permitted; both arms roll back every sample.
fn chunk_live_cost(root:&Path)->Result<()> {
    let cpu=|| unsafe {let mut u:libc::rusage=std::mem::zeroed();libc::getrusage(libc::RUSAGE_SELF,&mut u);u.ru_utime.tv_sec as f64+u.ru_utime.tv_usec as f64/1e6+u.ru_stime.tv_sec as f64+u.ru_stime.tv_usec as f64/1e6};
let db=store::open(root)?;
 let rows:Vec<(i64,String)>=db.prepare("SELECT session,id FROM utterances ORDER BY rowid LIMIT 512")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<std::result::Result<_,_>>()?;
 let mut indexed=0.;let mut baseline=0.;let mut count=0;
 // Interleaved baseline/candidate; setup and rollback are outside the CPU window.
 for (session,id) in rows {
  for candidate in [false,true] {
   db.execute_batch("SAVEPOINT live_cost")?;
   db.execute("UPDATE timeline_chunks SET content_hash='force-new-projection' WHERE session=?1",[session])?;
   let start=cpu();
   db.execute("UPDATE utterances SET decision=decision WHERE session=?1 AND id=?2",(&session,&id))?;
   if candidate {timeline::utterance(&db,session,&id)?;}
   let used=cpu()-start;
   if candidate {indexed+=used;} else {baseline+=used;}
   db.execute_batch("ROLLBACK TO live_cost; RELEASE live_cost")?;
  }
  count+=1;
 }
 let spans:Vec<(i64,String,i64,i64,i64)>=db.prepare("SELECT session,source,start_ns,end_ns,start_wall_ns FROM timeline_chunks WHERE kind='other_audio' ORDER BY id LIMIT 128")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<std::result::Result<_,_>>()?;
 let mut audio_cpu=0.;let samples=spans.len();
 for (session,source,start,end,wall) in spans {
  db.execute_batch("SAVEPOINT live_audio_cost")?;
  db.execute("UPDATE timeline_chunks SET content_hash='force-live-span' WHERE session=?1 AND source=?2 AND kind='other_audio'",(&session,&source))?;
  let a=start.max(end-1_000_000_000);let now=cpu();
  timeline::audio_span(&db,session,&source,a as u64,end as u64,(wall+a-start) as u64)?;audio_cpu+=cpu()-now;
  db.execute_batch("ROLLBACK TO live_audio_cost; RELEASE live_audio_cost")?;
 }
 let source_seconds:f64=db.query_row("SELECT coalesce(sum(end_ns-start_ns),0)/1e9 FROM timeline_chunks WHERE kind='other_audio'",[],|r|r.get(0))?;
 println!("{{\"audio_span_samples\":{samples},\"audio_span_cpu_s\":{audio_cpu},\"sound_source_seconds\":{source_seconds}}}");
 println!("{{\"finals\":{count},\"baseline_cpu_s\":{baseline},\"indexed_cpu_s\":{indexed},\"marginal_cpu_s\":{}}}",indexed-baseline);
    Ok(())
}
