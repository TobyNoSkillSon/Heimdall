use anyhow::Result;
use heimdall_archive::{
    compact, store,
    timeline::{
        self, BackfillGate, Chunk, Embedder, Model, ScreenChange, ScreenFrame, Search, Tag,
    },
    Journal, Record, DAY_NS,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::Path;
const S: u64 = 1_000_000_000;
const WALL: u64 = 1_800_000_000_000_000_000;
fn setup() -> (tempfile::TempDir, Connection, Journal, i64, i64) {
    let t = tempfile::tempdir().unwrap();
    let db = store::open(t.path()).unwrap();
    let j = Journal::open(&t.path().join("session-timeline")).unwrap();
    let sid = j.session_id();
    let seg = store::create_segment(&db, sid, "capture", WALL, 0, "native").unwrap();
    db.execute("INSERT INTO sources VALUES(?1,'process:call',?2,0,NULL)",params![seg,json!({"kind":{"Process":{"bundle_id":"helper","owner":{"bundle_id":"com.hnc.Discord"}}}}).to_string()]).unwrap();
    (t, db, j, sid, seg)
}
fn event(j: &Journal, kind: &str, source: &str, host: u64, payload: Value) -> Result<()> {
    j.append(&Record {
        kind: kind.into(),
        source: source.into(),
        host_ns: host,
        wall_ns: WALL + host,
        utterance_ids: vec![],
        payload,
    })
}
fn final_row(
    db: &Connection,
    sid: i64,
    id: &str,
    source: &str,
    start: u64,
    end: u64,
    text: &str,
    speaker: &str,
) -> Result<()> {
    let words=text.split_whitespace().enumerate().map(|(i,w)|json!({"text":w,"start_ns":start+i as u64*100_000_000,"end_ns":start+(i as u64+1)*100_000_000})).collect::<Vec<_>>();
    db.execute("INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,speaker,start_ns,end_ns,text,words) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![sid,id,((WALL+end)/1_000_000) as i64,source,speaker,start as i64,end as i64,text,serde_json::to_string(&words)?])?;
    Ok(())
}
fn all(db: &Connection) -> Result<Vec<Chunk>> {
    timeline::search(
        db,
        &Search {
            limit: 100,
            ..Default::default()
        },
    )
}
fn by_role(db: &Connection, role: &str) -> Result<Vec<Chunk>> {
    Ok(all(db)?.into_iter().filter(|c| c.role == role).collect())
}
fn count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
/// Bug note 22 Oct 23:57: turn 125 joined an echo from 23:40:42 with the user's words from 23:56:56. A turn reaches back
/// only across a short pause; after a long silence it starts fresh.
#[test]
fn a_turn_after_a_long_silence_does_not_join_the_utterance_before_it() -> Result<()> {
    let (_t, db, _j, sid, _) = setup();
    let later = S + 16 * 60 * S;
    for (id, start, text) in [
        ("a", S, "Plug your earphones back in until tonight's fix lands."),
        ("b", 10 * S, "Short pause, same talk."),
        ("c", later, "Hey Heimdall, I was watching a video."),
    ] {
        final_row(&db, sid, id, "process:call", start, start + 2 * S, text, "User")?;
        timeline::transaction(&db, || timeline::utterance_language(&db, sid, id, "en"))?;
    }
    let turns = by_role(&db, "turn")?;
    let turn = |id: &str| turns.iter().find(|c| c.id.ends_with(&format!(":text:turn:{id}"))).unwrap();
    assert!(turn("b").text.starts_with("Plug your earphones"), "a short pause keeps the overlap: {}", turn("b").text);
    let c = turn("c");
    assert_eq!(c.text, "Hey Heimdall, I was watching a video.");
    assert_eq!(c.start_ns, later);
    assert!(!c.links.iter().any(|l| l.relation == "overlap_chunk"));
    Ok(())
}
/// Bug note 2026-10-24T13-35-27: Heimdall's reply heard through the speakers was archived as its own voice, and the
/// next turn on the same microphone glued the echo's last sentence to the user's words and took Heimdall's speaker. The user's
/// turn keeps only his words and his label; his own turns still overlap each other.
#[test]
fn a_turn_never_joins_heimdalls_echo_with_owners_words() -> Result<()> {
    let (_t, db, _j, sid, _) = setup();
    for (id, start, text, speaker) in [
        ("echo", S, "Run it twice. On the second run, interrupt me halfway.", "Heimdall"),
        ("owner", 4 * S, "Okay, the speaker was open and it worked.", "User"),
        ("more", 8 * S, "I used the Shure microphone.", "User"),
    ] {
        final_row(&db, sid, id, "mic:shure", start, start + 2 * S, text, speaker)?;
        timeline::transaction(&db, || timeline::utterance_language(&db, sid, id, "en"))?;
    }
    let turns = by_role(&db, "turn")?;
    let turn = |id: &str| turns.iter().find(|c| c.id.ends_with(&format!(":text:turn:{id}"))).unwrap();
    let owner = turn("owner");
    assert_eq!(owner.text, "Okay, the speaker was open and it worked.");
    assert_eq!(owner.start_ns, 4 * S);
    assert!(!owner.links.iter().any(|l| l.relation == "overlap_chunk"));
    let speakers: Vec<_> = owner.tags.iter().filter(|t| t.key == "speaker").map(|t| t.value.as_str()).collect();
    assert_eq!(speakers, ["User"]);
    // His next utterance still reaches back to his own last sentence.
    assert!(turn("more").text.starts_with("Okay, the speaker was open"), "{}", turn("more").text);
    Ok(())
}
#[test]
fn finals_turn_overlap_exact_tags_quote_and_late_names_are_live_and_idempotent() -> Result<()> {
    let (_t, db, j, sid, _) = setup();
    event(
        &j,
        "timeline_settings",
        "heimdall",
        0,
        json!({"settings":{"dry_run":false,"paused":false}}),
    )?;
    for (id, start, text) in [
        ("a", S, "First sentence. Last sentence."),
        ("b", 5 * S, "Next turn."),
    ] {
        event(
            &j,
            "listen_event",
            "process:call",
            start,
            json!({"event":"final_utterance","end_ns":start+S,"forced":false}),
        )?;
        final_row(
            &db,
            sid,
            id,
            "process:call",
            start,
            start + S,
            text,
            "Speaker 1",
        )?;
        timeline::transaction(&db, || timeline::utterance_language(&db, sid, id, "en"))?;
    }
    let before = count(&db, "timeline_chunks");
    timeline::utterance_language(&db, sid, "b", "en")?;
    assert_eq!(count(&db, "timeline_chunks"), before);
    let mut q = Search {
        text: Some("Last sentence. Next".into()),
        kinds: vec!["text".into()],
        tags: vec![
            Tag {
                key: "app".into(),
                value: "com.hnc.Discord".into(),
            },
            Tag {
                key: "agent".into(),
                value: "on".into(),
            },
            Tag {
                key: "language".into(),
                value: "en".into(),
            },
        ],
        ..Default::default()
    };
    let hit = timeline::search(&db, &q)?;
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].role, "turn");
    assert_eq!(hit[0].start_ns, S + 200_000_000);
    assert_eq!(hit[0].end_ns, 6 * S);
    assert_eq!(hit[0].start_wall_ns, WALL + S + 200_000_000);
    assert!(hit[0].links.iter().any(|l| l.relation == "overlap_chunk"));
    assert_eq!(hit[0].details["boundary"], "smart_turn");
    db.execute(
        "UPDATE utterances SET speaker='Sam' WHERE session=?1 AND id='a'",
        [sid],
    )?;
    timeline::utterance(&db, sid, "a")?;
    let hit = timeline::search(&db, &q)?;
    assert!(hit[0]
        .tags
        .iter()
        .any(|t| t.key == "speaker" && t.value == "Sam"));
    q.tags.push(Tag {
        key: "speaker".into(),
        value: "Sa".into(),
    });
    assert!(timeline::search(&db, &q)?.is_empty());
    assert!(by_role(&db, "window")?
        .iter()
        .any(|m| m.tags.iter().any(|t| t.value == "Sam")));
    assert_eq!(count(&db, "chunk_embeddings"), 0);
    Ok(())
}
#[test]
fn gap_does_not_overlap_and_partial_or_silence_never_becomes_text() -> Result<()> {
    let (_t, db, j, sid, _) = setup();
    event(
        &j,
        "listen_event",
        "mic",
        S,
        json!({"event":"partial_transcript","text":"not final"}),
    )?;
    assert_eq!(count(&db, "timeline_chunks"), 0);
    final_row(&db, sid, "a", "mic", S, 2 * S, "hello.", "User")?;
    timeline::utterance(&db, sid, "a")?;
    event(&j, "gap", "mic", 3 * S, json!({"reason":"gap"}))?;
    final_row(&db, sid, "b", "mic", 4 * S, 5 * S, "later.", "User")?;
    timeline::utterance(&db, sid, "b")?;
    assert_eq!(
        by_role(&db, "turn")?
            .iter()
            .filter(|c| c.kind == "text" && c.text == "later.")
            .count(),
        1
    );
    assert!(by_role(&db, "utterance")?
        .iter()
        .all(|c| !c.text.contains("not final")));
    Ok(())
}
#[test]
fn command_phases_are_separate_small_evidence_linked_chunks_and_vector_slot_errors() -> Result<()> {
    let (_t, db, j, _sid, _) = setup();
    for phase in ["candidate", "chain", "abandoned"] {
        event(
            &j,
            "command_decision",
            "mic",
            S,
            json!({"phase":phase,"candidate":{"phrase_guess":"bug_note"},"transcript":"Bug note."}),
        )?;
    }
    let chunks = by_role(&db, "command")?;
    assert_eq!(chunks.len(), 3);
    assert!(chunks
        .iter()
        .all(|c| c.links.iter().any(|l| l.relation == "evidence")
            && c.tags
                .iter()
                .any(|t| t.key == "command_kind" && t.value == "bug_note")));
    assert!(timeline::search(
        &db,
        &Search {
            vector: Some(json!([1.])),
            ..Default::default()
        }
    )
    .unwrap_err()
    .to_string()
    .contains("no embedding"));
    assert!(timeline::search(
        &db,
        &Search {
            limit: 0,
            ..Default::default()
        }
    )
    .is_err());
    Ok(())
}
#[test]
fn speaker_revisions_named_tracks_and_non_speech_exclusion() -> Result<()> {
    let (_t, db, j, sid, _) = setup();
    timeline::audio_span(&db, sid, "process:call", 0, 60 * S, WALL)?;
    assert_eq!(by_role(&db, "sound")?.len(), 3);
    for total in [2, 1] {
        event(
            &j,
            "speaker_segments_delta",
            "process:call",
            0,
            json!({"origin_ns":0,"segments":[{"index":0,"segment":{"start_time":0.,"end_time":20.,"speaker":2}},{"index":1,"segment":{"start_time":20.,"end_time":40.,"speaker":3}}],"total_segments":total}),
        )?;
    }
    assert_eq!(by_role(&db, "speaker_turn")?.len(), 1);
    event(
        &j,
        "speaker_name",
        "process:call",
        30 * S,
        json!({"LabelChanged":{"track":{"source":"process:call","session":0,"speaker":2},"name":"Sam"}}),
    )?;
    assert!(by_role(&db, "speaker_turn")?[0]
        .tags
        .iter()
        .any(|t| t.value == "Sam"));
    let sounds = by_role(&db, "sound")?;
    assert_eq!(sounds.len(), 2);
    assert!(sounds.iter().all(|c| c.start_ns >= 20 * S));
    // A revised shorter segment re-opens precisely the newly unlabelled source span.
    event(
        &j,
        "speaker_segments_delta",
        "process:call",
        0,
        json!({"origin_ns":0,"segments":[{"index":0,"segment":{"start_time":0.,"end_time":10.,"speaker":2}}],"total_segments":1}),
    )?;
    assert_eq!(by_role(&db, "sound")?.len(), 3);
    assert_eq!(
        by_role(&db, "sound")?
            .iter()
            .find(|c| c.start_ns == 0)
            .unwrap()
            .details["sound_ranges_ns"],
        json!([[10 * S, 20 * S]])
    );
    Ok(())
}
#[test]
fn screen_fake_frames_close_on_change_cap_stop_and_context_is_joined() -> Result<()> {
    let (_t, db, _j, sid, _) = setup();
    db.execute(
        "INSERT INTO images VALUES('fake','image/png','hot','fake',?1,0)",
        [WALL as i64],
    )?;
    let mut f = ScreenFrame {
        display: "display:1".into(),
        host_ns: S,
        wall_ns: WALL + S,
        app: Some("com.apple.Safari".into()),
        window_title: "Page one".into(),
        image_id: "fake".into(),
        video_segment: Some("video-1".into()),
        change: ScreenChange::VisualChange,
    };
    let first = timeline::screen_frame(&db, sid, &f)?;
    f.host_ns = 10 * S;
    f.wall_ns = WALL + f.host_ns;
    f.change = ScreenChange::Unchanged;
    assert_eq!(timeline::screen_frame(&db, sid, &f)?, first);
    f.host_ns = 20 * S;
    f.wall_ns = WALL + f.host_ns;
    f.change = ScreenChange::Scroll;
    f.window_title = "Page two".into();
    let second = timeline::screen_frame(&db, sid, &f)?;
    assert_ne!(second, first);
    f.host_ns = 65 * S;
    f.wall_ns = WALL + f.host_ns;
    f.change = ScreenChange::Unchanged;
    let third = timeline::screen_frame(&db, sid, &f)?;
    assert_ne!(third, second);
    timeline::screen_stop(&db, sid, &f.display, 70 * S)?;
    f.host_ns = 72 * S;
    f.wall_ns = WALL + f.host_ns;
    assert_ne!(timeline::screen_frame(&db, sid, &f)?, third);
    let screens = by_role(&db, "state")?;
    assert_eq!(screens.len(), 4);
    assert!(screens
        .iter()
        .all(|c| c.end_ns - c.start_ns <= timeline::SCREEN_MAX_NS));
    assert!(screens
        .iter()
        .all(|c| c.links.iter().any(|l| l.relation == "video_segment")));
    assert!(by_role(&db, "window")?
        .iter()
        .any(|m| m.details.to_string().contains("Page two")));
    f.host_ns = 1;
    assert!(timeline::screen_frame(&db, sid, &f).is_err());
    Ok(())
}
#[test]
fn model_free_backfill_reads_cold_evidence_resumes_and_is_idempotent() -> Result<()> {
    let (t, db, j, sid, seg) = setup();
    for i in 0..8 {
        event(
            &j,
            "command_decision",
            "mic",
            i * S,
            json!({"phase":"chain","transcript":format!("command {i}")}),
        )?;
    }
    final_row(&db, sid, "a", "mic", S, 2 * S, "actual words", "User")?;
    // Nonzero and exact-zero app ranges; no raw bytes need decoding to build the index.
    for (host, codec) in [(0, "zero"), (S, "f32-zstd"), (40 * S, "f32-zstd")] {
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,'process:call',?2,16000,1,16000,?3,0,?4)",params![seg,host as i64,codec,(WALL+host) as i64])?;
    }
    db.execute("UPDATE evidence SET wall_ns=1", [])?;
    compact::compact_metadata(t.path(), DAY_NS + 100, false)?;
    assert_eq!(count(&db, "evidence"), 0);
    for table in [
        "timeline_chunks",
        "chunk_links",
        "chunk_tags",
        "timeline_state",
        "timeline_finals",
    ] {
        db.execute(&format!("DELETE FROM {table}"), [])?;
    }
    let mut left = 4;
    timeline::backfill(t.path(), &mut || {
        left -= 1;
        left > 0
    })?;
    assert!(by_role(&db, "command")?.len() < 8);
    while timeline::backfill(t.path(), &mut || true)? > 0 {}
    assert_eq!(by_role(&db, "command")?.len(), 8);
    assert_eq!(by_role(&db, "utterance")?.len(), 2);
    assert_eq!(by_role(&db, "sound")?.len(), 2);
    let before = count(&db, "timeline_chunks");
    assert_eq!(timeline::backfill(t.path(), &mut || true)?, 0);
    assert_eq!(count(&db, "timeline_chunks"), before);
    assert_eq!(count(&db, "chunk_embeddings"), 0);
    Ok(())
}
struct Fake {
    model: Model,
    calls: usize,
    values: Vec<f32>,
}
impl Embedder for Fake {
    fn model(&self) -> Model {
        self.model.clone()
    }
    fn embed(&mut self, _: &Path, _: &Chunk) -> Result<Vec<f32>> {
        self.calls += 1;
        Ok(self.values.clone())
    }
}
fn on() -> BackfillGate {
    BackfillGate {
        on_ac: true,
        idle_seconds: 300.,
    }
}
#[test]
fn embedding_interface_is_gated_resumable_versioned_and_invalidated() -> Result<()> {
    let (t, db, _j, sid, _) = setup();
    final_row(&db, sid, "a", "mic", S, 2 * S, "hello", "User")?;
    timeline::utterance(&db, sid, "a")?;
    let mut p = Fake {
        model: Model {
            id: "test_only".into(),
            version: "1".into(),
            dimensions: 2,
        },
        calls: 0,
        values: vec![0.2, 0.3],
    };
    for gate in [
        BackfillGate {
            on_ac: false,
            idle_seconds: 999.,
        },
        BackfillGate {
            on_ac: true,
            idle_seconds: 299.,
        },
        BackfillGate {
            on_ac: true,
            idle_seconds: f64::NAN,
        },
    ] {
        assert_eq!(
            timeline::embed_batch(t.path(), &mut p, 100, &mut || gate)?,
            0
        );
    }
    assert_eq!(p.calls, 0);
    let n = timeline::embed_batch(t.path(), &mut p, 100, &mut on)?;
    assert_eq!(n, count(&db, "timeline_chunks") as usize);
    assert_eq!(count(&db, "vectors"), 1);
    assert_eq!(timeline::embed_batch(t.path(), &mut p, 100, &mut on)?, 0);
    p.model.version = "2".into();
    assert_eq!(timeline::embed_batch(t.path(), &mut p, 100, &mut on)?, n);
    db.execute("UPDATE utterances SET speaker='New name' WHERE id='a'", [])?;
    timeline::utterance(&db, sid, "a")?;
    assert_eq!(count(&db, "chunk_embeddings"), 0);
    p.values = vec![f32::NAN, 1.];
    assert!(timeline::embed_batch(t.path(), &mut p, 1, &mut on).is_err());
    Ok(())
}
#[test]
fn gate_closing_during_inference_discards_the_result() -> Result<()> {
    let (t, db, _j, sid, _) = setup();
    final_row(&db, sid, "a", "mic", S, 2 * S, "hello", "User")?;
    timeline::utterance(&db, sid, "a")?;
    let mut p = Fake {
        model: Model {
            id: "test_only".into(),
            version: "1".into(),
            dimensions: 1,
        },
        calls: 0,
        values: vec![0.],
    };
    let mut gates = 0;
    let n = timeline::embed_batch(t.path(), &mut p, 10, &mut || {
        gates += 1;
        BackfillGate {
            on_ac: true,
            idle_seconds: if gates<3 {300.} else {0.},
        }
    })?;
    assert_eq!(p.calls, 1);
    assert_eq!(n, 0);
    assert_eq!(count(&db, "chunk_embeddings"), 0);
    Ok(())
}

