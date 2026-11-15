//! The memory line's decisions (bug note 2026-11-03T22-38-44), on injected samples and times only.
use super::*;

const GB: u64 = 1 << 30;

fn sample(used_gb: u64, need_gb: u64) -> Sample {
    Sample { used: used_gb * GB, need: need_gb * GB, why: if need_gb > 0 { vec!["Arsenal locomotive restore".into()] } else { vec![] }, pressure: 1, available: None, yield_for: vec![] }
}

const QUIET_NOW: Option<Duration> = Some(Duration::from_secs(30));

#[test]
fn a_declared_need_over_the_105_gb_line_unloads_jarvis_for_phonon() {
    // 3 Nov 22:36: Jarvis (~22 GB) resident, the user's Mac about 70 GB in use, the Arsenal restore needs 30 GB more. Under
    // the 105 GB line (6 Nov) that fits; 80 GB in use with the same need passes it.
    let mut p = Policy::default();
    let t = Instant::now();
    let Action::GiveWay(reason) = p.decide(&sample(80, 30), true, QUIET_NOW, t) else { panic!("must give way") };
    assert_eq!(reason, "Memory would pass 105 GB (80.0 GB in use plus 30.0 GB needed for Arsenal locomotive restore). \
        Jarvis unloaded to free memory; Phonon S speaks.");
    assert_eq!(p.held(), Some(reason.as_str()));
    // At the line exactly he stays; one byte over, he goes.
    let mut p = Policy::default();
    assert_eq!(p.decide(&Sample { used: LINE - 10 * GB, need: 10 * GB, ..sample(0, 0) }, true, QUIET_NOW, t), Action::Stay);
    assert!(matches!(p.decide(&Sample { used: LINE - 10 * GB, need: 10 * GB + 1, ..sample(0, 0) }, true, QUIET_NOW, t), Action::GiveWay(_)));
    // Memory in use alone over the line, nothing declared: he goes too.
    let mut p = Policy::default();
    let Action::GiveWay(reason) = p.decide(&sample(106, 0), true, QUIET_NOW, t) else { panic!() };
    assert_eq!(reason, "Memory would pass 105 GB (106.0 GB in use). Jarvis unloaded to free memory; Phonon S speaks.");
}

#[test]
fn native_warning_or_critical_unloads_him_with_plenty_free() {
    for (level, word) in [(WARN, "warning"), (CRITICAL, "critical")] {
        let mut p = Policy::default();
        let s = Sample { pressure: level, ..sample(40, 0) };
        let Action::GiveWay(reason) = p.decide(&s, true, QUIET_NOW, Instant::now()) else { panic!("{level}") };
        assert!(reason.starts_with(&format!("System memory pressure ({word})")), "{reason}");
    }
}

#[test]
fn under_the_line_he_stays_and_nothing_is_held() {
    let mut p = Policy::default();
    let t = Instant::now();
    for s in [sample(60, 0), sample(50, 20), sample(104, 0)] {
        assert_eq!(p.decide(&s, true, QUIET_NOW, t), Action::Stay);
    }
    for s in [sample(40, 0), sample(50, 21), sample(71, 0)] {
        assert_eq!(p.decide(&s, false, QUIET_NOW, t), Action::Stay, "not held: the usual admission loads him");
    }
    assert_eq!(p.held(), None);
}

#[test]
fn he_does_not_load_when_loading_him_would_pass_the_line() {
    // Speaking turned on at 82 GB in use: Jarvis's 24 GiB would pass 105 GB, so he would load only to unload again.
    let mut p = Policy::default();
    let t = Instant::now();
    assert_eq!(p.decide(&sample(82, 0), false, QUIET_NOW, t), Action::Hold);
    assert_eq!(p.held(), Some("Not enough room under 105 GB for Jarvis (82.0 GB in use). Phonon S speaks."));
    let mut p = Policy::default();
    assert_eq!(p.decide(&sample(60, 30), false, QUIET_NOW, t), Action::Hold, "a declared need counts too");
    let mut p = Policy::default();
    assert_eq!(p.decide(&Sample { pressure: WARN, ..sample(20, 0) }, false, QUIET_NOW, t), Action::Hold);
    // From there he returns the usual way: back with the margin for a minute.
    assert_eq!(p.decide(&sample(20, 0), false, QUIET_NOW, t + Duration::from_secs(1)), Action::Hold);
    assert_eq!(p.decide(&sample(20, 0), false, QUIET_NOW, t + Duration::from_secs(61)), Action::Return);
}

