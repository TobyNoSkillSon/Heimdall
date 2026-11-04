//! Nothing can cover (docs/claude-relay.md): emergency mode found no T3 Code relay to drive
//! Heimdall. The island says so under its large warning triangle, then asks the one useful question:
//! - the local engine and its model are on this Mac: "Switch to the local model?" Yes / No;
//! - the engine is there but not the model: the same offer with what it needs (the download, the engine's memory
//!   and disk ceilings), Yes starts the engine on it, which downloads it first (`docs/inference-engine.md`);
//! - no engine at all: said once, then waiting for the quota.
//! No (or no engine) leaves a quiet "Waiting for the quota" line with the reset when it is known. Yes remembers the
//! main model (`Emergency::local_fallback`, so the hand-back returns to it) and picks the engine's model through the
//! menu's own path (`UiAction::SelectEngineModel`). The answer arrives in `pipeline/actions.rs` by `CARD_PREFIX`.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ui::agent_notice::{AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};

/// The question card's request id prefix (answers route to `answer`).
pub const CARD_PREFIX: &str = "emergency-local:";
pub const YES: &str = "Yes";
pub const NO: &str = "No";
/// The warning line's read before the question arrives (a line reads 1.8 to 3.4 s, `island::read_seconds`).
const WARNING_READ: std::time::Duration = std::time::Duration::from_millis(3500);
/// How long the question stays open.
pub const CARD_MS: u64 = 15 * 60_000;

/// What the local engine can do now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Engine {
    /// The engine and the model's weights are on this Mac.
    Ready { model: String, label: String },
    /// The engine is installed, the model is not: it downloads on the engine's first start.
    Download { model: String, label: String, download_bytes: Option<u64>, memory: String, disk: String },
    /// No engine executable: nothing Heimdall can set up from here.
    Missing,
}

/// The engine as `<data>/splash/engine.json` (else the bundled default) describes it, the weights looked up in
/// `hub` (the Hugging Face cache), the download's size asked of `size` when they are missing.
pub fn engine(data: &Path, engine_model: &str, hub: &Path, size: &dyn Fn(&str, &str) -> Option<u64>) -> Engine {
    let raw = std::fs::read_to_string(data.join("splash/engine.json")).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v["models"].as_array().is_some_and(|m| !m.is_empty()))
        .unwrap_or_else(|| serde_json::from_str(include_str!("../config/inference-engine.json")).expect("bundled engine config"));
    let Some(exe) = raw["executable"].as_str().map(|e| executable(e, std::env::current_exe().ok().as_deref().and_then(Path::parent))) else { return Engine::Missing };
    if !exe.exists() { return Engine::Missing }
    let models = raw["models"].as_array().cloned().unwrap_or_default();
    let wanted = if engine_model.is_empty() { raw["default_model"].as_str().unwrap_or_default() } else { engine_model };
    let Some(m) = models.iter().find(|m| m["id"] == wanted).or(models.first()) else { return Engine::Missing };
    let (id, label, quant, source) = (s(&m["id"]), s(&m["label"]), s(&m["quant"]), s(&m["source"]));
    let repo = source.split(':').next().unwrap_or_default().to_owned();
    if weights_cached(hub, &repo, &quant) { return Engine::Ready { model: id, label } }
    // The size Hugging Face reports, else the one the engine's catalog keeps for that quant (the quick scan reads no
    // network: `size` answers `None` there).
    let listed = m["quants"].as_array().into_iter().flatten().find(|q| q["quant"].as_str() == Some(quant.as_str())).and_then(|q| q["size_bytes"].as_u64());
    Engine::Download { model: id, label, download_bytes: size(&repo, &quant).or(listed), memory: gigabytes(&raw["max_memory"]), disk: gigabytes(&raw["max_cache_disk"]) }
}

/// The engine's executable: a bare name is the binary beside Heimdall's own (`Contents/MacOS/heimdall-splash`, as
/// `inference_engine::Config::executable` starts it); an absolute path as given. Bug note 2026-11-02T14-47-39: the
/// bare name was looked up in the working folder, so the engine always read as missing and was never offered.
fn executable(name: &str, beside: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(name);
    if path.is_absolute() { return path }
    beside.map_or(path.clone(), |dir| dir.join(&path))
}

fn s(v: &serde_json::Value) -> String { v.as_str().unwrap_or_default().to_owned() }

