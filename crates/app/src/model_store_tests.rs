use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A local HTTP server for one file: honours `Range: bytes=N-`, and for its first requests can cut the body short
/// (`cut`: bytes sent before the connection closes) or send wrong bytes (`corrupt`). Records each request's Range.
struct Server {
    origin: String,
    requests: Arc<Mutex<Vec<Option<u64>>>>,
}

#[derive(Clone, Default)]
struct Faults {
    /// Requests whose body stops after this many bytes.
    cut: Vec<usize>,
    /// The first this-many requests send the wrong bytes.
    corrupt: usize,
    /// Every request answers this status with no body.
    status: Option<u16>,
    /// Requests whose body pauses for 3 s after this many bytes (a stalled connection), then closes.
    stall: Vec<usize>,
}

fn serve(body: Vec<u8>, faults: Faults) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(vec![]));
    let seen = requests.clone();
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let n = count.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut range = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    range = v.trim().trim_end_matches('-').parse::<u64>().ok();
                }
            }
            seen.lock().unwrap().push(range);
            if let Some(status) = faults.status {
                let _ = write!(stream, "HTTP/1.1 {status} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let mut data = body.clone();
            if n < faults.corrupt {
                data.iter_mut().for_each(|b| *b ^= 0xff);
            }
            let start = range.unwrap_or(0) as usize;
            let slice = &data[start.min(data.len())..];
            let head = match range {
                Some(r) => format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {r}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len() - 1, data.len(), slice.len()),
                None => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", slice.len()),
            };
            let _ = stream.write_all(head.as_bytes());
            let send = faults.cut.get(n).copied().unwrap_or(slice.len()).min(slice.len());
            if let Some(stall) = faults.stall.get(n).copied() {
                let _ = stream.write_all(&slice[..stall.min(slice.len())]);
                let _ = stream.flush();
                let stalled = stream;
                std::thread::spawn(move || { std::thread::sleep(Duration::from_secs(3)); drop(stalled); });
                continue;
            }
            let _ = stream.write_all(&slice[..send]);
            let _ = stream.flush();
            // Dropped: a short body is a closed connection.
        }
    });
    Server { origin, requests }
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

fn row(path: &str, data: &[u8]) -> FileRow {
    FileRow { path: path.into(), url: format!("https://huggingface.co/org/repo/resolve/abc/{path}"), sha256: hex(&Sha256::digest(data)), bytes: data.len() as u64 }
}

fn options(server: &Server) -> Options {
    Options { attempts: 3, backoff: Duration::from_millis(1), request_timeout: Duration::from_secs(10), retry_after: Duration::from_millis(30), origin: Some(server.origin.clone()) }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().build().unwrap()
}

#[test]
fn a_partial_file_resumes_with_a_range_request_and_lands_whole() {
    let data = body(300_000);
    let server = serve(data.clone(), Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let file = row("parakeet-ultra/weights/weight.bin", &data);
    let part = part_path(&dir.path().join(&file.path));
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &data[..120_000]).unwrap();
    let mut last = 0;
    fetch_file(&client(), dir.path(), &file, &options(&server), &mut |n| last = n, &|| false).unwrap();
    assert_eq!(*server.requests.lock().unwrap(), [Some(120_000)], "only the rest was asked for");
    assert_eq!(std::fs::read(dir.path().join(&file.path)).unwrap(), data);
    assert!(!part.exists(), "the partial file was renamed into place");
    assert_eq!(last, data.len() as u64);
}