#[test]
fn he_returns_only_when_memory_is_back_with_a_margin_for_a_minute_and_between_replies() {
    let mut p = Policy::default();
    let t0 = Instant::now();
    assert!(matches!(p.decide(&sample(80, 30), true, QUIET_NOW, t0), Action::GiveWay(_)));
    let at = |s: u64| t0 + Duration::from_secs(s);
    // Unloaded: the need still stands. Held.
    assert_eq!(p.decide(&sample(58, 30), false, QUIET_NOW, at(1)), Action::Hold);
    // The job ends (need gone): 58 GB in use, room for Jarvis (24 GiB) and the 4 GiB margin under 105 GiB. Back,
    // but not for a minute yet.
    assert!(back(&sample(58, 0), JARVIS_BYTES));
    assert_eq!(p.decide(&sample(58, 0), false, QUIET_NOW, at(10)), Action::Hold);
    assert_eq!(p.decide(&sample(58, 0), false, QUIET_NOW, at(69)), Action::Hold);
    // A dip in between restarts the minute.
    assert_eq!(p.decide(&sample(80, 0), false, QUIET_NOW, at(40)), Action::Hold);
    let mut p = Policy::default();
    p.hold("x".into(), t0);
    assert_eq!(p.decide(&sample(58, 0), false, QUIET_NOW, at(10)), Action::Hold);
    assert_eq!(p.decide(&sample(78, 0), false, QUIET_NOW, at(40)), Action::Hold, "78 GB + 24 GiB + 4 GiB is over the line");
    assert_eq!(p.decide(&sample(58, 0), false, QUIET_NOW, at(41)), Action::Hold);
    assert_eq!(p.decide(&sample(58, 0), false, QUIET_NOW, at(100)), Action::Hold, "59 s since the dip");
    // A minute back, but a reply is being spoken, or one just ended: he waits for the gap between replies.
    assert_eq!(p.decide(&sample(58, 0), false, None, at(101)), Action::Hold);
    assert_eq!(p.decide(&sample(58, 0), false, Some(Duration::from_secs(1)), at(102)), Action::Hold);
    assert_eq!(p.decide(&sample(58, 0), false, QUIET_NOW, at(103)), Action::Return);
    assert_eq!(p.held(), None);
}

