//! Agent-operated full-store size receipt; no inference, permissions or capture.
//! Every fixture is its own store (index + packs); sizes are whole files after checkpoint.
use anyhow::{Result, ensure};
use heimdall_archive::{DAY_NS, Journal, Record, audio, compact, store};
use rusqlite::params;
use serde_json::json;
use std::path::{Path, PathBuf};
fn main() -> Result<()> {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let outputs_only = args.iter().any(|a| a == "--outputs-only");
    let partials = !args.iter().any(|a| a == "--without-partials");
    args.retain(|a| a != "--outputs-only" && a != "--without-partials");
    if outputs_only {
        ensure!(args.len()==1,"usage: heimdall-archive-size NEW_DIR --outputs-only [--without-partials]");
        let dir=PathBuf::from(&args[0]);ensure!(!dir.exists(),"size directory exists");
        let (hot,cold,png,images)=other_outputs(&dir,partials)?;
        let receipt=json!({"schema":4,"one_tenth_day":true,"partials":partials,"raw_partial_decodes":if partials {1620}else{0},"partial_ui_events":if partials {1260}else{0},"hot_bytes":hot,"cold_bytes":cold,"images_png":png,"images_cold":images,"hot_GB_per_day":hot as f64*10./1e9,"cold_GB_per_day":cold as f64*10./1e9});
        std::fs::write(dir.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
        println!("{receipt}");return Ok(());
    }
    let non_speech = args.last().is_some_and(|x| x == "--non-speech");
    if non_speech {
        args.pop();
    }
    ensure!(args.len() >= 2, "usage: heimdall-archive-size NEW_DIR WAV...");
    let dir = PathBuf::from(&args[0]);
    ensure!(!dir.exists(), "size directory exists");
    let root = dir.join("audio-fixture");
    let session = store::session_path(&root, "session-size");
    let db = store::open(&root)?;
    let session_id = store::ensure_session(&db, "session-size")?;
    let segment = store::create_segment(&db, session_id, "session-size-capture", 1_000_000_000, 0, "native")?;
    let mut seconds = 0.;
    let mut frames = 0usize;
    for (i, p) in args[1..].iter().enumerate() {
        let mut wav = hound::WavReader::open(p)?;
        let spec = wav.spec();
        ensure!(spec.sample_format == hound::SampleFormat::Float, "expected native f32 fixture");
        let samples: Vec<f32> = wav.samples().collect::<std::result::Result<_, _>>()?;
        let source = format!("source-{i}");
        db.execute("INSERT INTO sources VALUES(?1,?2,?3,0,NULL)",params![segment,source,json!({"kind":{"Process":{}},"format":{"rate":spec.sample_rate,"channels":spec.channels}}).to_string()])?;
        let step = 512 * spec.channels as usize;
        let mut native_frame = 0usize;
        let mut silence_row = None::<(i64, usize)>;
        for group in samples.chunks(step * (spec.sample_rate as usize / 512)) {
            let zero = group.iter().all(|s| s.to_bits() == 0);
            let host = |frame: usize| (frame as u64 * 1_000_000_000 / spec.sample_rate as u64 + i as u64 * 1_000_000_000_000) as i64;
            let wall = |frame: usize| 1_000_000_000i64 + frame as i64 * 1_000_000_000 / spec.sample_rate as i64 + i as i64 * 1_000_000_000_000;
            if zero {
                let n = group.len() / spec.channels as usize;
                if let Some((row, old)) = silence_row {
                    db.execute("UPDATE blocks SET frames=?2 WHERE id=?1", params![row, (old + n) as i64])?;
                    silence_row = Some((row, old + n));
                } else {
                    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,?2,?3,?4,?5,?6,'zero',0,?7)",params![segment,source,host(native_frame),spec.sample_rate,spec.channels,n as i64,wall(native_frame)])?;
                    silence_row = Some((db.last_insert_rowid(), n));
                    frames += 1;
                }
                native_frame += n;
                continue;
            }
            silence_row = None;
            let id = audio::put_hot(&db, spec.sample_rate, spec.channels, group)?;
            db.execute_batch("BEGIN IMMEDIATE")?;
            for (n, pcm) in group.chunks(step).enumerate() {
                let len = pcm.len() / spec.channels as usize;
                db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,?2,?3,?4,?5,?6,'f32-zstd',0,?7,?8,?9)",params![segment,source,host(native_frame),spec.sample_rate,spec.channels,len as i64,id,(n*512) as i64,wall(native_frame)])?;
                native_frame += len;
                frames += 1;
            }
            db.execute_batch("COMMIT")?;
        }
        seconds += samples.len() as f64 / spec.channels as f64 / spec.sample_rate as f64;
    }
    let hot = store::bytes_on_disk(&root)?;
    let mut audio_sources: Vec<serde_json::Value> = vec![];
    for i in 0..args.len() - 1 {
        let source = format!("source-{i}");
        let (rate, ch, frames): (u32, u16, i64) = db.query_row(
            "SELECT rate,channels,sum(frames) FROM blocks WHERE source_id=?1",
            [&source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let ids: Vec<String> = db.prepare("SELECT DISTINCT object_id FROM blocks WHERE source_id=?1 AND object_id IS NOT NULL")?.query_map([&source], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut payload = 0;
        for id in ids {
            payload += audio::object_bytes(&db, &root, &id)?.4.len();
        }
        audio_sources.push(json!({"source":source,"rate":rate,"channels":ch,"seconds":frames as f64/rate as f64,"hot_payload_bytes":payload}));
    }
    if non_speech {
        let j = Journal::open(&session)?;
        for i in 0..args.len() - 1 {
            j.append(&Record {kind:"model_output".into(),source:format!("source-{i}"),utterance_ids:vec![],host_ns:0,wall_ns:1,payload:json!({"model":"silero","input":{"source":format!("source-{i}"),"start_ns":i as u64*1_000_000_000_000,"end_ns":i as u64*1_000_000_000_000+120_000_000_000u64},"probability":0.001})})?;
        }
    }
    compact::compact_audio(&root, DAY_NS * 365, 32000, usize::MAX)?;
    while compact::compact_metadata(&root, DAY_NS * 365, true)? > 0 {}
    while compact::compact_metadata(&root, DAY_NS * 365, false)? > 0 {}
    store::reclaim(&db, u32::MAX)?;
    store::reclaim_pack(&root, store::HOT, u32::MAX)?;
    let cold = store::bytes_on_disk(&root)?;
    for row in &mut audio_sources {
        let ids: Vec<String> = db.prepare("SELECT DISTINCT object_id FROM audio_spans WHERE source=?1")?.query_map([row["source"].as_str().unwrap()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut n = 0;
        for id in ids {
            n += audio::object_bytes(&db, &root, &id)?.4.len();
        }
        row["cold_payload_bytes"] = json!(n);
    }
    drop(db);
    // Actual schema/dictionary metadata with 3 independent source-clock streams.
    let evidence_root = dir.join("evidence-fixture");
    let evidence = store::session_path(&evidence_root, "session-vad");
    let journal = Journal::open(&evidence)?;
    let records = 12000usize;
    for i in 0..records {
        let source = format!("mic:{}", i % 3);
        let ns = i as u64 / 3 * 32_000_000;
        let p = ((i * 7919) % 65536) as f64 / 65536.;
        journal.append(&Record {kind:"model_output".into(),source:source.clone(),utterance_ids:vec![format!("listen:{source}:0")],host_ns:ns,wall_ns:1,payload:json!({"model":"silero","input":{"source":source,"start_ns":ns,"end_ns":ns+32_000_000,"rate":16000,"samples":512,"context_samples":64,"state":"previous source/generation Silero state; zero on reset"},"generation":1,"probability":p,"utterance_start_ns":0})})?;
    }
    let evidence_hot = store::bytes_on_disk(&evidence_root)?;
    loop {
        let r = compact::compact_records(&evidence_root, DAY_NS * 365)?;
        if r.payloads == 0 {
            break;
        }
    }
    {
        let db = store::open(&evidence_root)?;
        for i in 0..3 {
            heimdall_archive::records::discard_activity_in(&db, journal.session_id(), &format!("mic:{i}"), u64::MAX / 2, usize::MAX >> 1)?;
        }
    }
    drop(journal);
    while compact::compact_metadata(&evidence_root, DAY_NS * 365, false)? > 0 {}
    store::reclaim(&store::open(&evidence_root)?, u32::MAX)?;
    let evidence_cold = store::bytes_on_disk(&evidence_root)?;
    let other = dir.join("other-fixture");
    let (other_hot, other_cold, image_png, image_webp) = other_outputs(&other,partials)?;
    let receipt = json!({"schema":3,"baseline":"one 48k mono mic + two 48k stereo app sources; public speech resampled, independent -90dB float dither; 512 native-frame callbacks; separate source time epochs prevent gain-dedup savings; no digital silence savings in fixture","source_seconds":seconds,"audio_sources":audio_sources,"non_speech":non_speech,"blocks":frames,"hot_store_bytes":hot,"cold_store_bytes":cold,"hot_audio_GB_per_30_source_hours":hot as f64/seconds*108000./1e9,"cold_audio_GB_per_30_source_hours":cold as f64/seconds*108000./1e9,"other_baseline":"one tenth of day: 540 finals x 40 words; 180 mic TCL decisions x two 512-float outputs; 900 speaker pairs x two 192-float outputs; 540 SmartTurn outputs; 36 wake/payload/reply sets; 12 speak-back requests; 10 unique 1280x720 generated desktop-like images", "other_hot_bytes":other_hot,"other_cold_bytes":other_cold,"other_hot_GB_per_day":other_hot as f64*10./1e9,"other_cold_GB_per_day":other_cold as f64*10./1e9,"images_png_bytes":image_png,"images_cold_bytes":image_webp,"vad_records":records,"vad_hot_bytes":evidence_hot,"vad_cold_bytes":evidence_cold,"vad_hot_GB_per_30_source_hours":evidence_hot as f64/records as f64*108000./0.032/1e9,"vad_cold_GB_per_30_source_hours":evidence_cold as f64/records as f64*108000./0.032/1e9});
    std::fs::write(dir.join("receipt.json"), serde_json::to_vec_pretty(&receipt)?)?;
    println!("{receipt}");
    Ok(())
}
fn image_bytes(root: &Path) -> Result<u64> {
    let db = store::open(root)?;
    let ids: Vec<String> = db.prepare("SELECT id FROM images")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for id in ids {
        n += heimdall_archive::records::image_bytes(&db, root, &id)?.1.len() as u64;
    }
    Ok(n)
}

fn vector(n: usize, seed: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            (((i + seed * 13) as f32 * 0.0183).sin() + ((i * 37 + seed * 71) as f32 * 0.0719).cos())
                / 30.
        })
        .collect()
}
fn other_outputs(dir: &Path, partials: bool) -> Result<(u64, u64, u64, u64)> {
    let session = store::session_path(dir, "session-outputs");
    let journal = Journal::open(&session)?;
    let index = store::open(dir)?;
    let phrase = "you open the window you read through it you might click on close it again straight away we want everything saved including model outputs for training later";
    let base: Vec<_> = phrase.split_whitespace().cycle().take(40).collect();
    for i in 0..540 {
        let source = format!("source-{}", i % 3);
        let id = format!("session:{}", i);
        let start = i as u64 / 3 * 20_000_000_000;
        let end = start + 20_000_000_000;
        let words:Vec<_>=base.iter().enumerate().map(|(n,w)|json!({"text":w,"start_ns":start+n as u64*500_000_000,"end_ns":start+(n+1) as u64*500_000_000,"confidence":0.91+(n%7) as f32/100.})).collect();
        let text = base.join(" ");
        if partials {
            // 7 responsive mic decodes / 20-s turn, plus one window-roll decode
            // on each final-only app source. All raw revisions, even un-emitted ones.
            let count=if i%3==0 {7}else{1};
            for revision in 1..=count {
                let n=if count==1 {30} else {(revision*5).min(40)};
                let partial_words=&words[..n];
                let text=base[..n].join(" ");
                let partial_end=start+n as u64*500_000_000;
                journal.append(&Record{kind:"model_output".into(),source:source.clone(),utterance_ids:vec![format!("listen:{source}:{start}")],host_ns:start,wall_ns:1,payload:json!({"model":"parakeet","is_final":false,"revision":revision,"generation":1,"input":{"audio_id":source,"source":source,"start_ns":start,"end_ns":partial_end,"rate":16000,"samples":n*8000,"transform":"capture mono/resample 16k"},"utterance_start_ns":start,"transcript":{"text":text,"words":partial_words}})})?;
                if i%3==0 {journal.append(&Record{kind:"listen_event".into(),source:source.clone(),utterance_ids:vec![id.clone()],host_ns:start,wall_ns:1,payload:json!({"event":"partial_transcript","source":source,"text":text,"words":partial_words,"stable_words":n.saturating_sub(5)})})?;}
            }
        }
        let decision = json!({"send":i%15==0,"addressed_probability":0.8,"end_of_turn_probability":0.97,"send_probability":0.99,"forced":false,"notes":[],"latency":{"total_ms":21.3,"d1_ms":0.1,"tcl_ms":17.1}});
        index.execute(
            "INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?8,?1,1,?2,?2,'User',?3,?4,?5,?6,'[]',?7)",
            params![
                id,
                source,
                start as i64,
                end as i64,
                text,
                serde_json::to_string(&words)?,
                decision.to_string(),
                journal.session_id()
            ],
        )?;
        for (kind, payload) in [
            (
                "asr",
                json!({"model":"parakeet","input":{"source":source,"start_ns":start,"end_ns":end,"samples":320000,"rate":16000},"transcript":{"text":text,"words":words}}),
            ),
            (
                "smart_turn",
                json!({"model":"smart_turn","input":{"source":source,"start_ns":end-8_000_000_000,"end_ns":end,"samples":128000,"rate":16000},"probability":0.97+(i%3) as f64/1000.}),
            ),
        ] {
            journal.append(&Record {
                kind: kind.into(),
                source: source.clone(),
                utterance_ids: vec![id.clone()],
                host_ns: start,
                wall_ns: 1,
                payload,
            })?;
        }
        if i % 3 == 0 {
            let a = journal.vector(&vector(512, i))?;
            let b = journal.vector(&vector(512, i + 500))?;
            journal.append(&Record{kind:"d1_tcl".into(),source:source.clone(),utterance_ids:vec![id.clone()],host_ns:end,wall_ns:1,payload:json!({"input":{"current":{"id":id,"transcript":text,"audio":{"source_id":source,"start_ns":start,"end_ns":end,"rate":16000,"samples":320000}},"context":[],"media":{"kind":"text","text":"Desktop editor"},"agent_state":"idle"},"model_outputs":{"current_embedding_id":a,"pristine_embedding_id":b,"history_embedding_ids":[a,b],"probability":0.8,"d1_output":{"answers":{"send_now":{"probabilities":{"yes":0.91,"no":0.09}}},"usage":{"input_tokens":201,"output_tokens":0}},"send_head_output":{"model":"send_head","input":{"utterance_id":id,"d1_probability":0.91,"audio_transform":"WhisperFeatures 80x800 from current audio reference"},"probability":0.99}},"decision":decision,"d1_questions":["is this directed at Heimdall"],"d1_state":"Agent idle"})})?;
        }
        if i % 15 == 0 {
            for (kind, payload) in [
                ("agent_wake", json!({"urgent":false})),
                (
                    "agent_send",
                    json!({"query":true,"payload":{"uuid":id,"message":{"role":"user","content":[{"type":"text","text":format!("event={id} [Mic / User] {text}")}]}}}),
                ),
                (
                    "agent_reply",
                    json!({"event":{"kind":"complete","id":id,"text":"I have saved the decision and checked the matching input.","error":false}}),
                ),
            ] {
                journal.append(&Record {
                    kind: kind.into(),
                    source: "agent:dry".into(),
                    utterance_ids: vec![id.clone()],
                    host_ns: end,
                    wall_ns: 1,
                    payload,
                })?;
            }
        }
        if i % 45 == 0 {
            journal.append(&Record {
                kind: "speak_back".into(),
                source: "heimdall".into(),
                utterance_ids: vec![id],
                host_ns: end,
                wall_ns: 1,
                payload: json!({"text":"The window is ready.","language":"en","enabled":true}),
            })?;
        }
    }
    for i in 0..900 {
        let a = journal.vector(&vector(192, i + 9000))?;
        let b = journal.vector(&vector(192, i + 10000))?;
        journal.append(&Record {kind:"speaker_embeddings".into(),source:format!("source-{}",i%3),utterance_ids:vec![],host_ns:i as u64/3*12_000_000_000,wall_ns:1,payload:json!({"track":i%7,"input_spans_seconds":[[i as f64*12.,i as f64*12.+12.]],"embedding_ids":[a,b],"rate":16000,"samples":192000})})?;
    }
    for invocation in 0..15000_u64 {
        let start=invocation*240_000_000;
        let changes:Vec<_>=(0..24).map(|frame|json!({"frame":invocation*24+frame,"probabilities":(0..8).map(|speaker|((invocation*7919+frame*17+speaker*31)%65536) as f32/65536.).collect::<Vec<_>>()})).collect();
        journal.append(&Record{kind:"speaker_probabilities".into(),source:"source-2".into(),utterance_ids:vec![],host_ns:start,wall_ns:1,payload:json!({"model":"nemotron","revision":invocation+1,"input":{"audio_id":"source-2","start_ns":0,"end_ns":start+240_000_000,"rate":16000,"state":"previous invocation source/session stream state"},"output":{"first_frame":invocation.saturating_sub(100)*24,"frame_count":(invocation+1)*24,"speakers":8,"seconds_per_frame":0.01,"previous_revision":invocation,"changed_frames":changes}})})?;
    }
    // Reuse one actual offline eight-component TTS receipt. Perturb fixture
    // vectors deterministically to charge unique output bytes on each of 12 replies.
    if let Some(path)=std::env::var_os("HEIMDALL_TTS_RECEIPT") {
        let measured:serde_json::Value=serde_json::from_slice(&std::fs::read(path)?)?;
        for reply in 0..12 {
            let mut ids=std::collections::HashMap::new();
            for (id,v) in measured["vectors"].as_object().unwrap() {
                let floats:Vec<f32>=v.as_array().unwrap().iter().map(|v|v.as_f64().unwrap() as f32*(1.+reply as f32*0.00001)).collect();
                ids.insert(id.clone(),journal.vector(&floats)?);
            }
            fn replace(v:&mut serde_json::Value,ids:&std::collections::HashMap<String,String>) {
                if let Some(a)=v.get_mut("vector_ids").and_then(serde_json::Value::as_array_mut) {for id in a { *id=json!(ids[id.as_str().unwrap()]); }}
                match v {serde_json::Value::Object(m)=>for v in m.values_mut(){replace(v,ids);},serde_json::Value::Array(a)=>for v in a{replace(v,ids);},_=>()}
            }
            for r in measured["records"].as_array().unwrap() {
                let mut payload=r["payload"].clone();replace(&mut payload,&ids);
                payload["fixture_reply"]=json!(reply);
                journal.append(&Record{kind:"tts_model_output".into(),source:"heimdall:synthesis".into(),utterance_ids:vec![],host_ns:reply*30_000_000_000,wall_ns:1,payload})?;
            }
        }
    }
    let mut png_bytes = 0;
    for i in 0..10 {
        let image = image::RgbaImage::from_fn(1280, 720, |x, y| {
            let text = y % 40 > 12 && y % 40 < 23 && x % 160 > 12 && x % 160 < 110;
            let panel = x / 320 + y / 240;
            let photo = x > 960;
            let noise = if photo {
                ((x * 1543 + y * 7919 + i * 37) % 73) as u8
            } else {
                0
            };
            image::Rgba([
                (if text { 180u8 } else { 20 + panel as u8 * 7 }).saturating_add(noise),
                (if text { 180u8 } else { 26 }).saturating_add(noise),
                (if text { 190u8 } else { 35 }).saturating_add(noise),
                255,
            ])
        });
        let mut png = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png)?;
        let png = png.into_inner();
        png_bytes += png.len() as u64;
        let id = journal.image(&png, "image/png")?;
        journal.append(&Record{kind:"screenshot".into(),source:"mic".into(),utterance_ids:vec![format!("session:{}",i*15)],host_ns:i as u64*60_000_000_000,wall_ns:1,payload:json!({"image_id":id,"command_host_ns":i as u64*60_000_000_000,"captured_host_ns":i as u64*60_000_000_000})})?;
    }
    index.execute("UPDATE images SET wall_ns=1", [])?;
    drop(journal);
    let hot = store::bytes_on_disk(dir)?;
    loop {
        let r = compact::compact_records(dir, DAY_NS * 365)?;
        if r.payloads == 0 && r.images == 0 {
            break;
        }
    }
    while compact::compact_metadata(dir, DAY_NS * 365, false)? > 0 {}
    store::reclaim(&index, u32::MAX)?;
    store::reclaim_pack(dir, store::HOT, u32::MAX)?;
    drop(index);
    let compressed_images = image_bytes(dir)?;
    let cold = store::bytes_on_disk(dir)?;
    Ok((hot, cold, png_bytes, compressed_images))
}
