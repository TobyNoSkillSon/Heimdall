use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// 10:00 local on a day, as epoch seconds, and that day's start.
fn morning() -> (i64, i64) {
    use chrono::TimeZone;
    let t = chrono::Local.with_ymd_and_hms(2026, 10, 23, 10, 0, 0).earliest().unwrap().timestamp();
    (t, day_of(t).1)
}

fn screen(app: &str, from: i64, to: i64) -> Screen { Screen { start: from, end: to, app: app.into(), title: None } }
fn line(speaker: &str, at: i64, text: &str) -> Line { Line { at, speaker: speaker.into(), text: text.into() } }

/// 10:00-10:45 in T3 Code talking with Heimdall, nothing until 11:30, then a Discord call until 12:00.
fn sample() -> (Evidence, i64) {
    let (t, _) = morning();
    let mut ev = Evidence::default();
    ev.screens.push(screen("com.t3tools.t3code", t, t + 45 * 60));
    for k in 0..6 { ev.lines.push(line(if k % 2 == 0 { "User" } else { "Heimdall" }, t + k * 400, "the islands branch")) }
    ev.screens.push(screen("com.hnc.Discord", t + 90 * 60, t + 120 * 60));
    for k in 0..8 { ev.lines.push(line("Morgan", t + 90 * 60 + k * 120, "mac mini")) }
    (ev, t + 120 * 60)
}

#[test]
fn the_day_starts_at_five_so_the_small_hours_belong_to_the_evening_before() {
    use chrono::TimeZone;
    let night = chrono::Local.with_ymd_and_hms(2026, 10, 24, 1, 10, 0).earliest().unwrap().timestamp();
    let (date, start) = day_of(night);
    assert_eq!(date, "2026-10-23");
    assert_eq!(chrono::Local.timestamp_opt(start, 0).unwrap().format("%d %H:%M").to_string(), "23 05:00");
    let (t, _) = morning();
    assert_eq!(day_of(t).0, "2026-10-23");
    // 25 Oct 2026, clocks go back at 03:00: 04:30 is still the evening of the 24th, 05:00 starts the 25th.
    let late = chrono::Local.with_ymd_and_hms(2026, 10, 25, 4, 30, 0).earliest().unwrap().timestamp();
    assert_eq!(day_of(late).0, "2026-10-24");
    let (date, start) = day_of(late + 31 * 60);
    assert_eq!((date.as_str(), chrono::Local.timestamp_opt(start, 0).unwrap().format("%H:%M").to_string().as_str()), ("2026-10-25", "05:00"));
    let spring = chrono::Local.with_ymd_and_hms(2027, 3, 28, 5, 30, 0).earliest().unwrap().timestamp();
    assert_eq!(day_of(spring).0, "2027-03-28");
}

#[test]
fn segments_carry_apps_people_and_a_kind_and_gaps_stay_empty() {
    let (ev, now) = sample();
    let (t, start) = morning();
    let segs = segments(&ev, start, now);
    let at = |x: i64| &segs[((x - start) / SEGMENT_S) as usize];
    let first = at(t);
    assert_eq!(first.apps, vec![("T3 Code".to_string(), 15 * 60)]);
    assert_eq!(first.kind, "build");
    assert!(first.active() && first.people.iter().any(|p| p.0 == "Heimdall"));
    assert!(!at(t + 60 * 60).active(), "nothing at 11:00");
    assert_eq!(at(t + 95 * 60).kind, "people", "a call outweighs the app in front");
    assert!(!at(t - 60 * 60).active(), "nothing before 10:00");
    assert_eq!(app_name("com.citrolabs.ego.lite"), "Ego");
    assert_eq!(app_name("org.example.someTool"), "SomeTool");
    assert_eq!(app_kind("com.valvesoftware.steam.helper"), "play");
}

#[test]
fn without_a_model_blocks_follow_the_tags_and_split_at_gaps() {
    let (ev, now) = sample();
    let (_, start) = morning();
    let segs = segments(&ev, start, now);
    let active: Vec<&Segment> = segs.iter().filter(|s| s.active()).collect();
    let b = fallback_blocks(&active);
    assert_eq!(b.len(), 2, "{b:?}");
    assert_eq!((b[0].title.as_str(), b[0].kind.as_str()), ("T3 Code", "build"));
    assert_eq!((b[1].title.as_str(), b[1].kind.as_str()), ("Call with Morgan", "people"));
    assert!(!b[0].named && b[0].segments.len() == 3);
    let (t, _) = morning();
    assert_eq!((b[0].start, b[0].end), (t, t + 45 * 60));
}