#[test]
fn live_last_hour_and_cosine_search_preserve_revision_filters_and_dimensions() -> Result<()> {
    let (t,db,_j,sid,_) = setup();
    final_row(&db,sid,"old","mic",S,2*S,"old words","User")?;
    timeline::utterance(&db,sid,"old")?;
    final_row(&db,sid,"recent","mic",4000*S,4001*S,"recent words","User")?;
    timeline::utterance(&db,sid,"recent")?;
    let mut p=Fake {model:Model{id:"gate-test".into(),version:"1".into(),dimensions:2},calls:0,values:vec![1.,0.]};
    let now=WALL+4500*S;
    assert_eq!(timeline::embed_live(t.path(),&mut p,100,now,&mut ||false)?,0);
    assert_eq!(p.calls,0);
    let expected:i64=db.query_row("SELECT count(*) FROM timeline_chunks WHERE end_wall_ns>=?1",[(now-3600*S) as i64],|r|r.get(0))?;
    assert_eq!(timeline::embed_live(t.path(),&mut p,100,now,&mut ||true)?,expected as usize);
    assert_eq!(timeline::embed_live(t.path(),&mut p,100,now,&mut ||true)?,0);
    let vector=json!({"model_id":"gate-test","model_version":"1","values":[0.6,0.8]});
    let q=Search {vector:Some(vector.clone()),kinds:vec!["text".into()],tags:vec![Tag{key:"speaker".into(),value:"User".into()}],text:Some("recent".into()),limit:100,..Default::default()};
    let rows=timeline::search(&db,&q)?;
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|c| (c.similarity.unwrap()-0.6).abs()<1e-6 && c.text.contains("recent")));
    assert!(rows.windows(2).all(|c|c[0].id<c[1].id)); // deterministic equal-score order
    assert!(timeline::search(&db,&Search{vector:Some(json!({"model_id":"gate-test","model_version":"1","values":[0.,0.]})),..Default::default()}).is_err());
    assert!(timeline::search(&db,&Search{vector:Some(json!({"model_id":"gate-test","model_version":"wrong","values":[1.,0.]})),..Default::default()}).is_err());
    assert!(timeline::search(&db,&Search{vector:Some(json!({"model_id":"gate-test","model_version":"1","values":[1.]})),..Default::default()}).is_err());
    // An obsolete stored vector is never ranked even if its row remains after a crash.
    db.execute("UPDATE timeline_chunks SET content_hash='changed' WHERE text LIKE '%recent%'",[])?;
    let q=Search {vector:Some(vector),..q};
    let result=timeline::search(&db,&q);
    assert!(result.is_err() || result?.is_empty());
    Ok(())
}