#[test]
fn a_dropped_connection_resumes_where_it_stopped() {
    let data = body(200_000);
    let server = serve(data.clone(), Faults { cut: vec![50_000, 70_000], ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let file = row("speakers/ReDimNet2B6.onnx", &data);
    fetch_file(&client(), dir.path(), &file, &options(&server), &mut |_| {}, &|| false).unwrap();
    assert_eq!(*server.requests.lock().unwrap(), [None, Some(50_000), Some(120_000)]);
    assert_eq!(std::fs::read(dir.path().join(&file.path)).unwrap(), data);
}

/// Review 5 Nov: a file that does not match its checksum fails at once with the reason (fetching gigabytes again
/// would bring the same bytes), and nothing is left in place.
#[test]
fn a_checksum_mismatch_fails_the_file_at_once() {
    let data = body(10_000);
    let server = serve(data.clone(), Faults { corrupt: 99, ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let file = row("decide/d1/x/model.safetensors", &data);
    let err = fetch_file(&client(), dir.path(), &file, &options(&server), &mut |_| {}, &|| false).unwrap_err();
    assert!(format!("{err:#}").contains(CHECKSUM), "{err:#}");
    assert_eq!(server.requests.lock().unwrap().len(), 1, "one download, not six");
    assert!(!dir.path().join(&file.path).exists() && !part_path(&dir.path().join(&file.path)).exists());
}

/// A full-size `.part` another process wrote with other bytes: hashed from disk before the rename, so it never goes
/// into place.
#[test]
fn the_part_file_is_hashed_from_disk_before_it_goes_into_place() {
    let data = body(10_000);
    let server = serve(data.clone(), Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let file = row("speakers/ReDimNet2B6.onnx", &data);
    let part = part_path(&dir.path().join(&file.path));
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, vec![0u8; data.len()]).unwrap();
    let err = fetch_file(&client(), dir.path(), &file, &options(&server), &mut |_| {}, &|| false).unwrap_err();
    assert!(format!("{err:#}").contains(CHECKSUM));
    assert!(server.requests.lock().unwrap().is_empty() && !dir.path().join(&file.path).exists());
    // Next time it starts clean and lands.
    fetch_file(&client(), dir.path(), &file, &options(&server), &mut |_| {}, &|| false).unwrap();
    assert_eq!(std::fs::read(dir.path().join(&file.path)).unwrap(), data);
}

/// Review 5 Nov: a stalled connection costs one short request, not 15 minutes; its bytes are kept and resumed.
#[test]
fn a_stalled_connection_times_out_and_resumes() {
    let data = body(100_000);
    let server = serve(data.clone(), Faults { stall: vec![30_000], ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let file = row("parakeet-ultra/weights/weight.bin", &data);
    let started = Instant::now();
    fetch_file(&client(), dir.path(), &file, &Options { request_timeout: Duration::from_millis(400), ..options(&server) }, &mut |_| {}, &|| false).unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert_eq!(*server.requests.lock().unwrap(), [None, Some(30_000)]);
    assert_eq!(std::fs::read(dir.path().join(&file.path)).unwrap(), data);
}

#[test]
fn a_server_error_is_retried_then_named() {
    let server = serve(vec![], Faults { status: Some(503), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let file = row("a.bin", b"hello");
    let err = fetch_file(&client(), dir.path(), &file, &options(&server), &mut |_| {}, &|| false).unwrap_err();
    assert!(format!("{err:#}").contains("503"), "{err:#}");
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}

fn lock_of(groups: Vec<(&str, Vec<FileRow>)>) -> Lock {
    Lock { groups: groups.into_iter().map(|(id, files)| Group { id: id.into(), title: id.into(), files }).collect() }
}

fn wait_for(downloads: &Downloads, done: impl Fn(&[Row]) -> bool) -> Vec<Row> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = downloads.rows();
        if done(&rows) || Instant::now() > until {
            return rows;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_finished_group_is_in_the_manifest_and_never_fetched_again() {
    let (a, b) = (body(40_000), body(9_000));
    let server = serve(a.clone(), Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let lock = lock_of(vec![("parakeet", vec![row("p/a.bin", &a)])]);
    let downloads = Downloads::new(dir.path().into(), lock.clone(), options(&server), || {});
    assert!(!downloads.ready("parakeet"));
    downloads.want(&["parakeet".into()]);
    let rows = wait_for(&downloads, |r| r.iter().all(|r| r.state == State::Done));
    assert_eq!(rows, [Row { id: "parakeet".into(), title: "parakeet".into(), done: 40_000, total: 40_000, state: State::Done }]);
    assert!(downloads.ready("parakeet") && dir.path().join(MANIFEST).is_file());
    // A new launch: done from the manifest, no request at all.
    let again = Downloads::new(dir.path().into(), lock, options(&server), || {});
    again.want(&["parakeet".into()]);
    assert_eq!(again.rows()[0].state, State::Done);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    // A lock that pins other bytes for the group is not done any more.
    let changed = lock_of(vec![("parakeet", vec![row("p/a.bin", &b)])]);
    assert!(!is_done(dir.path(), changed.group("parakeet").unwrap()));
    // A file removed by hand: not done.
    std::fs::remove_file(dir.path().join("p/a.bin")).unwrap();
    assert!(!again.ready("parakeet"));
}

#[test]
fn a_failed_group_shows_its_reason_the_next_still_downloads_and_retry_resumes() {
    let data = body(30_000);
    let dir = tempfile::tempdir().unwrap();
    // The first group's URL is wrong (404 from this one-file server is not possible, so: a server that refuses).
    let bad = serve(vec![], Faults { status: Some(404), ..Default::default() });
    let lock = lock_of(vec![("decider-4bit", vec![row("d/m.bin", &data)]), ("embedding", vec![row("e/w.bin", &data)])]);
    let downloads = Downloads::new(dir.path().into(), lock.clone(), options(&bad), || {});
    downloads.want(&["decider-4bit".into(), "embedding".into()]);
    let rows = wait_for(&downloads, |r| r.iter().all(|r| matches!(r.state, State::Failed(_))));
    assert!(matches!(&rows[0].state, State::Failed(why) if why.contains("m.bin") && why.contains("404")), "{rows:?}");
    // Retry against a working server: both land.
    let good = serve(data.clone(), Faults::default());
    let downloads = Downloads::new(dir.path().into(), lock, options(&good), || {});
    downloads.want(&["decider-4bit".into()]);
    wait_for(&downloads, |r| r.iter().all(|r| r.state == State::Done));
    downloads.want(&["embedding".into()]);
    let rows = wait_for(&downloads, |r| r.iter().all(|r| r.state == State::Done));
    assert_eq!(rows.len(), 2);
    assert!(downloads.ready("decider-4bit") && downloads.ready("embedding"));
}

/// Review 5 Nov: a failed group is tried again by itself after its wait (never hot), and lands when the server does.
#[test]
fn a_failed_group_is_tried_again_by_itself_after_a_wait() {
    let data = body(5_000);
    let bad = serve(vec![], Faults { status: Some(503), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let lock = lock_of(vec![("parakeet", vec![row("p/a.bin", &data)])]);
    let downloads = Downloads::new(dir.path().into(), lock.clone(), Options { attempts: 1, retry_after: Duration::from_millis(300), ..options(&bad) }, || {});
    downloads.want(&["parakeet".into()]);
    wait_for(&downloads, |r| matches!(r[0].state, State::Failed(_)));
    downloads.tick();
    assert!(matches!(downloads.rows()[0].state, State::Failed(_)), "not before its wait");
    assert!(downloads.retry_in("parakeet").is_some_and(|d| d > Duration::from_millis(100)));
    std::thread::sleep(Duration::from_millis(350));
    downloads.tick();
    wait_for(&downloads, |r| matches!(r[0].state, State::Failed(_)));
    assert_eq!(bad.requests.lock().unwrap().len(), 2, "tried once more");
    // The next wait is longer (600 ms): an early tick does nothing.
    std::thread::sleep(Duration::from_millis(350));
    downloads.tick();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(bad.requests.lock().unwrap().len(), 2, "the wait doubles");
}

/// Review 5 Nov (merge): a checksum mismatch is never tried again by itself (a bad pin or a corrupting proxy would
/// bring whole files again every few minutes, forever); the user's Retry tries it.
#[test]
fn a_checksum_mismatch_waits_for_the_users_retry() {
    let data = body(5_000);
    let bad = serve(data.clone(), Faults { corrupt: 99, ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let lock = lock_of(vec![("parakeet", vec![row("p/a.bin", &data)])]);
    let downloads = Downloads::new(dir.path().into(), lock, Options { attempts: 1, retry_after: Duration::from_millis(30), ..options(&bad) }, || {});
    downloads.want(&["parakeet".into()]);
    wait_for(&downloads, |r| matches!(&r[0].state, State::Failed(why) if why.contains(CHECKSUM)));
    std::thread::sleep(Duration::from_millis(100));
    downloads.tick();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(bad.requests.lock().unwrap().len(), 1, "not again by itself");
    assert!(matches!(downloads.rows()[0].state, State::Failed(_)) && downloads.retry_in("parakeet").is_none());
    downloads.retry();
    wait_for(&downloads, |r| matches!(r[0].state, State::Failed(_)));
    assert_eq!(bad.requests.lock().unwrap().len(), 2, "Retry tries it");
}

/// Review 5 Nov: one process downloads into a folder at a time; the other waits and goes on once the lock is free.
#[test]
fn a_second_process_waits_for_the_folders_lock() {
    let data = body(5_000);
    let server = serve(data.clone(), Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let held = folder_lock(dir.path()).unwrap();
    let downloads = Downloads::new(dir.path().into(), lock_of(vec![("parakeet", vec![row("p/a.bin", &data)])]), options(&server), || {});
    downloads.want(&["parakeet".into()]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(downloads.rows()[0].state, State::Waiting);
    assert!(server.requests.lock().unwrap().is_empty(), "nothing fetched while another holds the folder");
    drop(held);
    downloads.tick();
    wait_for(&downloads, |r| r[0].state == State::Done);
    assert!(downloads.ready("parakeet"));
}

/// Another precision chosen: the waiting one leaves, the new one downloads before the models listening does not need.
#[test]
fn a_new_decider_precision_replaces_the_waiting_one_and_goes_before_memory_search() {
    let server = serve(vec![], Faults { status: Some(503), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let f = |p: &str| row(p, b"x");
    let lock = lock_of(vec![("parakeet", vec![f("p")]), ("speakers", vec![f("s")]), ("decider-4bit", vec![f("d4")]), ("decider-8bit", vec![f("d8")]), ("night", vec![f("n")]), ("embedding", vec![f("e")])]);
    // A server that never answers well: rows stay as asked, in order (they fail one by one).
    let downloads = Downloads::new(dir.path().into(), lock, Options { attempts: 1, ..options(&server) }, || {});
    let ids = |d: &Downloads| d.rows().iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    downloads.want(&["parakeet".into(), "speakers".into(), "decider-4bit".into(), "night".into(), "embedding".into()]);
    assert_eq!(ids(&downloads), ["parakeet", "speakers", "decider-4bit", "night", "embedding"]);
    wait_for(&downloads, |r| r.iter().all(|r| matches!(r.state, State::Failed(_))));
    downloads.choose_decider("decider-8bit");
    assert_eq!(ids(&downloads), ["parakeet", "speakers", "decider-8bit", "night", "embedding"]);
}

/// An update that changes a pin: the old files keep listening running until the new ones land.
#[test]
fn an_older_pins_files_stay_usable_until_the_new_ones_land() {
    let dir = tempfile::tempdir().unwrap();
    let old = lock_of(vec![("parakeet", vec![row("p/a.bin", b"old")])]);
    std::fs::create_dir_all(dir.path().join("p")).unwrap();
    std::fs::write(dir.path().join("p/a.bin"), b"old").unwrap();
    mark_done(dir.path(), old.group("parakeet").unwrap()).unwrap();
    let new = lock_of(vec![("parakeet", vec![row("p/a.bin", b"newer")])]);
    let group = new.group("parakeet").unwrap();
    assert!(!is_done(dir.path(), group) && is_usable(dir.path(), group));
    std::fs::remove_file(dir.path().join("p/a.bin")).unwrap();
    assert!(!is_usable(dir.path(), group), "gone by hand: not usable");
}

#[test]
fn the_models_folder_links_the_bundle_and_keeps_downloads_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("Heimdall Alpha.app/Contents/Resources/models");
    for f in ["silero/silero_vad.onnx", "phonon-31m-en/model.q8.gguf", "decide/runtime-v5/libd1v5_engine.dylib", "embeddinggemma2/native/heimdall-embedding-worker", "manifest.json"] {
        std::fs::create_dir_all(bundle.join(f).parent().unwrap()).unwrap();
        std::fs::write(bundle.join(f), f).unwrap();
    }
    let models = dir.path().join("data/models");
    let lock = Lock::embedded();
    prepare(&bundle, &models, &lock).unwrap();
    let is_link = |p: &str| std::fs::symlink_metadata(models.join(p)).is_ok_and(|m| m.file_type().is_symlink());
    assert!(is_link("silero") && is_link("phonon-31m-en") && is_link("decide/runtime-v5") && is_link("embeddinggemma2/native"));
    assert!(!is_link("decide") && !is_link("embeddinggemma2"), "folders the downloads write into are real");
    assert!(!models.join("manifest.json").exists(), "the bundle's manifest is the bundle's");
    assert_eq!(std::fs::read_to_string(models.join("silero/silero_vad.onnx")).unwrap(), "silero/silero_vad.onnx");
    // The app moved (an update): the links follow it on the next launch; a downloaded file stays.
    std::fs::create_dir_all(models.join("decide/d1/d1-2B-heimdall-v6-4bit")).unwrap();
    std::fs::write(models.join("decide/d1/d1-2B-heimdall-v6-4bit/config.json"), "{}").unwrap();
    let moved = dir.path().join("Applications/Heimdall Alpha.app/Contents/Resources/models");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&bundle, &moved).unwrap();
    prepare(&moved, &models, &lock).unwrap();
    assert_eq!(std::fs::read_link(models.join("silero")).unwrap(), moved.join("silero"));
    assert_eq!(std::fs::read_to_string(models.join("decide/runtime-v5/libd1v5_engine.dylib")).unwrap(), "decide/runtime-v5/libd1v5_engine.dylib");
    assert!(models.join("decide/d1/d1-2B-heimdall-v6-4bit/config.json").is_file());
}

/// The lock agrees with the locks it was written from, and every group the app asks for is in it.
#[test]
fn the_download_lock_pins_the_bundled_files_from_their_official_repos() {
    let lock = Lock::embedded();
    for p in decide::precision::Precision::ALL {
        for id in first_run_groups(p) {
            assert!(lock.group(&id).is_some(), "{id}");
        }
    }
    let listen: serde_json::Value = serde_json::from_str(include_str!("../../listen/scripts/models.lock.json")).unwrap();
    let parakeet = lock.group(PARAKEET).unwrap();
    let rows: Vec<&serde_json::Value> = listen["files"].as_array().unwrap().iter().filter(|r| r["path"].as_str().unwrap().starts_with("parakeet-ultra/")).collect();
    assert_eq!(parakeet.files.len(), rows.len());
    for (f, r) in parakeet.files.iter().zip(rows) {
        assert_eq!((f.path.as_str(), f.url.as_str(), f.sha256.as_str(), f.bytes), (r["path"].as_str().unwrap(), r["url"].as_str().unwrap(), r["sha256"].as_str().unwrap(), r["size"].as_u64().unwrap()));
    }
    let embedding: serde_json::Value = serde_json::from_str(include_str!("../../archive/embedding/assets.lock.json")).unwrap();
    let rows = embedding["files"].as_array().unwrap();
    let group = lock.group(EMBEDDING).unwrap();
    assert_eq!(group.files.len(), rows.len());
    assert!(group.files.last().unwrap().path.ends_with("model.safetensors"), "the weights land last: the worker starts on them");
    for f in &group.files {
        let r = rows.iter().find(|r| r["path"] == f.path.as_str()).unwrap();
        assert_eq!((f.sha256.as_str(), f.bytes), (r["sha256"].as_str().unwrap(), r["bytes"].as_u64().unwrap()));
        assert!(f.url.starts_with("https://huggingface.co/google/embeddinggemma-2/resolve/914f7f89142e33e77833254d9c9b90c3cef7303b/"));
    }
    let extra = include_str!("../../../scripts/extra-models.lock.json");
    let night = &lock.group(NIGHT).unwrap().files[0];
    assert!(extra.contains(&night.sha256) && night.url.starts_with("https://huggingface.co/ipsilondev/MossFormer2-SS-16K-ONNX/resolve/abd8b1268dfded6b0e972ddae4c2dd6a3faf4f8a/"));
    let bundle = include_str!("../../../scripts/bundle_models.py");
    for f in &lock.group(SPEAKERS).unwrap().files {
        assert!(bundle.contains(&f.sha256), "{} is the bundled speaker model", f.path);
    }
    // The decider: the published weights, tokenizer and calibration are the bundled bytes; the published config.json
    // drops `auto_map` and the 16-bit weights are the text model alone (scripts/downloads_lock.py, DECIDER_DIFFERS).
    let decide: serde_json::Value = serde_json::from_str(include_str!("../../decide/scripts/models.lock.json")).unwrap();
    for p in decide::precision::Precision::ALL {
        let group = lock.group(&decider_group(p)).unwrap();
        let folder = decide::precision::Pick::new(decide::precision::DeciderModel::V6, p).folder();
        assert_eq!(group.files.len(), 5);
        for f in &group.files {
            assert!(f.path.starts_with(&format!("decide/{folder}/")), "{}", f.path);
            assert!(f.url.starts_with("https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6/resolve/c015ecf6fed37b9477c27ab5790adb25ad9507df/"));
            let local = decide["files"].as_array().unwrap().iter().find(|r| format!("decide/{}", r["path"].as_str().unwrap()) == f.path).unwrap();
            // This tree pins the published 8-bit config itself (scripts/fetch_decider.sh checks the download against it).
            let differs = (f.path.ends_with("/config.json") && !f.path.contains("-v6-8bit/")) || (p == decide::precision::Precision::Bf16 && f.path.ends_with("model.safetensors"));
            assert_eq!(local["sha256"].as_str() != Some(&f.sha256), differs, "{}", f.path);
        }
    }
}

/// Opt-in (HEIMDALL_NETWORK_TESTS=1): one real, small pinned file from Hugging Face through the downloader, hashed.
#[test]
fn network_a_real_hugging_face_file_downloads_and_matches_its_hash() {
    if std::env::var("HEIMDALL_NETWORK_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set HEIMDALL_NETWORK_TESTS=1 to download from Hugging Face");
        return;
    }
    let lock = Lock::embedded();
    // Parakeet's vocabulary (151 KB) and one 243-byte file, from the pinned revision.
    let files: Vec<FileRow> = lock.group(PARAKEET).unwrap().files.iter().filter(|f| f.bytes < 200_000).take(2).cloned().collect();
    assert!(!files.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::blocking::Client::builder().connect_timeout(Duration::from_secs(30)).build().unwrap();
    for f in &files {
        fetch_file(&client, dir.path(), f, &Options::default(), &mut |_| {}, &|| false).unwrap();
        let mut h = Sha256::new();
        sha256_file(&dir.path().join(&f.path), &mut h).unwrap();
        assert_eq!(hex(&h.finalize()), f.sha256);
    }
}

/// The VM's cache server (HEIMDALL_MODEL_MIRROR): only an origin is taken, the pinned paths and checksums stay, and a
/// file the mirror serves with other bytes is refused like any other.
#[test]
fn a_test_mirror_replaces_only_the_origin_and_keeps_every_checksum() {
    assert_eq!(mirror(Some("http://192.168.64.1:8800/".into())).as_deref(), Some("http://192.168.64.1:8800"));
    assert_eq!(mirror(None), None);
    for bad in ["", "ftp://x", "http://", "http://host/path", "192.168.64.1:8800"] {
        assert_eq!(mirror(Some(bad.into())), None, "{bad}");
    }
    let o = Options { origin: mirror(Some("http://192.168.64.1:8800".into())), ..Options::default() };
    let lock = Lock::embedded();
    let f = &lock.group(PARAKEET).unwrap().files[0];
    assert_eq!(o.url(&f.url), f.url.replace("https://huggingface.co", "http://192.168.64.1:8800"));
    // A mirror serving other bytes: refused by the checksum.
    let data = body(4_000);
    let server = serve(data.iter().map(|b| b ^ 1).collect(), Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let err = fetch_file(&client(), dir.path(), &row("p/a.bin", &data), &Options { origin: mirror(Some(server.origin.clone())), ..options(&server) }, &mut |_| {}, &|| false).unwrap_err();
    assert!(format!("{err:#}").contains(CHECKSUM));
}