/// "48G" → "48 GB".
fn gigabytes(v: &serde_json::Value) -> String {
    let t = v.as_str().unwrap_or_default();
    t.strip_suffix('G').map_or_else(|| t.to_owned(), |n| format!("{n} GB"))
}

/// A `.gguf` of the quant in any snapshot of `repo` in the Hugging Face cache.
fn weights_cached(hub: &Path, repo: &str, quant: &str) -> bool {
    let dir = hub.join(format!("models--{}", repo.replace('/', "--"))).join("snapshots");
    let Ok(snapshots) = std::fs::read_dir(dir) else { return false };
    snapshots.flatten().any(|snap| std::fs::read_dir(snap.path()).is_ok_and(|files| files.flatten().any(|f| {
        let name = f.file_name().to_string_lossy().into_owned();
        name.ends_with(".gguf") && (quant.is_empty() || name.contains(quant)) && f.path().metadata().is_ok_and(|m| m.len() > 0)
    })))
}

/// The Hugging Face cache Splash downloads into.
pub fn hub() -> PathBuf {
    if let Ok(h) = std::env::var("HF_HUB_CACHE") { return h.into() }
    if let Ok(h) = std::env::var("HF_HOME") { return PathBuf::from(h).join("hub") }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/huggingface/hub")
}

/// The quant's files' size from Hugging Face's file list (one request, 5 s); `None` when it cannot be read.
pub fn download_size(repo: &str, quant: &str) -> Option<u64> {
    let url = format!("https://huggingface.co/api/models/{repo}/tree/main");
    let list: serde_json::Value = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build().ok()?
        .get(url).send().ok()?.error_for_status().ok()?.json().ok()?;
    let total: u64 = list.as_array()?.iter()
        .filter(|f| f["path"].as_str().is_some_and(|p| p.ends_with(".gguf") && p.contains(quant)))
        .filter_map(|f| f["lfs"]["size"].as_u64().or(f["size"].as_u64())).sum();
    (total > 0).then_some(total)
}

/// "16.8 GB".
pub fn size_text(bytes: u64) -> String { format!("{:.1} GB", bytes as f64 / 1e9) }

fn until(reset_ms: Option<u64>) -> String { reset_ms.map(|ms| format!(" · back at {}", ui::model::local_hhmm(ms))).unwrap_or_default() }

/// What the island shows, in order: the warning line (its triangle), then the question or the closing line.
pub fn cards(engine: &Engine, reset_ms: Option<u64>, now_ms: u64) -> Vec<AgentNotice> {
    let warn = AgentNotice::line(format!("{CARD_PREFIX}warning"), "Quota ran out · no T3 Code relay can drive Heimdall", NoticeKind::Warning);
    // Yes / No, no Other row; what Yes takes goes under it.
    let ask = |text: String, needs: Option<String>| AgentNotice {
        id: format!("{CARD_PREFIX}{now_ms}"),
        text: text.clone(),
        kind: NoticeKind::Warning,
        action: Some(NoticeAction::Ask {
            request_id: format!("{CARD_PREFIX}{now_ms}"),
            questions: vec![AskQuestion { id: "q1".into(), header: None, text, multi_select: false, allow_other: false, field: None,
                options: vec![AskOption { label: YES.into(), description: needs, grant: vec![] }, AskOption::label(NO)] }],
            expires_ms: now_ms + CARD_MS, countdown: false,
        }),
        outcome: None,
        expires_ms: None,
        detail: None,
    };
    let second = match engine {
        Engine::Ready { label, .. } => ask(format!("Switch to the local model, {label}?"), None),
        Engine::Download { label, download_bytes, memory, disk, .. } => {
            let download = download_bytes.map(|b| format!("{} download · ", size_text(b))).unwrap_or_default();
            ask(format!("Download the local model, {label}?"), Some(format!("{download}up to {memory} memory · {disk} disk")))
        }
        Engine::Missing => AgentNotice::line(format!("{CARD_PREFIX}missing"), format!("No local engine on this Mac · waiting for the quota{}", until(reset_ms)), NoticeKind::Info),
    };
    vec![warn, second]
}

/// The quiet line after No (or an unanswered card): waiting for the quota, with its reset when known.
pub fn waiting(reset_ms: Option<u64>) -> AgentNotice {
    AgentNotice::line(format!("{CARD_PREFIX}waiting"), format!("Waiting for the quota{}", until(reset_ms)), NoticeKind::Info)
}