#[test]
fn embedding_loader_preserves_stereo_mix_clipping_and_capture_gaps() -> Result<()> {
    use heimdall_archive::{audio,embedding::{prepare_chunk,Part}};
    let (t,db,_j,sid,seg)=setup();
    final_row(&db,sid,"media","process:call",S,4*S,"a recorded phrase","User")?;
    timeline::utterance(&db,sid,"media")?;
    for (host,left,right) in [(S,0.25_f32,0.75_f32),(3*S,-0.75_f32,-0.25_f32)] {
        let pcm=[left,right].repeat(16000);
        let id=audio::put_hot(&db,16000,2,&pcm)?;
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,wall_ns) VALUES(?1,'process:call',?2,16000,2,16000,'native-ref',0,?3,?4)",params![seg,host as i64,id,(WALL+host) as i64])?;
    }
    let chunk=timeline::load(&db,&format!("{sid}:speech_audio:utterance:media"))?;
    let temp=tempfile::tempdir()?;
    let inputs=prepare_chunk(t.path(),&chunk,temp.path())?;
    let clips=inputs[0].parts.iter().filter_map(|p|if let Part::Audio{path}=p {Some(audio::pcm_from_bytes(&std::fs::read(path).unwrap()).unwrap())}else{None}).collect::<Vec<_>>();
    assert_eq!(clips.len(),2); // no invented waveform for the missing second
    assert_eq!(clips[0],vec![0.5;16000]);assert_eq!(clips[1],vec![-0.5;16000]);
    let moment=all(&db)?.into_iter().find(|c|c.kind=="moment").unwrap();
    let fused=prepare_chunk(t.path(),&moment,temp.path())?;
    assert!(fused[0].parts.iter().any(|p|matches!(p,Part::Text{..})));
    assert_eq!(fused[0].parts.iter().filter(|p|matches!(p,Part::Audio{..})).count(),2);
    Ok(())
}
#[test]
fn failed_projection_rolls_back_and_time_filters_are_capture_time() -> Result<()> {
    let (_t, db, _j, sid, _) = setup();
    final_row(&db, sid, "a", "mic", S, 2 * S, "hello", "User")?;
    let error: Result<()> = timeline::transaction(&db, || {
        timeline::utterance(&db, sid, "a")?;
        anyhow::bail!("injected failure")
    });
    assert!(error.is_err());
    assert_eq!(count(&db, "timeline_chunks"), 0);
    timeline::utterance(&db, sid, "a")?;
    assert!(timeline::search(
        &db,
        &Search {
            start_wall_ns: Some(WALL + 3 * S),
            kinds: vec!["text".into()],
            ..Default::default()
        }
    )?
    .is_empty());
    assert_eq!(
        timeline::search(
            &db,
            &Search {
                start_wall_ns: Some(WALL + S),
                end_wall_ns: Some(WALL + 2 * S),
                kinds: vec!["text".into()],
                ..Default::default()
            }
        )?
        .len(),
        2
    );
    Ok(())
}
#[test]
fn bounded_audio_backfill_never_skips_or_starves_interleaved_sources() -> Result<()> {
    let (t, db, _j, sid, seg) = setup();
    // Insert alternating sources so bucket groups have interleaved block IDs.
    for i in 0..48 {
        for source in ["process:a", "process:b"] {
            db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,?2,?3,16000,1,16000,'f32-zstd',0,?4)",params![seg,source,(i*S) as i64,(WALL+i*S) as i64])?;
        }
    }
    let mut total = 0;
    for _ in 0..15 {
        let mut left = 3;
        let n = timeline::backfill(t.path(), &mut || {
            left -= 1;
            left > 0
        })?;
        total += n;
        if n == 0 {
            break;
        }
    }
    assert!(total > 0);
    assert_eq!(timeline::backfill(t.path(), &mut || true)?, 0);
    assert_eq!(by_role(&db, "sound")?.len(), 6);
    assert!(
        db.query_row(
            "SELECT block_id FROM timeline_backfill WHERE session=?1",
            [sid],
            |r| r.get::<_, i64>(0)
        )? >= 96
    );
    assert!(by_role(&db, "sound")?
        .iter()
        .all(|c| c.end_ns - c.start_ns <= 20 * S));
    Ok(())
}
#[test]
fn audio_interval_links_resolve_after_callback_rows_are_packed() -> Result<()> {
    let (t, db, _j, sid, seg) = setup();
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,'mic',?2,16000,1,16000,'zero',0,1)",params![seg,S as i64])?;
    final_row(&db, sid, "a", "mic", S, 2 * S, "hello", "User")?;
    timeline::utterance(&db, sid, "a")?;
    let chunk = by_role(&db, "utterance")?
        .into_iter()
        .find(|c| c.kind == "speech_audio")
        .unwrap();
    let before = timeline::audio_refs(t.path(), &chunk)?;
    assert_eq!(before.len(), 1);
    compact::compact_metadata(t.path(), DAY_NS + 10 * S, true)?;
    assert_eq!(count(&db, "blocks"), 0);
    assert_eq!(timeline::audio_refs(t.path(), &chunk)?, before);
    Ok(())
}

