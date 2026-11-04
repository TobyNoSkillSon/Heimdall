//! Offline session maintenance, only after Pi has exited. The full source is archived
//! before a compaction checkpoint bounds the hot file. No old tool result is edited.
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader},
    path::Path,
};
pub const ROTATE_BYTES: u64 = 32 * 1024 * 1024;
#[derive(Clone, Debug)]
pub struct Receipt {
    pub id: String,
    pub completed: bool,
    pub effects: bool,
}
pub fn receipts(path: &Path) -> Result<Vec<Receipt>> {
    Ok(evidence(path)?.receipts)
}
/// What Pi durably recorded about Heimdall's deliveries: receipts, the deliveries a tool call belonged to (the
/// extension appends that record before each tool runs), and every delivery whose words Pi took in (its user message
/// with the delivery marker, or any receipt or effects record of it).
#[derive(Clone, Debug, Default)]
pub struct Evidence {
    pub receipts: Vec<Receipt>,
    pub effects: HashSet<String>,
    pub consumed: HashSet<String>,
}
/// The delivery id a Pi user message line carries (`harness::marked_prompt`), read without parsing the line (wakes
/// can carry megabytes of images) and in any key order (Pi writes its own order, `rotate` rewrites lines sorted).
fn delivery_marker(line: &str) -> Option<&str> {
    if !line.contains("\"type\":\"message\"") || !line.contains("\"role\":\"user\"") { return None; }
    let at = ["\"text\":\"[Heimdall delivery: ", "\"content\":\"[Heimdall delivery: "].iter().find_map(|m| line.find(m).map(|at| at + m.len()))?;
    let rest = &line[at..];
    let id = &rest[..rest.find(']')?];
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(id)
}
pub fn evidence(path: &Path) -> Result<Evidence> {
    let mut out = Evidence::default();
    if !path.exists() {
        return Ok(out);
    }
    // A line cut off mid-character (a write that stopped half-way) is read like any other unparseable line.
    for line in BufReader::new(std::fs::File::open(path)?).split(b'\n') {
        out.read(&String::from_utf8_lossy(&line?));
    }
    Ok(out)
}
impl Evidence {
    /// What one session line says about a delivery (the simulator's in-memory session reads through it too).
    pub fn read(&mut self, line: &str) {
        if let Some(id) = delivery_marker(line) {
            self.consumed.insert(id.into());
            return;
        }
        // Otherwise only custom entries matter; skip the large message lines without parsing them.
        if !line.contains("\"heimdall_") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if v["type"] != "custom" {
            return;
        }
        if v["customType"] == "heimdall_receipt" {
            if let Some(id) = v["data"]["id"].as_str() {
                self.consumed.insert(id.into());
                self.receipts.push(Receipt {
                    id: id.into(),
                    completed: v["data"]["completed"] == true,
                    effects: v["data"]["effects"] == true,
                });
            }
        } else if v["customType"] == "heimdall_effects" {
            for id in v["data"]["ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                self.effects.insert(id.into());
                self.consumed.insert(id.into());
            }
        }
    }
}
/// The last request that carried the history, as Pi recorded it: the assistant message's start time (ms), provider and
/// model, from the newest assistant message with prompt usage after the last compaction (`maintenance`). `None` when
/// a compaction came after it (nothing cached is worth keeping) or there is none. Reads the file from its end, a block
/// at a time, and stops at the newest line that decides (night review: the 31 MB session was read whole at every
/// agent start).
pub fn last_request(path: &Path) -> Result<Option<(u64, String, String)>> {
    Ok(last_request_from_tail(path, TAIL_BLOCK)?.0)
}
/// Bytes `last_request` reads per step from the end of the session.
const TAIL_BLOCK: usize = 256 * 1024;
/// `last_request` with `block` bytes per step; also returns how many bytes it read.
fn last_request_from_tail(path: &Path, block: usize) -> Result<(Option<(u64, String, String)>, u64)> {
    scan_tail(path, block, decisive_request)
}
/// When the conversation's own record ends: the newest message or context message Pi wrote (any role but system,
/// failed requests included: their words are in the history), as Unix ms. A compaction entry does not count: it
/// summarises what came before it, and its own time is when the summary finished. Reads from the end (catch-up after
/// a start, lab/harness/EXPECTED.md E4.1).
pub fn history_end(path: &Path) -> Result<Option<u64>> {
    Ok(scan_tail(path, TAIL_BLOCK, history_entry_time)?.0)
}
fn history_entry_time(line: &[u8]) -> Option<Option<u64>> {
    let line = std::str::from_utf8(line).ok()?;
    if !(line.contains("\"type\":\"message\"") || line.contains("\"type\":\"custom_message\"")) { return None; }
    let v = serde_json::from_str::<Value>(line).ok()?;
    let counts = (v["type"] == "message" && v["message"]["role"] != "system") || v["type"] == "custom_message";
    if !counts { return None; }
    let at = chrono::DateTime::parse_from_rfc3339(v["timestamp"].as_str()?).ok()?.timestamp_millis();
    Some(u64::try_from(at).ok())
}
/// Reads `path` from its end, a block at a time, and returns the first line (newest first) `decide` answers for;
/// also how many bytes it read.
fn scan_tail<T>(path: &Path, block: usize, decide: impl Fn(&[u8]) -> Option<Option<T>>) -> Result<(Option<T>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let decisive_request = &decide;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, 0)),
        Err(e) => return Err(e.into()),
    };
    let mut end = file.metadata()?.len();
    let mut read = 0u64;
    // The rest of the line the current block ends inside (read from later blocks), in file order.
    let mut parts: std::collections::VecDeque<Vec<u8>> = Default::default();
    while end > 0 {
        let start = end.saturating_sub(block as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0; (end - start) as usize];
        file.read_exact(&mut buf)?;
        read += buf.len() as u64;
        let mut right = buf.len();
        for i in (0..buf.len()).rev().filter(|&i| buf[i] == b'\n') {
            let decided = if parts.is_empty() {
                decisive_request(&buf[i + 1..right])
            } else {
                let mut line = buf[i + 1..right].to_vec();
                for part in parts.drain(..) { line.extend_from_slice(&part); }
                decisive_request(&line)
            };
            if let Some(decided) = decided { return Ok((decided, read)); }
            right = i;
        }
        if right > 0 { parts.push_front(buf[..right].to_vec()); }
        end = start;
    }
    // The first line has no newline before it.
    let line: Vec<u8> = parts.into_iter().flatten().collect();
    Ok((decisive_request(&line).flatten(), read))
}
#[cfg(test)]
mod history_end_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn the_history_ends_at_its_newest_message_not_at_a_compaction_or_a_receipt() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        assert_eq!(history_end(&p).unwrap(), None);
        let rows = [
            json!({"type":"session","version":3,"id":"s","timestamp":"2026-10-20T16:38:00.000Z"}),
            json!({"type":"message","id":"u","timestamp":"2026-10-20T17:20:43.492Z","message":{"role":"user","content":"hi"}}),
            json!({"type":"message","id":"e","timestamp":"2026-10-20T17:20:55.365Z","message":{"role":"assistant","content":[],"stopReason":"error"}}),
            json!({"type":"compaction","id":"c","timestamp":"2026-10-20T17:22:04.000Z","summary":"s"}),
            json!({"type":"custom","id":"r","timestamp":"2026-10-20T17:22:05.000Z","customType":"heimdall_receipt","data":{}}),
            json!({"type":"message","id":"sys","timestamp":"2026-10-20T17:22:06.000Z","message":{"role":"system","content":""}}),
        ];
        std::fs::write(&p, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let errored = chrono::DateTime::parse_from_rfc3339("2026-10-20T17:20:55.365Z").unwrap().timestamp_millis() as u64;
        assert_eq!(history_end(&p).unwrap(), Some(errored), "a failed request's words are in the history");
        let context = json!({"type":"custom_message","id":"ctx","timestamp":"2026-10-20T17:22:07.699Z","customType":"heimdall_context","content":"catch-up"});
        std::fs::write(&p, format!("{}{context}\n", std::fs::read_to_string(&p).unwrap())).unwrap();
        assert_eq!(history_end(&p).unwrap(), Some(errored + 72_334), "a start's context message is history too");
    }
}
/// One session line's say on `last_request`: `Some(None)` a compaction (nothing cached since), `Some(Some(..))` an
/// assistant message with prompt usage, `None` neither (read on).
fn decisive_request(line: &[u8]) -> Option<Option<(u64, String, String)>> {
    let line = std::str::from_utf8(line).ok()?;
    // Text inside a message is escaped, so an unescaped `"type":"compaction"` is an entry's own type.
    let compaction = line.contains("\"type\":\"compaction\"");
    if !compaction && !(line.contains("\"role\":\"assistant\"") && line.contains("\"usage\"")) {
        return None;
    }
    let v = serde_json::from_str::<Value>(line).ok()?;
    if v["type"] == "compaction" {
        return Some(None);
    }
    let m = &v["message"];
    if v["type"] != "message" || m["role"] != "assistant" {
        return None;
    }
    let prompt = ["input", "cacheRead", "cacheWrite"].iter().map(|k| m["usage"][*k].as_u64().unwrap_or(0)).sum::<u64>();
    match (prompt > 0, m["timestamp"].as_u64()) {
        (true, Some(at)) => Some(Some((at, m["provider"].as_str().unwrap_or_default().to_owned(), m["model"].as_str().unwrap_or_default().to_owned()))),
        _ => None,
    }
}
/// How large the conversation was at its last request, in tokens, as Pi counts it for compaction (the newest
/// assistant message's usage: `totalTokens`, else input + output + cache reads and writes). `None` when a compaction
/// came after it, or no request has usage. A model switch uses it to carry a history past the new budget to the next
/// quiet moment (`provider::Limits::carrying`). Reads from the end, like `last_request`.
pub fn history_tokens(path: &Path) -> Result<Option<u64>> {
    Ok(scan_tail(path, TAIL_BLOCK, history_size)?.0)
}
fn history_size(line: &[u8]) -> Option<Option<u64>> {
    let line = std::str::from_utf8(line).ok()?;
    let compaction = line.contains("\"type\":\"compaction\"");
    if !compaction && !(line.contains("\"role\":\"assistant\"") && line.contains("\"usage\"")) {
        return None;
    }
    let v = serde_json::from_str::<Value>(line).ok()?;
    if v["type"] == "compaction" {
        return Some(None);
    }
    let m = &v["message"];
    if v["type"] != "message" || m["role"] != "assistant" {
        return None;
    }
    let u = &m["usage"];
    let total = u["totalTokens"].as_u64().filter(|t| *t > 0)
        .unwrap_or_else(|| ["input", "output", "cacheRead", "cacheWrite"].iter().map(|k| u[*k].as_u64().unwrap_or(0)).sum());
    // An aborted or failed request with no usage says nothing about the size; an earlier one does.
    (total > 0).then_some(Some(total))
}
/// Returns true only when a safe checkpoint was present and every entry the model sees was kept (with the model and
/// thinking settings); context edits of summarised entries and unparseable lines go with the summarised history.
/// `archive` must durably preserve the ENTIRE original before this function replaces it.
pub fn rotate(
    path: &Path,
    threshold: u64,
    archive: impl FnOnce(&[u8]) -> Result<()>,
) -> Result<bool> {
    if !path.is_file() || path.metadata()?.len() < threshold {
        return Ok(false);
    }
    let bytes = std::fs::read(path)?;
    // Pi skips a line it cannot parse (one cut off when a write stopped half-way); the rewrite drops it the same way,
    // so such a line never blocks rotation. The archive keeps the original.
    let mut lines: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
        .collect();
    let header = lines.first().context("missing Pi session header")?.clone();
    ensure!(
        header["type"] == "session" && header["version"] == 3,
        "unsupported Pi session format"
    );
    let entries = lines.split_off(1);
    let map: HashMap<_, _> = entries
        .iter()
        .filter_map(|v| v["id"].as_str().map(|id| (id.to_string(), v.clone())))
        .collect();
    let Some(mut cursor) = entries
        .last()
        .and_then(|v| v["id"].as_str())
        .map(str::to_owned)
    else {
        return Ok(false);
    };
    let mut seen = HashSet::new();
    let mut branch = vec![];
    loop {
        ensure!(seen.insert(cursor.clone()), "session tree cycle");
        let Some(v) = map.get(&cursor) else {
            anyhow::bail!("session parent missing");
        };
        branch.push(v.clone());
        match v["parentId"].as_str() {
            Some(id) => cursor = id.into(),
            None => break,
        }
    }
    branch.reverse();
    let Some(index) = branch
        .iter()
        .rposition(|v| v["type"] == "compaction" && !v["systemMessage"].is_null())
    else {
        return Ok(false);
    };
    let compaction = &branch[index];
    let kept = compaction["firstKeptEntryId"]
        .as_str()
        .context("compaction boundary missing")?;
    let Some(start) = branch.iter().position(|v| v["id"] == kept) else {
        return Ok(false);
    };
    ensure!(start <= index, "invalid compaction boundary");
    let mut keep = vec![header];
    // Model and thinking are extracted from the full branch, even before compaction.
    for kind in ["model_change", "thinking_level_change"] {
        if let Some(v) = branch[..start].iter().rev().find(|v| v["type"] == kind) {
            keep.push(v.clone());
        }
    }
    // A context edit in the kept range whose target lies before it changes nothing the model sees: Pi applies edits
    // only to entries in the compacted context (`buildSessionProjection`), and the target is summarised. Such edits
    // are dropped with the summarised history (night review 21 Oct: the start's image repair and the running
    // agent's repair both write edits, and refusing to rotate over them looped maintenance compactions and reloads).
    let ids: HashSet<String> = branch[start..].iter().filter_map(|v| v["id"].as_str().map(str::to_owned)).collect();
    let dangling = |v: &Value| v["type"] == "context_edit" && v["targetId"].as_str().is_none_or(|id| !ids.contains(id));
    let mut tail: Vec<Value> = branch[start..].iter().filter(|v| !dangling(v)).cloned().collect();
    // The compaction's first kept entry must stay in the file, or Pi would keep nothing before the compaction: when
    // that entry is a dropped edit, the next kept entry takes its place (the same model view).
    if dangling(&branch[start]) {
        let next = tail.first().map(|v| v["id"].clone()).context("nothing kept")?;
        for v in tail.iter_mut().filter(|v| v["type"] == "compaction" && v["firstKeptEntryId"] == kept) {
            v["firstKeptEntryId"] = next.clone();
        }
    }
    keep.append(&mut tail);
    let mut parent = Value::Null;
    for v in keep.iter_mut().skip(1) {
        v["parentId"] = parent;
        parent = v["id"].clone();
    }
    let mut new = Vec::new();
    for v in &keep {
        serde_json::to_writer(&mut new, v)?;
        new.push(b'\n');
    }
    if new.len() >= bytes.len() {
        return Ok(false);
    }
    archive(&bytes)?;
    crate::runtime::atomic_write(path, &new)?;
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn file(path: &Path) {
        let rows = [
            json!({"type":"session","version":3,"id":"same"}),
            json!({"type":"model_change","id":"m","parentId":null,"modelId":"qwen3.8-27b","provider":"splash"}),
            json!({"type":"message","id":"old","parentId":"m","message":{"role":"toolResult","content":"original"}}),
            json!({"type":"message","id":"recent","parentId":"old","message":{"role":"user","content":"keep"}}),
            json!({"type":"compaction","id":"c","parentId":"recent","firstKeptEntryId":"recent","summary":"continuity","systemMessage":{"role":"system","content":"stable"}}),
            json!({"type":"message","id":"reply","parentId":"c","message":{"role":"assistant","content":[]}}),
        ];
        std::fs::write(
            path,
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
    }
    /// The forward reading `last_request` replaced (reads every line), kept as the reference.
    fn last_request_forward(path: &Path) -> Option<(u64, String, String)> {
        let mut last = None;
        for line in BufReader::new(std::fs::File::open(path).unwrap()).lines() {
            let line = line.unwrap();
            if line.contains("\"type\":\"compaction\"") && serde_json::from_str::<Value>(&line).is_ok_and(|v| v["type"] == "compaction") {
                last = None;
                continue;
            }
            if !line.contains("\"role\":\"assistant\"") || !line.contains("\"usage\"") { continue; }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let m = &v["message"];
            if v["type"] != "message" || m["role"] != "assistant" { continue; }
            let prompt = ["input", "cacheRead", "cacheWrite"].iter().map(|k| m["usage"][*k].as_u64().unwrap_or(0)).sum::<u64>();
            if let (true, Some(at)) = (prompt > 0, m["timestamp"].as_u64()) {
                last = Some((at, m["provider"].as_str().unwrap_or_default().to_owned(), m["model"].as_str().unwrap_or_default().to_owned()));
            }
        }
        last
    }
    /// Night review (session tail): the tail reader gives the forward reader's answer on random sessions, for block
    /// sizes down to a few bytes: lines longer than a block, multi-byte text across block edges, compactions anywhere,
    /// assistant messages without usage or timestamp, an escaped compaction inside text, a half-written last line.
    #[test]
    fn last_request_from_the_tail_matches_reading_every_line() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let mut rng = 0x9e3779b97f4a7c15u64;
        let mut next = |n: u64| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (rng >> 33) % n };
        for case in 0..300 {
            let mut text = String::new();
            for i in 0..next(14) {
                let row = match next(7) {
                    0 => json!({"type":"compaction","id":format!("c{i}"),"summary":"kept"}),
                    1 => json!({"type":"message","id":format!("a{i}"),"message":{"role":"assistant","provider":"anthropic","model":format!("m{i}"),"timestamp":1000 + i,
                        "usage":{"input":next(3),"cacheRead":0,"cacheWrite":0},"content":[{"type":"text","text":"żółw ".repeat(next(400) as usize)}]}}),
                    2 => json!({"type":"message","id":format!("n{i}"),"message":{"role":"assistant","usage":{"input":5},"content":[]}}),
                    3 => json!({"type":"message","id":format!("u{i}"),"message":{"role":"user","content":"\"type\":\"compaction\" said aloud"}}),
                    4 => json!({"type":"message","id":format!("t{i}"),"message":{"role":"toolResult","content":"x".repeat(next(3000) as usize)}}),
                    5 => json!({"type":"custom","customType":"heimdall_receipt","data":{"id":format!("d{i}"),"completed":true}}),
                    _ => json!({"type":"message","id":format!("s{i}"),"message":{"role":"assistant","provider":"openai","model":"x","timestamp":2000 + i,"usage":{"cacheRead":7},"content":[]}}),
                };
                text.push_str(&row.to_string());
                text.push('\n');
            }
            if next(3) == 0 { text.push_str("{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"usage\""); }
            std::fs::write(&p, &text).unwrap();
            let expected = last_request_forward(&p);
            for block in [1, 7, 64, 1000, TAIL_BLOCK] {
                assert_eq!(last_request_from_tail(&p, block).unwrap().0, expected, "case {case}, block {block}");
            }
        }
        assert_eq!(last_request(&d.path().join("missing")).unwrap(), None);
    }
    /// The 20 Oct session was 31 MB: the newest assistant message decides after reading one block from the end.
    #[test]
    fn last_request_reads_only_the_tail_of_a_long_session() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let filler = format!("{}\n", json!({"type":"message","message":{"role":"toolResult","content":"x".repeat(4000)}}));
        let mut text = filler.repeat(5_000);
        text.push_str(&format!("{}\n", json!({"type":"message","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-5-5","timestamp":42,"usage":{"input":3,"cacheRead":90000},"content":[]}})));
        text.push_str(&filler);
        std::fs::write(&p, &text).unwrap();
        assert!(text.len() > 20_000_000);
        let (found, read) = last_request_from_tail(&p, TAIL_BLOCK).unwrap();
        assert_eq!(found, Some((42, "anthropic".into(), "claude-opus-5-5".into())));
        assert!(read <= TAIL_BLOCK as u64, "read {read} bytes");
    }
    /// F6: the size a switched model inherits is the newest request's, as Pi counts it; nothing after a compaction.
    #[test]
    fn history_tokens_is_the_newest_requests_size_until_a_compaction() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let line = |v: Value| format!("{v}\n");
        let mut text = line(json!({"type":"message","message":{"role":"assistant","usage":{"input":10,"output":5,"cacheRead":200000,"cacheWrite":3000,"totalTokens":203015},"content":[]}}));
        text.push_str(&line(json!({"type":"message","message":{"role":"user","content":"x"}})));
        // A request aborted before any usage says nothing; the one before it decides.
        text.push_str(&line(json!({"type":"message","message":{"role":"assistant","stopReason":"aborted","usage":{"input":0,"output":0},"content":[]}})));
        std::fs::write(&p, &text).unwrap();
        assert_eq!(history_tokens(&p).unwrap(), Some(203_015));
        std::fs::write(&p, format!("{text}{}", line(json!({"type":"message","message":{"role":"assistant","usage":{"input":7,"output":3,"cacheRead":90},"content":[]}})))).unwrap();
        assert_eq!(history_tokens(&p).unwrap(), Some(100), "without totalTokens: the sum");
        std::fs::write(&p, format!("{text}{}", line(json!({"type":"compaction","summary":"s"})))).unwrap();
        assert_eq!(history_tokens(&p).unwrap(), None);
        assert_eq!(history_tokens(&d.path().join("missing")).unwrap(), None);
    }
    #[test]
    fn rotation_archives_original_keeps_ids_summary_tools_and_recent_history() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        file(&p);
        let before = std::fs::read(&p).unwrap();
        let mut saved = vec![];
        assert!(rotate(&p, 0, |b| {
            saved = b.to_vec();
            Ok(())
        })
        .unwrap());
        assert_eq!(saved, before);
        let text = std::fs::read_to_string(p).unwrap();
        assert!(
            text.contains("continuity")
                && text.contains("stable")
                && text.contains("recent")
                && text.contains("reply")
                && text.contains("same")
        );
        assert!(!text.contains("original"));
    }
    #[test]
    fn failed_archive_does_not_touch_session() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        file(&p);
        let before = std::fs::read(&p).unwrap();
        assert!(rotate(&p, 0, |_| anyhow::bail!("disk full")).is_err());
        assert_eq!(std::fs::read(p).unwrap(), before);
    }
    /// Night review 21 Oct (item 4): edits in the kept range whose targets were summarised (the start's image repair,
    /// the running agent's repair) no longer block rotation; they change nothing the model sees and are dropped with
    /// the summarised history, also when one of them is the compaction's first kept entry. Edits of kept entries stay.
    #[test]
    fn edits_of_summarised_entries_go_with_them_and_never_block_rotation() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let rows = [
            json!({"type":"session","version":3,"id":"same"}),
            json!({"type":"message","id":"old","parentId":null,"message":{"role":"toolResult","content":"summarised"}}),
            json!({"type":"context_edit","id":"e-old","parentId":"old","targetId":"old","replacement":null}),
            json!({"type":"message","id":"recent","parentId":"e-old","message":{"role":"user","content":"keep"}}),
            json!({"type":"context_edit","id":"e-old2","parentId":"recent","targetId":"old","replacement":{"content":"note"}}),
            json!({"type":"context_edit","id":"e-recent","parentId":"e-old2","targetId":"recent","replacement":{"content":"kept edit"}}),
            json!({"type":"compaction","id":"c","parentId":"e-recent","firstKeptEntryId":"e-old","summary":"continuity","systemMessage":{"role":"system","content":"stable"}}),
            json!({"type":"message","id":"reply","parentId":"c","message":{"role":"assistant","content":[]}}),
        ];
        std::fs::write(&p, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        assert!(rotate(&p, 0, |_| Ok(())).unwrap(), "rotates");
        let lines: Vec<Value> = std::fs::read_to_string(&p).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let ids: Vec<&str> = lines.iter().skip(1).filter_map(|v| v["id"].as_str()).collect();
        assert_eq!(ids, ["recent", "e-recent", "c", "reply"], "{lines:?}");
        let compaction = lines.iter().find(|v| v["type"] == "compaction").unwrap();
        assert_eq!(compaction["firstKeptEntryId"], "recent", "the first kept entry moved past the dropped edit");
        assert_eq!(lines[2]["parentId"], "recent");
        // Rotating the result again changes nothing it should not.
        assert!(!rotate(&p, 0, |_| Ok(())).unwrap(), "already bounded");
    }
    /// A line Pi skipped (a write cut off in an older session) never blocks rotation: it goes like Pi ignores it.
    #[test]
    fn an_unparseable_line_is_dropped_by_rotation_like_pi_skips_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        file(&p);
        let text = std::fs::read_to_string(&p).unwrap();
        let text: String = text.lines().map(|l| if l.contains("\"id\":\"recent\"") { format!("{{\"type\":\"message\",\"id\":\"x\",\"mess\n{l}\n") } else { format!("{l}\n") }).collect();
        assert!(text.contains("\"mess\n"));
        std::fs::write(&p, text).unwrap();
        assert!(rotate(&p, 0, |_| Ok(())).unwrap());
        assert!(std::fs::read_to_string(&p).unwrap().lines().all(|l| serde_json::from_str::<Value>(l).is_ok()));
    }
    /// Which deliveries Pi read: its user messages with the delivery marker (in Pi's key order and the sorted order a
    /// rotated file has), plus any receipt or effects record; another role quoting a marker does not count.
    #[test]
    fn evidence_names_every_delivery_pi_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let rows = [
            r#"{"type":"message","id":"1","parentId":null,"timestamp":"t","message":{"role":"user","content":[{"type":"text","text":"[Heimdall delivery: 1b089955-f758-4d2e]\n[Now: x]\nhi"}]}}"#.to_owned(),
            r#"{"id":"2","message":{"content":[{"text":"[Heimdall delivery: adab3e06_x]\nTranscript:","type":"text"}],"role":"user"},"parentId":"1","type":"message"}"#.to_owned(),
            r#"{"type":"message","id":"3","message":{"role":"user","content":"[Heimdall delivery: plain]\nhello"}}"#.to_owned(),
            r#"{"type":"message","id":"4","message":{"role":"toolResult","content":[{"type":"text","text":"[Heimdall delivery: quoted]\n"}]}}"#.to_owned(),
            json!({"type":"custom","id":"5","customType":"heimdall_receipt","data":{"id":"r1","completed":false}}).to_string(),
            json!({"type":"custom","id":"6","customType":"heimdall_effects","data":{"ids":["e1"],"toolCallId":"t"}}).to_string(),
        ];
        let mut bytes = rows.join("\n").into_bytes();
        bytes.extend(b"\n{\"type\":\"message\",\"cut\":\"\xe2\x82");
        std::fs::write(&p, bytes).unwrap();
        let e = evidence(&p).unwrap();
        let mut consumed: Vec<&str> = e.consumed.iter().map(String::as_str).collect();
        consumed.sort();
        assert_eq!(consumed, ["1b089955-f758-4d2e", "adab3e06_x", "e1", "plain", "r1"]);
        assert_eq!(e.receipts.len(), 1);
    }
    #[test]
    fn no_checkpoint_no_rotation() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        std::fs::write(&p, "{\"type\":\"session\",\"version\":3}\n").unwrap();
        assert!(!rotate(&p, 0, |_| panic!()).unwrap());
    }
}
