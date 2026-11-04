//! Evidence extraction from committed screen recording; no native capture or permission prompts.
use agent::{
    delivery::DeliveryPermit,
    mcp_contract::{utc, wall_ns, IMAGE_BYTES},
};
use anyhow::{ensure, Context, Result};
use base64::Engine;
use heimdall_archive::{store, video};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
};
/// `screen_frames` while Seeing is off: a plain failure that never names the switch.
pub const SCREEN_FRAMES_UNAVAILABLE: &str = "CAPTURE_UNAVAILABLE: no screen frames are available";
pub async fn frames(
    tools: &crate::bridge::Tools,
    args: Value,
    permit: &DeliveryPermit,
) -> Result<Value> {
    permit.check()?;
    // Seeing off covers current and historical reads; the error never names the switch.
    ensure!(!tools.screen_off.load(Ordering::Acquire),"{}",SCREEN_FRAMES_UNAVAILABLE);
    let session = tools
        .archive_session
        .lock()
        .unwrap()
        .clone()
        .context("ARCHIVE_UNAVAILABLE: screen store is not attached")?;
    let root = crate::archive::store_root(&session);
    let permit = permit.clone();
    let cancellation = Arc::new(crate::index::SearchCancellation::new());
    let _cancel = crate::index::CancelSearch(cancellation.clone());
    tokio::task::spawn_blocking(move || extract(root, session, args, permit, cancellation)).await?
}
fn extract(
    root: PathBuf,
    session: PathBuf,
    args: Value,
    permit: DeliveryPermit,
    cancellation: Arc<crate::index::SearchCancellation>,
) -> Result<Value> {
    permit.check()?;
    let _pin = heimdall_archive::compact::reader_lock(&root)?;
    let db = store::read_index(&root)?;
    crate::index::install_search_cancellation(&db, cancellation.clone(), permit.clone())?;
    *cancellation.active.lock().unwrap() = Some(db.get_interrupt_handle());
    let count = args["count"].as_u64().unwrap_or(1) as usize;
    let display = args["display_id"]
        .as_str()
        .map(str::parse::<u32>)
        .transpose()?;
    let (session_id, from, to, chunk, whole_second) = if let Some(id) = args["chunk_id"].as_str() {
        let row:Option<(i64,i64,i64)>=db.query_row("SELECT session,start_wall_ns,end_wall_ns FROM timeline_chunks WHERE id=? AND kind='screen'",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (s, a, b) =
            row.context("FRAME_NOT_FOUND: use a screen chunk returned by timeline_search")?;
        (Some(s), a as u64, b as u64, true, false)
    } else {
        let at = args["at"].as_str().unwrap_or("now");
        let wall = if at == "now" {
            agent::now_ms() * 1_000_000
        } else {
            wall_ns(at)?
        };
        let current = at == "now";
        let id = if current {
            let name = session
                .file_name()
                .context("session name missing")?
                .to_string_lossy();
            Some(store::find_session(&db, &name)?.context("screen session missing")?)
        } else {
            None
        };
        (id, wall.saturating_sub(10_000_000_000), wall, false, !current && wall % 1_000_000_000 == 0)
    };
    let query=|to:u64|->Result<Vec<_>>{Ok(db.prepare("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE (?1 IS NULL OR session=?1) AND (?2 IS NULL OR display=?2) AND wall_ns<=?4 AND wall_ns+(end_ns-start_ns)>=?3 AND end_ns>start_ns ORDER BY wall_ns DESC,display,id DESC LIMIT 16")?.query_map(params![session_id,display,from as i64,to as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(7)?,r.get::<_,String>(8)?,r.get::<_,bool>(9)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)};
    // An archived time is exact: the frame at it (or the last one before it). `timeline_search` prints times cut to
    // whole seconds, so a time on the second with nothing at or before it means that second: its first frame, from a
    // segment that began 33 ms into it (bug note 2026-10-26T21-31, part 2). Never the second's last frame (review
    // 27 Oct: the widened window returned that even when a frame stood at the time itself).
    let mut segments=query(to)?;
    let first_in_second=whole_second&&segments.is_empty();
    if first_in_second { segments=query(to+999_999_999)?; }
    let to=if first_in_second { to+999_999_999 } else { to };
    ensure!(
        !segments.is_empty(),
        "FRAME_NOT_FOUND: no committed frame in this ten-second lookup window"
    );
    let mut candidates = vec![];
    let mut used = std::collections::HashSet::new();
    for (id, s, geometry, format, start, end, wall, fps, tier, closed) in segments {
        let geometry: video::Geometry = serde_json::from_str(&geometry)?;
        if !chunk && !used.insert(geometry.display) {
            continue;
        }
        let seg = video::Segment {
            id,
            session: s,
            geometry,
            format: serde_json::from_str(&format)?,
            start_ns: start as u64,
            end_ns: end as u64,
            wall_ns: wall as u64,
            fps: fps as u32,
            tier,
            closed,
        };
        let low = from.max(seg.wall_ns);
        let high = to.min(seg.wall_ns + seg.end_ns - seg.start_ns - 1);
        if low > high {
            continue;
        }
        for i in 0..if chunk { count } else { 1 } {
            let wall = if count > 1 && chunk {
                low + (high - low) * i as u64 / (count - 1) as u64
            } else if first_in_second {
                low
            } else {
                high
            };
            let host = seg.start_ns + wall - seg.wall_ns;
            candidates.push((wall, host, seg.clone()));
        }
        if !chunk {
            break;
        }
        if candidates.len() >= count {
            break;
        }
    }
    candidates.sort_by_key(|c| c.0);
    candidates.truncate(count);
    let mut content = vec![];
    let mut bytes = 0;
    let mut warnings = vec![];
    for (wall, host, seg) in candidates {
        permit.check()?;
        let result = (|| -> Result<(Value, Vec<u8>)> {
            let samples = video::samples(&root, seg.id, host)?;
            let sample = samples
                .last()
                .context("recording has no sample at this time")?;
            let pixels = if seg.format.parameters.is_empty() {
                image::load_from_memory_with_format(&sample.data, image::ImageFormat::Png)?
                    .into_rgb8()
            } else {
                #[cfg(target_os = "macos")]
                {
                    let mut decoder = video::native::Decoder::new(&seg.format)?;
                    for s in samples.iter().take(samples.len() - 1) {
                        permit.check()?;
                        decoder.advance(s)?;
                    }
                    image::RgbImage::from_raw(
                        seg.format.width,
                        seg.format.height,
                        decoder.decode(sample)?,
                    )
                    .context("invalid decoded frame")?
                }
                #[cfg(not(target_os = "macos"))]
                {
                    anyhow::bail!("HEVC decoding requires macOS")
                }
            };
            permit.check()?;
            let quality = args["quality"].as_str().unwrap_or("preview");
            let (w, h) = pixels.dimensions();
            ensure!(
                w as usize * h as usize <= 32 * 1024 * 1024,
                "frame pixel bound exceeded"
            );
            let pixels = if quality == "preview" && w.max(h) > 1280 {
                let ratio = 1280. / w.max(h) as f64;
                image::imageops::resize(
                    &pixels,
                    (w as f64 * ratio) as u32,
                    (h as f64 * ratio) as u32,
                    image::imageops::FilterType::Triangle,
                )
            } else {
                pixels
            };
            let mut data = vec![];
            if quality == "full" {
                pixels.write_to(
                    &mut std::io::Cursor::new(&mut data),
                    image::ImageFormat::Png,
                )?;
            } else {
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, 82)
                    .encode_image(&pixels)?;
            }
            let capture = seg.wall_ns + sample.host_ns - seg.start_ns;
            Ok((
                json!({"frame_id":format!("screen:{}:{}:{}",seg.session,seg.id,sample.host_ns),"capture":utc(capture),"coverage_at":utc(wall),"display_id":seg.geometry.display.to_string(),"display":{"x":seg.geometry.x,"y":seg.geometry.y,"width":seg.geometry.width,"height":seg.geometry.height},"width":pixels.width(),"height":pixels.height(),"quality":quality,"archived":args.get("chunk_id").is_some()||args["at"].as_str().is_some_and(|a|a!="now"),"source_tier":seg.tier}),
                data,
            ))
        })();
        match result {
            Ok((metadata, data)) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(data);
                ensure!(
                    bytes + encoded.len() < IMAGE_BYTES,
                    "OUTPUT_TOO_LARGE: request a preview or fewer frames"
                );
                bytes += encoded.len();
                content.push(json!({"type":"text","text":metadata.to_string()}));
                content.push(json!({"type":"image","mimeType":if args["quality"]=="full"{"image/png"}else{"image/jpeg"},"data":encoded}));
            }
            Err(_) => warnings.push(format!(
                "Frame at {} could not be decoded; choose another time",
                utc(wall)
            )),
        }
    }
    ensure!(
        !content.is_empty(),
        "FRAME_NOT_FOUND: committed frames could not be decoded; choose another time"
    );
    for warning in warnings {
        content.push(json!({"type":"text","text":warning}));
    }
    permit.check()?;
    Ok(json!({"content":content}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn archived_fixture_metadata_preview_and_seeing_off() -> Result<()> {
        let d = tempfile::tempdir()?;
        let session = d.path().join("archive/session-test");
        std::fs::create_dir_all(&session)?;
        let journal = heimdall_archive::Journal::open(&session)?;
        let writer = video::Writer::open(&session)?;
        let wall = 1_760_000_000_000_000_000;
        let geometry = video::Geometry {
            display: 7,
            x: -100.,
            y: 0.,
            width: 1600,
            height: 4,
        };
        let format = video::Format {
            width: 1600,
            height: 4,
            hvcc: vec![],
            parameters: vec![],
        };
        let segment = writer.begin(&geometry, &format, 10_000_000_000, wall, 5)?;
        let pixels = image::RgbImage::from_pixel(1600, 4, image::Rgb([10, 20, 30]));
        let mut data = std::io::Cursor::new(vec![]);
        pixels.write_to(&mut data, image::ImageFormat::Png)?;
        writer.append(
            segment,
            &video::Sample {
                host_ns: 10_000_000_000,
                end_ns: 11_000_000_000,
                key: true,
                data: data.into_inner(),
            },
        )?;
        let tools = crate::bridge::Tools::new(d.path(), d.path());
        *tools.archive_session.lock().unwrap() = Some(session);
        let p = tools.agent_enabled.permit()?;
        let result = frames(
            &tools,
            json!({"at":utc(wall+200_000_000),"display_id":"7"}),
            &p,
        )
        .await?;
        let meta: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())?;
        assert_eq!(meta["display_id"], "7");
        assert_eq!(meta["width"], 1280);
        assert_eq!(meta["capture"], utc(wall));
        assert!(meta["archived"] == true);
        assert_eq!(result["content"][1]["mimeType"], "image/jpeg");
        assert!(result.to_string().len() < IMAGE_BYTES);
        tools.screen_off.store(true, Ordering::Release);
        assert!(frames(&tools, json!({"at":utc(wall)}), &p)
            .await
            .unwrap_err()
            .to_string()
            .contains(SCREEN_FRAMES_UNAVAILABLE));
        drop(journal);
        Ok(())
    }
    /// Bug note 2026-10-26T21-31 part 2: `timeline_search` listed a frame at 21:30:07, the segment began at
    /// 21:30:07.033, and `screen_frames` at 21:30:07 found nothing.
    #[tokio::test]
    async fn a_whole_second_time_finds_the_frame_starting_inside_that_second() -> Result<()> {
        let d = tempfile::tempdir()?;
        let session = d.path().join("archive/session-test");
        std::fs::create_dir_all(&session)?;
        let journal = heimdall_archive::Journal::open(&session)?;
        let writer = video::Writer::open(&session)?;
        let second = 1_760_000_000_000_000_000;
        let geometry = video::Geometry { display: 7, x: 0., y: 0., width: 16, height: 4 };
        let format = video::Format { width: 16, height: 4, hvcc: vec![], parameters: vec![] };
        let segment = writer.begin(&geometry, &format, 10_000_000_000, second + 33_000_000, 5)?;
        let mut data = std::io::Cursor::new(vec![]);
        image::RgbImage::from_pixel(16, 4, image::Rgb([10, 20, 30])).write_to(&mut data, image::ImageFormat::Png)?;
        writer.append(segment, &video::Sample { host_ns: 10_000_000_000, end_ns: 11_000_000_000, key: true, data: data.into_inner() })?;
        let tools = crate::bridge::Tools::new(d.path(), d.path());
        *tools.archive_session.lock().unwrap() = Some(session);
        let p = tools.agent_enabled.permit()?;
        let result = frames(&tools, json!({"at":utc(second),"display_id":"7"}), &p).await?;
        let meta: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())?;
        assert_eq!(meta["display_id"], "7");
        assert_eq!(meta["coverage_at"], utc(second + 33_000_000), "the second's first frame, not its last");
        // Review 27 Oct: a whole second the segment already covers is exact too, not the last frame of that second.
        let result = frames(&tools, json!({"at":utc(second + 1_000_000_000),"display_id":"7"}), &p).await?;
        let meta: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())?;
        assert_eq!(meta["coverage_at"], utc(second + 1_000_000_000));
        // A time inside a second is exact: nothing later than it.
        assert!(frames(&tools, json!({"at":utc(second + 20_000_000),"display_id":"7"}), &p).await.is_err());
        drop(journal);
        Ok(())
    }
}