#[test]
fn backfill_tail_checkpoints_do_not_skip_later_appends() -> Result<()> {
    let (t, db, j, sid, seg) = setup();
    event(
        &j,
        "command_decision",
        "mic",
        S,
        json!({"phase":"candidate"}),
    )?;
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,'process:call',?2,16000,1,16000,'f32-zstd',0,?3)",params![seg,S as i64,(WALL+S) as i64])?;
    while timeline::backfill(t.path(), &mut || true)? > 0 {}
    assert_eq!(
        db.query_row(
            "SELECT evidence_id,block_id FROM timeline_backfill WHERE session=?1",
            [sid],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        )?,
        (1, 1)
    );
    event(
        &j,
        "command_decision",
        "mic",
        2 * S,
        json!({"phase":"chain"}),
    )?;
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,'process:call',?2,16000,1,16000,'f32-zstd',0,?3)",params![seg,(41*S) as i64,(WALL+41*S) as i64])?;
    while timeline::backfill(t.path(), &mut || true)? > 0 {}
    assert_eq!(by_role(&db, "sound")?.len(), 2);
    assert_eq!(
        db.query_row(
            "SELECT evidence_id,block_id FROM timeline_backfill WHERE session=?1",
            [sid],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        )?,
        (2, 2)
    );
    // A nullable owner bundle must still retain an available emitting-app bundle tag.
    db.execute(
        "UPDATE sources SET metadata_json=?1",
        [
            json!({"kind":{"Process":{"owner":{"bundle_id":null},"bundle_id":"org.example.app"}}})
                .to_string(),
        ],
    )?;
    final_row(
        &db,
        sid,
        "u",
        "process:call",
        50 * S,
        51 * S,
        "quote",
        "unknown",
    )?;
    timeline::utterance(&db, sid, "u")?;
    assert!(by_role(&db, "utterance")?.iter().all(|c| c
        .tags
        .iter()
        .any(|t| t.key == "app" && t.value == "org.example.app")));
    Ok(())
}