fn answer(blocks: Value) -> String { json!({"blocks": blocks}).to_string() }

#[test]
fn answers_must_cover_every_segment_once_in_order_without_bridging_a_gap() {
    let (ev, now) = sample();
    let (_, start) = morning();
    let segs = segments(&ev, start, now);
    let open: Vec<&Segment> = segs.iter().filter(|s| s.active()).collect();
    let ids: Vec<String> = open.iter().map(|s| s.id()).collect();
    let good = answer(json!([
        {"segments": ids[..3], "title": "Heimdall islands", "line": "Planned the island", "kind": "build"},
        {"segments": ids[3..], "title": "Call with Morgan", "line": "", "kind": "people"}]));
    let b = validate(&good, &open).unwrap();
    assert_eq!(b.len(), 2);
    assert!(b[0].named && b[0].title == "Heimdall islands");
    let gap = answer(json!([{"segments": ids, "title": "All", "line": "", "kind": "other"}]));
    assert!(validate(&gap, &open).unwrap_err().to_string().contains("gap"));
    let short = answer(json!([{"segments": ids[..3], "title": "A", "line": "", "kind": "build"}]));
    assert!(validate(&short, &open).is_err(), "a segment left out");
    // 23 Oct 21:30, the replay: the model put a quiet quarter between two open ones into a block.
    let filled = answer(json!([
        {"segments": [ids[0].clone(), ids[1].clone(), ids[2].clone(), "s99".to_string()], "title": "A", "line": "", "kind": "build"},
        {"segments": ids[3..], "title": "B", "line": "", "kind": "people"}]));
    assert_eq!(validate(&filled, &open).unwrap()[0].segments, ids[..3].to_vec());
    let twice = answer(json!([
        {"segments": ids[..1], "title": "Echo", "line": "first", "kind": "build"},
        {"segments": ids[1..3], "title": "Echo", "line": "then", "kind": "build"},
        {"segments": ids[3..], "title": "Call", "line": "", "kind": "people"}]));
    let b = validate(&twice, &open).unwrap();
    assert_eq!((b.len(), b[0].segments.len(), b[0].line.as_str()), (2, 3, "then"), "neighbours of one title are one block");
    let swapped = answer(json!([{"segments": [ids[1].clone(), ids[0].clone()], "title": "A", "line": "", "kind": "build"}]));
    assert!(validate(&swapped, &open).is_err());
    let long = answer(json!([
        {"segments": ids[..3], "title": "A title far too long for the island to show it whole", "line": "x".repeat(300), "kind": "nonsense"},
        {"segments": ids[3..], "title": "B", "line": "", "kind": "people"}]));
    let b = validate(&long, &open).unwrap();
    assert!(b[0].title.chars().count() <= BLOCK_TITLE_CHARS && b[0].line.chars().count() <= BLOCK_LINE_CHARS && b[0].kind == "other");
}

/// A model's answer with its usage: `input` prompt tokens (all uncached), `output` out, at Haiku's price.
fn answered(text: String, input: u64, output: u64) -> Answer {
    let usage = crate::background_call::Usage { input, output, ..Default::default() };
    Answer { text, usage, usd: usage.dollars(&crate::background_call::model("claude-haiku-5-5")) }
}

/// A fake model that names each active run as one block and counts its calls.
fn fake(calls: Arc<AtomicU64>, prompts: Arc<std::sync::Mutex<Vec<String>>>) -> Ask {
    Arc::new(move |c: &crate::background_call::Call| {
        calls.fetch_add(1, Ordering::SeqCst);
        let prompt = c.prompt.text();
        prompts.lock().unwrap().push(prompt.clone());
        let ids: Vec<String> = prompt.match_indices("<segment id=\"").map(|(i, _)| prompt[i + 13..i + 16].to_owned()).collect();
        let mut blocks = vec![];
        let mut cur: Vec<String> = vec![];
        for id in ids {
            let n: i64 = id[1..].parse().unwrap();
            if cur.last().is_some_and(|l: &String| n - l[1..].parse::<i64>().unwrap() > 3) { blocks.push(std::mem::take(&mut cur)) }
            cur.push(id);
        }
        if !cur.is_empty() { blocks.push(cur) }
        let blocks: Vec<Value> = blocks.into_iter().enumerate().map(|(k, s)| json!({"segments": s, "title": format!("Block {k}"), "line": "did things", "kind": "build"})).collect();
        Ok(answered(answer(json!(blocks)), 1000, 100))
    })
}

