//! Lab harness, never shipped (`cfg(test)` with `--features lab-timeline`, ignored): the old and the new `timeline_search` output for the same
//! queries against a real archive, read-only. `HEIMDALL_COMPARE_ARCHIVE` (the archive directory),
//! `HEIMDALL_COMPARE_MODELS` (the models directory with `embeddinggemma2`), `HEIMDALL_COMPARE_QUERIES` (a JSON list
//! of calls) and `HEIMDALL_COMPARE_OUT` (the JSON written). lab/timeline-diet/ has the queries and the page builder.
pub(crate) use crate::index;
#[allow(dead_code, unused_imports, clippy::all)]
mod old_timeline {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../lab/timeline-diet/old_mcp_timeline.rs"));
}
#[allow(dead_code, unused_imports, clippy::all)]
mod old_plain {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../lab/timeline-diet/old_plain.rs"));
}
use agent::delivery::DeliveryGate;
use agent::mcp::TranscriptSearch;
use anyhow::Result;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

#[tokio::test]
#[ignore]
async fn compare_old_and_new_output_on_the_live_archive() -> Result<()> {
    let env = |k: &str| std::env::var(k).map_err(|_| anyhow::anyhow!("{k} unset"));
    let root = PathBuf::from(env("HEIMDALL_COMPARE_ARCHIVE")?);
    let models = PathBuf::from(env("HEIMDALL_COMPARE_MODELS")?);
    let queries: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(env("HEIMDALL_COMPARE_QUERIES")?)?)?;
    let search = crate::index::ArchiveSearch { root: root.clone(), embeddings: Arc::new(heimdall_archive::embedding::Service::new(&models)) };
    let gate = Arc::new(DeliveryGate::new(true));
    let permit = gate.permit()?;
    let mut out = vec![];
    for q in queries {
        // Before: the installed build's path (50 candidates, the old page and lines).
        let mut old_q = q.clone();
        if q.get("vector").is_some() {
            let ranked = search.chunks(json!({"vector":q["vector"],"limit":50}), &permit).await?;
            old_q["_ranked"] = json!(ranked.as_array().unwrap().iter().map(|r| json!([r["id"], r["similarity"]])).collect::<Vec<_>>());
        }
        let started = std::time::Instant::now();
        let old = old_timeline::search(root.clone(), Arc::new(old_timeline::Pages::default()), old_q, &permit).await?;
        let old_ms = started.elapsed().as_millis();
        let before = old_plain::timeline(&old, &chrono::Local);
        // After: this build's path.
        let started = std::time::Instant::now();
        let new = search.timeline(q.clone(), &permit).await?;
        let new_ms = started.elapsed().as_millis();
        let after = agent::plain::timeline(&new, &chrono::Local);
        let mut full_q = q.clone();
        full_q["output"] = json!("full");
        let full = agent::plain::timeline(&search.timeline(full_q, &permit).await?, &chrono::Local);
        out.push(json!({"call":q,"before":format!("CALL: timeline_search {q}\n\n{before}"),"after":after,"full":full,
            "before_ids":old["hits"].as_array().unwrap().iter().map(|h|h["id"].clone()).collect::<Vec<_>>(),
            "after_ids":new["hits"].as_array().unwrap().iter().map(|h|h["h"].clone()).collect::<Vec<_>>(),
            "old_ms":old_ms,"new_ms":new_ms}));
    }
    std::fs::write(env("HEIMDALL_COMPARE_OUT")?, serde_json::to_string_pretty(&out)?)?;
    Ok(())
}
