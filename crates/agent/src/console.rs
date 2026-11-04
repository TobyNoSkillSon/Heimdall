//! OpenCode Console sign-in: the device login OpenCode's own apps use (packages/core/src/plugin/provider/opencode.ts
//! in github.com/anomalyco/opencode). It is undocumented and uses OpenCode's public CLI client id, so it can break
//! when OpenCode changes it; the pasted API key stays as the fallback.
//!
//! Flow: POST `{server}/auth/device/code` → open `verification_uri_complete` in the browser (the user signs in and
//! approves the code) → poll `{server}/auth/device/token` every `interval` → access + refresh tokens. Requests carry
//! `Authorization: Bearer <access>` (and `x-org-id` when org-scoped); the access token is refreshed with the same
//! token URL. `GET {server}/api/v2/config` says which providers the account has and their endpoints.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const SERVER: &str = "https://opencode.ai/console";
pub const CLIENT_ID: &str = "opencode-cli";
/// Lab binaries hand Pi the access token through this variable; the installed app uses `token_command`.
pub const TOKEN_ENV: &str = "HEIMDALL_OPENCODE_TOKEN";

/// A signed-in Console account. Kept only in the Keychain (installed app) or memory (lab).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub access: String,
    pub refresh: String,
    pub expires_ms: u64,
    #[serde(default)]
    pub org_id: Option<String>,
    pub server: String,
    /// The account's OpenCode Go endpoint and headers from `/api/v2/config`, when it names them.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The models the Console config lists for the provider: only these take its endpoint (as in OpenCode).
    #[serde(default)]
    pub models: Vec<String>,
    /// [providers] Sign in with ChatGPT: the client id OpenAI issued Heimdall at registration (refresh needs it).
    #[serde(default)]
    pub client_id: Option<String>,
}
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("expires_ms", &self.expires_ms).field("org_id", &self.org_id).field("server", &self.server).field("base_url", &self.base_url).finish_non_exhaustive()
    }
}
impl Session {
    /// Headers every OpenCode request needs besides the bearer token (never the token itself).
    pub fn request_headers(&self) -> BTreeMap<String, String> {
        let mut h = self.headers.clone();
        h.retain(|k, _| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("x-api-key"));
        if let Some(org) = &self.org_id {
            h.insert("x-org-id".into(), org.clone());
        }
        h
    }
}

/// The Console session ended (revoked or expired refresh token, or a Console 401): only a new sign-in recovers.
#[derive(Debug)]
pub struct SignedOut;
impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OpenCode Go: sign in again")
    }
}
impl std::error::Error for SignedOut {}

/// [providers] A sign-in that ended for good, OpenCode's or ChatGPT's.
pub fn is_signed_out(e: &anyhow::Error) -> bool {
    e.downcast_ref::<SignedOut>().is_some() || e.downcast_ref::<crate::chatgpt::SignedOut>().is_some()
}

/// The Console's 403: the organization requires its SSO sign-in again; retrying does not help.
#[derive(Debug)]
pub struct SsoRequired;
impl std::fmt::Display for SsoRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OpenCode Go: sign in with your organization's SSO in the Console")
    }
}
impl std::error::Error for SsoRequired {}

/// How long one caller may wait for the refresh lock and for the Console. Pi kills its key command after 10 s.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub lock: std::time::Duration,
    pub http: std::time::Duration,
    /// A token this close to expiry is refreshed first. The app's margin is the wider one, so it (with its 20 s budget)
    /// rotates the token before Pi's command must: a rotation whose answer outlived the command's 5 s would be lost, and
    /// ChatGPT signs out a reused refresh token.
    pub margin_ms: u64,
}
impl Budget {
    pub const APP: Budget = Budget { lock: std::time::Duration::from_secs(15), http: std::time::Duration::from_secs(20), margin_ms: 10 * 60_000 };
    pub const COMMAND: Budget = Budget { lock: std::time::Duration::from_secs(3), http: std::time::Duration::from_secs(5), margin_ms: 5 * 60_000 };
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("heimdall/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client")
}