#[test]
fn chunk_backfill_requires_idle_ac_and_cancels_when_activity_resumes() -> Result<()> {
    let (t,db,j,_,_)=setup();
    for i in 0..10 {event(&j,"command_decision","mic",i*S,json!({"phase":"chain","transcript":format!("command {i}")}))?;}
    db.execute_batch("DELETE FROM chunk_links; DELETE FROM chunk_tags; DELETE FROM timeline_chunks;")?;
    for gate in [BackfillGate{on_ac:false,idle_seconds:999.},BackfillGate{on_ac:true,idle_seconds:0.},BackfillGate{on_ac:true,idle_seconds:f64::NAN}] {
        // The emergency audio gate may be open; it cannot admit chunk catch-up.
        assert_eq!(compact::backfill_chunks_while(t.path(),&mut ||true,&mut ||gate.allowed())?,0);
        assert_eq!(count(&db,"timeline_backfill"),0);
        assert_eq!(count(&db,"timeline_chunks"),0);
    }
    let mut checks=0;
    compact::backfill_chunks_while(t.path(),&mut ||true,&mut ||{checks+=1;checks<5})?;
    assert!(by_role(&db,"command")?.len()<10);
    while compact::backfill_chunks_while(t.path(),&mut ||true,&mut ||on().allowed())?>0 {}
    assert_eq!(by_role(&db,"command")?.len(),10);
    Ok(())
}

#[test]
fn embedding_retry_failures_remain_health_until_repaired_and_ignore_stale_revisions() -> Result<()> {
    let (t,db,_j,sid,_)=setup();
    final_row(&db,sid,"a","mic",S,2*S,"hello","User")?;
    timeline::utterance(&db,sid,"a")?;
    struct Fails;
    impl Embedder for Fails {
        fn model(&self)->Model {Model{id:"health-test".into(),version:"1".into(),dimensions:2}}
        fn embed(&mut self,_:&Path,_:&Chunk)->Result<Vec<f32>> {anyhow::bail!("broken recording")}
    }
    let mut fails=Fails;
    assert_eq!(timeline::embed_batch(t.path(),&mut fails,100,&mut on)?,0);
    let status=timeline::embedding_error_status(t.path(),&fails.model())?.unwrap();
    assert_eq!(status,"Search indexing is failing because some items cannot be read.");
    // Backoff returns success/no work; health still exposes the saved errors.
    assert_eq!(timeline::embed_batch(t.path(),&mut fails,100,&mut on)?,0);
    assert_eq!(timeline::embedding_error_status(t.path(),&fails.model())?,Some(status));
    let mut fixed=Fake{model:fails.model(),calls:0,values:vec![1.,0.]};
    db.execute("UPDATE chunk_embedding_errors SET retry_wall_ns=0",[])?;
    assert_eq!(timeline::embed_batch(t.path(),&mut fixed,1,&mut on)?,1);
    assert!(timeline::embedding_error_status(t.path(),&fixed.model)?.is_some());
    timeline::embed_batch(t.path(),&mut fixed,100,&mut on)?;
    assert!(timeline::embedding_error_status(t.path(),&fixed.model)?.is_none());
    db.execute_batch("DELETE FROM chunk_embeddings; DELETE FROM vectors;")?;
    timeline::embed_batch(t.path(),&mut fails,100,&mut on)?;
    let mut other=fails.model();other.version="2".into();
    assert!(timeline::embedding_error_status(t.path(),&other)?.is_none());
    db.execute("UPDATE chunk_embedding_errors SET content_hash='obsolete'",[])?;
    assert!(timeline::embedding_error_status(t.path(),&fails.model())?.is_none());
    Ok(())
}

