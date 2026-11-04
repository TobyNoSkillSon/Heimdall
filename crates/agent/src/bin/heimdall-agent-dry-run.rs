//! JSONL observation/wake replay. This binary cannot instantiate a live harness.
use anyhow::Result;
use serde_json::Value;
use std::io::{self, BufRead};
#[tokio::main]
async fn main() -> Result<()> {
    let mut b = agent::Bridge::dry_run(":memory:")?;
    let mut emitted = 0;
    for line in io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line?)?;
        match v["type"].as_str() {
            Some("observation") => {
                b.observe(serde_json::from_value(v["observation"].clone())?)
                    .await?;
            }
            Some("wake") => {
                let images = if let Some(a) = v.get("attachments") {
                    serde_json::from_value(a.clone())?
                } else {
                    Vec::new()
                };
                let id = b
                    .wake(
                        v["text"].as_str().unwrap_or(""),
                        &images,
                        v["urgent"].as_bool().unwrap_or(false),
                    )
                    .await?;
                if let Some(id) = id {
                    b.handle(&agent::protocol::Event::Complete {
                        id: Some(id),
                        query: true,
                        error: false,
                        text: "dry-run simulated completion".into(),
                    })?;
                }
            }
            _ => anyhow::bail!("expected observation or wake"),
        };
        for record in &b.harness.log[emitted..] {
            println!("{record}");
        }
        emitted = b.harness.log.len();
    }
    b.shutdown().await
}