/// The open offer: what Yes switches to, and the reset for the waiting line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer { pub request_id: String, pub model: String, pub reset_ms: Option<u64> }

static DATA: Mutex<Option<PathBuf>> = Mutex::new(None);
static OPEN: Mutex<Option<Offer>> = Mutex::new(None);

/// The pipeline's data folder (set once at launch, `pipeline/mod.rs`): where the engine's configuration and settings are.
pub fn attach(data: &Path) { *DATA.lock().unwrap() = Some(data.into()) }

/// Emergency mode found no relay (`crate::emergency::AppSide::no_relay`). Off the caller's thread: the download's size
/// is one request to Hugging Face.
pub fn offer(tools: &Arc<crate::bridge::Tools>, reset_ms: Option<u64>) {
    let Some(data) = DATA.lock().unwrap().clone() else { eprintln!("emergency: no relay, and the local offer has no data folder"); return };
    let tools = Arc::downgrade(tools);
    std::thread::spawn(move || {
        let model = crate::settings::Settings::load(&data.join("settings.json")).map(|s| s.engine_model).unwrap_or_default();
        let engine = engine(&data, &model, &hub(), &download_size);
        eprintln!("emergency: no relay can cover; offering the local engine ({engine:?})");
        let now = agent::now_ms();
        let shown = cards(&engine, reset_ms, now);
        *OPEN.lock().unwrap() = match &engine {
            Engine::Ready { model, .. } | Engine::Download { model, .. } => Some(Offer { request_id: format!("{CARD_PREFIX}{now}"), model: model.clone(), reset_ms }),
            Engine::Missing => None,
        };
        // The warning reads alone first (its large triangle), then the question or the closing line.
        for (k, card) in shown.into_iter().enumerate() {
            if k > 0 { std::thread::sleep(WARNING_READ) }
            let Some(tools) = tools.upgrade() else { return };
            tools.emergency_ui(ui::UiUpdate::AgentNotice(card));
        }
    });
}

/// What the user's answer does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Yes: the local model.
    Switch(Offer),
    /// No: the waiting line, with the quota's reset when known.
    Wait(Option<u64>),
}

/// The user's answer to the open card `request_id` (`None`: not the open card, or already answered).
pub fn take_answer(request_id: &str, choice: &str) -> Option<Answer> {
    let mut open = OPEN.lock().unwrap();
    if open.as_ref().is_none_or(|o| o.request_id != request_id) { return None }
    let offer = open.take()?;
    Some(if choice == YES { Answer::Switch(offer) } else { Answer::Wait(offer.reset_ms) })
}

/// The main model's own card after the local stand-in (30 Oct 01:2x: nothing switches the agent's model by itself):
/// when the main model answers again, the island asks "Back to <model>?" Yes / No instead of switching back.
pub const BACK_PREFIX: &str = "emergency-back:";

/// The open "Back to …?" question: the model Yes picks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Back { pub request_id: String, pub provider: String, pub id: String }

static BACK: Mutex<Option<Back>> = Mutex::new(None);

/// The card for `name` (the main model's menu name) at `now_ms`.
pub fn back_card(name: &str, now_ms: u64) -> AgentNotice {
    let request_id = format!("{BACK_PREFIX}{now_ms}");
    let text = format!("{name} answers again. Back to it?");
    AgentNotice {
        id: request_id.clone(),
        text: text.clone(),
        kind: NoticeKind::Info,
        action: Some(NoticeAction::Ask {
            request_id,
            questions: vec![AskQuestion { id: "q1".into(), header: None, text, multi_select: false, allow_other: false, field: None,
                options: vec![AskOption::label(YES), AskOption::label(NO)] }],
            expires_ms: now_ms + CARD_MS,
            countdown: false,
        }),
        outcome: None,
        expires_ms: None,
        detail: None,
    }
}

/// The main model works again after the local stand-in (`crate::emergency::AppSide::restore_model`): ask, never switch.
pub fn offer_back(tools: &Arc<crate::bridge::Tools>, model: &agent::provider::ApiModel) {
    let now = agent::now_ms();
    let card = back_card(&model.name, now);
    *BACK.lock().unwrap() = Some(Back { request_id: card.id.clone(), provider: model.provider.clone(), id: model.id.clone() });
    eprintln!("emergency: {} answers again; asking before switching back", model.id);
    tools.emergency_ui(ui::UiUpdate::AgentNotice(card));
}