#[test]
fn a_pass_calls_the_model_only_for_something_new_and_never_resends_fixed_blocks() {
    let (ev, now) = sample();
    let calls = Arc::new(AtomicU64::new(0));
    let prompts = Arc::new(std::sync::Mutex::new(vec![]));
    let ask = fake(calls.clone(), prompts.clone());
    let (day, p) = update(None, &ev, now, Some(&ask), "claude-haiku-5-5");
    assert!(p.called && day.blocks.len() == 2 && day.blocks.iter().all(|b| b.named));
    assert_eq!(day.usage, Usage { calls: 1, input_tokens: 1000, output_tokens: 100, failed: 0, dollars: 0.00015, ..Default::default() });
    // Fifteen minutes later, nothing new: no call, same blocks.
    let (day2, p) = update(Some(day.clone()), &ev, now + 15 * 60, Some(&ask), "claude-haiku-5-5");
    assert!(!p.called && day2.blocks == day.blocks);
    // An hour and a half later the morning block is fixed; new work names only what is open.
    let mut ev = ev;
    let (t, _) = morning();
    ev.screens.push(screen("com.citrolabs.ego.lite", t + 200 * 60, t + 215 * 60));
    let (day3, p) = update(Some(day2), &ev, t + 216 * 60, Some(&ask), "claude-haiku-5-5");
    assert!(p.called && day3.frozen >= 1, "{day3:?}");
    let last = prompts.lock().unwrap().last().unwrap().clone();
    assert!(last.contains("<earlier_today") && last.contains("Block 0"), "fixed blocks go as context");
    assert!(!last.contains(&format!("<segment id=\"{}\"", day3.blocks[0].segments[0])), "and never as segments");
    assert_eq!(day3.blocks[0], day.blocks[0], "a fixed block never changes");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// Emergency mode (06: "Day timeline: naming failed … usage limit" during emergency): a pass whose
/// request the pause refused is no failure (nothing logged, nothing counted, no call) and is asked again after resume.
#[test]
fn a_paused_pass_is_not_a_failure_and_names_after_resume() {
    let (ev, now) = sample();
    let calls = Arc::new(AtomicU64::new(0));
    let real = fake(calls.clone(), Default::default());
    // The real path: the job's ask goes through `background_gate::check` first, as `background_call::post` does.
    let ask: Ask = { let real = real.clone(); Arc::new(move |c: &crate::background_call::Call| { crate::background_gate::check()?; real(c) }) };
    crate::background_gate::emergency_pause(Some(None));
    let (day, p) = update(None, &ev, now, Some(&ask), "m");
    assert!(p.failed.is_none() && !p.called, "no failure line, no call counted: {p:?}");
    assert_eq!((day.usage.failed, day.usage.calls, calls.load(Ordering::SeqCst)), (0, 0, 0));
    assert_eq!(day.tail_digest, 0, "not settled: asked again once resumed");
    crate::background_gate::emergency_pause(None);
    let (day, p) = update(Some(day), &ev, now + 60, Some(&ask), "m");
    assert!(p.called && p.failed.is_none() && day.blocks.iter().all(|b| b.named));
}

#[test]
fn a_bad_answer_keeps_the_last_names_and_is_paid_for_once() {
    let (ev, now) = sample();
    let calls = Arc::new(AtomicU64::new(0));
    let ask = fake(calls.clone(), Default::default());
    let (day, _) = update(None, &ev, now, Some(&ask), "m");
    let bad_calls = Arc::new(AtomicU64::new(0));
    let n = bad_calls.clone();
    let bad: Ask = Arc::new(move |_| { n.fetch_add(1, Ordering::SeqCst); Ok(answered("{\"blocks\": []}".into(), 10, 1)) });
    let mut ev2 = ev.clone();
    ev2.lines.push(line("User", now - 30, "one more thing"));
    let (day2, p) = update(Some(day.clone()), &ev2, now, Some(&bad), "m");
    assert!(p.failed.is_some() && day2.usage.failed == 1);
    assert_eq!(day2.blocks.iter().map(|b| &b.title).collect::<Vec<_>>(), day.blocks.iter().map(|b| &b.title).collect::<Vec<_>>());
    // Night review 24 Oct, finding 4: a paid answer `validate` rejects is settled like a cut-off run. The same
    // segments would get the same bad answer, so the next pass waits for something new.
    assert_ne!(day2.tail_digest, 0, "settled: the same input is not paid for again");
    let (day2b, p) = update(Some(day2), &ev2, now + 15 * 60, Some(&bad), "m");
    assert!(!p.called && bad_calls.load(Ordering::SeqCst) == 1, "the same segments are not sent again");
    ev2.lines.push(line("User", now + 20 * 60, "and another"));
    let (_, p) = update(Some(day2b), &ev2, now + 30 * 60, Some(&bad), "m");
    assert!(p.called && bad_calls.load(Ordering::SeqCst) == 2, "something new: asked again");
    // No model at all: named by app.
    let (day3, p) = update(None, &ev, now, None, "");
    assert!(!p.called && day3.blocks[1].title == "Call with Morgan");
}

#[test]
fn the_island_docks_left_after_settled_only_when_new_and_keeps_owners_place() {
    let (ev, now) = sample();
    let (day, _) = update(None, &ev, now, None, "");
    let fresh = island(&day, now, None);
    assert_eq!((fresh["side"].clone(), fresh["order"].clone(), fresh["expanded"].clone()), (json!("left"), json!(3), json!(false)));
    assert_eq!(fresh["title"], TITLE);
    assert_eq!(fresh["parts"][0]["type"], "timeline");
    assert_eq!(fresh["parts"][0]["blocks"].as_array().unwrap().len(), 2);
    let again = island(&day, now, Some(&fresh));
    assert!(again.get("side").is_none() && again.get("expanded").is_none() && again.get("order").is_none());

    let dir = tempfile::tempdir().unwrap();
    let root = Root(dir.path().into());
    show(&root, &day, now).unwrap();
    root.patch(ISLAND, |o| { o.insert("edgeY".into(), json!(500)); o.insert("expanded".into(), json!(true)); }).unwrap();
    show(&root, &day, now + 60).unwrap();
    let o = root.read(ISLAND).unwrap();
    assert_eq!((o["edgeY"].clone(), o["expanded"].clone(), o["updated"].clone()), (json!(500), json!(true), json!(now + 60)));
    // The next day before anything happened: yesterday's island hides, keeping the user's place; it is back with the
    // first block.
    let next = Day { date: "2026-10-24".into(), ..Default::default() };
    show(&root, &next, now).unwrap();
    let o = root.read(ISLAND).unwrap();
    assert_eq!((o["hidden"].clone(), o["edgeY"].clone(), o["expanded"].clone()), (json!(true), json!(500), json!(true)));
    let next = Day { blocks: day.blocks.clone(), ..next };
    show(&root, &next, now).unwrap();
    let o = root.read(ISLAND).unwrap();
    assert!(o.get("hidden").is_none() && o["edgeY"] == json!(500) && o["day"] == json!("2026-10-24"));
}

#[test]
fn days_are_kept_one_file_each() {
    let dir = tempfile::tempdir().unwrap();
    let (ev, now) = sample();
    let (day, _) = update(None, &ev, now, None, "");
    save_day(dir.path(), &day).unwrap();
    assert!(dir.path().join("day-timeline/2026-10-23.json").exists());
    assert_eq!(load_day(dir.path(), "2026-10-23").unwrap(), day);
    assert!(load_day(dir.path(), "2026-10-22").is_none());
    // A day file written before the cache fields loads with them empty.
    let old: Usage = serde_json::from_str(r#"{"calls":1,"input_tokens":10,"output_tokens":2}"#).unwrap();
    assert_eq!(old, Usage { calls: 1, input_tokens: 10, output_tokens: 2, ..Default::default() });
}

#[test]
fn the_archive_is_read_by_its_tags() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("archive");
    let db = heimdall_archive::store::open(&root)?;
    let (t, _) = morning();
    let ns = |s: i64| s * 1_000_000_000;
    db.execute("INSERT INTO timeline_chunks VALUES('s1',1,'screen','state','display:1',0,1,?1,?2,'','{}','h')", rusqlite::params![ns(t), ns(t + 40)])?;
    db.execute("INSERT INTO chunk_tags VALUES('s1','app','com.t3tools.t3code'),('s1','window_title','Heimdall')", [])?;
    db.execute("INSERT INTO timeline_chunks VALUES('u1',1,'text','utterance','mic',0,1,?1,?1,'  merge   it ','{}','h')", [ns(t + 5)])?;
    db.execute("INSERT INTO chunk_tags VALUES('u1','speaker','User')", [])?;
    db.execute("INSERT INTO timeline_chunks VALUES('u2',1,'text','turn','mic',0,1,?1,?1,'turns are not lines','{}','h')", [ns(t + 6)])?;
    db.execute("INSERT INTO timeline_chunks VALUES('s0',1,'screen','state','display:1',0,1,?1,?1,'','{}','h')", [ns(t - 100_000)])?;
    // A game that began before the span and runs into it counts.
    db.execute("INSERT INTO timeline_chunks VALUES('s2',1,'screen','state','display:1',0,1,?1,?2,'','{}','h')", rusqlite::params![ns(t - 3600), ns(t + 60)])?;
    db.execute("INSERT INTO chunk_tags VALUES('s2','app','com.valvesoftware.steam')", [])?;
    drop(db);
    let ev = read_archive(&root, t - 10, t + 100)?;
    assert_eq!(ev.screens, vec![
        Screen { start: t - 3600, end: t + 60, app: "com.valvesoftware.steam".into(), title: None },
        Screen { start: t, end: t + 40, app: "com.t3tools.t3code".into(), title: Some("Heimdall".into()) }]);
    assert_eq!(ev.lines, vec![line("User", t + 5, "merge it")]);
    Ok(())
}

/// Lab replay (opt-in, read-only on the real archive): a whole day as the 15-minute passes would have seen it.
///   DAY_TIMELINE_DATE=2026-10-23 DAY_TIMELINE_OUT=lab/day-timeline/replay-1023 [DAY_TIMELINE_PROBE=lab/day-timeline/probe]
///   [DAY_TIMELINE_ARCHIVE=…] cargo test -p app day_timeline::tests::replay_a_real_day -- --ignored --nocapture
/// With the probe each pass asks Claude Haiku 5.5 through Heimdall's stored key; without it, blocks are named by app.
/// Stops before it spends $1.50.
#[test]
#[ignore]
fn replay_a_real_day() -> Result<()> {
    use chrono::TimeZone;
    let date = std::env::var("DAY_TIMELINE_DATE").unwrap_or_else(|_| "2026-10-23".into());
    let out = PathBuf::from(std::env::var("DAY_TIMELINE_OUT").context("DAY_TIMELINE_OUT")?);
    let archive = std::env::var("DAY_TIMELINE_ARCHIVE").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support/Heimdall/archive")
    });
    let d = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")?;
    let start = chrono::Local.from_local_datetime(&d.and_hms_opt(DAY_START_HOUR, 0, 0).unwrap()).earliest().unwrap().timestamp();
    let end = (start + 24 * 3600).min(chrono::Utc::now().timestamp());
    let all = read_archive(&archive, start, end)?;
    eprintln!("replay {date}: {} screen states, {} lines", all.screens.len(), all.lines.len());
    std::fs::create_dir_all(out.join("passes"))?;
    let probe = std::env::var("DAY_TIMELINE_PROBE").ok();
    let n = Arc::new(AtomicU64::new(0));
    let ask: Option<Ask> = probe.map(|probe| {
        let (out, n) = (out.clone(), n.clone());
        Arc::new(move |c: &crate::background_call::Call| {
            let k = n.fetch_add(1, Ordering::SeqCst);
            let (req, resp) = (out.join(format!("passes/{k:03}-request.json")), out.join(format!("passes/{k:03}-response.json")));
            let prepared = crate::background_call::prepare(c)?;
            std::fs::write(&req, serde_json::to_vec_pretty(&prepared.body)?)?;
            let o = std::process::Command::new(&probe).arg(&req).arg(&resp).output()?;
            let status = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            ensure!(status == "200", "probe: {status}");
            crate::background_call::finish(c, &prepared, &serde_json::from_slice(&std::fs::read(&resp)?)?)
        }) as Ask
    });
    let mut day: Option<Day> = None;
    let mut log = String::new();
    let mut t = start + PERIOD.as_secs() as i64;
    while t <= end + PERIOD.as_secs() as i64 - 1 {
        let now = t.min(end);
        let ev = Evidence {
            screens: all.screens.iter().filter(|s| s.start < now).map(|s| Screen { end: s.end.min(now), ..s.clone() }).collect(),
            lines: all.lines.iter().filter(|l| l.at < now).cloned().collect(),
        };
        let started = std::time::Instant::now();
        let (d, p) = update(day.take(), &ev, now, ask.as_ref(), "claude-haiku-5-5");
        log += &format!("{} open={} called={} blocks={} frozen={} {:?} {:.0} ms\n", local_hhmm(now), p.open_segments, p.called, p.blocks, d.frozen, p.failed, started.elapsed().as_secs_f64() * 1000.0);
        day = Some(d);
        ensure!(day.as_ref().unwrap().usage.dollars < 1.5, "spend cap reached");
        t += PERIOD.as_secs() as i64;
    }
    let day = day.unwrap();
    let u = day.usage;
    log += &format!("\ncalls {} failed {} input {} (cache read {}, write {}) output {} = ${:.4} at list price\n", u.calls, u.failed, u.input_tokens, u.cache_read_tokens, u.cache_write_tokens, u.output_tokens, u.dollars);
    std::fs::write(out.join("passes.txt"), &log)?;
    std::fs::write(out.join(format!("{date}.json")), serde_json::to_vec_pretty(&day)?)?;
    let mut isl = island(&day, end, None);
    isl.insert("id".into(), json!(ISLAND));
    std::fs::write(out.join("day.json"), serde_json::to_vec_pretty(&Value::Object(isl))?)?;
    eprintln!("{log}");
    for b in &day.blocks { eprintln!("{}–{} [{}] {} · {}", local_hhmm(b.start), local_hhmm(b.end), b.kind, b.title, b.line) }
    Ok(())
}

