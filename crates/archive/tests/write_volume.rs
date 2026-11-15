//! Lab (opt-in, `--ignored`): disk writes of the archive under listening's VAD notes, notes gathered for
//! `live::NOTE_GROUP` against one commit per arrival as before (log sweep F1, 9 Nov). An isolated archive in a temp
//! dir is first filled with 600,000 earlier notes, so the hash-keyed payload table and its evidence index are large
//! enough that each new note lands on its own leaf page, as on the user's 9 GB index. Writes are this process's own
//! (`proc_pid_rusage`, logical writes), measured over 20 s of notes at 124 a second for each setting in turn.
use heimdall_archive::{live, records::Writer, store};
use serde_json::json;
use std::time::{Duration, Instant};

fn logical_writes() -> u64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4, &mut info as *mut _ as *mut libc::rusage_info_t) };
    assert_eq!(r, 0);
    info.ri_logical_writes
}

fn vad(writer: &Writer, source: &str, ns: u64, p: f64) {
    writer.event("model_output", source, &[], ns, json!({"model":"silero","input":{"source":source,"start_ns":ns,"end_ns":ns+32_000_000,"rate":16000,"samples":512,"context_samples":64,"state":"previous source/generation Silero state; zero on reset"},"generation":1,"probability":p,"utterance_start_ns":null})).unwrap();
}

#[test]
#[ignore = "lab: about two minutes of disk writes in a temp dir"]
fn vad_notes_write_volume_grouped_and_as_before() {
    // `HEIMDALL_LAB_ARCHIVE=<dir>`: keep the filled archive there and reuse it on the next run.
    let t = tempfile::tempdir().unwrap();
    let root = std::env::var_os("HEIMDALL_LAB_ARCHIVE").map_or_else(|| t.path().join("archive"), std::path::PathBuf::from);
    let filled = store::exists(&root);
    let session = store::session_path(&root, "session-lab");
    let writer = Writer::open(&session).unwrap();
    let queue = live::Queue::shared(&root).unwrap();
    let started = Instant::now();
    for i in (0..600_000u64).filter(|_| !filled) {
        vad(&writer, &format!("mic-{}", i % 4), i * 8_000_000, (i % 997) as f64 / 997.);
        if i % 20_000 == 0 {
            assert!(queue.settle(Duration::from_secs(120)));
        }
    }
    assert!(queue.settle(Duration::from_secs(300)));
    eprintln!("filled 600,000 notes in {:.0} s", started.elapsed().as_secs_f64());
    let mut ns = 600_000u64 * 8_000_000;
    for (name, group) in [("as before", Duration::ZERO), ("grouped", live::NOTE_GROUP), ("as before", Duration::ZERO), ("grouped", live::NOTE_GROUP)] {
        // The previous phase's checkpoint finishes before this one is counted.
        std::thread::sleep(Duration::from_secs(6));
        queue.set_note_group(group);
        eprintln!("{:?} {name} starts", std::time::SystemTime::now());
        let (writes, commits, began) = (logical_writes(), queue.groups(), Instant::now());
        let mut n = 0u64;
        while began.elapsed() < Duration::from_secs(20) {
            vad(&writer, &format!("mic-{}", n % 4), ns, (n % 997) as f64 / 997.);
            ns += 8_000_000;
            n += 1;
            std::thread::sleep(Duration::from_micros(8_065));
        }
        let secs = began.elapsed().as_secs_f64();
        assert!(queue.settle(Duration::from_secs(60)));
        // This phase's own checkpoint (the checkpointer runs within a second or two of the last commit) counts in it.
        std::thread::sleep(Duration::from_secs(4));
        let written = logical_writes() - writes;
        eprintln!("{:?} {name}: {n} notes in {secs:.1} s, {} commits, {:.2} MB/s written, {:.1} KB per note", std::time::SystemTime::now(), queue.groups() - commits, written as f64 / 1e6 / secs, written as f64 / 1e3 / n as f64);
    }
}
