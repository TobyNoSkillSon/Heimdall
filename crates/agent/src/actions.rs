//! Durable action receipts and a delivery outbox. Host callbacks are short enqueue operations.
use crate::delivery::DeliveryPermit;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub text: String,
    pub kind: String,
    /// `island_notice.detail` as validated by the contract: text, table, svg or image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}
/// One option of an `ask` question: what he picks (its label, which comes back as his answer) and an optional short
/// description under it on the card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What the user's click on this option approves that Heimdall's deny list refuses otherwise (`AskGrant`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<AskGrant>,
}
/// An option's grant (bug notes 2026-11-03T23-04-19, 2026-11-03T18-04-11): exact paths to delete, login items to
/// remove and one new keychain file to make (2026-11-04T23-01-22), each named on the card. Only his click on the option writes it for the hook
/// (`heimdall_emergency::grants`), for 30 minutes and once per path or item; his voice never does.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskGrant {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub login_items: Vec<String>,
    /// One new keychain file to make and fill for signing (bug note 2026-11-04T23-01-22,
    /// `heimdall_emergency::grants::grant_keychain`): `security` create, unlock, import and partition-list on it only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keychain: Option<String>,
}
/// One question of an `ask` card (24 Oct: "one adaptive card"). The legacy `question` + `options` call is one item with
/// id `q1`, no header, single select and Other on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskItem {
    /// `q1` to `q4` by position in the call: the key his answer comes back under.
    pub id: String,
    /// The step chip's word (up to 12 characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default = "allow_other")]
    pub allow_other: bool,
    /// Two to four options, numbered 1 to 4 on the card.
    pub options: Vec<AskOption>,
}
fn allow_other() -> bool {
    true
}
impl AskItem {
    /// The one question of a legacy call: `q1`, single select, Other on, no descriptions.
    pub fn one(question: &str, labels: &[&str]) -> AskItem {
        AskItem {
            id: "q1".into(),
            header: None,
            question: question.into(),
            multi_select: false,
            allow_other: true,
            options: labels.iter().map(|l| AskOption { label: (*l).into(), description: None, grant: None }).collect(),
        }
    }
}
/// The questions of an `ask` call, checked (24 Oct): the legacy form (`question` with `options`, one question) or
/// `questions` (one to four), never both. Ids are `q1` to `q4` by position; labels differ within a question.
pub fn parse_ask(args: &serde_json::Value) -> Result<Vec<AskItem>> {
    use crate::mcp_contract::{OPTION_CHARS, QUESTIONS_MAX, QUESTION_CHARS};
    let has_questions = args.get("questions").is_some();
    let has_legacy = args.get("question").is_some() || args.get("options").is_some();
    ensure!(!(has_questions && has_legacy), "ask: send questions, or question with options, not both");
    if has_questions {
        let rows = args["questions"].as_array().context("questions: a list of one to four questions")?;
        ensure!((1..=QUESTIONS_MAX).contains(&rows.len()), "questions: one to four questions per card");
        return rows.iter().enumerate().map(|(i, row)| ask_item(row, i + 1)).collect();
    }
    let question = args["question"].as_str().context("ask: give questions, or question with options")?;
    ensure!(!question.trim().is_empty() && question.chars().count() <= QUESTION_CHARS, "question: 1 to 120 characters");
    let rows = args["options"].as_array().context("ask: give questions, or question with options")?;
    let labels = rows
        .iter()
        .map(|o| {
            // A grant rides only on the questions form (bug note 2026-11-04T18-59-48: the relay's short cards named the
            // path in the question and granted nothing); say so instead of "a string".
            ensure!(o.get("grant").is_none(), "ASK_BAD_GRANT: options: a grant rides on an option of the questions form: send questions: [{{question, options: [{{label, grant}}, ...]}}]");
            let label = o.as_str().context("options: each option is a string")?.trim();
            ensure!(!label.is_empty() && label.chars().count() <= OPTION_CHARS, "options: each option is 1 to 24 characters");
            Ok(label.to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let item = AskItem::one(question, &refs);
    check_options(&item, "options")?;
    Ok(vec![item])
}
/// One item of the `questions` form (`n` is its one-based place in the call).
fn ask_item(row: &serde_json::Value, n: usize) -> Result<AskItem> {
    use crate::mcp_contract::{DESCRIPTION_CHARS, HEADER_CHARS, LABEL_CHARS, QUESTION_CHARS};
    let at = |name: &str| format!("questions[{n}].{name}");
    let question = row["question"].as_str().with_context(|| format!("{}: text", at("question")))?.trim();
    ensure!(!question.is_empty() && question.chars().count() <= QUESTION_CHARS, "{}: 1 to 120 characters", at("question"));
    let header = match row.get("header") {
        None => None,
        Some(h) => {
            let h = h.as_str().with_context(|| format!("{}: text", at("header")))?.trim();
            ensure!(!h.is_empty() && h.chars().count() <= HEADER_CHARS, "{}: 1 to 12 characters", at("header"));
            Some(h.to_owned())
        }
    };
    let rows = row["options"].as_array().with_context(|| format!("{}: two to four options", at("options")))?;
    let options = rows
        .iter()
        .map(|o| {
            let (label, description) = match o {
                serde_json::Value::String(label) => (label.as_str(), None),
                serde_json::Value::Object(_) => (o["label"].as_str().unwrap_or_default(), o["description"].as_str()),
                _ => bail!("{}: an option is a label, or {{label, description}}", at("options")),
            };
            let label = label.trim();
            ensure!(!label.is_empty() && label.chars().count() <= LABEL_CHARS, "{}: label 1 to 40 characters", at("options"));
            let description = description.map(str::trim).filter(|d| !d.is_empty());
            if let Some(d) = description {
                ensure!(d.chars().count() <= DESCRIPTION_CHARS, "{}: description up to 80 characters", at("options"));
            }
            let grant = match o.get("grant") {
                None | Some(serde_json::Value::Null) => None,
                Some(g) => Some(ask_grant(g, &at("options")).map_err(|e| anyhow::anyhow!("ASK_BAD_GRANT: {e}"))?),
            };
            Ok(AskOption { label: label.to_owned(), description: description.map(str::to_owned), grant })
        })
        .collect::<Result<Vec<_>>>()?;
    let item = AskItem {
        id: format!("q{n}"),
        header,
        question: question.to_owned(),
        multi_select: row["multiSelect"].as_bool().unwrap_or(false),
        allow_other: row["allowOther"].as_bool().unwrap_or(true),
        options,
    };
    check_options(&item, &at("options"))?;
    let lines: usize = item.options.iter().flat_map(|o| o.grant.iter()).flat_map(|g| g.delete.iter().chain(&g.login_items).chain(&g.keychain)).map(|s| s.chars().count().div_ceil(GRANT_LINE_CHARS)).sum();
    ensure!(lines <= GRANT_LINES, "ASK_BAD_GRANT: {}: the grants of one question fill at most {GRANT_LINES} lines of {GRANT_LINE_CHARS} characters on the card (these need {lines}); split them over several asks", at("options"));
    Ok(item)
}
/// How much a question's grants may fill on the island, so every one is drawn above the window's bottom (review 4 Nov
/// evening; the island also refuses a click on a grant drawn past it): lines of SF Mono 12, at fewer characters a line
/// than the card's well holds.
const GRANT_LINES: usize = 8;
const GRANT_LINE_CHARS: usize = 56;
/// An option's grant, checked as the hook will keep it (`heimdall_emergency::grants`). The island draws every path and
/// login item in full under the option (`ui` ask card, 4 Nov evening), so the question and description need not repeat
/// them.
fn ask_grant(g: &serde_json::Value, field: &str) -> std::result::Result<AskGrant, String> {
    use heimdall_emergency::grants::{grant_keychain, grant_login_item, grant_path, MAX_LOGIN_ITEMS, MAX_PATHS};
    let list = |key: &str| -> std::result::Result<Vec<String>, String> {
        match g.get(key) {
            None => Ok(vec![]),
            Some(v) => v.as_array().ok_or(format!("{field}.grant.{key}: a list"))?.iter()
                .map(|s| s.as_str().map(|s| s.trim().to_owned()).ok_or(format!("{field}.grant.{key}: text"))).collect(),
        }
    };
    ensure_str(g.is_object() && g.as_object().unwrap().keys().all(|k| matches!(k.as_str(), "delete" | "loginItems" | "keychain")), format!("{field}.grant: {{delete: [exact paths]}}, {{loginItems: [names]}} and/or {{keychain: one new file path}}"))?;
    let (delete, login_items) = (list("delete")?, list("loginItems")?);
    // Exactly one path, as text (never a list): the hook covers one new file per grant.
    let keychain = match g.get("keychain") {
        None => None,
        Some(k) => Some(k.as_str().ok_or(format!("{field}.grant.keychain: one new file path, as text"))?.trim().to_owned()),
    };
    ensure_str(!delete.is_empty() || !login_items.is_empty() || keychain.is_some(), format!("{field}.grant: name a path to delete, a login item or a new keychain"))?;
    ensure_str(delete.len() <= MAX_PATHS && login_items.len() <= MAX_LOGIN_ITEMS, format!("{field}.grant: at most {MAX_PATHS} paths and {MAX_LOGIN_ITEMS} login items"))?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| "/var/empty".into());
    let vault = heimdall_home::home_dir();
    for p in &delete {
        grant_path(p, &home, &vault)?;
    }
    let login_items = login_items.iter().map(|n| grant_login_item(n)).collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(k) = &keychain {
        // The hook's own rules (one new `<name>.keychain-db` right inside ~/Library/Keychains or a Vault project,
        // never macOS's own), and new at the card as the writer checks again at the click.
        let path = grant_keychain(k, &home, &vault)?;
        ensure_str(std::fs::symlink_metadata(&path).is_err(), format!("{field}.grant.keychain: {} already exists; a grant makes only a new keychain", path.display()))?;
    }
    let unique = |v: &[String]| v.iter().enumerate().all(|(i, a)| !v[..i].contains(a));
    ensure_str(unique(&delete) && unique(&login_items), format!("{field}.grant: name each path and login item once"))?;
    Ok(AskGrant { delete, login_items, keychain })
}
fn ensure_str(ok: bool, why: String) -> std::result::Result<(), String> {
    if ok { Ok(()) } else { Err(why) }
}
/// The grants the user's picks on a card make: each picked option that carries one and that his own pointer clicked
/// (`by_hand`, the island's `click_source`: in both lists), with its question id and label. Typed Other, a key, his
/// voice, an accessibility press or another process's event never does.
pub fn card_grants(items: &[AskItem], answers: &[AskAnswer], by_hand: &[AskAnswer]) -> Vec<(String, String, AskGrant)> {
    let mut out = vec![];
    for a in answers {
        let Some(item) = items.iter().find(|i| i.id == a.question_id) else { continue };
        let clicked = |label: &String| by_hand.iter().any(|h| h.question_id == a.question_id && h.labels.contains(label));
        for label in a.labels.iter().filter(|l| clicked(l)) {
            if let Some(grant) = item.options.iter().find(|o| &o.label == label).and_then(|o| o.grant.clone()) {
                out.push((item.id.clone(), label.clone(), grant));
            }
        }
    }
    out
}
/// Two to four options whose labels differ (case-insensitively).
fn check_options(item: &AskItem, field: &str) -> Result<()> {
    ensure!((2..=4).contains(&item.options.len()), "{field}: two to four options");
    for (i, o) in item.options.iter().enumerate() {
        let lower = o.label.to_lowercase();
        ensure!(!item.options[..i].iter().any(|p| p.label.to_lowercase() == lower), "{field}: labels must differ");
    }
    Ok(())
}

/// An `ask` card: one to four questions, answered together.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub questions: Vec<AskItem>,
    pub created_ms: u64,
    pub expires_ms: u64,
}
/// The user's answer to one question of a card: the labels he picked (one for single select, any for multi-select) and
/// his own words under Other.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub question_id: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}
/// How long an unanswered question stays answerable.
pub const QUESTION_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;
/// What `ask_answers` stores for a question the user left without a pick when he sent the card.
pub const UNANSWERED: &str = "(unanswered)";
/// UI owner presents the question's options and calls Actions::answer with the receipt ID and the clicked label.
/// Harness owner enqueues notices as follow-up prompts, deduplicating their durable ID.
pub trait ActionSink: Send + Sync {
    fn notice(&self, notice: &Notice, permit: &DeliveryPermit) -> Result<()>;
    fn question(&self, question: &Question, permit: &DeliveryPermit) -> Result<()>;
    /// The agent closed an open question after the user answered by voice: the card shows `answer` and closes.
    fn question_closed(&self, id: &str, answer: &str, permit: &DeliveryPermit) -> Result<()>;
    fn deliver(&self, id: &str, text: &str, permit: &DeliveryPermit) -> Result<()>;
    /// Must wait for Pi's tool-result checkpoint/turn settlement before restarting.
    fn reload(&self, receipt: &str, permit: &DeliveryPermit) -> Result<()>;
}
pub struct Actions {
    db: Mutex<Connection>,
    sink: Mutex<Option<Arc<dyn ActionSink>>>,
    archive: Mutex<Option<crate::PayloadArchive>>,
    /// The largest message the running agent takes, in bytes (`Harness::max_text_bytes`); the host sets it at each
    /// agent start.
    message_limit: std::sync::atomic::AtomicUsize,
    /// Confirmation questions only a click answers (`require_click`), with when they stop counting.
    click_only: Mutex<std::collections::HashMap<String, u64>>,
}
/// What became of the user's click on a question card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clicked {
    /// The agent is resumed with it.
    Delivered,
    /// A second click on a question already clicked: nothing more.
    AlreadyAnswered,
    /// His voice answered first and closed the card: the click is dropped.
    ClosedByVoice,
}
/// What became of a close (`Actions::close`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Closed,
    AlreadyClosed,
    AnsweredByClick,
    NotOpen,
}
/// The agent's message limit before the host reports the live one: the smallest context setting's (128K tokens,
/// half of it in bytes; bridge/startup.rs).
pub const DEFAULT_MESSAGE_LIMIT: usize = 64 * 1024;
/// A notice too long for the agent, when its producer gave no short form: its first line and a pointer.
pub fn too_long(text: &str) -> String {
    let first = text.lines().next().unwrap_or("");
    let mut end = first.len().min(400);
    while !first.is_char_boundary(end) { end -= 1; }
    format!("{}\nThe rest ({} KB) is too long for one message; read task replies with task_status and thread replies with t3_thread_read.", &first[..end], text.len().div_ceil(1024))
}
impl Actions {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS monitor_failures(id TEXT PRIMARY KEY);
          CREATE TABLE IF NOT EXISTS monitor(id INTEGER PRIMARY KEY,unavailable INTEGER NOT NULL,episode INTEGER NOT NULL);
          INSERT OR IGNORE INTO monitor VALUES(1,0,0);
          CREATE TABLE IF NOT EXISTS retired_payloads(kind TEXT NOT NULL,id TEXT NOT NULL,reference TEXT NOT NULL,PRIMARY KEY(kind,id));
          CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY, operation TEXT UNIQUE NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL, state TEXT NOT NULL, expires_ms INTEGER);
          CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY,text TEXT NOT NULL,delivered INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS ask_answers(id TEXT PRIMARY KEY,question TEXT NOT NULL,answer TEXT NOT NULL);")?;
        // Clicked answers are marked: only a click approves a guarded T3 call. Rows from before have no mark (0).
        if !db.prepare("PRAGMA table_info(ask_answers)")?.query_map([], |r| r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|n| n == "clicked") {
            db.execute_batch("ALTER TABLE ask_answers ADD COLUMN clicked INTEGER NOT NULL DEFAULT 0")?;
        }
        let outbox_columns = db.prepare("PRAGMA table_info(outbox)")?.query_map([], |r| r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if !outbox_columns.iter().any(|n| n == "epoch") {
            db.execute_batch("ALTER TABLE outbox ADD COLUMN epoch INTEGER NOT NULL DEFAULT -1")?;
        }
        // The producer's short form of a notice, delivered instead when the agent rejects the whole one as too long.
        if !outbox_columns.iter().any(|n| n == "short") {
            db.execute_batch("ALTER TABLE outbox ADD COLUMN short TEXT")?;
        }
        Ok(Self {
            db: Mutex::new(db),
            sink: Mutex::new(None),
            archive: Mutex::new(None),
            message_limit: std::sync::atomic::AtomicUsize::new(DEFAULT_MESSAGE_LIMIT),
            click_only: Mutex::new(Default::default()),
        })
    }
    /// The running agent's message limit in bytes (`Harness::max_text_bytes`), set at each agent start.
    pub fn set_message_limit(&self, bytes: usize) {
        self.message_limit.store(bytes.max(4096), std::sync::atomic::Ordering::Release);
    }
    pub fn message_limit(&self) -> usize {
        self.message_limit.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Bytes one notice may carry: the agent's message limit less an eighth, room for the notice header the harness
    /// adds. (56 KB at the smallest context setting.)
    pub fn notice_budget(&self) -> usize {
        let limit = self.message_limit();
        limit - limit / 8
    }
    pub fn monitor_failure(&self,id:&str,failed:bool)->Result<()> {
        let db=self.db.lock().unwrap();if failed{db.execute("INSERT OR IGNORE INTO monitor_failures VALUES(?)",[id])?;}else{db.execute("DELETE FROM monitor_failures WHERE id=?",[id])?;}Ok(())
    }
    /// Used by the T3 worker watcher (`crates/t3`).
    pub fn reconcile_monitor_failures(&self, current: &std::collections::HashSet<String>) -> Result<()> {
        let db=self.db.lock().unwrap();
        let keys=db.prepare("SELECT id FROM monitor_failures")?.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for key in keys {
            if key!="control-plane" && key!="wait-supervisor" && !current.contains(&key) {
                db.execute("DELETE FROM monitor_failures WHERE id=?",[key])?;
            }
        }
        Ok(())
    }
    pub fn monitor_failed(&self)->Result<bool>{Ok(self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM monitor_failures)",[],|r|r.get(0))?)}
    pub fn monitor_state(&self)->Result<(bool,i64)> {
        Ok(self.db.lock().unwrap().query_row("SELECT unavailable,episode FROM monitor WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?)
    }
    pub fn set_monitor(&self,unavailable:bool,episode:i64)->Result<()> {
        self.db.lock().unwrap().execute("UPDATE monitor SET unavailable=?,episode=? WHERE id=1",params![unavailable,episode])?;Ok(())
    }
    pub fn attach_archive(&self, archive:crate::PayloadArchive) { *self.archive.lock().unwrap()=Some(archive); }
    pub fn archive_payload(&self,body:&serde_json::Value)->Result<Option<String>> {
        self.archive.lock().unwrap().as_ref().map(|a|a(body)).transpose()
    }
    pub fn retire(&self)->Result<()> {
        let db=self.db.lock().unwrap();
        let rows=db.prepare("SELECT id,body,state FROM receipts WHERE state IN ('shown','completed','answered','declined','closed','cancelled','failed') AND NOT (kind IN ('ask','fastread') AND state='shown') AND NOT EXISTS(SELECT 1 FROM retired_payloads WHERE kind='receipt' AND retired_payloads.id=receipts.id) AND NOT EXISTS(SELECT 1 FROM outbox WHERE outbox.id=receipts.id AND delivered=0) LIMIT 100")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id,body,state) in rows {
            // Shown questions still need their body for the future click.
            let kind:String=db.query_row("SELECT kind FROM receipts WHERE id=?",[&id],|r|r.get(0))?;
            if matches!(kind.as_str(),"ask"|"fastread") && state=="shown" {continue;}
            if let Some(reference)=self.archive_payload(&serde_json::json!({"store":"receipt","id":id,"kind":kind,"body":body,"state":state}))?{let tx=db.unchecked_transaction()?;tx.execute("INSERT OR REPLACE INTO retired_payloads VALUES('receipt',?,?)",params![id,reference])?;tx.execute("UPDATE receipts SET body=? WHERE id=?",params![crate::body_digest(&body),id])?;tx.commit()?;}
        }
        let rows=db.prepare("SELECT id,text FROM outbox WHERE delivered!=0 AND NOT EXISTS(SELECT 1 FROM retired_payloads WHERE kind='outbox' AND retired_payloads.id=outbox.id) LIMIT 100")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id,text) in rows {if let Some(reference)=self.archive_payload(&serde_json::json!({"store":"outbox","id":id,"text":text}))?{let tx=db.unchecked_transaction()?;tx.execute("INSERT OR REPLACE INTO retired_payloads VALUES('outbox',?,?)",params![id,reference])?;tx.execute("UPDATE outbox SET text='' WHERE id=?",[&id])?;tx.commit()?;}}
        Ok(())
    }
    pub fn attach(&self, sink: Arc<dyn ActionSink>) {
        *self.sink.lock().unwrap() = Some(sink);
    }
    pub fn detach(&self) { self.sink.lock().unwrap().take(); }
    fn sink(&self) -> Result<Arc<dyn ActionSink>> {
        self.sink.lock().unwrap().clone().context(
            "UI_UNAVAILABLE: action channel is not attached; open the agent after integration",
        )
    }
    pub fn receipt(
        &self,
        operation: &str,
        kind: &str,
        body: &str,
        expires: Option<u64>,
        permit: &DeliveryPermit,
    ) -> Result<String> {
        permit.check()?;
        let db = self.db.lock().unwrap();
        if let Some((id, k, b)) = db
            .query_row(
                "SELECT id,kind,body FROM receipts WHERE operation=?",
                [operation],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            ensure!(
                k == kind && (b == body || b == crate::body_digest(body)),
                "REPLAY_CONFLICT: operation identity was reused for different content"
            );
            return Ok(id);
        }
        let id = format!("{kind}-{}", uuid::Uuid::new_v4());
        permit.with(|| {
            db.execute(
                "INSERT INTO receipts VALUES(?1,?2,?3,?4,'pending',?5)",
                params![id, operation, kind, body, expires.map(|n| n as i64)],
            )?;
            Ok(())
        })?;
        Ok(id)
    }
    pub fn notice(
        &self,
        op: &str,
        text: &str,
        kind: &str,
        update: Option<&str>,
        permit: &DeliveryPermit,
    ) -> Result<String> {
        self.notice_with(op, text, kind, update, None, permit)
    }
    /// `notice` with `island_notice.detail` (pull-down content).
    pub fn notice_with(
        &self,
        op: &str,
        text: &str,
        kind: &str,
        update: Option<&str>,
        detail: Option<&serde_json::Value>,
        permit: &DeliveryPermit,
    ) -> Result<String> {
        let sink = self.sink()?;
        // Without detail the body is the original triple, so earlier receipts still deduplicate.
        let body = match detail {
            None => serde_json::to_string(&(text, kind, update))?,
            Some(d) => serde_json::to_string(&(text, kind, update, d))?,
        };
        if let Some(id) = update {
            ensure!(
                self.db.lock().unwrap().query_row(
                    "SELECT EXISTS(SELECT 1 FROM receipts WHERE id=? AND kind='notice')",
                    [id],
                    |r| r.get::<_, bool>(0)
                )?,
                "NOTICE_NOT_FOUND: create a new notice"
            );
        }
        let id = self.receipt(op, "notice", &body, None, permit)?;
        if self.state(&id)? == "pending" {
            permit.with(|| {
                sink.notice(
                    &Notice {
                        id: update.unwrap_or(&id).into(),
                        text: text.into(),
                        kind: kind.into(),
                        detail: detail.cloned(),
                    },
                    permit,
                )
            })?;
            self.set_state(&id, "shown", permit)?;
        }
        Ok(update.unwrap_or(&id).to_owned())
    }
    /// An `ask` card: returns its request id. Replaying the same operation returns the same question.
    pub fn ask(&self, op: &str, questions: &[AskItem], permit: &DeliveryPermit) -> Result<String> {
        ensure!((1..=crate::mcp_contract::QUESTIONS_MAX).contains(&questions.len()), "ASK_BAD_QUESTIONS: one to four questions per card");
        let sink = self.sink()?;
        let now = crate::now_ms();
        let body = serde_json::json!({"questions": questions}).to_string();
        let id = self.receipt(op, "ask", &body, Some(now + QUESTION_LIFETIME_MS), permit)?;
        let state = self.state(&id)?;
        ensure!(matches!(state.as_str(), "pending" | "shown"), "ASK_CANCELLED: question is no longer active; ask again");
        if state == "pending" {
            let created = self.db.lock().unwrap().query_row(
                "SELECT expires_ms FROM receipts WHERE id=?",
                [&id],
                |r| r.get::<_, i64>(0).map(|n| n as u64),
            )? - QUESTION_LIFETIME_MS;
            permit.with(|| {
                sink.question(
                    &Question {
                        id: id.clone(),
                        questions: questions.to_vec(),
                        created_ms: created,
                        expires_ms: created + QUESTION_LIFETIME_MS,
                    },
                    permit,
                )
            })?;
            self.set_state(&id, "shown", permit)?;
        }
        Ok(id)
    }
    /// The questions of an `ask` receipt: `{"questions": [...]}`, or the legacy `{"question", "options"}` (one item, `q1`)
    /// that receipts written before the adaptive card still hold.
    fn items_of(body: &str) -> Result<Vec<AskItem>> {
        let v: serde_json::Value = serde_json::from_str(body).context("ASK_NOT_FOUND: question body is unreadable")?;
        if v.get("questions").is_some() {
            return serde_json::from_value(v["questions"].clone()).context("ASK_NOT_FOUND: question body is unreadable");
        }
        let labels: Vec<&str> = v["options"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
        Ok(vec![AskItem::one(v["question"].as_str().unwrap_or_default(), &labels)])
    }
    /// What the card's question column holds: the one question, or every question's text on its own line.
    fn question_text(items: &[AskItem]) -> String {
        items.iter().map(|i| i.question.as_str()).collect::<Vec<_>>().join("\n")
    }
    /// The user clicked `choice` on question `id`: the agent is resumed with it, once. A click on a question his voice
    /// already closed is dropped (talking "should cancel the question"; a later click is never a
    /// second, contradicting answer).
    pub fn answer(&self, id: &str, choice: &str, permit: &DeliveryPermit) -> Result<Clicked> {
        // The legacy one-click call: `choice` answers the card's first question.
        let body: String = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT body FROM receipts WHERE id=? AND kind='ask'", [id], |r| r.get(0))
            .optional()?
            .context("ASK_NOT_FOUND: unknown question")?;
        let question_id = Self::items_of(&body)?.first().map_or_else(|| "q1".to_owned(), |q| q.id.clone());
        self.answer_card(id, &[AskAnswer { question_id, labels: vec![choice.to_owned()], other: None }], permit)
    }
    /// The user answered card `id` (a click, or Send): `answers` holds at most one answer per question, and a question with
    /// none is delivered as left unanswered (Send works with some answered). The agent is resumed with them, once. Answers on a card his voice already closed are dropped, as clicks are.
    pub fn answer_card(&self, id: &str, answers: &[AskAnswer], permit: &DeliveryPermit) -> Result<Clicked> {
        permit.check()?;
        let db = self.db.lock().unwrap();
        let (state, expires, body) = db
            .query_row(
                "SELECT state,expires_ms,body FROM receipts WHERE id=? AND kind='ask'",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .context("ASK_NOT_FOUND: unknown question")?;
        ensure!(
            crate::now_ms() < expires,
            "ASK_EXPIRED: ask again; silence never approved"
        );
        match state.as_str() {
            "answered" => return Ok(Clicked::AlreadyAnswered),
            "closed" => return Ok(Clicked::ClosedByVoice),
            _ => {}
        }
        ensure!(state == "shown", "ASK_CANCELLED: question is no longer active; ask again");
        let items = Self::items_of(&body)?;
        let parts = Self::check_answers(&items, answers)?;
        let question = Self::question_text(&items);
        // The legacy line for one question and one label, byte for byte; otherwise one line per question, keyed by id.
        let (delivered, stored) = match (items.len(), answers) {
            (1, [only]) if only.labels.len() == 1 && only.other.is_none() => {
                let choice = &only.labels[0];
                (format!("Question {id}: User clicked {choice}. Question: {question}"), choice.clone())
            }
            _ => {
                // A question he left without a pick says so (bug note 2026-11-02T16-27-21: Send works with some
                // questions answered).
                let lines: Vec<String> = items
                    .iter()
                    .zip(&parts)
                    .map(|(item, (qid, part))| match part {
                        Some(part) => format!("Question {id}: User answered {qid} \"{}\": {part}", item.question),
                        None => format!("Question {id}: User left {qid} \"{}\" unanswered", item.question),
                    })
                    .collect();
                let shown = |part: &Option<String>| part.clone().unwrap_or_else(|| UNANSWERED.to_string());
                let stored = if parts.len() == 1 {
                    shown(&parts[0].1)
                } else {
                    parts.iter().map(|(qid, part)| format!("{qid}: {}", shown(part))).collect::<Vec<_>>().join("\n")
                };
                (lines.join("\n"), stored)
            }
        };
        permit.with(|| {
            db.execute_batch("BEGIN IMMEDIATE")?;
            let r = (|| -> Result<()> {
                db.execute("UPDATE receipts SET state='answered' WHERE id=?", params![id])?;
                db.execute("INSERT OR IGNORE INTO ask_answers(id,question,answer,clicked) VALUES(?1,?2,?3,1)", params![id, question, stored])?;
                db.execute(
                    "INSERT OR IGNORE INTO outbox(id,text,delivered,epoch) VALUES(?1,?2,0,?3)",
                    params![id, delivered, permit.generation() as i64],
                )?;
                Ok(())
            })();
            match r {
                Ok(()) => db.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = db.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
            Ok(())
        })?;
        drop(db);
        self.drain(permit)?;
        Ok(Clicked::Delivered)
    }
    /// Checks a card's answers against its questions: each question answered at most once, labels from its options and
    /// picked once, one choice for single select (any number for multi-select), Other only where the question allows
    /// it. A question with no answer, or an empty one, is left unanswered (`None`), but at least one must be answered
    /// (bug note 2026-11-02T16-27-21). Returns each question's answer text in card order ("A, C; Other: \"text\"").
    fn check_answers(items: &[AskItem], answers: &[AskAnswer]) -> Result<Vec<(String, Option<String>)>> {
        for (i, a) in answers.iter().enumerate() {
            ensure!(items.iter().any(|q| q.id == a.question_id), "ASK_UNKNOWN_QUESTION: no question {} on this card", a.question_id);
            ensure!(!answers[..i].iter().any(|b| b.question_id == a.question_id), "ASK_BAD_ANSWER: one answer per question ({})", a.question_id);
        }
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let Some(a) = answers.iter().find(|a| a.question_id == item.id).filter(|a| !a.labels.is_empty() || a.other.is_some()) else {
                out.push((item.id.clone(), None));
                continue;
            };
            for (i, label) in a.labels.iter().enumerate() {
                ensure!(item.options.iter().any(|o| &o.label == label), "ASK_UNKNOWN_CHOICE: answer with one of the question's options");
                ensure!(!a.labels[..i].contains(label), "ASK_BAD_ANSWER: pick each option once ({})", item.id);
            }
            let other = match a.other.as_deref() {
                None => None,
                Some(text) => {
                    let text = text.trim();
                    ensure!(item.allow_other, "ASK_BAD_ANSWER: {} has no Other", item.id);
                    ensure!(!text.is_empty() && text.chars().count() <= crate::mcp_contract::OTHER_CHARS, "ASK_BAD_ANSWER: Other is 1 to 500 characters ({})", item.id);
                    Some(text)
                }
            };
            let picks = a.labels.len() + usize::from(other.is_some());
            if item.multi_select {
                ensure!(picks >= 1, "ASK_BAD_ANSWER: {} needs at least one pick", item.id);
            } else {
                ensure!(picks == 1, "ASK_BAD_ANSWER: {} takes one answer", item.id);
            }
            let mut part = a.labels.join(", ");
            if let Some(text) = other {
                let other_part = format!("Other: \"{text}\"");
                part = if part.is_empty() { other_part } else { format!("{part}; {other_part}") };
            }
            out.push((item.id.clone(), Some(part)));
        }
        ensure!(out.iter().any(|(_, part)| part.is_some()), "ASK_INCOMPLETE: answer at least one question on the card");
        Ok(out)
    }
    /// `ask_close`: the agent closes an open question with the user's spoken answer. Nothing is delivered back (the agent
    /// already knows); the card shows the answer and closes. Closing twice, or a question already clicked, is fine.
    pub fn close_question(&self, id: &str, answer: &str, permit: &DeliveryPermit) -> Result<String> {
        Ok(match self.close(id, answer, permit)? {
            Closed::Closed | Closed::AlreadyClosed => "closed",
            Closed::AnsweredByClick => "already answered by click",
            Closed::NotOpen => "no longer open",
        }
        .into())
    }
    /// Close an open question with the user's words: the agent's `ask_close`, or the host when he spoke to Heimdall while
    /// the card was up (`Tools::voice_closes`). Only the first close shows; the rest report what happened.
    pub fn close(&self, id: &str, answer: &str, permit: &DeliveryPermit) -> Result<Closed> {
        let sink = self.sink()?;
        permit.check()?;
        let state: String = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT state FROM receipts WHERE id=? AND kind='ask'", [id], |r| r.get(0))
            .optional()?
            .context("ASK_NOT_FOUND: unknown request_id; use the id ask returned")?;
        match state.as_str() {
            "shown" => {
                // The state moves first: a click racing this close finds it closed and is dropped, never delivered.
                let moved = permit.with(|| Ok(self.db.lock().unwrap().execute("UPDATE receipts SET state='closed' WHERE id=? AND state='shown'", [id])?))?;
                if moved == 0 { return self.close(id, answer, permit); }
                self.record_answer(id, answer)?;
                permit.with(|| sink.question_closed(id, answer, permit))?;
                Ok(Closed::Closed)
            }
            "closed" => Ok(Closed::AlreadyClosed),
            "answered" => Ok(Closed::AnsweredByClick),
            _ => Ok(Closed::NotOpen),
        }
    }
    /// A guarded T3 call's confirmation question (`CONFIRM_REQUIRED`): only the user's click approves it, so his voice
    /// does not close its card. Remembered for an hour, by its exact words.
    pub fn require_click(&self, question: &str) {
        let now = crate::now_ms();
        let mut q = self.click_only.lock().unwrap();
        q.retain(|_, until| *until > now);
        q.insert(question.trim().to_owned(), now + 60 * 60 * 1000);
    }
    /// Whether question `id` is a confirmation only a click answers (`require_click`).
    pub fn needs_click(&self, id: &str) -> bool {
        let Ok(body) = self.db.lock().unwrap().query_row("SELECT body FROM receipts WHERE id=? AND kind='ask'", [id], |r| r.get::<_, String>(0)) else { return false };
        let Ok(items) = Self::items_of(&body) else { return false };
        // A card that grants something answers only to a click, for its whole life and across restarts.
        if items.iter().flat_map(|q| &q.options).any(|o| o.grant.is_some()) { return true; }
        let question = Self::question_text(&items);
        let now = crate::now_ms();
        self.click_only.lock().unwrap().get(question.trim()).is_some_and(|until| *until > now)
    }
    /// The question and the user's answer (the clicked label, or his spoken words via `ask_close`), kept after the
    /// receipt body is retired. T3 confirmations check it.
    pub fn ask_answer(&self, id: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.lock().unwrap().query_row("SELECT question,answer FROM ask_answers WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }
    /// The question and the label the user clicked; `None` for an unanswered question or one closed by voice.
    pub fn ask_click(&self, id: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.lock().unwrap().query_row("SELECT question,answer FROM ask_answers WHERE id=? AND clicked=1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }
    fn record_answer(&self, id: &str, answer: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        let body: String = db.query_row("SELECT body FROM receipts WHERE id=?", [id], |r| r.get(0))?;
        if let Ok(items) = Self::items_of(&body) {
            db.execute("INSERT OR IGNORE INTO ask_answers(id,question,answer,clicked) VALUES(?1,?2,?3,0)", params![id, Self::question_text(&items), answer])?;
        }
        Ok(())
    }
    /// Revoking a question cannot grant permission; it is allowed after its permit is revoked.
    pub fn cancel_question(&self, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE receipts SET state='cancelled' WHERE id=? AND kind='ask' AND state IN ('pending','shown')", [id])?;
        Ok(())
    }
    pub fn reload(&self, op: &str, permit: &DeliveryPermit) -> Result<String> {
        self.reload_reason(op, None, permit)
    }
    pub fn reload_reason(&self, op: &str, reason: Option<&str>, permit: &DeliveryPermit) -> Result<String> {
        let sink = self.sink()?;
        let id = self.receipt(op, "reload", reason.unwrap_or("prepared MCP/Pi reload"), None, permit)?;
        if self.state(&id)? == "pending" {
            let request = reason.map_or_else(||id.clone(),|r|format!("{id}: {r}"));
            permit.with(|| sink.reload(&request, permit))?;
            self.set_state(&id, "scheduled", permit)?;
        }
        Ok(id)
    }
    /// Receipts of one kind in one state, with their bodies, oldest first.
    pub fn receipts_in(&self, kind: &str, state: &str) -> Result<Vec<(String, String)>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT id,body FROM receipts WHERE kind=? AND state=? ORDER BY rowid")?;
        let rows = q.query_map([kind, state], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn state(&self, id: &str) -> Result<String> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT state FROM receipts WHERE id=?",
            [id],
            |r| r.get(0),
        )?)
    }
    pub fn set_state(&self, id: &str, state: &str, permit: &DeliveryPermit) -> Result<()> {
        permit.with(|| {
            self.db
                .lock()
                .unwrap()
                .execute("UPDATE receipts SET state=? WHERE id=?", params![state, id])?;
            Ok(())
        })
    }
    pub fn enqueue(&self, id: &str, text: &str, permit: &DeliveryPermit) -> Result<()> {
        self.enqueue_with_short(id, text, None, permit)
    }
    /// `enqueue` with the short form delivered instead if the agent rejects `text` as too long (`MessageRejected`).
    pub fn enqueue_with_short(&self, id: &str, text: &str, short: Option<&str>, permit: &DeliveryPermit) -> Result<()> {
        permit.with(|| {
            self.db.lock().unwrap().execute(
                "INSERT OR IGNORE INTO outbox(id,text,delivered,epoch,short) VALUES(?1,?2,0,?3,?4)",
                params![id, text, permit.generation() as i64, short],
            )?;
            Ok(())
        })
    }
    pub fn drain(&self, permit: &DeliveryPermit) -> Result<()> {
        let sink = self.sink()?;
        permit.with(|| { self.db.lock().unwrap().execute("UPDATE outbox SET delivered=-1 WHERE delivered=0 AND epoch!=?", [permit.generation() as i64])?; Ok(()) })?;
        let rows = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT id,text,short FROM outbox WHERE delivered=0 ORDER BY rowid LIMIT 100")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let rejected = |e: &anyhow::Error| e.downcast_ref::<crate::MessageRejected>().is_some();
        for (id, text, short) in rows {
            if let Err(e) = permit.with(|| sink.deliver(&id, &text, permit)) {
                // Too long for the agent: the short pointer goes instead, so one long reply never jams the notices
                // behind it (a rejection is final; retrying the same text would fail forever).
                if !rejected(&e) { return Err(e); }
                let short = short.filter(|s| s.len() < text.len()).unwrap_or_else(|| too_long(&text));
                match permit.with(|| sink.deliver(&id, &short, permit)) {
                    Ok(()) => {}
                    Err(e) if rejected(&e) => {
                        eprintln!("Notice {id} dropped: even its short form is too long for the agent");
                        permit.with(|| { self.db.lock().unwrap().execute("UPDATE outbox SET delivered=-2 WHERE id=?", [&id])?; Ok(()) })?;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            permit.with(|| {
                self.db
                    .lock()
                    .unwrap()
                    .execute("UPDATE outbox SET delivered=1 WHERE id=?", [id])?;
                Ok(())
            })?;
        }
        Ok(())
    }
}

// Scheduled wakes (`agent::wakes`, night 21 Oct): the heartbeat reads its own outbox rows, re-arms one an Agent Off/On
// dropped before delivery, and lists what is still undelivered.
impl Actions {
    /// An outbox row's `(delivered, epoch)`: delivered 1, waiting 0, dropped by an Off/On -1, too long -2.
    pub fn outbox_state(&self, id: &str) -> Result<Option<(i64, i64)>> {
        Ok(self.db.lock().unwrap().query_row("SELECT delivered,epoch FROM outbox WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }
    /// Re-arm an undelivered row from an older epoch (or one already dropped) in `permit`'s epoch with new text. Same
    /// id, so the agent's own deduplication still holds.
    pub fn outbox_rearm(&self, id: &str, text: &str, permit: &DeliveryPermit) -> Result<()> {
        let epoch = permit.generation() as i64;
        permit.with(|| {
            self.db.lock().unwrap().execute(
                "UPDATE outbox SET delivered=0,epoch=?2,text=?3 WHERE id=?1 AND (delivered=-1 OR (delivered=0 AND epoch!=?2))",
                params![id, epoch, text],
            )?;
            Ok(())
        })
    }
    /// Ids of outbox rows still waiting for delivery, oldest first.
    pub fn outbox_undelivered(&self) -> Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT id FROM outbox WHERE delivered=0 ORDER BY rowid")?;
        let rows = q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod outbox_tests {
    use super::*;
    /// An agent that takes messages up to `limit` bytes, like `Bridge::notice` (`MessageRejected` above it).
    struct Agent {
        limit: usize,
        got: Mutex<Vec<(String, String)>>,
    }
    impl ActionSink for Agent {
        fn notice(&self, _: &Notice, _: &DeliveryPermit) -> Result<()> { Ok(()) }
        fn question(&self, _: &Question, _: &DeliveryPermit) -> Result<()> { Ok(()) }
        fn question_closed(&self, _: &str, _: &str, _: &DeliveryPermit) -> Result<()> { Ok(()) }
        fn deliver(&self, id: &str, text: &str, _: &DeliveryPermit) -> Result<()> {
            if text.len() > self.limit { return Err(crate::MessageRejected.into()); }
            self.got.lock().unwrap().push((id.into(), text.into()));
            Ok(())
        }
        fn reload(&self, _: &str, _: &DeliveryPermit) -> Result<()> { Ok(()) }
    }

    /// Review 19 Oct (MUST 2): a worker reply longer than the agent takes jammed the outbox (drain stopped at the
    /// rejection and retried it forever). Now its short form goes instead and the notices behind it still arrive.
    #[test]
    fn a_notice_too_long_for_the_agent_goes_short_and_never_jams_the_outbox() {
        let d = tempfile::tempdir().unwrap();
        let actions = Actions::open(&d.path().join("a.sqlite")).unwrap();
        let agent = Arc::new(Agent { limit: 1024, got: Mutex::new(vec![]) });
        actions.attach(agent.clone());
        let gate = Arc::new(crate::delivery::DeliveryGate::new(true));
        let p = gate.permit().unwrap();
        let long = format!("Task 'Report' (w1) completed. Final reply:\n\n{}", "zażółć ".repeat(400));
        actions.enqueue_with_short("terminal:w1", &long, Some("Task 'Report' (w1) completed. Its reply is 4 KB; read it with task_status."), &p).unwrap();
        actions.enqueue("t3:batch:x", &format!("T3 update:\n- 'Other' completed.\n{}", "y".repeat(3000)), &p).unwrap();
        actions.enqueue_with_short("terminal:w2", &"z".repeat(5000), Some(&"s".repeat(2000)), &p).unwrap();
        actions.enqueue("terminal:w3", "Task w3 completed with no reply text.", &p).unwrap();
        actions.drain(&p).unwrap();
        let got = agent.got.lock().unwrap().clone();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], ("terminal:w1".into(), "Task 'Report' (w1) completed. Its reply is 4 KB; read it with task_status.".into()));
        // No short form from its producer: the first line and a pointer.
        assert_eq!(got[1].0, "t3:batch:x");
        assert!(got[1].1.starts_with("T3 update:\nThe rest (3 KB) is too long for one message; read task replies with task_status"), "{}", got[1].1);
        // Even the short form too long: dropped with a log line, never blocking the next one.
        assert_eq!(got[2], ("terminal:w3".into(), "Task w3 completed with no reply text.".into()));
        let db = actions.db.lock().unwrap();
        assert_eq!(db.query_row("SELECT delivered FROM outbox WHERE id='terminal:w2'", [], |r| r.get::<_, i64>(0)).unwrap(), -2);
        assert_eq!(db.query_row("SELECT count(*) FROM outbox WHERE delivered=0", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        drop(db);
        // A drain that finds nothing pending delivers nothing again.
        actions.drain(&p).unwrap();
        assert_eq!(agent.got.lock().unwrap().len(), 3);
    }

    #[test]
    fn notice_budget_follows_the_live_message_limit() {
        let d = tempfile::tempdir().unwrap();
        let actions = Actions::open(&d.path().join("a.sqlite")).unwrap();
        assert_eq!(actions.message_limit(), DEFAULT_MESSAGE_LIMIT);
        assert_eq!(actions.notice_budget(), 56 * 1024);
        actions.set_message_limit(100 * 1024);
        assert_eq!(actions.notice_budget(), 100 * 1024 - 100 * 1024 / 8);
        assert!(actions.notice_budget() < actions.message_limit());
    }
}