/// Item 28: the fixed blocks and the system prompt are the cached prefix; the clock and the open segments come last.
#[test]
fn the_fixed_blocks_are_cached_and_the_clock_is_last() {
    let (ev, now) = sample();
    let ask = fake(Arc::new(AtomicU64::new(0)), Default::default());
    let (mut day, _) = update(None, &ev, now, Some(&ask), "claude-haiku-5-5");
    day.frozen = 1;
    let open: Vec<&Segment> = vec![];
    let (a, b) = (request_body("claude-haiku-5-5", user_prompt(&day, &open, now)), request_body("claude-haiku-5-5", user_prompt(&day, &open, now + 900)));
    let (ca, cb) = (a["messages"][0]["content"].as_array().unwrap(), b["messages"][0]["content"].as_array().unwrap());
    assert_eq!(ca.len(), 2);
    assert!(ca[0]["text"].as_str().unwrap().starts_with("<earlier_today") && ca[0]["cache_control"]["ttl"] == "1h");
    assert_eq!((ca[0].clone(), a["system"].clone()), (cb[0].clone(), b["system"].clone()), "the same prefix a quarter later");
    assert!(ca[1].get("cache_control").is_none() && ca[1]["text"].as_str().unwrap().contains("Now: ") && ca[1]["text"].as_str().unwrap().ends_with("Group and name these segments now."));
    assert_eq!((a["max_tokens"].as_u64(), a["output_config"]["effort"].as_str()), (Some(128_000), Some("medium")));
}

