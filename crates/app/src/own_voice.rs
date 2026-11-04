//! [voices] Speaking ▸ Voice ▸ Own Engine: any OpenAI-compatible speech server on the user's machine (54:
//! "you can add any compatible third-party API that runs on your machine"): a base URL ("http://localhost:8880/v1"), an
//! optional key, and the voices it lists. Speech is `POST <base>/audio/speech` (`model`, `voice`, `input`,
//! `response_format: "wav"`), one sentence at a time like the other voices. Voices come from `GET <base>/audio/voices`
//! (Kokoro-FastAPI's and others' extension) when the server has it, else OpenAI's standard names; the model is the
//! first speech-looking id from `GET <base>/models`, else `tts-1`.
//!
//! The endpoint and key live together in the login Keychain (installed app; service "Heimdall voice provider: Own
//! Engine"), or in memory in lab builds, like ElevenLabs' key.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// OpenAI's built-in voice names, offered when the server does not list its own.
pub const STANDARD_VOICES: [&str; 9] = ["alloy", "ash", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer"];
/// The public edition has its own Keychain name, so it never reads the personal app's item; the personal name is unchanged.
pub(crate) const KEYCHAIN_SERVICE: &str = if crate::edition::PUBLIC { "Heimdall Alpha voice provider: Own Engine" } else { "Heimdall voice provider: Own Engine" };
const ACCOUNT: &str = "own-voice";
/// A sentence's audio is read whole; nothing a sentence says is this large.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// What Connect keeps.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub model: String,
}
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("base", &self.base).field("model", &self.model).field("key", &self.key.as_ref().map(|_| "<set>")).finish()
    }
}

static LAB: Mutex<Option<Endpoint>> = Mutex::new(None);

pub fn saved() -> Option<Endpoint> {
    if !agent::provider::is_installed_app() {
        return LAB.lock().unwrap().clone();
    }
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    let items = ItemSearchOptions::new().class(ItemClass::generic_password()).service(KEYCHAIN_SERVICE).account(ACCOUNT)
        .load_data(true).skip_authenticated_items(true).search().ok()?;
    items.into_iter().find_map(|i| match i { SearchResult::Data(b) => serde_json::from_slice(&b).ok(), _ => None })
}
pub fn save(e: &Endpoint) -> Result<()> {
    if !agent::provider::is_installed_app() {
        *LAB.lock().unwrap() = Some(e.clone());
        return Ok(());
    }
    forget()?;
    let mut options = security_framework::passwords::PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, ACCOUNT);
    options.set_label(KEYCHAIN_SERVICE);
    security_framework::passwords::set_generic_password_options(&serde_json::to_vec(e)?, options).map_err(|_| anyhow::anyhow!("Could not save the endpoint in the Keychain"))
}
pub fn forget() -> Result<()> {
    if !agent::provider::is_installed_app() {
        *LAB.lock().unwrap() = None;
        return Ok(());
    }
    use security_framework::item::{ItemClass, ItemSearchOptions};
    match ItemSearchOptions::new().class(ItemClass::generic_password()).service(KEYCHAIN_SERVICE).account(ACCOUNT).skip_authenticated_items(true).delete() {
        Ok(()) => Ok(()),
        Err(e) if e.code() == -25300 => Ok(()),
        Err(_) => bail!("Could not remove the endpoint from the Keychain"),
    }
}

/// A local server: a sentence that takes longer than this is a hung server, and the lighter voice says it instead.
fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().connect_timeout(std::time::Duration::from_secs(3)).timeout(std::time::Duration::from_secs(15))
        .build().expect("HTTP client")
}
fn get(e: &Endpoint, path: &str) -> Result<Option<serde_json::Value>> {
    let mut r = http().get(format!("{}{path}", e.base));
    if let Some(k) = &e.key { r = r.bearer_auth(k); }
    let r = r.send().map_err(|_| anyhow::anyhow!("{} unreachable · check the URL", host(&e.base)))?;
    match r.status().as_u16() {
        401 | 403 => bail!("Own Engine: key rejected"),
        404 | 405 => Ok(None),
        s if (200..300).contains(&s) => Ok(r.json().ok()),
        s => bail!("Own Engine answered {s}"),
    }
}
fn host(base: &str) -> &str {
    base.split_once("://").map_or(base, |(_, r)| r).split('/').next().unwrap_or(base)
}

