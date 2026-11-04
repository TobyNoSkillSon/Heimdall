//! 26 Oct 01:15 and 01:36: Jarvis, the screen and the spend log failed with "Too many open files (os error 24)". The
//! app held 25 handles to `packs/hot.sqlite` and 21 to `heimdall.sqlite`: every thread kept its own pack connection,
//! and SQLite keeps a closed connection's file open while any other connection to that file lives, so the count only
//! ever rose to the most connections that once existed together. Pack connections are now shared by every thread and
//! capped per pack (`store::PACK_CONNECTIONS`), and only a few packs keep idle ones (`store::PACK_POOLS`).
//!
//! One test in its own binary: it counts this process's open descriptors, which no other test may disturb.
use heimdall_archive::store;
use std::{
    path::Path,
    sync::{Arc, Barrier},
};

fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}
/// A pack holds at most `PACK_CONNECTIONS` connections, each with the pack and its WAL open, plus one shared `-shm`.
fn per_pack() -> usize {
    2 * store::PACK_CONNECTIONS + 1
}
fn touch(root: &Path, pack: &str, key: &str) {
    store::put(root, pack, &[(key.to_owned(), key.as_bytes())]).unwrap();
    assert_eq!(store::get(root, pack, key).unwrap().as_deref(), Some(key.as_bytes()));
}

#[test]
fn pack_and_index_handles_stay_bounded_however_many_threads_and_opens() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    // The index stays open throughout, as in the app (capture, the journal and the live writer hold it).
    let keeper = store::open(&root).unwrap();
    touch(&root, store::HOT, "warm");
    let base = open_fds();

    // 32 long-lived threads write and read the hot pack and this month's pack, then stay alive together: a pack
    // connection kept per thread would hold 128 descriptors here.
    let threads = 32;
    let barrier = Arc::new(Barrier::new(threads + 1));
    let done = Arc::new(Barrier::new(threads + 1));
    let workers: Vec<_> = (0..threads)
        .map(|i| {
            let (root, barrier, done) = (root.clone(), barrier.clone(), done.clone());
            std::thread::spawn(move || {
                for round in 0..5 {
                    touch(&root, store::HOT, &format!("hot/{i}/{round}"));
                    touch(&root, "2026-10", &format!("month/{i}/{round}"));
                }
                barrier.wait();
                done.wait();
            })
        })
        .collect();
    barrier.wait();
    let alive = open_fds() - base;
    done.wait();
    for w in workers {
        w.join().unwrap();
    }
    assert!(alive <= 2 * per_pack(), "{alive} descriptors with 32 threads alive (bound {})", 2 * per_pack());

    // Months of packs read and written one after another, and the index opened and closed hundreds of times
    // (every `mark_complete`, `sessions`, `View`): idle packs are dropped beyond the pool, and closed index
    // connections leave nothing behind.
    for round in 0..3 {
        for month in 1..=12 {
            touch(&root, &format!("2025-{month:02}"), &format!("old/{round}/{month}"));
        }
    }
    for _ in 0..300 {
        let db = store::open(&root).unwrap();
        store::ensure_session(&db, "session-fd").unwrap();
        drop(db);
        drop(store::read_index(&root).unwrap());
    }
    let after = open_fds() - base;
    let bound = store::PACK_POOLS * per_pack() + 4;
    println!("descriptors over the start: {alive} with 32 threads alive, {after} after 36 packs and 600 index opens");
    assert!(after <= bound, "{after} descriptors after 36 packs and 600 index opens (bound {bound})");
    drop(keeper);
}