/// Review finding (item 28): a paid run without an answer is counted in the day and not asked again until the open
/// segments change.
#[test]
fn a_stopped_run_is_counted_and_not_asked_again_on_the_same_segments() {
    let (ev, now) = sample();
    let calls = Arc::new(AtomicU64::new(0));
    let n = calls.clone();
    let cut: Ask = Arc::new(move |_| {
        n.fetch_add(1, Ordering::SeqCst);
        let usage = crate::background_call::Usage { input: 1000, output: 128_000, ..Default::default() };
        Err(crate::background_call::Stopped { usage, usd: 0.0641, reason: "cut off".into() }.into())
    });
    let (day, p) = update(None, &ev, now, Some(&cut), "claude-haiku-5-5");
    assert!(p.failed.is_some() && day.usage.failed == 1 && (day.usage.dollars - 0.0641).abs() < 1e-12 && day.usage.output_tokens == 128_000);
    assert_eq!(day.blocks[1].title, "Call with Morgan", "named by app meanwhile");
    let (day2, p) = update(Some(day), &ev, now + 15 * 60, Some(&cut), "claude-haiku-5-5");
    assert!(!p.called && calls.load(Ordering::SeqCst) == 1, "the same segments are not sent again");
    let mut ev2 = ev.clone();
    ev2.lines.push(line("User", now + 20 * 60, "one more thing"));
    let (_, p) = update(Some(day2), &ev2, now + 30 * 60, Some(&cut), "claude-haiku-5-5");
    assert!(p.called && calls.load(Ordering::SeqCst) == 2, "something new: asked again");
}