/// The voices a server lists: `{"voices":["af_bella", …]}`, `{"voices":[{"id"|"voice_id"|"name", "name"}]}` or a bare
/// array of either.
pub fn parse_voices(v: &serde_json::Value) -> Vec<ui::speech_voice::ApiVoice> {
    let list = v["voices"].as_array().or_else(|| v["data"].as_array()).or_else(|| v.as_array());
    let mut seen = std::collections::BTreeSet::new();
    list.into_iter().flatten().filter_map(|item| {
        let id = item.as_str().or_else(|| item["id"].as_str()).or_else(|| item["voice_id"].as_str()).or_else(|| item["name"].as_str())?.trim();
        if id.is_empty() || id.len() > 128 { return None; }
        let name: String = item["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(id).chars().take(128).collect();
        Some(ui::speech_voice::ApiVoice { id: id.into(), name, desc: String::new() })
    }).filter(|v| seen.insert(v.id.clone())).collect()
}

/// The speech model from `/models`: the first id that names speech ("tts", "kokoro", "speech", "voice"), else the first.
pub fn pick_model(v: &serde_json::Value) -> Option<String> {
    let ids: Vec<&str> = v["data"].as_array().or_else(|| v["models"].as_array()).into_iter().flatten()
        .filter_map(|m| m["id"].as_str().or_else(|| m.as_str())).collect();
    ids.iter().find(|id| ["tts", "kokoro", "speech", "voice"].iter().any(|w| id.to_ascii_lowercase().contains(w))).or(ids.first()).map(|s| s.to_string())
}

/// Connect: the server must answer (its model list or its voice list); returns the endpoint to keep and its voices.
pub fn connect(base: &str, key: Option<&str>) -> Result<(Endpoint, Vec<ui::speech_voice::ApiVoice>)> {
    let base = agent::provider::normalize_base_url(base)?;
    let key = key.map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned);
    let mut e = Endpoint { base, key, model: "tts-1".into() };
    let models = get(&e, "/models")?;
    let voices = get(&e, "/audio/voices")?;
    ensure!(models.is_some() || voices.is_some(), "{} answers, but not as an OpenAI-compatible server", host(&e.base));
    if let Some(m) = models.as_ref().and_then(pick_model) { e.model = m; }
    let listed = voices.as_ref().map(parse_voices).filter(|v| !v.is_empty());
    Ok((e, listed.unwrap_or_else(standard)))
}

pub fn standard() -> Vec<ui::speech_voice::ApiVoice> {
    STANDARD_VOICES.iter().map(|v| {
        let mut name = v.to_string();
        if let Some(f) = name.get_mut(..1) { f.make_ascii_uppercase(); }
        ui::speech_voice::ApiVoice { id: (*v).into(), name, desc: String::new() }
    }).collect()
}

/// The voices again (Refresh Voices).
pub fn voices(e: &Endpoint) -> Result<Vec<ui::speech_voice::ApiVoice>> {
    Ok(get(e, "/audio/voices")?.as_ref().map(parse_voices).filter(|v| !v.is_empty()).unwrap_or_else(standard))
}

/// One sentence in `voice`, as mono samples at the server's rate.
pub fn speak(e: &Endpoint, voice: &str, text: &str, cancel: &dyn Fn() -> bool) -> Result<(Vec<f32>, u32)> {
    ensure!(!cancel(), "speech interrupted");
    let mut r = http().post(format!("{}/audio/speech", e.base))
        .json(&serde_json::json!({"model": e.model, "voice": voice, "input": text, "response_format": "wav"}));
    if let Some(k) = &e.key { r = r.bearer_auth(k); }
    let r = r.send().map_err(|_| anyhow::anyhow!("{} unreachable", host(&e.base)))?;
    let status = r.status().as_u16();
    if !(200..300).contains(&status) {
        let body: serde_json::Value = r.json().unwrap_or_default();
        let why = body.pointer("/error/message").or_else(|| body.pointer("/detail")).and_then(|d| d.as_str()).map(|s| s.chars().take(160).collect::<String>())
            .unwrap_or_else(|| if status == 401 { "key rejected".into() } else { format!("HTTP {status}") });
        bail!("Own Engine: {why}");
    }
    let mut bytes = vec![];
    std::io::Read::read_to_end(&mut std::io::Read::take(r, MAX_BYTES), &mut bytes).context("Own Engine audio")?;
    ensure!(!cancel(), "speech interrupted");
    decode_wav(&bytes)
}