fn now_ms() -> u64 {
    crate::now_ms()
}

/// What the user approves in the browser.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    /// Absolute: the Console returns it relative to itself.
    pub url: String,
    pub interval_s: u64,
    pub expires_s: u64,
}

pub async fn start(server: &str) -> Result<DeviceCode> {
    let r = client()
        .post(format!("{server}/auth/device/code"))
        .json(&serde_json::json!({"client_id": CLIENT_ID, "supports_org_scope": true}))
        .send()
        .await
        .context("OpenCode unreachable")?;
    ensure!(r.status().is_success(), "OpenCode sign-in unavailable ({})", r.status().as_u16());
    let v: Value = r.json().await.context("OpenCode sign-in unreadable")?;
    let s = |k: &str| v[k].as_str().map(str::to_owned).with_context(|| format!("OpenCode sign-in missing {k}"));
    let base = url::Url::parse(&format!("{server}/"))?;
    let url = base.join(&s("verification_uri_complete")?).context("OpenCode sign-in link invalid")?;
    ensure!(matches!(url.scheme(), "https" | "http"), "OpenCode sign-in link invalid");
    Ok(DeviceCode {
        device_code: s("device_code")?,
        user_code: s("user_code")?,
        url: url.to_string(),
        interval_s: v["interval"].as_u64().unwrap_or(5).clamp(1, 60),
        expires_s: v["expires_in"].as_u64().unwrap_or(600).min(3600),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum Poll {
    Pending,
    SlowDown,
    Token(Token),
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub org_id: Option<String>,
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").field("expires_in", &self.expires_in).field("org_id", &self.org_id).finish_non_exhaustive()
    }
}

pub async fn poll_once(server: &str, device_code: &str) -> Result<Poll> {
    let r = client()
        .post(format!("{server}/auth/device/token"))
        .json(&serde_json::json!({"grant_type": "urn:ietf:params:oauth:grant-type:device_code", "device_code": device_code, "client_id": CLIENT_ID}))
        .send()
        .await
        .context("OpenCode unreachable")?;
    let v: Value = r.json().await.context("OpenCode sign-in unreadable")?;
    if v.get("access_token").is_some() {
        return Ok(Poll::Token(serde_json::from_value(v).context("OpenCode sign-in unreadable")?));
    }
    match v["error"].as_str() {
        Some("authorization_pending") => Ok(Poll::Pending),
        Some("slow_down") => Ok(Poll::SlowDown),
        Some("expired_token") => bail!("The sign-in code expired"),
        Some("access_denied") => bail!("Sign-in was declined"),
        Some(other) => bail!("Sign-in failed: {other}"),
        None => bail!("OpenCode sign-in unreadable"),
    }
}

/// Polls until approved, `cancelled()` or the code expires. Honors `slow_down` (+5 s), as OpenCode does.
pub async fn wait_for_approval(server: &str, code: &DeviceCode, cancelled: impl Fn() -> bool) -> Result<Token> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(code.expires_s);
    let mut wait = code.interval_s;
    loop {
        for _ in 0..wait * 4 {
            if cancelled() { bail!("Sign-in cancelled"); }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        ensure!(std::time::Instant::now() < deadline, "The sign-in code expired");
        match poll_once(server, &code.device_code).await? {
            Poll::Token(t) => return Ok(t),
            Poll::Pending => {}
            Poll::SlowDown => wait += 5,
        }
    }
}

/// The approved token as a session: the account's organization (the token's, or the first by name, as OpenCode
/// picks it) and its OpenCode Go endpoint.
pub async fn finish(server: &str, provider_id: &str, t: Token) -> Result<Session> {
    let orgs: Value = get(server, "/api/orgs", &t.access_token, None).await?.unwrap_or(Value::Array(vec![]));
    let mut list: Vec<(String, String)> = orgs.as_array().into_iter().flatten()
        .filter_map(|o| Some((o["name"].as_str()?.to_owned(), o["id"].as_str()?.to_owned()))).collect();
    list.sort();
    let org_id = match &t.org_id {
        Some(id) => { ensure!(list.iter().any(|(_, i)| i == id), "OpenCode organization not found"); Some(id.clone()) }
        None => list.first().map(|(_, id)| id.clone()),
    };
    let mut s = Session { access: t.access_token, refresh: t.refresh_token, expires_ms: now_ms() + t.expires_in * 1000, org_id, server: server.into(), base_url: None, headers: BTreeMap::new(), models: vec![], client_id: None };
    if let Some(config) = fetch_config(&s).await? { apply_config(&mut s, provider_id, &config); }
    Ok(s)
}

async fn get(server: &str, path: &str, access: &str, org: Option<&str>) -> Result<Option<Value>> {
    let mut r = client().get(format!("{server}{path}")).bearer_auth(access).header("accept", "application/json");
    if let Some(org) = org { r = r.header("x-org-id", org); }
    let r = r.send().await.context("OpenCode unreachable")?;
    match r.status().as_u16() {
        404 => Ok(None),
        401 => Err(SignedOut.into()),
        403 => Err(SsoRequired.into()),
        s if (200..300).contains(&s) => Ok(Some(r.json().await.context("OpenCode answer unreadable")?)),
        s => bail!("OpenCode answered {s}"),
    }
}

/// The account's Console configuration; `None` when the Console has none for it (404). A 401 means signed out.
pub async fn fetch_config(s: &Session) -> Result<Option<Value>> {
    get(&s.server, "/api/v2/config", &s.access, s.org_id.as_deref()).await
}

/// The provider's endpoint and extra headers from the Console config (`providers.<id>` or the entry whose
/// `canonical` is the id). Credentials in the config are ignored.
pub fn apply_config(s: &mut Session, provider_id: &str, config: &Value) {
    let providers = config["providers"].as_object();
    let item = providers.and_then(|p| p.get(provider_id).or_else(|| p.values().find(|v| v["canonical"] == provider_id)));
    let Some(item) = item else { return };
    s.base_url = item["settings"]["baseURL"].as_str().filter(|u| u.starts_with("https://") || u.starts_with("http://127.0.0.1")).map(|u| u.trim_end_matches('/').to_owned());
    s.headers = item["headers"].as_object().into_iter().flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
        .filter(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("x-api-key"))
        .collect();
    s.models = item["models"].as_object().into_iter().flatten()
        .flat_map(|(id, m)| [Some(id.clone()), m["modelID"].as_str().map(str::to_owned)])
        .flatten()
        .collect();
}

/// Rotates the tokens. A 400 `invalid_grant` means the session ended.
pub async fn refresh(s: &Session) -> Result<Session> {
    refresh_within(s, Budget::APP.http).await
}

async fn refresh_within(s: &Session, timeout: std::time::Duration) -> Result<Session> {
    let r = client()
        .post(format!("{}/auth/device/token", s.server))
        .timeout(timeout)
        .json(&serde_json::json!({"grant_type": "refresh_token", "refresh_token": s.refresh, "client_id": CLIENT_ID}))
        .send()
        .await
        .context("OpenCode unreachable")?;
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if status == 400 && v["error"] == "invalid_grant" || status == 401 { return Err(SignedOut.into()); }
    ensure!((200..300).contains(&status), "OpenCode refresh failed ({status})");
    let t: Token = serde_json::from_value(v).context("OpenCode refresh unreadable")?;
    Ok(Session { access: t.access_token, refresh: t.refresh_token, expires_ms: now_ms() + t.expires_in * 1000, org_id: t.org_id.or_else(|| s.org_id.clone()), ..s.clone() })
}

/// A current access token, refreshing and saving the session first when it is near expiry. Every process that
/// refreshes (the app, Pi's token command) holds one lock, because a refresh rotates the refresh token. Within
/// `budget`: when the lock or the Console is slow and the current token has not expired yet, it is used as is; a
/// rotated token is saved before it is returned, so a caller killed afterwards loses nothing.
pub async fn access_token(p: &crate::provider::ProviderInfo, lock_dir: &Path, budget: Budget) -> Result<Session> {
    crate::runtime::private_dir(lock_dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(lock_dir.join(format!("{}-console.lock", p.id)))?;
    let until = std::time::Instant::now() + budget.lock;
    let mut locked = true;
    while fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        if std::time::Instant::now() >= until { locked = false; break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let session = crate::provider::console_session(p).ok_or(crate::provider::KeyMissing(p.name))?;
    let usable = session.expires_ms > now_ms() + 5_000;
    if session.expires_ms > now_ms() + budget.margin_ms { return Ok(session); }
    if !locked {
        ensure!(usable, "{} sign-in busy", p.name);
        return Ok(session);
    }
    let refreshed = if p.id == crate::provider::CHATGPT { crate::chatgpt::refresh(&session, budget.http).await } else { refresh_within(&session, budget.http).await };
    match refreshed {
        Ok(next) => {
            // A sign-in that replaced (or a Disconnect that removed) this session meanwhile wins over its refresh.
            match crate::provider::console_session(p) {
                Some(now) if now.refresh == session.refresh => crate::provider::save_console_session(p, &next)?,
                Some(now) => return Ok(now),
                None => bail!("{}", crate::provider::KeyMissing(p.name)),
            }
            Ok(next)
        }
        Err(e) if !is_signed_out(&e) && usable => Ok(session),
        Err(e) => Err(e),
    }
}

/// Holds the refresh lock (blocking, up to 15 s) while `f` replaces or removes a sign-in, so no refresh in flight saves
/// over it.
pub fn with_lock<T>(p: &crate::provider::ProviderInfo, lock_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::runtime::private_dir(lock_dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(lock_dir.join(format!("{}-console.lock", p.id)))?;
    let until = std::time::Instant::now() + Budget::APP.lock;
    while fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        ensure!(std::time::Instant::now() < until, "{} sign-in busy", p.name);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    f()
}

/// Pi's per-request key command for the installed app: prints a current access token (no secret in the command).
pub fn token_command(p: &crate::provider::ProviderInfo, data: &Path) -> Result<String> {
    let exe = std::env::current_exe()?;
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    Ok(format!("!{} --provider-token {} --data {}", quote(&exe.to_string_lossy()), p.id, quote(&data.join("agent").to_string_lossy())))
}

/// The value Pi's key command prints for a provider: the current Console access token (refreshed under the lock), else
/// the saved API key. Only the installed app's Keychain holds either; a lab process finds none and fails.
pub fn command_token(p: &crate::provider::ProviderInfo, data: &Path) -> Result<String> {
    if crate::provider::console_session(p).is_none() {
        // A sign-in being saved is briefly absent (the Keychain write deletes, then adds): look once more.
        if let Ok(key) = crate::provider::api_key(p) { return Ok(key); }
        std::thread::sleep(std::time::Duration::from_millis(300));
        if crate::provider::console_session(p).is_none() { return crate::provider::api_key(p); }
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    Ok(rt.block_on(access_token(p, data, Budget::COMMAND))?.access)
}

/// `heimdall --provider-token <id> --data <dir>`: Pi's key command. Prints only the token or key; errors go to stderr.
pub fn token_mode() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == "--provider-token")?;
    let run = || -> Result<String> {
        let p = crate::provider::provider(args.get(i + 1).context("provider id")?).context("unknown provider")?;
        let d = args.iter().position(|a| a == "--data").and_then(|j| args.get(j + 1)).context("--data")?;
        command_token(p, Path::new(d))
    };
    Some(match run() {
        Ok(token) => { println!("{token}"); 0 }
        Err(e) => { eprintln!("{e:#}"); 1 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::{HeaderMap, StatusCode}, routing::{get, post}, Json, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake { polls: Arc<Mutex<u32>>, refreshed: Arc<Mutex<u32>>, seen: Arc<Mutex<Vec<String>>> }

    async fn fake() -> (String, Fake, tokio::task::JoinHandle<()>) {
        let state = Fake::default();
        let app = Router::new()
            .route("/console/auth/device/code", post(|Json(b): Json<Value>| async move {
                assert_eq!((b["client_id"].as_str(), b["supports_org_scope"].as_bool()), (Some(CLIENT_ID), Some(true)));
                Json(json!({"device_code":"dev-1","user_code":"ABCD-EFGH","verification_uri":"/console/device","verification_uri_complete":"/console/device?user_code=ABCD-EFGH&client_id=opencode-cli","expires_in":600,"interval":1}))
            }))
            .route("/console/auth/device/token", post(|State(f): State<Fake>, Json(b): Json<Value>| async move {
                match b["grant_type"].as_str() {
                    Some("refresh_token") if b["refresh_token"] == "refresh-1" => { *f.refreshed.lock().unwrap() += 1; (StatusCode::OK, Json(json!({"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600}))) }
                    Some("refresh_token") => (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"}))),
                    _ => {
                        let mut n = f.polls.lock().unwrap(); *n += 1;
                        match *n { 1 => (StatusCode::BAD_REQUEST, Json(json!({"error":"authorization_pending"}))),
                            _ => (StatusCode::OK, Json(json!({"access_token":"access-1","refresh_token":"refresh-1","expires_in":60,"org_id":null}))) }
                    }
                }
            }))
            .route("/console/api/orgs", get(|State(f): State<Fake>, h: HeaderMap| async move {
                f.seen.lock().unwrap().push(h["authorization"].to_str().unwrap().into());
                Json(json!([{"id":"org-b","name":"Zeta"},{"id":"org-a","name":"Personal"}]))
            }))
            .route("/console/api/v2/config", get(|h: HeaderMap| async move {
                assert_eq!(h["x-org-id"], "org-a");
                if h["authorization"] == "Bearer sso" { return Err(StatusCode::FORBIDDEN); }
                if h["authorization"] != "Bearer access-1" && h["authorization"] != "Bearer access-2" { return Err(StatusCode::UNAUTHORIZED); }
                Ok(Json(json!({"providers":{"go":{"canonical":"opencode-go","settings":{"baseURL":"https://opencode.ai/zen/go/v1/","apiKey":"secret"},"headers":{"x-opencode-client":"console","authorization":"Bearer leaked"},"models":{"kimi-k3":{},"fast":{"modelID":"glm-5.3"}}}}})))
            }))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/console", listener.local_addr().unwrap());
        (server, state, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    #[tokio::test]
    async fn device_login_poll_org_config_and_refresh_against_a_fake_console() {
        let (server, f, task) = fake().await;
        let code = start(&server).await.unwrap();
        assert_eq!(code.user_code, "ABCD-EFGH");
        assert!(code.url.ends_with("/console/device?user_code=ABCD-EFGH&client_id=opencode-cli"), "{}", code.url);
        assert!(code.url.starts_with(&server[..server.len() - "/console".len()]));
        let token = wait_for_approval(&server, &code, || false).await.unwrap();
        assert_eq!(*f.polls.lock().unwrap(), 2, "pending once, then approved");
        let s = finish(&server, "opencode-go", token).await.unwrap();
        assert_eq!(f.seen.lock().unwrap()[0], "Bearer access-1");
        // No org in the token: the first organization by name, as OpenCode picks it.
        assert_eq!(s.org_id.as_deref(), Some("org-a"));
        // The Go endpoint and headers come from the Console config; credentials in it are ignored.
        assert_eq!(s.base_url.as_deref(), Some("https://opencode.ai/zen/go/v1"));
        assert_eq!(s.request_headers(), BTreeMap::from([("x-opencode-client".into(), "console".into()), ("x-org-id".into(), "org-a".into())]));
        assert!(!format!("{s:?}").contains("access-1") && !format!("{s:?}").contains("refresh-1"));
        let next = refresh(&s).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_str(), next.org_id.as_deref()), ("access-2", "refresh-2", Some("org-a")));
        assert!(refresh(&next).await.unwrap_err().downcast_ref::<SignedOut>().is_some(), "a spent refresh token signs out");
        let mut stale = next.clone(); stale.access = "revoked".into();
        assert!(fetch_config(&stale).await.unwrap_err().downcast_ref::<SignedOut>().is_some());
        stale.access = "sso".into();
        assert!(fetch_config(&stale).await.unwrap_err().downcast_ref::<SsoRequired>().is_some(), "a Console 403 is SSO, not unreachable");
        assert_eq!(s.models, ["fast", "glm-5.3", "kimi-k3"], "the models the config lists take its endpoint");
        // Cancelling stops polling.
        let code = start(&server).await.unwrap();
        assert_eq!(wait_for_approval(&server, &code, || true).await.unwrap_err().to_string(), "Sign-in cancelled");
        task.abort();
    }

    #[tokio::test]
    async fn access_token_refreshes_near_expiry_once_under_the_lock() {
        let _lab = crate::provider::LAB_ITEMS_TEST.lock().await;
        let (server, f, task) = fake().await;
        let p = crate::provider::provider(crate::provider::OPENCODE_GO).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let s = Session { access: "access-1".into(), refresh: "refresh-1".into(), expires_ms: now_ms() + 60_000, org_id: Some("org-a".into()), server: server.clone(), base_url: None, headers: BTreeMap::new(), models: vec![], client_id: None };
        crate::provider::save_console_session(p, &s).unwrap();
        let (a, b) = tokio::join!(access_token(p, dir.path(), Budget::APP), access_token(p, dir.path(), Budget::COMMAND));
        assert_eq!((a.unwrap().access, b.unwrap().access), ("access-2".into(), "access-2".into()));
        assert_eq!(*f.refreshed.lock().unwrap(), 1, "the second caller reuses the rotated session");
        assert_eq!(access_token(p, dir.path(), Budget::COMMAND).await.unwrap().refresh, "refresh-2");
        // An unreachable Console with a token that has not expired: the token is used, not an error.
        let offline = Session { server: "http://127.0.0.1:9/console".into(), expires_ms: now_ms() + 60_000, ..s.clone() };
        crate::provider::save_console_session(p, &offline).unwrap();
        assert_eq!(access_token(p, dir.path(), Budget::COMMAND).await.unwrap().access, "access-1");
        // Expired and unreachable: an error that is not "signed out".
        crate::provider::save_console_session(p, &Session { expires_ms: now_ms(), ..offline.clone() }).unwrap();
        let e = access_token(p, dir.path(), Budget::COMMAND).await.unwrap_err();
        assert!(e.downcast_ref::<SignedOut>().is_none());
        // A held lock: the command gives up inside its budget and uses the unexpired token.
        crate::provider::save_console_session(p, &offline).unwrap();
        let held = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.path().join("opencode-go-console.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(access_token(p, dir.path(), Budget::COMMAND).await.unwrap().access, "access-1");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "inside Pi's 10 s");
        crate::provider::forget_console_session(p).unwrap();
        task.abort();
    }

    #[test]
    fn command_token_prints_the_saved_key_and_nothing_without_one() {
        let _lab = crate::provider::LAB_ITEMS_TEST.blocking_lock();
        let p = crate::provider::provider(crate::provider::ANTHROPIC).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        let dir = tempfile::tempdir().unwrap();
        // Lab items live in this process only, so a lab Pi's command child (a separate process) finds none: the test
        // covers the in-process lookup the installed app's child performs against the Keychain.
        crate::provider::forget_key(p).unwrap();
        assert!(command_token(p, dir.path()).is_err(), "no key, no output");
        crate::provider::save_key(p, "sk-ant-api03-fake-0123456789abcdef", None).unwrap();
        assert_eq!(command_token(p, dir.path()).unwrap(), "sk-ant-api03-fake-0123456789abcdef");
        crate::provider::forget_key(p).unwrap();
    }
    #[test]
    fn token_command_quotes_paths_and_carries_no_secret() {
        let p = crate::provider::provider(crate::provider::OPENCODE_GO).unwrap();
        let c = token_command(p, Path::new("/tmp/it's data")).unwrap();
        assert!(c.starts_with("!'") && c.contains(" --provider-token opencode-go --data '/tmp/it'\\''s data/agent'"), "{c}");
    }
}