/// A day file as version 1 wrote it (the installed build of 24 Oct): no `version`, no `meta`.
const V1_DAY: &str = r#"{"date":"2026-10-23","start":1761188400,"updated":1761210000,"blocks":[
  {"start":1761206400,"end":1761209100,"kind":"build","title":"Heimdall islands","line":"Menu merged","segments":["s20","s21","s22"],"named":true},
  {"start":1761211800,"end":1761213600,"kind":"people","title":"Call with Morgan","segments":["s26","s27"]}],
  "frozen":1,"tail_digest":42,"usage":{"calls":3,"input_tokens":10,"output_tokens":2}}"#;

#[test]
fn a_version_one_day_reads_as_before_and_gets_its_activity() {
    let day = parse_day(V1_DAY.as_bytes()).unwrap();
    assert_eq!(day.version, VERSION);
    assert_eq!((day.blocks.len(), day.frozen, day.tail_digest, day.usage.calls), (2, 1, 42, 3));
    let b = &day.blocks[0];
    assert_eq!((b.title.as_str(), b.line.as_str(), b.kind.as_str(), b.named), ("Heimdall islands", "Menu merged", "build", true));
    assert!(b.meta.is_empty() && day.blocks[1].meta.is_empty(), "details wait for a pass with the archive");
    // Saved again it is a version 2 file that reads back the same.
    let dir = tempfile::tempdir().unwrap();
    save_day(dir.path(), &day).unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(day_path(dir.path(), "2026-10-23")).unwrap()).unwrap();
    assert_eq!(raw["version"], json!(2));
    assert_eq!(load_day(dir.path(), "2026-10-23").unwrap(), day);
    // On the island a block without details has nothing to open.
    let o = island(&day, day.updated, None);
    assert!(o["parts"][0]["blocks"][0].get("meta").is_none());
}