#[test]
fn speaker_turn_indexing_waits_for_committed_samples_and_backfill_repairs_old_errors() -> Result<()> {
    use heimdall_archive::{audio, embedding::{prepare_chunk, Part}};
    struct Samples { turns: usize }
    impl Embedder for Samples {
        fn model(&self) -> Model { Model { id:"sample-test".into(), version:"1".into(), dimensions:2 } }
        fn embed(&mut self, root:&Path, c:&Chunk) -> Result<Vec<f32>> {
            if c.role == "speaker_turn" {
                let temp = tempfile::tempdir()?;
                let inputs = prepare_chunk(root, c, temp.path())?;
                assert!(inputs.iter().flat_map(|i| &i.parts).any(|p| matches!(p,Part::Audio{..})));
                self.turns += 1;
            }
            Ok(vec![0.2,0.3])
        }
    }
    let (t, db, j, sid, seg) = setup();
    event(&j,"speaker_segments_delta","process:call",0,json!({"origin_ns":0,"segments":[
        {"index":0,"segment":{"start_time":10.,"end_time":11.,"speaker":1}},
        {"index":1,"segment":{"start_time":12.,"end_time":13.,"speaker":2}}
    ],"total_segments":2}))?;
    let mut provider = Samples { turns:0 };
    timeline::embed_batch(t.path(), &mut provider, 100, &mut on)?;
    assert_eq!(provider.turns,0,"speaker evidence must wait for the capture commit");
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    // Simulate the old build's error before capture flushed its one-second buffer.
    let id = format!("{sid}:speech_audio:speaker_turn:process:call:0:0");
    let c = timeline::load(&db,&id)?;
    db.execute("INSERT INTO chunk_embedding_errors VALUES(?1,'sample-test','1',?2,?3,?4)",params![id,c.content_hash,(WALL+100*S) as i64,format!("audio chunk {id} has no captured samples for process:call")])?;
    let pcm = vec![0.25;16000];
    let object = audio::put_hot(&db,16000,1,&pcm)?;
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,wall_ns) VALUES(?1,'process:call',?2,16000,1,16000,'native-ref',0,?3,?4)",params![seg,(10*S) as i64,object,(WALL+10*S) as i64])?;
    // An old completed evidence checkpoint must not strand its sample error.
    db.execute("INSERT INTO timeline_backfill VALUES(?1,9223372036854775807,0,9223372036854775807)",[sid])?;
    assert_eq!(timeline::backfill(t.path(),&mut ||true)?,1);
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    timeline::embed_batch(t.path(),&mut provider,100,&mut on)?;
    assert_eq!(provider.turns,1);
    let temp = tempfile::tempdir()?;
    let inputs = prepare_chunk(t.path(),&timeline::load(&db,&id)?,temp.path())?;
    let path = inputs[0].parts.iter().find_map(|p|if let Part::Audio{path}=p {Some(path)} else {None}).unwrap();
    assert_eq!(audio::pcm_from_bytes(&std::fs::read(path)?)?,pcm,"the full audio tail is embedded");
    // A retired row stays retired: repair must not delete it and send the turn back to the
    // embedder on every pass (review, 23 Oct).
    let c = timeline::load(&db,&id)?;
    db.execute("INSERT OR REPLACE INTO chunk_embedding_errors VALUES(?1,'sample-test','1',?2,?3,?4)",params![id,c.content_hash,i64::MAX,format!("audio chunk {id} has no captured samples for process:call")])?;
    timeline::backfill(t.path(),&mut ||true)?;
    assert_eq!(count(&db,"chunk_embedding_errors"),1,"a retired row survives the speaker repair pass");
    db.execute("DELETE FROM chunk_embedding_errors WHERE chunk_id=?1",[&id])?;
    // The second span is genuinely absent, not unflushed: completed backfill retires
    // its projection and errors through remove(), preserving the diarization evidence.
    let missing = format!("{sid}:speech_audio:speaker_turn:process:call:0:1");
    let c = timeline::load(&db,&missing)?;
    db.execute("INSERT INTO chunk_embedding_errors VALUES(?1,'sample-test','1',?2,0,'audio chunk missing has no captured samples for process:call')",params![missing,c.content_hash])?;
    // Source removal seals a failed old Quit's session even if it was never marked complete.
    db.execute("UPDATE sources SET removed_ns=?2 WHERE segment=?1",params![seg,(13*S) as i64])?;
    assert!(!store::is_complete(&t.path().join("session-timeline"))?);
    assert_eq!(timeline::backfill(t.path(),&mut ||true)?,1);
    assert!(timeline::load(&db,&missing).is_err());
    store::mark_complete(&t.path().join("session-timeline"))?;
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    assert_eq!(timeline::backfill(t.path(),&mut ||true)?,0);
    assert_eq!(count(&db,"evidence"),1);
    Ok(())
}

#[test]
fn embedding_database_contention_retries_without_persistent_health_error() -> Result<()> {
    let (t,db,_j,sid,_)=setup();
    final_row(&db,sid,"busy","mic",S,2*S,"hello","User")?;
    timeline::utterance(&db,sid,"busy")?;
    struct Busy;
    impl Embedder for Busy {
        fn model(&self)->Model {Model{id:"busy-test".into(),version:"1".into(),dimensions:2}}
        fn embed(&mut self,_:&Path,_:&Chunk)->Result<Vec<f32>> {
            Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(5),Some("database is locked".into())).into())
        }
    }
    let mut busy=Busy;
    assert_eq!(timeline::embed_batch(t.path(),&mut busy,100,&mut on)?,0);
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    assert!(timeline::embedding_error_status(t.path(),&busy.model())?.is_none());
    // Old installed versions persisted contention as a five-minute chunk error.
    db.execute("INSERT INTO chunk_embedding_errors SELECT id,'busy-test','1',content_hash,?1,'database is locked' FROM timeline_chunks",[(heimdall_archive::wall_ns()+300*S) as i64])?;
    assert!(timeline::embedding_error_status(t.path(),&busy.model())?.is_none());
    let mut fixed=Fake{model:busy.model(),calls:0,values:vec![1.,0.]};
    assert!(timeline::embed_batch(t.path(),&mut fixed,100,&mut on)?>0);
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    Ok(())
}

