//! Hub access against a local stand-in for the Hub (no real download): the listing, resumed and verified downloads
//! into the standard cache layout, reuse of cached blobs, offline resolution, and GGUF headers read by range.
mod hub_support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hub_support::{git_sha1, listing, serve, Stub};

use serde_json::json;
use sha2::{Digest, Sha256};
use splash::hub::cache::{Hub, Repository};
use splash::hub::gguf::Metadata;
use splash::hub::Event;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_COMMIT: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn hub(cache: &Path, stub: &Stub, offline: bool) -> Hub {
    let mut hub = Hub::new(Some(cache.to_path_buf()), offline);
    hub.endpoint = stub.url.clone();
    hub
}

fn names(list: &[&str]) -> std::collections::BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn downloads_resume_verify_and_land_in_the_hub_cache_layout() {
    let weights: Vec<u8> = (0..300_000u32).flat_map(|i| i.to_le_bytes()).collect();
    let config = br#"{"model_type": "x"}"#.to_vec();
    let mut corrupt = weights.clone();
    corrupt[12345] ^= 1;
    let sha = hex::encode(Sha256::digest(&weights));
    let mut routes = HashMap::new();
    let files: [(&str, &[u8], bool); 2] = [("config.json", &config, false), ("w/model.gguf", &weights, true)];
    routes.insert("/api/models/o/r?blobs=true".into(), listing(COMMIT, &files));
    routes.insert(format!("/api/models/o/r/revision/{OTHER_COMMIT}?blobs=true"), listing(OTHER_COMMIT, &files));
    routes.insert(format!("/o/r/resolve/{COMMIT}/config.json"), config.clone());
    routes.insert(format!("/o/r/resolve/{COMMIT}/w/model.gguf"), weights.clone());
    routes.insert("/api/models/o/bad?blobs=true".into(), listing(COMMIT, &[("w.bin", &weights, true)]));
    routes.insert(format!("/o/bad/resolve/{COMMIT}/w.bin"), corrupt);
    let stub = serve(routes);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("hub");
    let hub = hub(&cache, &stub, false);

    let repo = Repository::resolve(&hub, "o/r", None, None, None, None).unwrap();
    assert_eq!(repo.revision.as_deref(), Some(COMMIT));
    assert_eq!(repo.files, names(&["config.json", "w/model.gguf"]));
    // A download an earlier attempt left part-way is resumed from where it stopped.
    let folder = cache.join("models--o--r");
    std::fs::create_dir_all(folder.join("blobs")).unwrap();
    std::fs::write(folder.join(format!("blobs/{sha}.incomplete")), &weights[..1000]).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let progress = move |e: &Event| sink.lock().unwrap().push(e.clone());
    let paths = repo.download(&hub, &names(&["config.json", "w/model.gguf"]), &progress).unwrap();
    assert!(stub.requests.lock().unwrap().iter().any(|(p, r)| p.ends_with("model.gguf") && r.as_deref() == Some("bytes=1000-")));
    assert_eq!(std::fs::read(&paths["w/model.gguf"]).unwrap(), weights);
    assert_eq!(std::fs::read(&paths["config.json"]).unwrap(), config);
    // huggingface_hub's layout: relative snapshot links to blobs named by digest and the tree listing; no ref, as
    // Splash downloads by commit and never writes one.
    let pointer = folder.join(format!("snapshots/{COMMIT}/w/model.gguf"));
    assert_eq!(std::fs::read_link(&pointer).unwrap(), Path::new(&format!("../../../blobs/{sha}")));
    assert_eq!(std::fs::read_link(folder.join(format!("snapshots/{COMMIT}/config.json"))).unwrap(),
        Path::new(&format!("../../blobs/{}", git_sha1(&config))));
    assert!(!folder.join("refs").exists());
    let tree: serde_json::Value =
        serde_json::from_slice(&std::fs::read(folder.join(format!("trees/{COMMIT}.json"))).unwrap()).unwrap();
    assert_eq!(tree["files"]["w/model.gguf"]["lfs_sha256"], json!(sha));
    assert!(!folder.join(format!("blobs/{sha}.incomplete")).exists());
    let done = events.lock().unwrap().iter().any(|e| matches!(e, Event::Download { done, total, .. } if done == total));
    assert!(done);

    // Cached files are reused: no further file request, for this commit or another sharing its blobs.
    let before = stub.requests.lock().unwrap().len();
    repo.download(&hub, &names(&["config.json", "w/model.gguf"]), &|_| {}).unwrap();
    let other = Repository::resolve(&hub, "o/r", Some(OTHER_COMMIT), None, None, None).unwrap();
    let shared = other.download(&hub, &names(&["w/model.gguf"]), &|_| {}).unwrap();
    assert_eq!(std::fs::read(&shared["w/model.gguf"]).unwrap(), weights);
    let after: Vec<_> = stub.requests.lock().unwrap()[before..].iter().map(|(p, _)| p.clone()).collect();
    assert!(after.iter().all(|p| p.starts_with("/api/")), "{after:?}");

    // Offline, refs/main (as huggingface_hub writes it for a branch download) names the commit, and the cached
    // snapshot stands in.
    std::fs::create_dir_all(folder.join("refs")).unwrap();
    std::fs::write(folder.join("refs/main"), COMMIT).unwrap();
    let offline = Repository::resolve(&self::hub(&cache, &stub, true), "o/r", None, None, None, None).unwrap();
    assert_eq!(offline.revision.as_deref(), Some(COMMIT));
    assert!(offline.unreachable.unwrap().contains("HF_HUB_OFFLINE"));
    assert_eq!(offline.files, names(&["config.json", "w/model.gguf"]));

    // Content that does not match its published digest is refused and not kept.
    let bad = Repository::resolve(&hub, "o/bad", None, None, None, None).unwrap();
    let error = bad.download(&hub, &names(&["w.bin"]), &|_| {}).unwrap_err().to_string();
    assert!(error.contains("does not match its published digest"), "{error}");
    let blobs = cache.join("models--o--bad/blobs");
    assert!(!blobs.join(&sha).exists() && !blobs.join(format!("{sha}.incomplete")).exists());
}

