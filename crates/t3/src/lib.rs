//! Outside-client T3 control. No borrowed provider credentials or T3 database access.
pub mod client;
/// Finding T3 Code on this Mac and installing it (the setup wizard).
pub mod install;
pub mod observer;
pub mod orchestrator;
pub mod text;
pub mod threads;
pub mod workers;
pub use client::{Client, CredentialStore, Keychain};
pub use workers::Workers;


pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Whether a thread whose `pendingRequestCount` is above its open questions really waits on an approval. Heimdall's
/// Claude permission hook answers its own threads' prompts (`heimdall_emergency::approvals`); T3 still shows its card
/// for the same prompt and never closes it, so the count stays up after the command ran (live check 28 Oct). A card
/// whose tool call has run is answered; any other, or a card not found in the activity, still counts.
/// `row` is the thread as T3 listed or read it: the answer is kept per thread until its `updatedAt` or
/// `pendingRequestCount` changes, so a card left open costs one activity read, not one per pass.
pub(crate) async fn waits_on_approval(client: &Client, thread: &str, row: &serde_json::Value) -> anyhow::Result<bool> {
    static KNOWN: std::sync::Mutex<Option<std::collections::HashMap<String, (String, bool)>>> = std::sync::Mutex::new(None);
    let key = format!("{}|{}", row["updatedAt"], row["pendingRequestCount"]);
    if let Some((k, waits)) = KNOWN.lock().unwrap().get_or_insert_with(Default::default).get(thread) {
        if *k == key { return Ok(*waits); }
    }
    let mut items = Vec::new();
    let mut after: Option<u64> = None;
    for _ in 0..20 {
        let mut args = serde_json::json!({"threadId":thread,"view":"activity","limit":100,"maxCharsPerItem":1,"runLimit":1});
        if let Some(a) = after { args["afterPosition"] = a.into(); }
        let page = client.call("t3_thread_read", args).await?;
        items.extend(page["items"].as_array().cloned().unwrap_or_default());
        after = page["nextPosition"].as_u64();
        if page["hasMore"] != true || after.is_none() { break; }
    }
    let waits = text::open_approvals(&items) != Some(0);
    let mut known = KNOWN.lock().unwrap();
    let known = known.get_or_insert_with(Default::default);
    if known.len() > 500 { known.clear(); }
    known.insert(thread.to_owned(), (key, waits));
    Ok(waits)
}

/// T3's one pending-request tool since 0.1.6 (bug note 2026-11-02T14-56-51-3): `t3_pending_request` with `action`
/// list, read or respond took the place of one tool per operation (`_list`, `_read`, `_respond`). It lists questions only:
/// approvals are not listed and it cannot answer one (its own description says so).
pub const PENDING_REQUEST: &str = "t3_pending_request";
/// The ids of a thread's pending questions.
pub async fn question_ids(client: &Client, thread: &str) -> anyhow::Result<Vec<String>> {
    let r = client.call(PENDING_REQUEST, serde_json::json!({"action":"list","threadId":thread})).await?;
    Ok(r["requestIds"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect())
}
/// One pending question: `{requestId, questions: [...]}`.
pub async fn read_question(client: &Client, thread: &str, request: &str) -> anyhow::Result<serde_json::Value> {
    client.call(PENDING_REQUEST, serde_json::json!({"action":"read","threadId":thread,"requestId":request})).await
}
/// Answers one pending question (`answers`: question id to label, labels or text).
pub async fn answer_question(client: &Client, thread: &str, request: &str, answers: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    client.call(PENDING_REQUEST, serde_json::json!({"action":"respond","threadId":thread,"requestId":request,"answers":answers})).await
}

pub(crate) fn body_digest(text: &str) -> String {
    use sha2::Digest;
    format!("sha256:{:x}", sha2::Sha256::digest(text.as_bytes()))
}
