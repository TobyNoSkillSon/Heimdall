//! Heimdall yields its memory to Arsenal (bug note 2026-11-09T12-43-19; contract: docs/personal/arsenal-yield.md).
//!
//! When Arsenal must load a massive model, Heimdall unloads Drama and gives that RAM to Arsenal;
//! the listener (voice activity), smart-turn, Parakeet and the decider are never unloaded; the embedding model and
//! search index may be unloaded and catch up later.
//!
//! Arsenal asks with a need file whose `"yield": true` (`speak::memory_line::Needs::asking`). Jarvis gives way through
//! the memory line itself (`Sample::yield_for`); this watcher, once a second, tells the embeddings indexer to unload
//! and wait (`speak::compute_flag::yield_asked`) and writes `<data>/compute/yield.json`, which Arsenal reads to know
//! when the memory it asked for is out. Nothing else is touched: the listening models stay loaded.
use heimdall_archive::embedding;
use speak::compute_flag::{self, YieldFile};
use speak::memory_line::{Needs, NEEDS_DIR};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const EVERY: Duration = Duration::from_secs(1);

/// What one look found: who asks, and whether Jarvis and the embeddings worker are out of memory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Look {
    pub asked_by: Vec<String>,
    pub jarvis_out: bool,
    pub embeddings_out: bool,
}

/// The file to write after `look`, when it differs from the last one written (`last`): nothing at rest, when nobody
/// asks and nobody asked.
pub fn next(look: &Look, last: Option<&YieldFile>, now: SystemTime) -> Option<YieldFile> {
    let file = compute_flag::yield_file(look.asked_by.clone(), look.jarvis_out, look.embeddings_out, now);
    let same = last.is_some_and(|l| l.state == file.state && l.asked_by == file.asked_by && l.released == file.released);
    let idle = last.is_none() && file.state == "none";
    (!same && !idle).then_some(file)
}

pub fn start(data: &Path, drama3: Arc<speak::drama3::Service>, embeddings: Arc<embedding::Service>) {
    let needs = Needs::new(data.join(NEEDS_DIR));
    let path: PathBuf = data.join(compute_flag::DIR).join(compute_flag::YIELD_FILE);
    let _ = std::thread::Builder::new().name("heimdall-arsenal-yield".into()).spawn(move || {
        let mut last: Option<YieldFile> = None;
        loop {
            let now = SystemTime::now();
            let asked_by = needs.asking(now);
            compute_flag::set_yield_asked(!asked_by.is_empty());
            // The memory-search reranker (rerank.rs) is a short-lived helper: it gives its room back at once.
            if !asked_by.is_empty() { heimdall_archive::rerank::unload_all(); }
            let look = Look {
                asked_by,
                jarvis_out: !drama3.resident(),
                // Busy (indexing or answering a query) is not out.
                embeddings_out: embeddings.0.try_lock().is_ok_and(|e| !e.running()),
            };
            if let Some(file) = next(&look, last.as_ref(), now) {
                eprintln!("arsenal-yield: {} (asked by {}; out: {}; kept: {})", file.state,
                    if file.asked_by.is_empty() { "nobody".into() } else { file.asked_by.join(", ") },
                    if file.released.is_empty() { "nothing".into() } else { file.released.join(", ") }, file.keeps.join(", "));
                if compute_flag::write_atomic(&path, &file).is_ok() {
                    last = Some(file);
                }
            }
            std::thread::sleep(EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_is_written_on_each_change_and_never_at_rest() {
        let now = SystemTime::now();
        let rest = Look::default();
        assert_eq!(next(&rest, None, now), None, "nobody asked: nothing is written");
        let asked = Look { asked_by: vec!["Arsenal FLUX.2 Dev (arsenal-flux.json)".into()], jarvis_out: false, embeddings_out: true };
        let first = next(&asked, None, now).expect("an ask is written");
        assert_eq!(first.state, "yielding");
        assert_eq!(next(&asked, Some(&first), now), None, "unchanged: not rewritten");
        let out = Look { jarvis_out: true, ..asked.clone() };
        let done = next(&out, Some(&first), now).expect("Jarvis out is written");
        assert_eq!(done.state, "yielded");
        assert_eq!(done.released, compute_flag::YIELDS.to_vec());
        let released = next(&Look { asked_by: vec![], ..out }, Some(&done), now).expect("the release is written");
        assert_eq!(released.state, "none");
    }
}
