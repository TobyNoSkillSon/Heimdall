//! The memory line's decisions (bug note 2026-11-03T22-38-44), on injected samples and times only.
use super::*;

const GB: u64 = 1 << 30;

fn sample(used_gb: u64, need_gb: u64) -> Sample {
    Sample { used: used_gb * GB, need: need_gb * GB, why: if need_gb > 0 { vec!["Arsenal locomotive restore".into()] } else { vec![] }, pressure: 1, available: None }
}

const QUIET_NOW: Option<Duration> = Some(Duration::from_secs(30));

#[test]
fn a_declared_need_over_the_95_gb_line_unloads_jarvis_for_phonon() {
    // 3 Nov 22:36: Jarvis (~22 GB) resident, the user's Mac about 70 GB in use, the Arsenal restore needs 30 GB more.
    let mut p = Policy::default();
    let t = Instant::now();
    let Action::GiveWay(reason) = p.decide(&sample(70, 30), true, QUIET_NOW, t) else { panic!("must give way") };
    assert_eq!(reason, "Memory would pass 95 GB (70.0 GB in use plus 30.0 GB needed for Arsenal locomotive restore). \
        Jarvis unloaded to free memory; Phonon S speaks.");
    assert_eq!(p.held(), Some(reason.as_str()));
    // At the line exactly he stays; one byte over, he goes.
    let mut p = Policy::default();
    assert_eq!(p.decide(&Sample { used: LINE - 10 * GB, need: 10 * GB, ..sample(0, 0) }, true, QUIET_NOW, t), Action::Stay);
    assert!(matches!(p.decide(&Sample { used: LINE - 10 * GB, need: 10 * GB + 1, ..sample(0, 0) }, true, QUIET_NOW, t), Action::GiveWay(_)));
    // Memory in use alone over the line, nothing declared: he goes too.
    let mut p = Policy::default();
    let Action::GiveWay(reason) = p.decide(&sample(96, 0), true, QUIET_NOW, t) else { panic!() };
    assert_eq!(reason, "Memory would pass 95 GB (96.0 GB in use). Jarvis unloaded to free memory; Phonon S speaks.");
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
    for s in [sample(60, 0), sample(50, 20), sample(94, 0)] {
        assert_eq!(p.decide(&s, true, QUIET_NOW, t), Action::Stay);
    }
    for s in [sample(40, 0), sample(50, 21), sample(71, 0)] {
        assert_eq!(p.decide(&s, false, QUIET_NOW, t), Action::Stay, "not held: the usual admission loads him");
    }
    assert_eq!(p.held(), None);
}

#[test]
fn he_does_not_load_when_loading_him_would_pass_the_line() {
    // Speaking turned on at 72 GB in use: Jarvis's 24 GiB would pass 95 GB, so he would load only to unload again.
    let mut p = Policy::default();
    let t = Instant::now();
    assert_eq!(p.decide(&sample(72, 0), false, QUIET_NOW, t), Action::Hold);
    assert_eq!(p.held(), Some("Not enough room under 95 GB for Jarvis (72.0 GB in use). Phonon S speaks."));
    let mut p = Policy::default();
    assert_eq!(p.decide(&sample(50, 30), false, QUIET_NOW, t), Action::Hold, "a declared need counts too");
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
    assert!(matches!(p.decide(&sample(70, 30), true, QUIET_NOW, t0), Action::GiveWay(_)));
    let at = |s: u64| t0 + Duration::from_secs(s);
    // Unloaded: the need still stands. Held.
    assert_eq!(p.decide(&sample(48, 30), false, QUIET_NOW, at(1)), Action::Hold);
    // The job ends (need gone): 48 GB in use, room for Jarvis (24 GiB) and the 4 GiB margin under 95 GiB. Back,
    // but not for a minute yet.
    assert!(back(&sample(48, 0), JARVIS_BYTES));
    assert_eq!(p.decide(&sample(48, 0), false, QUIET_NOW, at(10)), Action::Hold);
    assert_eq!(p.decide(&sample(48, 0), false, QUIET_NOW, at(69)), Action::Hold);
    // A dip in between restarts the minute.
    assert_eq!(p.decide(&sample(70, 0), false, QUIET_NOW, at(40)), Action::Hold);
    let mut p = Policy::default();
    p.hold("x".into(), t0);
    assert_eq!(p.decide(&sample(48, 0), false, QUIET_NOW, at(10)), Action::Hold);
    assert_eq!(p.decide(&sample(68, 0), false, QUIET_NOW, at(40)), Action::Hold, "68 GB + 24 GiB + 4 GiB is over the line");
    assert_eq!(p.decide(&sample(48, 0), false, QUIET_NOW, at(41)), Action::Hold);
    assert_eq!(p.decide(&sample(48, 0), false, QUIET_NOW, at(100)), Action::Hold, "59 s since the dip");
    // A minute back, but a reply is being spoken, or one just ended: he waits for the gap between replies.
    assert_eq!(p.decide(&sample(48, 0), false, None, at(101)), Action::Hold);
    assert_eq!(p.decide(&sample(48, 0), false, Some(Duration::from_secs(1)), at(102)), Action::Hold);
    assert_eq!(p.decide(&sample(48, 0), false, QUIET_NOW, at(103)), Action::Return);
    assert_eq!(p.held(), None);
}