/// 22 Oct: native overflow left holes inside process-tap sound chunks. A range wholly in a
/// hole failed the entire chunk, and a chunk wholly in a hole retried every five minutes
/// forever, holding the menu's health line orange with a raw chunk id.
#[test]
fn sound_chunks_in_capture_holes_embed_present_audio_and_retire_missing_audio() -> Result<()> {
    use heimdall_archive::{audio, embedding::{prepare_chunk, Part}};
    struct Sounds { calls: usize }
    impl Embedder for Sounds {
        fn model(&self) -> Model { Model { id:"hole-test".into(), version:"1".into(), dimensions:2 } }
        fn embed(&mut self, root:&Path, c:&Chunk) -> Result<Vec<f32>> {
            if c.kind == "other_audio" {
                self.calls += 1;
                let temp = tempfile::tempdir()?;
                prepare_chunk(root, c, temp.path())?;
            }
            Ok(vec![0.2,0.3])
        }
    }
    let (t, db, j, sid, seg) = setup();
    // Captured callbacks [0,1) s and [3,4) s; the capture flush spans [0,4) s anyway.
    for host in [0, 3*S] {
        let object = audio::put_hot(&db,16000,1,&vec![0.25;16000])?;
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,wall_ns) VALUES(?1,'process:call',?2,16000,1,16000,'native-ref',0,?3,?4)",params![seg,host as i64,object,(WALL+host) as i64])?;
    }
    event(&j,"speaker_segments_delta","process:call",0,json!({"origin_ns":0,"segments":[
        {"index":0,"segment":{"start_time":0.5,"end_time":1.5,"speaker":1}},
        {"index":1,"segment":{"start_time":2.5,"end_time":3.5,"speaker":1}}
    ],"total_segments":2}))?;
    timeline::audio_span(&db, sid, "process:call", 0, 4*S, WALL)?;
    let partial = timeline::load(&db,&format!("{sid}:other_audio:sound:process:call:0"))?;
    assert_eq!(partial.details["sound_ranges_ns"],json!([[0,S/2],[3*S/2,5*S/2],[7*S/2,4*S]]));
    let temp = tempfile::tempdir()?;
    let inputs = prepare_chunk(t.path(),&partial,temp.path())?;
    assert_eq!(inputs[0].parts.iter().filter(|p|matches!(p,Part::Audio{..})).count(),2,"the captured ranges embed; the hole is skipped");
    // A flush whose span lies wholly inside a hole: no sample exists anywhere in it.
    timeline::audio_span(&db, sid, "process:call", 20*S, 22*S, WALL+20*S)?;
    let hole = format!("{sid}:other_audio:sound:process:call:1");
    let retry = |id:&str| db.query_row("SELECT retry_wall_ns FROM chunk_embedding_errors WHERE chunk_id=?1",[id],|r|r.get::<_,i64>(0));
    let mut provider = Sounds { calls:0 };
    timeline::embed_batch(t.path(),&mut provider,100,&mut on)?;
    assert_eq!(provider.calls,2);
    assert_eq!(count(&db,"chunk_embedding_errors"),1,"only the chunk without any audio fails");
    // Capture has not passed it yet (a flush could still land): retried, never health.
    assert!(retry(&hole)? < i64::MAX);
    assert_eq!(timeline::embedding_error_status(t.path(),&provider.model())?,None);
    // The tap closes: the audio can no longer arrive, so the chunk is retired.
    db.execute("UPDATE sources SET removed_ns=?2 WHERE segment=?1",params![seg,(4*S) as i64])?;
    db.execute("UPDATE chunk_embedding_errors SET retry_wall_ns=0",[])?;
    timeline::embed_batch(t.path(),&mut provider,100,&mut on)?;
    assert_eq!(provider.calls,3);
    assert_eq!(retry(&hole)?,i64::MAX);
    timeline::embed_batch(t.path(),&mut provider,100,&mut on)?;
    assert_eq!(provider.calls,3,"a retired chunk is never retried");
    assert_eq!(timeline::embedding_error_status(t.path(),&provider.model())?,None);
    // A row left by the old build (raw text, five-minute retry) retires on the next pass
    // and never reaches the status in the meantime; no manual database edit.
    let c = timeline::load(&db,&hole)?;
    db.execute("INSERT OR REPLACE INTO chunk_embedding_errors VALUES(?1,'hole-test','1',?2,0,?3)",params![hole,c.content_hash,format!("audio chunk {hole} has no captured samples for process:call [{},{})",20*S,22*S)])?;
    assert_eq!(timeline::embedding_error_status(t.path(),&provider.model())?,None);
    timeline::embed_batch(t.path(),&mut provider,100,&mut on)?;
    assert_eq!(retry(&hole)?,i64::MAX);
    // New audio revises the chunk: the retirement no longer applies.
    let object = audio::put_hot(&db,16000,1,&vec![0.25;16000])?;
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,wall_ns) VALUES(?1,'process:call',?2,16000,1,16000,'native-ref',0,?3,?4)",params![seg,(23*S) as i64,object,(WALL+23*S) as i64])?;
    timeline::audio_span(&db, sid, "process:call", 20*S, 24*S, WALL+20*S)?;
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    timeline::embed_batch(t.path(),&mut provider,100,&mut on)?;
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM chunk_embeddings WHERE chunk_id=?1)",[&hole],|r|r.get::<_,bool>(0))?);
    Ok(())
}

/// "Query returned no rows" in the menu: a speaker or transcript revision retracted the
/// chunk while it was being embedded, and the publish re-read failed the whole job.
#[test]
fn chunks_retracted_during_inference_are_skipped_quietly() -> Result<()> {
    let (t,db,_j,sid,_)=setup();
    final_row(&db,sid,"gone","mic",S,2*S,"hello there","User")?;
    timeline::utterance(&db,sid,"gone")?;
    struct Retracts { root:std::path::PathBuf, sid:i64, fail:bool, calls:usize }
    impl Embedder for Retracts {
        fn model(&self)->Model {Model{id:"retract-test".into(),version:"1".into(),dimensions:2}}
        fn embed(&mut self,_:&Path,_:&Chunk)->Result<Vec<f32>> {
            self.calls+=1;
            let db=store::open(&self.root)?;
            timeline::transaction(&db,||timeline::remove_utterance(&db,self.sid,"gone"))?;
            if self.fail {anyhow::bail!(rusqlite::Error::QueryReturnedNoRows)}
            Ok(vec![1.,0.])
        }
    }
    let mut p=Retracts{root:t.path().into(),sid,fail:false,calls:0};
    assert_eq!(timeline::embed_batch(t.path(),&mut p,100,&mut on)?,0);
    assert_eq!(p.calls,1,"later selections that vanished are skipped before inference");
    assert_eq!(count(&db,"chunk_embeddings"),0);
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    // The failure came from the vanished content (e.g. a moment's retracted member).
    final_row(&db,sid,"gone","mic",S,2*S,"hello again","User")?;
    timeline::utterance(&db,sid,"gone")?;
    let mut p=Retracts{root:t.path().into(),sid,fail:true,calls:0};
    assert_eq!(timeline::embed_live(t.path(),&mut p,100,WALL,&mut ||true)?,0);
    assert_eq!(count(&db,"chunk_embedding_errors"),0);
    assert_eq!(timeline::embedding_error_status(t.path(),&p.model())?,None);
    Ok(())
}

#[test]
fn embedding_failure_status_is_one_plain_menu_sentence() {
    for (cause,why) in [
        ("audio chunk 39:other_audio:sound:process:55657:1:29:70075 has no decodable block 12","some recordings cannot be read"),
        ("Query returned no rows","the archive database returned an error"),
        ("EmbeddingGemma 2 runtime assets unavailable at /Users/x/models","the search model is not installed"),
        ("embedding worker timed out","the search model is not responding correctly"),
        ("screen state has no representative frame","some screenshots cannot be read"),
        ("write audio-0.f32: No space left on device (os error 28)","the disk is full"),
    ] {
        let s=timeline::embedding_failure_status(cause);
        assert_eq!(s,format!("Search indexing is failing because {why}."));
        assert!(s.len()<110 && !s.contains(':'));
    }
}