#[test]
fn todays_version_one_file_gets_the_tags_facts_on_the_next_pass() {
    let (ev, now) = sample();
    let (day, _) = update(None, &ev, now, None, "");
    // The same day as version 1 wrote it: no meta.
    let mut old = day.clone();
    old.version = 1;
    for b in &mut old.blocks { b.meta = Meta::default() }
    let v1 = serde_json::to_string(&old).unwrap().replace("\"version\":1,", "");
    let (again, _) = update(parse_day(v1.as_bytes()), &ev, now + 60, None, "");
    assert_eq!(again.blocks[1].meta.people, vec!["Morgan".to_string()]);
    assert_eq!(again.blocks[0].meta.apps, vec![("T3 Code".to_string(), 45)]);
    assert_eq!(again.blocks[0].meta.activity, "agent work");
}

#[test]
fn the_tags_give_people_apps_threads_and_files() {
    let (t, start) = morning();
    let mut ev = Evidence::default();
    ev.screens.push(Screen { start: t, end: t + 20 * 60, app: "com.t3tools.t3code".into(), title: Some("Day timeline metadata".into()) });
    ev.screens.push(Screen { start: t + 20 * 60, end: t + 30 * 60, app: "com.microsoft.VSCode".into(), title: Some("parts.rs — Heimdall".into()) });
    ev.screens.push(Screen { start: t + 30 * 60, end: t + 31 * 60, app: "com.t3tools.t3code".into(), title: Some("T3 Code".into()) });
    ev.lines.push(line("Morgan", t + 60, "hi"));
    ev.lines.push(line("unknown speaker 2", t + 70, "a video"));
    let segs = segments(&ev, start, t + 40 * 60);
    let active: Vec<&Segment> = segs.iter().filter(|s| s.active()).collect();
    let b = block_of(&active, "build", "x".into(), String::new(), false);
    assert_eq!(b.meta.threads, vec!["Day timeline metadata".to_string()], "T3's own name is not a thread");
    assert_eq!(b.meta.files, vec!["parts.rs".to_string()]);
    assert_eq!(b.meta.people, vec!["Morgan".to_string()], "unknown speakers are not people");
    assert_eq!(b.meta.apps, vec![("T3 Code".to_string(), 21), ("VS Code".to_string(), 10)]);
    assert_eq!(b.meta.tags(), vec!["activity:agent work", "app:t3 code", "app:vs code"]);
    assert_eq!(file_names("~/Notes/README.md, Cargo.toml (edited) — v1.2 build"), vec!["README.md", "Cargo.toml"]);
}