#[test]
fn the_return_gap_is_hysteresis_so_an_unload_never_reloads_at_once() {
    // Just over the line with Jarvis in: after he leaves (~22 GB measured, 29 GB booked), memory in use is not
    // back, so he does not reload into the same shortage.
    let mut p = Policy::default();
    let t = Instant::now();
    assert!(matches!(p.decide(&sample(96, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    for s in 0..600 {
        assert_eq!(p.decide(&sample(96 - 22, 0), false, QUIET_NOW, t + Duration::from_secs(s)), Action::Hold);
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
    let Action::GiveWay(first) = p.decide(&sample(70, 30), true, None, t) else { panic!() };
    assert_eq!(p.decide(&sample(40, 0), true, None, t), Action::GiveWay(first));
}

/// Review 4 Nov: the return check assumed 24 GiB while Jarvis frees ~29 GB, so at 66 GB in use without him he came
/// back to ~95 GB and gave way again about once a minute. His size is what his give-way actually freed.
#[test]
fn his_return_counts_the_memory_his_give_way_actually_freed() {
    let mut p = Policy::default();
    let t = Instant::now();
    let at = |s: u64| t + Duration::from_secs(s);
    assert!(matches!(p.decide(&sample(96, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    // The worker is reaped over a few seconds: 96 GB in use, then 80, then 67 (29 GB freed).
    assert_eq!(p.decide(&sample(80, 0), false, QUIET_NOW, at(1)), Action::Hold);
    assert_eq!(p.decide(&sample(67, 0), false, QUIET_NOW, at(4)), Action::Hold);
    assert_eq!(p.jarvis_bytes(), 29 * GB);
    // Readings after the window measure nothing: other apps' memory is not his.
    assert_eq!(p.decide(&sample(40, 0), false, QUIET_NOW, at(20)), Action::Hold);
    assert_eq!(p.jarvis_bytes(), 29 * GB, "only the {FREED_WINDOW:?} after he left counts");
    // 64 GB in use fits 24 GiB and the margin, but not his 29: still held, however long.
    for s in 21..400 {
        assert_eq!(p.decide(&sample(64, 0), false, QUIET_NOW, at(s)), Action::Hold);
    }
    // 62 GB + 29 + 4 = 95: back, and a minute later he returns.
    assert_eq!(p.decide(&sample(62, 0), false, QUIET_NOW, at(400)), Action::Hold);
    assert_eq!(p.decide(&sample(62, 0), false, QUIET_NOW, at(460)), Action::Return);
    // A release smaller than the floor never lowers his size under 24 GiB, nor one bigger than the cap raises it past.
    let mut p = Policy::default();
    assert!(matches!(p.decide(&sample(96, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
    p.decide(&sample(90, 0), false, QUIET_NOW, at(1));
    assert_eq!(p.jarvis_bytes(), JARVIS_BYTES);
    let mut p = Policy::default();
    assert!(matches!(p.decide(&sample(96, 0), true, QUIET_NOW, t), Action::GiveWay(_)));
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
        assert!(matches!(p.decide(&sample(96, 0), true, QUIET_NOW, now), Action::GiveWay(_)));
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
        "let required = need + headroom + MARGIN + retained;"] {
        assert!(engine.is_empty() || engine.contains(line), "the sidecar's admission changed: update ADMISSION ({line})");
    }
    let with = |free: u64| Sample { available: Some(free), ..sample(40, 0) };
    assert!(back(&with(ADMISSION), JARVIS_BYTES));
    assert!(!back(&with(ADMISSION - 1), JARVIS_BYTES));
    let mut p = Policy::default();
    let t = Instant::now();
    p.hold("x".into(), t);
    for s in 0..300 {
        assert_eq!(p.decide(&with(25 * GB), false, QUIET_NOW, t + Duration::from_secs(s)), Action::Hold);
    }
    assert_eq!(p.decide(&with(40 * GB), false, QUIET_NOW, t + Duration::from_secs(300)), Action::Hold);
    assert_eq!(p.decide(&with(40 * GB), false, QUIET_NOW, t + Duration::from_secs(360)), Action::Return);
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
    assert_eq!(live, [(30 * GB, "Arsenal locomotive restore".to_owned()), (25 * GB / 2, "training".to_owned())]);
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