/// "We shouldn't embed from second to second ... a 10 or 15 minute buffer
/// and then do it all at once." Live work waits until its oldest chunk is a buffer old.
#[test]
fn live_buffer_waits_for_the_oldest_pending_chunk_then_drains_every_group() -> Result<()> {
    use std::time::Duration;
    let (t,db,_j,sid,_) = setup();
    let model=Model{id:"buffer-test".into(),version:"1".into(),dimensions:2};
    let now=WALL+4500*S;
    assert_eq!(timeline::oldest_pending_live(t.path(),&model,now)?,None,"nothing pending");
    for (i,id) in ["a","b","c"].iter().enumerate() {
        let start=4000*S+i as u64*20*S;
        final_row(&db,sid,id,"mic",start,start+5*S,&format!("buffered words {id}"),"User")?;
        timeline::utterance(&db,sid,id)?;
    }
    let oldest:i64=db.query_row("SELECT min(end_wall_ns) FROM timeline_chunks",[],|r|r.get(0))?;
    let oldest=oldest as u64;
    let pending:i64=db.query_row("SELECT count(*) FROM timeline_chunks",[],|r|r.get(0))?;
    assert!(pending>=3);
    assert_eq!(timeline::oldest_pending_live(t.path(),&model,oldest+60*S)?,Some(oldest));
    // Younger than the buffer: not due, and nothing runs.
    let mut buffer=timeline::LiveBuffer::new(Duration::from_secs(12*60));
    assert!(!buffer.due(t.path(),&model,oldest+11*60*S)?);
    // ZERO keeps today's continuous behaviour: due whenever something is pending.
    assert!(timeline::LiveBuffer::new(Duration::ZERO).due(t.path(),&model,oldest)?);
    // Once due, every pass embeds one group until a pass embeds nothing.
    let at=oldest+12*60*S;
    let mut p=Fake{model:model.clone(),calls:0,values:vec![1.,0.]};
    let mut passes=0;
    while buffer.due(t.path(),&model,at)? {
        passes+=1;
        assert!(passes<=pending+1,"flush must end");
        let n=timeline::embed_live(t.path(),&mut p,1,at,&mut ||true)?;
        assert!(n<=1,"one group per provider hold");
        buffer.embedded(n);
    }
    assert_eq!(p.calls as i64,pending,"one call per pending chunk");
    assert_eq!(passes,pending+1,"the flush ends on the pass that finds nothing");
    assert_eq!(count(&db,"chunk_embeddings"),pending);
    assert!(!buffer.flushing());
    assert_eq!(timeline::oldest_pending_live(t.path(),&model,at)?,None);
    assert!(!timeline::LiveBuffer::new(Duration::ZERO).due(t.path(),&model,at)?,"ZERO with nothing pending");
    Ok(())
}

/// While chunks wait in the buffer, semantic search shows those whose text holds a query word.
#[test]
fn pending_literal_finds_recent_unembedded_text_only() -> Result<()> {
    let (t,db,_j,sid,_) = setup();
    let model=Model{id:"pending-test".into(),version:"1".into(),dimensions:2};
    final_row(&db,sid,"old","mic",S,2*S,"Gemma overhaul planned","User")?;
    timeline::utterance(&db,sid,"old")?;
    final_row(&db,sid,"new","mic",4000*S,4001*S,"the GEMMA overhaul landed","User")?;
    timeline::utterance(&db,sid,"new")?;
    final_row(&db,sid,"other","mic",4002*S,4003*S,"unrelated chatter","User")?;
    timeline::utterance(&db,sid,"other")?;
    let since=WALL+3000*S;
    let words=timeline::literal_words("when did the gemma overhaul land?");
    assert_eq!(words,["gemma","land","overhaul","when"]);
    let ids=timeline::pending_literal(&db,&model,since,None,&words,&[],&[],10)?;
    assert!(!ids.is_empty());
    let texts:Vec<String>=ids.iter().map(|id|timeline::load(&db,id).map(|c|c.text)).collect::<Result<_>>()?;
    assert!(texts.iter().all(|t|t.contains("GEMMA")),"recent, case-insensitive, matching only: {texts:?}");
    // Embedded chunks are ranked by meaning instead, so they leave the fallback.
    let mut p=Fake{model:model.clone(),calls:0,values:vec![1.,0.]};
    timeline::embed_live(t.path(),&mut p,100,WALL+4500*S,&mut ||true)?;
    assert!(timeline::pending_literal(&db,&model,since,None,&words,&[],&[],10)?.is_empty());
    assert_eq!(timeline::literal_words("on it"),["on it"]);
    Ok(())
}

/// `LIVE_BUFFER` set to zero embeds whenever something is pending: a chunk is due the moment it is stored, the flush
/// ends once nothing is left, and the next chunk is due at once again (no wait for a buffer).
#[test]
fn live_buffer_zero_embeds_whenever_pending() -> Result<()> {
    use std::time::Duration;
    let (t,db,_j,sid,_) = setup();
    let model=Model{id:"zero-buffer-test".into(),version:"1".into(),dimensions:2};
    let mut buffer=timeline::LiveBuffer::new(Duration::ZERO);
    let mut p=Fake{model:model.clone(),calls:0,values:vec![1.,0.]};
    let mut drain=|buffer:&mut timeline::LiveBuffer,at:u64| -> Result<usize> {
        let mut total=0;
        while buffer.due(t.path(),&model,at)? {
            let n=timeline::embed_live(t.path(),&mut p,1,at,&mut ||true)?;
            buffer.embedded(n);
            total+=n;
            assert!(total<100,"flush must end");
        }
        Ok(total)
    };
    assert!(!buffer.due(t.path(),&model,WALL+4500*S)?,"nothing pending");
    final_row(&db,sid,"first","mic",4000*S,4005*S,"zero buffer first","User")?;
    timeline::utterance(&db,sid,"first")?;
    let stored:i64=db.query_row("SELECT max(end_wall_ns) FROM timeline_chunks",[],|r|r.get(0))?;
    let stored=stored as u64;
    // Due at the very moment the chunk ends, not a buffer later.
    let first=drain(&mut buffer,stored)?;
    assert!(first>=1);
    assert!(!buffer.flushing());
    assert_eq!(timeline::oldest_pending_live(t.path(),&model,stored)?,None);
    final_row(&db,sid,"second","mic",4010*S,4015*S,"zero buffer second","User")?;
    timeline::utterance(&db,sid,"second")?;
    let stored:i64=db.query_row("SELECT max(end_wall_ns) FROM timeline_chunks",[],|r|r.get(0))?;
    assert!(drain(&mut buffer,stored as u64)?>=1,"the next chunk is due at once");
    assert_eq!(timeline::oldest_pending_live(t.path(),&model,stored as u64)?,None);
    Ok(())
}