#[test]
fn the_models_details_are_kept_shown_and_searchable() {
    let (ev, now) = sample();
    let (_, start) = morning();
    let segs = segments(&ev, start, now);
    let open: Vec<&Segment> = segs.iter().filter(|s| s.active()).collect();
    let ids: Vec<String> = open.iter().map(|s| s.id()).collect();
    let text = answer(json!([
        {"segments": ids[..3], "title": "Heimdall islands", "line": "Branch merged", "kind": "build", "project": "Heimdall",
         "activity": "agent work", "about": "Merged the islands branch.  Picked the ack voices.", "people": [], "threads": ["islands polish"],
         "tasks": ["Render the menu", "render the menu"], "files": ["parts.rs"], "decisions": ["Keep the left dock"]},
        {"segments": ids[3..], "title": "Call with Morgan", "line": "", "kind": "people", "project": "", "activity": "dancing",
         "about": "", "people": ["Morgan", "User's brother", "Heimdall", "User"], "threads": [], "tasks": [], "files": [], "decisions": []}]));
    let blocks = validate(&text, &open).unwrap();
    let m = &blocks[0].meta;
    assert_eq!((m.project.as_str(), m.activity.as_str(), m.about.as_str()), ("Heimdall", "agent work", "Merged the islands branch. Picked the ack voices."));
    assert_eq!(m.tasks, vec!["Render the menu".to_string()], "each once");
    assert_eq!((m.threads.clone(), m.files.clone(), m.decisions.clone()), (vec!["islands polish".to_string()], vec!["parts.rs".to_string()], vec!["Keep the left dock".to_string()]));
    assert_eq!(m.apps, vec![("T3 Code".to_string(), 45)], "the tags' apps stay");
    assert_eq!(blocks[1].meta.activity, "call", "an activity off the list gives way to the guess");
    assert_eq!(blocks[1].meta.people, vec!["Morgan".to_string(), "User's brother".to_string()]);
    // An answer without the details (an older prompt) still names the blocks.
    assert!(validate(&answer(json!([{"segments": ids[..3], "title": "A", "line": "", "kind": "build"}, {"segments": ids[3..], "title": "B", "line": "", "kind": "people"}])), &open).is_ok_and(|b| b[0].meta.activity == "agent work"));

    // Round trip through the day file, onto the island and through search.
    let day = Day { date: "2026-10-23".into(), start, updated: now, blocks: blocks.clone(), version: VERSION, ..Default::default() };
    let dir = tempfile::tempdir().unwrap();
    save_day(dir.path(), &day).unwrap();
    assert_eq!(load_day(dir.path(), "2026-10-23").unwrap(), day);
    let o = island(&day, now, None);
    let meta = &o["parts"][0]["blocks"][0]["meta"];
    assert!(meta.get("tags").is_none(), "the title names the project; the colour says the activity");
    assert_eq!((meta["threads"].clone(), meta["tasks"].clone()), (json!(["islands polish"]), json!(["Render the menu"])));
    assert_eq!(meta["decisions"], json!(["Keep the left dock"]));
    assert!(meta.get("people").is_none(), "empty lists are left out");
    assert_eq!(o["parts"][0]["blocks"][1]["meta"]["people"], json!(["User's brother"]), "Morgan is in the title; User and Heimdall are not people");
    let found = |q: &str| search(dir.path(), q).into_iter().map(|(d, b)| format!("{d} {}", b.title)).collect::<Vec<_>>();
    assert_eq!(found("project:heimdall"), vec!["2026-10-23 Heimdall islands"]);
    assert_eq!(found("activity:call"), vec!["2026-10-23 Call with Morgan"]);
    assert_eq!(found("ack VOICES"), vec!["2026-10-23 Heimdall islands"]);
    assert_eq!(found("app:t3"), vec!["2026-10-23 Heimdall islands"]);
    assert!(found("brother islands").is_empty() && found("").is_empty());
}

#[test]
fn a_fixed_blocks_details_never_change_once_it_has_them() {
    let (ev, now) = sample();
    let calls = Arc::new(AtomicU64::new(0));
    let ask = fake(calls, Default::default());
    let (mut day, _) = update(None, &ev, now, Some(&ask), "m");
    day.frozen = 2;
    let fixed = day.blocks.clone();
    // The archive learns a speaker's name after the fact: the fixed call keeps its people.
    let mut later = ev.clone();
    for l in later.lines.iter_mut().filter(|l| l.speaker == "Morgan").take(3) { l.speaker = "Erin".into() }
    let (again, _) = update(Some(day.clone()), &later, now + 60, Some(&ask), "m");
    assert_eq!(again.blocks[..2], fixed[..], "fixed blocks never change");
    // A fixed block without details (a version 1 day) gets them once.
    day.blocks[1].meta = Meta::default();
    let (again, _) = update(Some(day), &ev, now + 60, Some(&ask), "m");
    assert_eq!((again.blocks[1].meta.people.clone(), again.blocks[1].meta.activity.as_str()), (vec!["Morgan".to_string()], "agent work"), "the fake model calls everything build");
}
