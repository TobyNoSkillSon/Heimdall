//! Agent-only vocabulary fixtures from an isolated archive, using the app's media reader.
use anyhow::{ensure, Context, Result};
use heimdall_archive::{
    embedding::{self, Part},
    timeline::Chunk,
};
use serde_json::{json, Value};
use std::path::PathBuf;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).map(PathBuf::from).collect();
    ensure!(
        args.len() == 3,
        "usage: heimdall-vocabulary-fixtures ARCHIVE SELECTION OUTPUT"
    );
    let root = args[0].canonicalize()?;
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    ensure!(
        root.starts_with(project.join("target")),
        "only this worktree's target archive copies are accepted"
    );
    let rows: Vec<Value> = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    std::fs::create_dir_all(&args[2])?;
    let mut manifest = vec![];
    for mut row in rows {
        let id = row["id"].as_str().context("id")?.to_owned();
        ensure!(
            id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "unsafe fixture id"
        );
        let destination = args[2].join(&id);
        let c = Chunk {
            id: id.clone(),
            session: row["session"].as_i64().context("session")?,
            kind: "speech_audio".into(),
            role: "speech".into(),
            source: row["source_id"].as_str().context("source")?.into(),
            start_ns: row["start_ns"].as_u64().context("start")?,
            end_ns: row["end_ns"].as_u64().context("end")?,
            start_wall_ns: 0,
            end_wall_ns: 0,
            text: String::new(),
            details: json!({}),
            tags: vec![],
            links: vec![],
            content_hash: String::new(),
            similarity: None,
        };
        match embedding::prepare_chunk(&root, &c, &destination) {
            Ok(inputs) => {
                let paths: Vec<_> = inputs
                    .iter()
                    .flat_map(|i| &i.parts)
                    .filter_map(|p| {
                        if let Part::Audio { path } = p {
                            Some(path)
                        } else {
                            None
                        }
                    })
                    .collect();
                // Keep captured discontinuities as separate clips; never join across gaps.
                for (index, path) in paths.iter().enumerate() {
                    let samples = heimdall_archive::audio::pcm_from_bytes(&std::fs::read(path)?)?;
                    let wav = destination.join(format!("part-{index}.wav"));
                    let mut writer = hound::WavWriter::create(
                        &wav,
                        hound::WavSpec {
                            channels: 1,
                            sample_rate: 16000,
                            bits_per_sample: 32,
                            sample_format: hound::SampleFormat::Float,
                        },
                    )?;
                    for sample in &samples {
                        writer.write_sample(*sample)?;
                    }
                    writer.finalize()?;
                    let mut part = row.clone();
                    part["id"] = json!(format!("{id}-{index}"));
                    part["path"] = json!(wav.canonicalize()?);
                    part["parts"] = json!(paths.len());
                    part["samples"] = json!(samples.len());
                    manifest.push(part);
                    std::fs::remove_file(path)?;
                }
            }
            Err(e) => {
                row["error"] = json!(format!("{e:#}"));
                eprintln!("{row}");
            }
        }
    }
    std::fs::write(
        args[2].join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!("{}", json!({"clips":manifest.len()}));
    Ok(())
}