/// Whether `request_id` is the open "Back to …?" card (an old card's answer does nothing).
pub fn open_back(request_id: &str) -> bool {
    BACK.lock().unwrap().as_ref().is_some_and(|o| o.request_id == request_id)
}

/// The user's answer to the open "Back to …?" card: Yes names the model to pick; No (or another card) nothing.
pub fn take_back(request_id: &str, choice: &str) -> Option<Back> {
    let mut open = BACK.lock().unwrap();
    if open.as_ref().is_none_or(|o| o.request_id != request_id) { return None }
    let back = open.take()?;
    (choice == YES).then_some(back)
}

/// Test seam: the "Back to …?" card `offer_back` would leave open.
#[cfg(test)]
pub fn set_back(b: Option<Back>) { *BACK.lock().unwrap() = b }

/// Test seam: the offer `offer` would leave open.
#[cfg(test)]
pub fn set_open(o: Option<Offer>) { *OPEN.lock().unwrap() = o }

#[cfg(test)]
mod tests {
    use super::*;

    fn data_with(exe: &Path) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("splash")).unwrap();
        let cfg = serde_json::json!({"executable": exe, "port": 8100, "max_memory": "48G", "max_cache_disk": "100G", "default_model": "qwen3.8-27b",
            "models": [{"id": "qwen3.8-27b", "label": "Qwen3.8 27B", "quant": "UD-Q4_K_M", "source": "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M"}]});
        std::fs::write(d.path().join("splash/engine.json"), cfg.to_string()).unwrap();
        d
    }
    fn none(_: &str, _: &str) -> Option<u64> { None }

    /// Installed (engine and weights): the warning line, then "Switch to the local model?" Yes / No.
    #[test]
    fn installed_asks_to_switch() {
        let exe = tempfile::NamedTempFile::new().unwrap();
        let data = data_with(exe.path());
        let hub = tempfile::tempdir().unwrap();
        let snap = hub.path().join("models--unsloth--Qwen3.8-27B-GGUF/snapshots/abc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("Qwen3.8-27B-UD-Q4_K_M.gguf"), b"weights").unwrap();
        let e = engine(data.path(), "qwen3.8-27b", hub.path(), &none);
        assert_eq!(e, Engine::Ready { model: "qwen3.8-27b".into(), label: "Qwen3.8 27B".into() });
        let c = cards(&e, None, 1);
        assert_eq!(c[0].kind, NoticeKind::Warning, "the large triangle first");
        assert!(c[0].text.contains("no T3 Code relay"));
        let Some(NoticeAction::Ask { questions, .. }) = &c[1].action else { panic!("{c:?}") };
        assert_eq!(questions[0].text, "Switch to the local model, Qwen3.8 27B?");
        assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), [YES, NO]);
    }

    /// The engine without the model: the setup offer names the download, the memory and the disk.
    #[test]
    fn not_installed_offers_the_download_with_its_needs() {
        let exe = tempfile::NamedTempFile::new().unwrap();
        let data = data_with(exe.path());
        let hub = tempfile::tempdir().unwrap();
        let asked = std::sync::Mutex::new(vec![]);
        let size = |repo: &str, quant: &str| { asked.lock().unwrap().push((repo.to_owned(), quant.to_owned())); Some(16_800_000_000) };
        let e = engine(data.path(), "", hub.path(), &size);
        assert_eq!(asked.lock().unwrap().as_slice(), [("unsloth/Qwen3.8-27B-GGUF".to_owned(), "UD-Q4_K_M".to_owned())]);
        let c = cards(&e, None, 1);
        let Some(NoticeAction::Ask { questions, .. }) = &c[1].action else { panic!("{c:?}") };
        assert_eq!(questions[0].text, "Download the local model, Qwen3.8 27B?");
        assert_eq!(questions[0].options[0].description.as_deref(), Some("16.8 GB download · up to 48 GB memory · 100 GB disk"), "what Yes takes, under it");
        // The size unknown (offline): the needs without it.
        let e = engine(data.path(), "", hub.path(), &none);
        let c = cards(&e, None, 1);
        let Some(NoticeAction::Ask { questions, .. }) = &c[1].action else { panic!("{c:?}") };
        assert_eq!(questions[0].options[0].description.as_deref(), Some("up to 48 GB memory · 100 GB disk"));
        // No engine at all: said, then waiting; nothing to answer.
        let gone = data_with(Path::new("/nonexistent/splash"));
        let e = engine(gone.path(), "", hub.path(), &none);
        assert_eq!(e, Engine::Missing);
        let c = cards(&e, Some(0), 1);
        assert!(c[1].action.is_none() && c[1].text.starts_with("No local engine on this Mac · waiting for the quota · back at "), "{}", c[1].text);
    }

    /// Bug note 2026-11-02T14-47-39: the bundled config names the engine bare ("heimdall-splash"), and it is the
    /// binary beside Heimdall's; the catalog's size stands in when Hugging Face is not asked.
    #[test]
    fn a_bare_engine_name_is_the_binary_beside_heimdall() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(executable("heimdall-splash", Some(dir.path())), dir.path().join("heimdall-splash"));
        assert_eq!(executable("/opt/splash", Some(dir.path())), PathBuf::from("/opt/splash"));
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join("splash")).unwrap();
        let exe = tempfile::NamedTempFile::new().unwrap();
        let cfg = serde_json::json!({"executable": exe.path(), "max_memory": "48G", "max_cache_disk": "100G", "default_model": "q",
            "models": [{"id": "q", "label": "Q", "quant": "Q4", "source": "o/r:Q4", "quants": [{"quant": "Q4", "size_bytes": 17_400_000_000u64}]}]});
        std::fs::write(data.path().join("splash/engine.json"), cfg.to_string()).unwrap();
        let hub = tempfile::tempdir().unwrap();
        assert!(matches!(engine(data.path(), "", hub.path(), &none), Engine::Download { download_bytes: Some(17_400_000_000), .. }));
    }

    /// No: the waiting line, with the reset when known; only the open card's answer counts, once.
    #[test]
    fn declined_waits_for_the_quota_with_or_without_its_reset() {
        assert_eq!(waiting(None).text, "Waiting for the quota");
        let reset = 1_900_000_000_000;
        assert_eq!(waiting(Some(reset)).text, format!("Waiting for the quota · back at {}", ui::model::local_hhmm(reset)));
        assert_eq!(waiting(Some(reset)).kind, NoticeKind::Info);
        set_open(Some(Offer { request_id: "emergency-local:5".into(), model: "qwen3.8-27b".into(), reset_ms: Some(reset) }));
        assert_eq!(take_answer("emergency-local:4", YES), None, "another card");
        assert_eq!(take_answer("emergency-local:5", NO), Some(Answer::Wait(Some(reset))), "No: wait, with the reset");
        assert_eq!(take_answer("emergency-local:5", YES), None, "answered once");
        let offer = Offer { request_id: "emergency-local:6".into(), model: "qwen3.8-27b".into(), reset_ms: None };
        set_open(Some(offer.clone()));
        assert_eq!(take_answer("emergency-local:6", YES), Some(Answer::Switch(offer)));
    }

    /// 30 Oct 01:2x: the main model back after the local stand-in is a question, never a switch; only its own card's
    /// Yes picks it, once.
    #[test]
    fn the_main_model_back_is_asked_not_switched() {
        let card = back_card("Claude Opus 5.5", 7);
        assert_eq!(card.text, "Claude Opus 5.5 answers again. Back to it?");
        let Some(NoticeAction::Ask { request_id, questions, .. }) = &card.action else { panic!("{card:?}") };
        assert!(request_id.starts_with(BACK_PREFIX) && !request_id.starts_with(CARD_PREFIX));
        assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), [YES, NO]);
        *BACK.lock().unwrap() = Some(Back { request_id: request_id.clone(), provider: "anthropic".into(), id: "claude-opus-5-5".into() });
        assert_eq!(take_back("emergency-back:6", YES), None, "another card");
        assert_eq!(take_back(request_id, NO), None, "No keeps the local model");
        assert_eq!(take_back(request_id, YES), None, "answered once");
        *BACK.lock().unwrap() = Some(Back { request_id: request_id.clone(), provider: "anthropic".into(), id: "claude-opus-5-5".into() });
        assert_eq!(take_back(request_id, YES).map(|b| b.id), Some("claude-opus-5-5".into()));
    }
}