/// WAV (16-bit, 32-bit float or 24/32-bit int; stereo mixed down). Servers that stream WAV leave the lengths at their
/// maximum, which `hound` reads as "until the data ends".
pub fn decode_wav(bytes: &[u8]) -> Result<(Vec<f32>, u32)> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes)).context("Own Engine: not WAV audio")?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map_while(|s| s.ok()).collect(),
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map_while(|s| s.ok()).map(|s| s as f32 / scale).collect()
        }
    };
    let channels = spec.channels.max(1) as usize;
    let mono: Vec<f32> = raw.chunks(channels).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    ensure!(!mono.is_empty(), "Own Engine: no audio");
    Ok((mono, spec.sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::{HeaderMap, StatusCode}, routing::{get as route_get, post}, Json, Router};
    use serde_json::json;

    fn wav(rate: u32, n: usize) -> Vec<u8> {
        let mut out = std::io::Cursor::new(vec![]);
        let mut w = hound::WavWriter::new(&mut out, hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int }).unwrap();
        for i in 0..n { w.write_sample(((i as f32 * 0.05).sin() * 8000.0) as i16).unwrap(); }
        w.finalize().unwrap();
        out.into_inner()
    }

    #[test]
    fn voices_models_and_wav_in_the_shapes_servers_use() {
        assert_eq!(parse_voices(&json!({"voices":["af_bella","am_adam","af_bella"]})).iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["af_bella", "am_adam"]);
        assert_eq!(parse_voices(&json!([{"voice_id":"v1","name":"Nova"}]))[0].name, "Nova");
        assert_eq!(pick_model(&json!({"data":[{"id":"llama"},{"id":"kokoro"}]})).as_deref(), Some("kokoro"));
        assert_eq!(pick_model(&json!({"data":[{"id":"only"}]})).as_deref(), Some("only"));
        assert_eq!(standard()[0].name, "Alloy");
        let (s, rate) = decode_wav(&wav(24_000, 480)).unwrap();
        assert_eq!((s.len(), rate), (480, 24_000));
        assert!(decode_wav(b"not audio").is_err());
        assert!(!format!("{:?}", Endpoint { base: "b".into(), key: Some("sk-secret".into()), model: "m".into() }).contains("sk-secret"));
    }

    #[test]
    fn connect_lists_voices_and_speaks_against_a_fake_server() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = Router::new()
            .route("/v1/models", route_get(|h: HeaderMap| async move {
                if h.get("authorization").is_some_and(|a| a != "Bearer good") { return Err(StatusCode::UNAUTHORIZED); }
                Ok(Json(json!({"data":[{"id":"kokoro"}]})))
            }))
            .route("/v1/audio/voices", route_get(|| async { Json(json!({"voices":["af_heart","am_adam"]})) }))
            .route("/v1/audio/speech", post(|Json(b): Json<serde_json::Value>| async move {
                assert_eq!((b["model"].as_str(), b["voice"].as_str(), b["response_format"].as_str()), (Some("kokoro"), Some("af_heart"), Some("wav")));
                ([("content-type", "audio/wav")], wav(24_000, 2400))
            }));
        let base = rt.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
            format!("http://{a}/v1")
        });
        let (e, voices) = connect(&format!("{base}/"), None).unwrap();
        assert_eq!((e.base.as_str(), e.model.as_str(), e.key.clone()), (base.as_str(), "kokoro", None));
        assert_eq!(voices.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["af_heart", "am_adam"]);
        assert_eq!(connect(&base, Some("bad")).unwrap_err().to_string(), "Own Engine: key rejected");
        let (s, rate) = speak(&e, "af_heart", "Hello.", &|| false).unwrap();
        assert_eq!((s.len(), rate), (2400, 24_000));
        assert!(connect("http://127.0.0.1:9/v1", None).unwrap_err().to_string().contains("unreachable"));
    }
}