#[test]
fn uncached_gguf_headers_are_read_by_range_requests() {
    let mut data = hub_support::tiny_qwen(12);
    data.extend(vec![0u8; 4 << 20]); // tensor data, which a header read never asks for
    let mut routes = HashMap::new();
    routes.insert("/api/models/o/g?blobs=true".into(), listing(COMMIT, &[("m.gguf", &data, true)]));
    routes.insert(format!("/o/g/resolve/{COMMIT}/m.gguf"), data.clone());
    let stub = serve(routes);
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(&dir.path().join("hub"), &stub, false);
    let repo = Repository::resolve(&hub, "o/g", None, None, None, None).unwrap();
    assert!(!repo.present(&hub, "m.gguf"));
    assert_eq!(repo.size(&hub, "m.gguf"), Some(data.len() as u64));
    let header = Metadata::read(repo.open(&hub, "m.gguf").unwrap(), true).unwrap();
    assert_eq!(header.tensors.len(), 3 + 4 * 2 + 3 * 9 + 6 + 4 * 3);
    let requests = stub.requests.lock().unwrap();
    let file_requests: Vec<_> = requests.iter().filter(|(p, _)| p.ends_with("m.gguf")).collect();
    assert!(file_requests.iter().all(|(_, r)| r.as_deref().is_some_and(|r| r.starts_with("bytes=0-"))));
    assert_eq!(file_requests.len(), 1);
    // Nothing was written to the cache.
    assert!(!dir.path().join("hub").exists());
}

#[test]
fn unknown_repositories_say_why() {
    let stub = serve(HashMap::new());
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(&dir.path().join("hub"), &stub, false);
    let error = Repository::resolve(&hub, "o/missing", None, None, None, None).unwrap_err().to_string();
    assert!(error.contains("404") && error.contains("the Hub cache records no commit"), "{error}");
}

#[test]
fn failed_range_reads_report_the_hub_not_a_truncated_header() {
    let data = hub_support::tiny_qwen(12);
    let mut routes = HashMap::new();
    routes.insert("/api/models/o/denied?blobs=true".into(), listing(COMMIT, &[("m.gguf", &data, true)]));
    let stub = serve(routes);
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(&dir.path().join("hub"), &stub, false);
    let repo = Repository::resolve(&hub, "o/denied", None, None, None, None).unwrap();
    let error = Metadata::read(repo.open(&hub, "m.gguf").unwrap(), true).unwrap_err().to_string();
    assert!(error.contains("403") && error.contains("HF_TOKEN"), "{error}");
    assert!(!error.contains("truncated"), "{error}");
}

#[test]
fn local_directories_do_not_follow_folder_links() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("draft/sub")).unwrap();
    std::fs::write(dir.path().join("draft/config.json"), "{}").unwrap();
    std::os::unix::fs::symlink(dir.path().join("draft"), dir.path().join("draft/sub/loop")).unwrap();
    let repo = Repository::local_directory(&dir.path().join("draft")).unwrap();
    assert_eq!(repo.files, names(&["config.json"]));
}