#[test]
fn the_return_gap_is_hysteresis_so_an_unload_never_reloads_at_once() {
    // Just over the line with Jarvis in: after he leaves (~22 GB measured, 29 GB booked), memory in use is not
    // back, so he does not reload into the same shortage.
    let mut p = Policy::default();
    let t = Instant::now();
    assert!(matches!(p.decide(&sample(106, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    for s in 0..600 {
        assert_eq!(p.decide(&sample(106 - 22, 0), false, QUIET_NOW, t + Duration::from_secs(s)), Action::Hold);
    }
    // Every reading under the line that is not "back" holds: the boundary sits a margin plus Jarvis below the line.
    let edge = LINE - JARVIS_BYTES - RETURN_MARGIN;
    assert!(back(&Sample { used: edge, ..sample(0, 0) }, JARVIS_BYTES));
    assert!(!back(&Sample { used: edge + 1, ..sample(0, 0) }, JARVIS_BYTES));
    assert!(!back(&Sample { pressure: WARN, ..sample(10, 0) }, JARVIS_BYTES), "pressure must be normal");
}

#[test]
fn a_sidecar_memory_exit_holds_him_out_the_same_way() {
    // The sidecar's own guard (WARN, low RAM) exits 73: the host holds him until memory is back.
    let mut p = Policy::default();
    let t = Instant::now();
    p.hold("System memory pressure. Using the lighter voice.".into(), t);
    assert_eq!(p.decide(&Sample { pressure: WARN, ..sample(50, 0) }, false, QUIET_NOW, t), Action::Hold);
    assert_eq!(p.decide(&sample(50, 0), false, QUIET_NOW, t + Duration::from_secs(1)), Action::Hold);
    assert_eq!(p.decide(&sample(50, 0), false, QUIET_NOW, t + Duration::from_secs(61)), Action::Return);
}

#[test]
fn a_resident_jarvis_still_held_keeps_giving_way_until_his_line_ends() {
    // He gave way mid-line: the line finishes, then the next tick unloads him, whatever memory does meanwhile.
    let mut p = Policy::default();
    let t = Instant::now();
    let Action::GiveWay(first) = p.decide(&sample(80, 30), true, None, t) else { panic!() };
    assert_eq!(p.decide(&sample(40, 0), true, None, t), Action::GiveWay(first));
}

/// Review 4 Nov: the return check assumed 24 GiB while Jarvis frees ~29 GB, so at 66 GB in use without him he came
/// back to ~95 GB and gave way again about once a minute. His size is what his give-way actually freed.
#[test]
fn his_return_counts_the_memory_his_give_way_actually_freed() {
    let mut p = Policy::default();
    let t = Instant::now();
    let at = |s: u64| t + Duration::from_secs(s);
    assert!(matches!(p.decide(&sample(106, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    // The worker is reaped over a few seconds: 106 GB in use, then 90, then 77 (29 GB freed).
    assert_eq!(p.decide(&sample(90, 0), false, QUIET_NOW, at(1)), Action::Hold);
    assert_eq!(p.decide(&sample(77, 0), false, QUIET_NOW, at(4)), Action::Hold);
    assert_eq!(p.jarvis_bytes(), 29 * GB);
    // Readings after the window measure nothing: other apps' memory is not his.
    assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, at(20)), Action::Hold);
    assert_eq!(p.jarvis_bytes(), 29 * GB, "only the {FREED_WINDOW:?} after he left counts");
    // 74 GB in use fits 24 GiB and the margin, but not his 29: still held, however long.
    for s in 21..400 {
        assert_eq!(p.decide(&sample(74, 0), false, QUIET_NOW, at(s)), Action::Hold);
    }
    // 72 GB + 29 + 4 = 105: back, and a minute later he returns.
    assert_eq!(p.decide(&sample(72, 0), false, QUIET_NOW, at(400)), Action::Hold);
    assert_eq!(p.decide(&sample(72, 0), false, QUIET_NOW, at(460)), Action::Return);
    // A release smaller than the floor never lowers his size under 24 GiB, nor one bigger than the cap raises it past.
    let mut p = Policy::default();
    assert!(matches!(p.decide(&sample(106, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    p.decide(&sample(100, 0), false, QUIET_NOW, at(1));
    assert_eq!(p.jarvis_bytes(), JARVIS_BYTES);
    let mut p = Policy::default();
    assert!(matches!(p.decide(&sample(106, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    p.decide(&sample(20, 0), false, QUIET_NOW, at(1));
    assert_eq!(p.jarvis_bytes(), JARVIS_MAX);
    // No hold, no measurement: Speaking off unloads him without telling the policy anything.
    let mut p = Policy::default();
    assert_eq!(p.decide(&sample(60, 0), true, QUIET_NOW, t), Action::Stay);
    assert_eq!(p.decide(&sample(30, 0), false, QUIET_NOW, at(1)), Action::Stay);
    assert_eq!(p.jarvis_bytes(), JARVIS_BYTES);
}

/// Review 4 Nov: each give-way that follows a return within 15 minutes doubles the hold (1, 2, 4, 8, 16 minutes,
/// capped), so a borderline Mac stops flip-flopping; after a calm quarter hour it is a minute again.
#[test]
fn holds_that_follow_a_return_double_up_to_16_minutes_and_calm_resets_them() {
    let mut p = Policy::default();
    let t = Instant::now();
    let mut now = t;
    let mut cycle = |p: &mut Policy, stayed: Duration| -> Duration {
        now += stayed;
        assert!(matches!(p.decide(&sample(106, 0), true, QUIET_NOW, now), Action::GiveWay(_)));
        let hold = p.hold_for();
        now += Duration::from_secs(1);
        assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, now), Action::Hold);
        assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, now + hold - Duration::from_secs(1)), Action::Hold);
        now += hold;
        assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, now), Action::Return);
        hold
    };
    let minute = Duration::from_secs(60);
    let holds: Vec<_> = (0..7).map(|_| cycle(&mut p, minute)).collect();
    assert_eq!(holds, [1, 2, 4, 8, 16, 16, 16].map(|m| minute * m));
    // Fifteen calm minutes back: the next hold is a minute.
    assert_eq!(cycle(&mut p, CALM), minute);
    assert_eq!(cycle(&mut p, minute), 2 * minute);
    // A sidecar refusal right after a return counts as a flip too.
    let refusal = now + Duration::from_secs(5);
    p.hold("Not enough free memory for Jarvis right now".into(), refusal);
    assert_eq!(p.hold_for(), 4 * minute);
}

/// Review 4 Nov: under the line but with less than the sidecar's admission free + inactive, his return was refused
/// (exit 73) every minute. He returns only once the sidecar would admit him.
#[test]
fn he_returns_only_when_the_sidecar_would_admit_him() {
    // The sidecar's own numbers (crates/drama-engine/src/memory.rs): RESIDENT 27 GB + 2 GB, MARGIN 2 GiB.
    // The sidecar's source is not part of every checkout: compare when it is there.
    let engine = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../drama-engine/src/memory.rs")).unwrap_or_default();
    for line in ["pub const DRAMA_RESIDENT: u64 = 27_000_000_000;", "pub const VERIFICATION_RESERVE: u64 = 2_000_000_000;",
        "pub const RESIDENT: u64 = DRAMA_RESIDENT + VERIFICATION_RESERVE;", "pub const MARGIN: u64 = 2 * (1 << 30);",
        "let required = need + MARGIN + retained;"] {
        assert!(engine.is_empty() || engine.contains(line), "the sidecar's admission changed: update ADMISSION ({line})");
    }
    // And the engine's own floor after it (heimdall-drama3-engine.rs and engine.py): the stricter one counts.
    // Like the sidecar's source, the engine's is not in every checkout: compare when it is there.
    let bin = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../drama-engine/src/bin/heimdall-drama3-engine.rs")).unwrap_or_default();
    assert!(bin.is_empty() || bin.contains("if available < 55_000_000_000 {"), "the engine's floor changed: update ENGINE_FLOOR");
    let py = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/drama3/engine/speed/integrate/engine.py")).unwrap_or_default();
    assert!(py.is_empty() || py.contains("available < 55e9"), "the Python engine's floor changed: update ENGINE_FLOOR");
    assert_eq!(ADMISSION, ENGINE_FLOOR.max(SIDECAR_ADMISSION));
    let with = |free: u64| Sample { available: Some(free), ..sample(40, 0) };
    assert!(back(&with(ADMISSION), JARVIS_BYTES));
    assert!(!back(&with(ADMISSION - 1), JARVIS_BYTES));
    // 31 GB free passed the sidecar's admission but not the engine's floor: no return into a refusal.
    assert!(!back(&with(SIDECAR_ADMISSION), JARVIS_BYTES));
    let mut p = Policy::default();
    let t = Instant::now();
    p.hold("x".into(), t);
    for s in 0..300 {
        assert_eq!(p.decide(&with(25 * GB), false, QUIET_NOW, t + Duration::from_secs(s)), Action::Hold);
    }
    assert_eq!(p.decide(&with(52 * GB), false, QUIET_NOW, t + Duration::from_secs(300)), Action::Hold);
    assert_eq!(p.decide(&with(52 * GB), false, QUIET_NOW, t + Duration::from_secs(360)), Action::Return);
}

struct Fake(Vec<Result<Sample>>, std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Source for Fake {
    fn sample(&mut self) -> Result<Sample> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.0.len() > 1 { self.0.remove(0) } else { self.0[0].as_ref().map(Clone::clone).map_err(|e| anyhow::anyhow!("{e}")) }
    }
}

#[test]
fn the_gate_samples_once_a_second_and_fails_open() {
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut g = Gate::new(Box::new(Fake(vec![Ok(sample(40, 0))], reads.clone())));
    let t = Instant::now();
    for ms in 0..1000 {
        assert_eq!(g.tick(true, QUIET_NOW, t + Duration::from_millis(ms)), Action::Stay);
    }
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1, "the capture tick never reads memory per event");
    g.tick(true, QUIET_NOW, t + Duration::from_millis(1000));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    // A reading that fails keeps him (the sidecar's own guard still fails closed), and never reloads a held one.
    let mut g = Gate::new(Box::new(Fake(vec![Err(anyhow::anyhow!("no stats"))], reads.clone())));
    assert_eq!(g.tick(true, QUIET_NOW, t), Action::Stay);
    assert_eq!(g.failing.as_deref(), Some("no stats"));
    g.policy.hold("x".into(), t);
    assert_eq!(g.tick(false, QUIET_NOW, t + Duration::from_secs(120)), Action::Hold);
}

#[test]
fn need_files_count_until_they_expire_and_malformed_ones_are_ignored() {
    let written = UNIX_EPOCH + Duration::from_secs(1_793_741_000);
    let at = |s: u64| written + Duration::from_secs(s);
    assert_eq!(parse_need(r#"{"gb": 30, "why": "Arsenal locomotive restore"}"#, written, at(60)),
        Some((30 * GB, Some("Arsenal locomotive restore".into()))));
    assert_eq!(parse_need(r#"{"bytes": 1234}"#, written, at(0)), Some((1234, None)));
    // Without `until`: two hours from the last write.
    assert!(parse_need(r#"{"gb": 30}"#, written, at(2 * 3600 - 1)).is_some());
    assert!(parse_need(r#"{"gb": 30}"#, written, at(2 * 3600)).is_none());
    // `until` is kept, but never past 12 hours after the write.
    let until = 1_793_741_000 + 3 * 3600;
    assert!(parse_need(&format!(r#"{{"gb": 30, "until": {until}}}"#), written, at(3 * 3600 - 1)).is_some());
    assert!(parse_need(&format!(r#"{{"gb": 30, "until": {until}}}"#), written, at(3 * 3600)).is_none());
    let far = 1_793_741_000 + 48 * 3600;
    assert!(parse_need(&format!(r#"{{"gb": 30, "until": {far}}}"#), written, at(12 * 3600)).is_none());
    for bad in ["", "{", r#"{"gb": -3}"#, r#"{"gb": 0}"#, r#"{"why": "x"}"#, r#"{"gb": "30"}"#, "[]"] {
        assert_eq!(parse_need(bad, written, at(1)), None, "{bad}");
    }
}

#[test]
fn the_needs_folder_sums_live_json_files_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let needs = Needs::new(dir.path().join(NEEDS_DIR));
    assert!(needs.live(SystemTime::now()).is_empty(), "no folder: no needs");
    std::fs::create_dir_all(&needs.dir)?;
    std::fs::write(needs.dir.join("arsenal.json"), r#"{"gb": 30, "why": "Arsenal locomotive restore"}"#)?;
    std::fs::write(needs.dir.join("training.json"), r#"{"gb": 12.5}"#)?;
    std::fs::write(needs.dir.join("notes.txt"), r#"{"gb": 99}"#)?;
    std::fs::write(needs.dir.join("broken.json"), "not json")?;
    let live = needs.live(SystemTime::now());
    // Each names its file, so a log line says which one to release (bug note 2026-11-08T02-03-45).
    assert_eq!(live, [(30 * GB, "Arsenal locomotive restore (arsenal.json)".to_owned()), (25 * GB / 2, "training.json".to_owned())]);
    Ok(())
}

#[test]
fn memory_used_is_app_plus_wired_plus_compressed() {
    let mut raw = [0i32; 64];
    raw[VM_WIRE / 4] = 100;
    raw[VM_PURGEABLE / 4] = 30;
    raw[VM_COMPRESSOR / 4] = 50;
    raw[VM_INTERNAL / 4] = 1000;
    // free, active, inactive and external (file cache) pages never count.
    raw[0] = 7777;
    raw[1] = 7777;
    raw[2] = 7777;
    raw[136 / 4] = 7777;
    assert_eq!(used_from(0, &raw, 38, 16384).unwrap(), (1000 - 30 + 100 + 50) * 16384);
    assert!(used_from(1, &raw, 38, 16384).is_err());
    assert!(used_from(0, &raw, 35, 16384).is_err());
}

#[test]
fn available_is_free_plus_inactive_like_the_sidecar() {
    let mut raw = [0i32; 64];
    raw[0] = 100;
    raw[1] = 7777;
    raw[2] = 50;
    raw[VM_WIRE / 4] = 7777;
    assert_eq!(available_from(0, &raw, 38, 16384).unwrap(), 150 * 16384);
    assert!(available_from(1, &raw, 38, 16384).is_err());
}

#[test]
fn native_readings_are_sane_on_this_mac() {
    let (used, available) = vm_bytes().unwrap();
    assert!(used > GB && used < 1024 * GB, "{used}");
    assert!(available < 1024 * GB, "{available}");
    assert!(matches!(pressure().unwrap(), 1 | 2 | 4));
}

/// Bug note 2026-11-08T02-03-44: a released need (file deleted, or `until` in the past) is seen at the next reading;
/// he returns once memory has stayed back for the hold, and the return says how long he was out and that the need was
/// released.
#[test]
fn a_released_need_is_seen_at_once_and_the_return_says_how_long_he_was_out() {
    let mut p = Policy::default();
    let t = Instant::now();
    let at = |s: u64| t + Duration::from_secs(s);
    // 8 Nov 01:38: about 46 GB in use, a 62 GB need declared: he gives way.
    assert!(matches!(p.decide(&sample(46, 62), true, QUIET_NOW, at(0)), Action::GiveWay(_)));
    assert_eq!(p.decide(&sample(26, 62), false, QUIET_NOW, at(5)), Action::Hold);
    assert!(!p.released(&sample(26, 62)));
    // Released while the job's model is still loaded: seen, but memory is short and he stays out.
    assert!(p.released(&sample(95, 0)));
    assert_eq!(p.decide(&sample(95, 0), false, QUIET_NOW, at(10)), Action::Hold);
    // Memory back at 12 s: the minute's hold, then between replies.
    assert_eq!(p.decide(&sample(26, 0), false, QUIET_NOW, at(12)), Action::Hold);
    assert_eq!(p.held_for(at(40)), Some(Duration::from_secs(40)));
    assert_eq!(p.decide(&sample(26, 0), false, QUIET_NOW, at(72)), Action::Return);
    assert_eq!(p.last_return(), Some((Duration::from_secs(72), true)));
    assert_eq!(p.held_for(at(80)), None);
    // Memory back by itself, no need declared: not a release.
    let mut p = Policy::default();
    assert!(matches!(p.decide(&sample(106, 0), true, QUIET_NOW, at(0)), Action::GiveWay(_)));
    assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, at(5)), Action::Hold);
    assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, at(66)), Action::Return);
    assert_eq!(p.last_return(), Some((Duration::from_secs(66), false)));
}

/// The need file written with `until` in the past (what the Vella task did when its `rm` was declined) counts as
/// released at the next reading.
#[test]
fn an_until_in_the_past_releases_the_need_at_the_next_reading() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let needs = Needs::new(dir.path().join(NEEDS_DIR));
    std::fs::create_dir_all(&needs.dir)?;
    let file = needs.dir.join("angel-nemotron-wer.json");
    let now = SystemTime::now();
    let secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
    std::fs::write(&file, format!(r#"{{"gb": 62, "why": "Vella Nemotron", "until": {}}}"#, secs + 2700))?;
    assert_eq!(needs.live(now), [(62 * GB, "Vella Nemotron (angel-nemotron-wer.json)".to_owned())]);
    std::fs::write(&file, format!(r#"{{"gb": 0, "why": "expired", "until": {}}}"#, secs - 60))?;
    assert!(needs.live(now).is_empty());
    std::fs::write(&file, format!(r#"{{"gb": 62, "until": {}}}"#, secs - 1))?;
    assert!(needs.live(now).is_empty(), "a past until alone releases it");
    Ok(())
}

// --- arsenal-yield (night 1110, bug note 2026-11-09T12-43-19) ---

#[test]
fn a_job_that_asks_for_room_unloads_jarvis_under_the_line_and_he_returns_after_its_release_and_the_hold() {
    // Arsenal's own admission (100 GiB line) refused a load that Heimdall's 105 GB line would let pass: the need
    // asks Heimdall to yield, and Jarvis goes whatever the line says.
    let mut p = Policy::default();
    let t = Instant::now();
    let asking = Sample { yield_for: vec!["Arsenal FLUX.2 Dev (arsenal-flux2-dev.json)".into()], ..sample(60, 30) };
    let Action::GiveWay(reason) = p.decide(&asking, true, QUIET_NOW, t) else { panic!("must give way") };
    assert_eq!(reason, "Arsenal FLUX.2 Dev (arsenal-flux2-dev.json) asked for room. Jarvis unloaded to free memory; Phonon S speaks.");
    // While the ask is live he stays out, with plenty of room.
    assert_eq!(p.decide(&Sample { yield_for: asking.yield_for.clone(), ..sample(40, 30) }, false, QUIET_NOW, t + Duration::from_secs(600)), Action::Hold);
    // Released: back after the hold, between replies.
    let t1 = t + Duration::from_secs(601);
    assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, t1), Action::Hold);
    assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, t1 + RETURN_HOLD), Action::Return);
}

#[test]
fn only_need_files_that_say_yield_true_ask_for_room() {
    let now = SystemTime::now();
    let ask = r#"{"bytes": 40000000000, "why": "Arsenal FLUX.2 Dev", "yield": true}"#;
    assert_eq!(parse_yield(ask, now, now), Some(Some("Arsenal FLUX.2 Dev".into())));
    assert_eq!(parse_yield(r#"{"gb": 40, "why": "Vella"}"#, now, now), None, "a plain need never asks");
    assert_eq!(parse_yield(r#"{"gb": 40, "yield": "yes"}"#, now, now), None);
    let past = now.duration_since(UNIX_EPOCH).unwrap().as_secs_f64() - 1.;
    assert_eq!(parse_yield(&format!(r#"{{"gb": 40, "yield": true, "until": {past}}}"#), now, now), None, "a released ask asks nothing");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("arsenal-flux.json"), ask).unwrap();
    std::fs::write(dir.path().join("vella.json"), r#"{"gb": 8, "why": "Vella"}"#).unwrap();
    let needs = Needs::new(dir.path().to_path_buf());
    assert_eq!(needs.asking(now), vec!["Arsenal FLUX.2 Dev (arsenal-flux.json)".to_owned()]);
    assert_eq!(needs.live(now).len(), 2, "an ask is also a need");
}

// --- arsenal-yield review fix 4 (night 1110 review, Temp/night-1110/review.md item 4) ---

#[test]
fn an_ask_whose_writer_is_gone_or_was_replaced_asks_nothing() {
    let written = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let at = |s: u64| written + Duration::from_secs(s);
    let ask = r#"{"bytes": 40000000000, "why": "Arsenal FLUX", "yield": true, "until": 1800000060, "pid": 4242, "pid_start": 1799999000.5}"#;
    let alive = |pid: u32, start: Option<f64>| pid == 4242 && start.is_none_or(|s| (s - 1_799_999_000.4).abs() <= START_SLACK);
    assert_eq!(parse_yield_with(ask, written, at(1), &alive), Some(Some("Arsenal FLUX".into())), "its Arsenal is running");
    assert_eq!(parse_need_with(ask, written, at(1), &alive).map(|n| n.0), Some(40_000_000_000));
    // Arsenal crashed or was force-quit mid-load: the ask and its need are gone at the next reading, not in 2 hours.
    let dead = |_: u32, _: Option<f64>| false;
    assert_eq!(parse_yield_with(ask, written, at(1), &dead), None);
    assert_eq!(parse_need_with(ask, written, at(1), &dead), None);
    // The pid now belongs to a process that started later: not the writer.
    let reused = |pid: u32, start: Option<f64>| pid == 4242 && start.is_none_or(|s| (s - 1_799_999_900.).abs() <= START_SLACK);
    assert_eq!(parse_yield_with(ask, written, at(1), &reused), None);
    // A need that names no writer is not checked (Vella, older writers).
    assert!(parse_need_with(r#"{"gb": 8, "why": "Vella"}"#, written, at(1), &dead).is_some());
}

#[test]
fn an_ask_is_a_lease_that_lapses_without_renewal() {
    // Review item 4, scenario B: a big model loaded by hand once kept its ask (until = 2 h) and so held Jarvis out the
    // whole time it stayed loaded. An ask now counts at most YIELD_LEASE_MAX after its last write.
    let written = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let at = |s: u64| written + Duration::from_secs(s);
    let alive = |_: u32, _: Option<f64>| true;
    let two_hours = r#"{"bytes": 40000000000, "why": "Arsenal FLUX", "yield": true, "until": 1800007200, "pid": 7}"#;
    let lease = YIELD_LEASE_MAX.as_secs();
    assert!(parse_yield_with(two_hours, written, at(lease - 1), &alive).is_some());
    assert_eq!(parse_yield_with(two_hours, written, at(lease), &alive), None, "not renewed: the ask lapses");
    assert_eq!(parse_need_with(two_hours, written, at(lease), &alive), None, "and so does its need");
    // Renewed (rewritten) 20 s before: live again from the new write.
    assert!(parse_yield_with(two_hours, at(lease - 20), at(lease + 30), &alive).is_some());
    // Without `until` an ask still lasts no longer than the lease; a plain need keeps its 2 hours.
    assert_eq!(parse_yield_with(r#"{"gb": 40, "yield": true}"#, written, at(lease), &alive), None);
    assert!(parse_need_with(r#"{"gb": 40}"#, written, at(lease), &alive).is_some());
}

#[test]
fn the_folder_drops_an_ask_from_a_process_that_has_exited() -> Result<()> {
    // Fakes on both sides: a real child process stands in for Arsenal; `asking` reads the real folder.
    let dir = tempfile::tempdir()?;
    let needs = Needs::new(dir.path().to_path_buf());
    let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn()?;
    let pid = child.id();
    let start = process_start(pid).expect("a child's start time is readable");
    let now = SystemTime::now();
    let until = now.duration_since(UNIX_EPOCH)?.as_secs_f64() + 60.;
    let body = |start: f64| format!(r#"{{"bytes": 40000000000, "why": "Arsenal FLUX", "yield": true, "until": {until}, "pid": {pid}, "pid_start": {start}}}"#);
    std::fs::write(dir.path().join("arsenal-flux.json"), body(start))?;
    assert_eq!(needs.asking(now), vec!["Arsenal FLUX (arsenal-flux.json)".to_owned()], "its writer runs");
    std::fs::write(dir.path().join("arsenal-flux.json"), body(start - 3600.))?;
    assert!(needs.asking(now).is_empty(), "same pid, another start: not its writer");
    std::fs::write(dir.path().join("arsenal-flux.json"), body(start))?;
    child.kill()?;
    child.wait()?;
    assert!(needs.asking(now).is_empty(), "its writer exited");
    assert!(needs.live(now).is_empty());
    // This test process's own start is what the system says it is.
    assert!(writer_alive(std::process::id(), process_start(std::process::id())));
    Ok(())
}
