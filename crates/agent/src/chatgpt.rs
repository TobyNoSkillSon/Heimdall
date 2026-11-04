//! [providers] Sign in with ChatGPT: OpenAI's token sharing for open-source and locally hosted apps
//! (developers.openai.com/siwc/token-sharing-open-source, read 23 Oct 2026). The agent's requests then run on the user's
//! ChatGPT plan through the public Responses API (`https://api.openai.com/v1`), the one route OpenAI documents for
//! this; ChatGPT's `backend-api` (the Codex CLI's endpoint) is not used, and the Codex CLI's own login
//! (`~/.codex/auth.json`) is never read: its token lacks `chatgpt.tokens.use.direct` and belongs to OpenAI's client.
//!
//! Flow: the first sign-in registers Heimdall as an agent (`client_id=dynamic_agent_client`, `agent_name_hint`, a
//! persisted opaque `ext_agent_host_id`); the browser opens the authorize URL; OpenAI redirects to
//! `http://127.0.0.1:1455/auth/callback` with `code`, `state` and the issued `client_id`; the code is exchanged (PKCE,
//! no secret) for a one-hour access token and a rotating 30-day refresh token. Pi gets the access token per request
//! from `heimdall --provider-token openai` (console::command_token), and Pi's OpenAI client recognises a non-`sk-`
//! token on api.openai.com as this flow (it drops the request fields the route rejects).
use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use serde_json::Value;
use sha2::Digest as _;
use std::path::Path;

pub const AUTH_BASE: &str = "https://auth.openai.com";
pub const API_BASE: &str = "https://api.openai.com/v1";
const REGISTER_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "Heimdall";
pub const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// OpenAI's documented loopback port (shared with the Codex CLI's own sign-in, which holds it only while it runs).
pub const PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
/// How long the browser has to come back.
pub const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// Where the user sees and changes the plan's usage for apps.
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// The model a new connection selects when the account lists it (OpenAI's own example model for this flow).
pub const DEFAULT_MODEL: &str = "gpt-6.1-sol";
/// The sign-in's one test request runs on the smallest model: GPT-6 Luna is the Codex model on every plan, Free and Go
/// included (learn.chatgpt.com/docs/pricing, read 24 Oct 2026); GPT-6.1 Sol needs Plus.
pub const TEST_MODEL: &str = "gpt-6-luna";

/// One sign-in step or failure in `heimdall.log` (stderr), never a code, token or verifier.
fn note(text: &str) {
    eprintln!("providers: ChatGPT sign-in: {text}");
}

/// The sign-in ended for good (revoked, expired or reused refresh token, or a 401 on the model list): only a new
/// sign-in recovers.
#[derive(Debug)]
pub struct SignedOut;
impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPT: sign in again")
    }
}
impl std::error::Error for SignedOut {}

/// A refusal that retrying cannot fix (plan usage not granted, Heimdall's app entry removed): named on the provider,
/// never retried in a loop; a new sign-in recovers.
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

/// An OAuth error's code in any of the shapes OpenAI's issuers use: `{"error":"x"}`, `{"error":{"code":"x"}}` or
/// `{"code":"x"}` (as the Codex CLI reads them), lowercased.
pub fn oauth_code(v: &Value) -> String {
    v["error"].as_str().or_else(|| v["error"]["code"].as_str()).or_else(|| v["code"].as_str()).unwrap_or_default().to_ascii_lowercase()
}
const TERMINAL_REFRESH: [&str; 6] = ["invalid_grant", "invalid_refresh_token", "token_expired", "refresh_token_expired", "refresh_token_invalidated", "refresh_token_reused"];

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("heimdall/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client")
}

fn random() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// This Mac's opaque agent-host id (`urn:uuid:…`), chosen once and kept in `dir` (not secret, not identifying).
pub fn host_id(dir: &Path) -> Result<String> {
    let path = dir.join("chatgpt-agent-host");
    if let Some(id) = std::fs::read_to_string(&path).ok().map(|s| s.trim().to_owned()).filter(|s| s.starts_with("urn:uuid:")) {
        return Ok(id);
    }
    crate::runtime::private_dir(dir)?;
    let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    crate::runtime::atomic_write(&path, id.as_bytes())?;
    Ok(id)
}

/// A pending browser sign-in: its URL to open, and the listener the browser returns to.
pub struct Login {
    pub url: String,
    listener: tokio::net::TcpListener,
    redirect: String,
    verifier: String,
    state: String,
    client_id: Option<String>,
    auth_base: String,
}

/// Starts a sign-in: binds the loopback listener first (OpenAI: "start the listener before opening the browser"),
/// then builds the authorize URL. `client_id` is the id an earlier sign-in was issued (reauthorization), else Heimdall
/// registers. `port` 0 picks a free one (tests).
pub async fn begin(auth_base: &str, host_id: &str, client_id: Option<&str>, port: u16) -> Result<Login> {
    // A sign-in cancelled a moment ago lets go of the port within a quarter second.
    let mut tries = 0;
    let bound = loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port != 0 && tries < 6 => { tries += 1; tokio::time::sleep(std::time::Duration::from_millis(250)).await; }
            other => break other,
        }
    };
    let listener = bound.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!("Port {port} is busy: finish or cancel the Codex or ChatGPT sign-in open elsewhere, then try again")
        } else {
            anyhow::anyhow!("Sign-in listener failed: {e}")
        }
    })?;
    let redirect = format!("http://127.0.0.1:{}{CALLBACK_PATH}", listener.local_addr()?.port());
    let verifier = random();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = random();
    let mut url = url::Url::parse(&format!("{auth_base}/api/accounts/authorize"))?;
    {
        let mut q = url.query_pairs_mut();
        match client_id {
            Some(id) => { q.append_pair("client_id", id); }
            None => { q.append_pair("client_id", REGISTER_CLIENT).append_pair("agent_name_hint", AGENT_NAME); }
        }
        q.append_pair("ext_agent_host_id", host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect)
            .append_pair("resource", API_BASE)
            .append_pair("scope", SCOPE)
            .append_pair("state", &state)
            .append_pair("nonce", &random())
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
    }
    Ok(Login { url: url.into(), listener, redirect, verifier, state, client_id: client_id.map(str::to_owned), auth_base: auth_base.into() })
}

/// What the browser brought back.
#[derive(Debug, PartialEq)]
enum Callback {
    Code { code: String, client_id: Option<String> },
    /// The browser came back with `error=<code>` (`access_denied`: the user said no).
    Denied(String),
    /// Not this sign-in's callback (wrong path or state): answered and ignored.
    Ignored,
}

fn parse_callback(target: &str, state: &str) -> Callback {
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else { return Callback::Ignored };
    if url.path() != CALLBACK_PATH { return Callback::Ignored; }
    let q: std::collections::BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    // State first: a callback for another attempt is not this one's answer, whatever it says.
    if q.get("state").map(String::as_str) != Some(state) { return Callback::Ignored; }
    if let Some(e) = q.get("error") { return Callback::Denied(e.clone()); }
    match q.get("code") {
        Some(code) if !code.is_empty() => Callback::Code { code: code.clone(), client_id: q.get("client_id").filter(|c| !c.is_empty()).cloned() },
        _ => Callback::Ignored,
    }
}

async fn answer(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    use tokio::io::AsyncWriteExt as _;
    let reply = format!("HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The request line's target ("/auth/callback?code=…"), read within a few seconds.
async fn read_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::with_capacity(2048);
    let read = async {
        let mut chunk = [0u8; 1024];
        while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 16 * 1024 {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 { return None; }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), read).await.ok()??;
    let line = String::from_utf8_lossy(&buf);
    let mut parts = line.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then(|| parts.next().map(str::to_owned))?
}

/// A finished sign-in: the session to keep, and the plan the grant names ("free", "plus", …) when it says.
#[derive(Debug)]
pub struct SignIn {
    pub session: crate::console::Session,
    pub plan: Option<String>,
}

/// Waits for the browser's callback (until `cancelled()` or `timeout`), then exchanges the code. Each connection is
/// read on its own task, so a browser's idle spare connection never delays the real callback or a Cancel; the
/// listener is closed before the exchange, so port 1455 is free again at once. The browser's tab waits for the
/// exchange and then says how it ended (24 Oct: it said "connected" before the exchange, which then failed).
pub async fn finish(login: Login, cancelled: impl Fn() -> bool, timeout: std::time::Duration) -> Result<SignIn> {
    let Login { listener, redirect, verifier, state, client_id: saved, auth_base, .. } = login;
    let deadline = tokio::time::Instant::now() + timeout;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(tokio::net::TcpStream, Option<String>)>();
    let (code, issued, mut tab) = loop {
        if cancelled() { note("cancelled before the browser came back"); bail!("Sign-in cancelled"); }
        if tokio::time::Instant::now() >= deadline { note("the browser never came back"); bail!("The sign-in timed out: the browser never came back to Heimdall"); }
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((mut stream, _)) = accepted {
                    let tx = tx.clone();
                    tokio::spawn(async move { let target = read_target(&mut stream).await; let _ = tx.send((stream, target)); });
                }
            }
            got = rx.recv() => {
                let Some((mut stream, target)) = got else { continue };
                let Some(target) = target else { answer(&mut stream, "400 Bad Request", "").await; continue };
                match parse_callback(&target, &state) {
                    Callback::Code { code, client_id } => {
                        note(&format!("the browser came back with a code ({})", if client_id.is_some() { "client id issued" } else { "no client id; using the saved one" }));
                        // On reauthorization OpenAI may omit the id; a different one is not this client's grant.
                        if let (Some(saved), Some(got)) = (&saved, &client_id) {
                            if saved != got {
                                let why = "ChatGPT answered for a different app";
                                note(why);
                                answer(&mut stream, "400 Bad Request", &page(false, why)).await;
                                bail!(why);
                            }
                        }
                        let Some(id) = client_id.or_else(|| saved.clone()) else {
                            let why = "ChatGPT did not issue Heimdall a client id";
                            note(why);
                            answer(&mut stream, "400 Bad Request", &page(false, why)).await;
                            bail!(why);
                        };
                        break (code, id, stream);
                    }
                    Callback::Denied(code) => {
                        let detail = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()
                            .and_then(|u| u.query_pairs().find(|(k, _)| k == "error_description").map(|(_, v)| description(&serde_json::json!({"error_description": v}))))
                            .unwrap_or_default();
                        let what = if detail.is_empty() { safe_code(&code) } else { format!("{}: {detail}", safe_code(&code)) };
                        note(&format!("the browser came back with error {what}"));
                        let declined = code == "access_denied";
                        answer(&mut stream, "200 OK", &page(false, &if declined { "the sign-in was declined".to_string() } else { format!("ChatGPT refused it ({what})") })).await;
                        if declined { bail!("Sign-in was declined"); }
                        // Heimdall's app entry was removed in ChatGPT: the next sign-in registers it again.
                        if matches!(code.as_str(), "invalid_client" | "unauthorized_client") && saved.is_some() { return Err(ClientGone.into()); }
                        bail!("ChatGPT refused the sign-in ({what})");
                    }
                    Callback::Ignored => answer(&mut stream, "404 Not Found", "").await,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
        }
    };
    drop(listener);
    if cancelled() { note("cancelled before the code was exchanged"); answer(&mut tab, "200 OK", &page(false, "the sign-in was cancelled in Heimdall")).await; bail!("Sign-in cancelled"); }
    let done = exchange(&auth_base, &issued, &code, &verifier, &redirect, saved.is_some()).await;
    // Cancel during the exchange: the hub keeps nothing, so the tab must not say signed in.
    if cancelled() { note("cancelled during the exchange; nothing kept"); answer(&mut tab, "200 OK", &page(false, "the sign-in was cancelled in Heimdall")).await; bail!("Sign-in cancelled"); }
    match &done {
        Ok(signed) => {
            note(&format!("signed in{}", signed.plan.as_deref().map(|p| format!(" (ChatGPT plan: {})", plan_name(p))).unwrap_or_default()));
            answer(&mut tab, "200 OK", &page(true, "")).await;
        }
        Err(e) => answer(&mut tab, "200 OK", &page(false, &e.to_string())).await,
    }
    done
}

/// The code for tokens (PKCE, no secret), each answer named in the log by status and OAuth code only.
async fn exchange(auth_base: &str, issued: &str, code: &str, verifier: &str, redirect: &str, reauthorizing: bool) -> Result<SignIn> {
    note("exchanging the code at OpenAI");
    let started = std::time::Instant::now();
    let r = client()
        .post(format!("{auth_base}/api/accounts/oauth/token"))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[("grant_type", "authorization_code"), ("client_id", issued), ("code", code),
            ("code_verifier", verifier), ("redirect_uri", redirect), ("resource", API_BASE)])
        .send()
        .await;
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_timeout() { "timed out" } else if e.is_connect() { "connection failed" } else { "request failed" };
            let mut cause = String::new();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source { cause.push_str(&format!(": {s}")); source = s.source(); }
            note(&format!("OpenAI's token endpoint unreachable after {} ms ({kind}){cause}", started.elapsed().as_millis()));
            bail!("Heimdall couldn't reach OpenAI to finish the sign-in · check the connection and try again");
        }
    };
    let status = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let code = safe_code(&oauth_code(&v));
    note(&format!("OpenAI's token endpoint answered {status} in {} ms{}", started.elapsed().as_millis(), if code.is_empty() { String::new() } else { format!(" ({code})") }));
    if !(200..300).contains(&status) {
        if matches!(code.as_str(), "invalid_client" | "unauthorized_client") && reauthorizing { return Err(ClientGone.into()); }
        let detail = description(&v);
        let what = match (code.as_str(), detail.is_empty()) {
            ("", true) => format!("HTTP {status}, {}", if v.is_null() { "not a JSON answer" } else { "no error code" }),
            ("", false) => format!("HTTP {status}: {detail}"),
            (c, true) => format!("{c}, HTTP {status}"),
            (c, false) => format!("{c}: {detail}"),
        };
        if !detail.is_empty() { note(&format!("OpenAI said: {detail}")); }
        bail!("OpenAI refused the sign-in code ({what}) · try again");
    }
    if v.is_null() { note("the token answer is not JSON"); bail!("OpenAI's sign-in answer was unreadable · try again"); }
    note(&format!("the token answer has {}", fields(&v)));
    let session = session_from(&v, issued, auth_base).inspect_err(|e| note(&format!("the token answer was not kept: {e}")))?;
    Ok(SignIn { session, plan: plan_of(&v) })
}

/// An OAuth error code fit for the log and the menu (OpenAI's are short snake_case words).
fn safe_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.' || *c == '-').take(64).collect()
}

/// OpenAI's `error_description` (or `error.message`), on one line and bounded.
fn description(v: &Value) -> String {
    let d = v["error_description"].as_str().or_else(|| v["error"]["message"].as_str()).or_else(|| v["message"].as_str()).unwrap_or_default();
    let flat = d.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(160).collect()
}

/// Which fields a token answer carries (names only, never values), for the log.
fn fields(v: &Value) -> String {
    v.as_object().map(|o| o.keys().map(String::as_str).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty()).unwrap_or_else(|| "no fields".into())
}

/// The page the browser's tab shows once the sign-in has ended.
fn page(ok: bool, why: &str) -> String {
    let text = if ok {
        "ChatGPT is signed in to Heimdall. You can close this tab.".to_string()
    } else {
        let why: String = why.chars().map(|c| match c { '<' => "&lt;".into(), '>' => "&gt;".into(), '&' => "&amp;".into(), '"' => "&quot;".into(), c => c.to_string() }).collect();
        format!("ChatGPT was not connected: {why}")
    };
    format!("<!doctype html><meta charset=utf-8><title>Heimdall</title><body style=\"font:15px -apple-system;background:#1c1c1e;color:#f2f2f7;display:grid;place-items:center;height:90vh;text-align:center;padding:0 2em\">{text}")
}

/// A JWT's payload claims, unverified (`None`: not a JWT). Read only for the grant's scope, expiry and plan name.
fn claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

/// The ChatGPT plan a token answer names ("free", "plus", …), from the ID or access token's OpenAI claims. Best effort:
/// OpenAI calls these claims opaque, so a missing or odd one is simply not named.
fn plan_of(v: &Value) -> Option<String> {
    ["id_token", "access_token"].iter().filter_map(|k| v[*k].as_str()).filter_map(claims)
        .find_map(|c| c["https://api.openai.com/auth"]["chatgpt_plan_type"].as_str().map(str::to_ascii_lowercase))
        .filter(|p| !p.is_empty() && p.len() <= 32 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// A plan id as the user reads it: "free" → "Free", "plus" → "Plus".
pub fn plan_name(plan: &str) -> String {
    let mut c = plan.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The plan refusal in the user's words, naming his plan when the sign-in said which it is.
pub fn not_eligible(plan: Option<&str>) -> String {
    match plan.filter(|p| !matches!(*p, "plus" | "pro")) {
        Some(p) => format!("ChatGPT: your {} plan can't share its usage with apps · OpenAI allows Plus and Pro", plan_name(p)),
        None => "ChatGPT: this plan can't share its usage with apps · OpenAI allows Plus and Pro".into(),
    }
}

/// The model the sign-in's test request runs on: GPT-6 Luna when listed, else the smallest the account lists (a Luna,
/// mini or nano), else the default, else the first; Luna when the list is unknown.
pub fn test_model(listed: &[Value]) -> String {
    let ids: Vec<&str> = listed.iter().filter_map(|m| m["id"].as_str()).collect();
    let pick = |want: &dyn Fn(&str) -> bool| ids.iter().copied().find(|id| want(id));
    pick(&|id| id == TEST_MODEL)
        .or_else(|| pick(&|id| ["luna", "mini", "nano"].iter().any(|s| id.contains(*s))))
        .or_else(|| pick(&|id| id == DEFAULT_MODEL))
        .or(ids.first().copied())
        .unwrap_or(TEST_MODEL).to_owned()
}

/// The client id OpenAI issued Heimdall is no longer valid (the app was removed in ChatGPT's settings): forget it, and
/// the next sign-in registers Heimdall again.
#[derive(Debug)]
pub struct ClientGone;
impl std::fmt::Display for ClientGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPT no longer knows Heimdall · try again")
    }
}
impl std::error::Error for ClientGone {}

/// The issued client id, kept beside the host id (not secret), so Disconnect and Connect reuse one app entry in ChatGPT.
pub fn saved_client(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("chatgpt-client-id")).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty() && s.len() < 256)
}
pub fn save_client(dir: &Path, id: Option<&str>) -> Result<()> {
    let path = dir.join("chatgpt-client-id");
    match id {
        Some(id) => { crate::runtime::private_dir(dir)?; crate::runtime::atomic_write(&path, id.as_bytes()) }
        None => match std::fs::remove_file(&path) { Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()), _ => Ok(()) },
    }
}

/// The token answer as a kept session. A grant without `chatgpt.tokens.use.direct` is a sign-in without plan usage
/// (consent declined): it is refused, as OpenAI asks, so no request runs without it. The granted scope is read from the
/// answer's `scope` (a string, or a list), else from the access token's own `scope` claim; the expiry from
/// `expires_in` (a number or numeric string), else the access token's `exp`.
fn session_from(v: &Value, client_id: &str, auth_base: &str) -> Result<crate::console::Session> {
    let s = |k: &str| v[k].as_str().filter(|t| !t.trim().is_empty()).map(str::to_owned);
    let access = s("access_token").context("OpenAI's sign-in answer has no access token")?;
    let refresh = s("refresh_token").context("OpenAI's sign-in answer has no refresh token")?;
    let token = claims(&access).unwrap_or(Value::Null);
    let words = |x: &Value| -> Option<Vec<String>> {
        match x {
            Value::String(t) => Some(t.split_whitespace().map(str::to_owned).collect()),
            Value::Array(a) => Some(a.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect()),
            _ => None,
        }
    };
    let scope = words(&v["scope"]).or_else(|| words(&token["scope"]).or_else(|| words(&token["scp"]))).unwrap_or_default();
    let positive = |e: u64| (e > 0).then_some(e);
    let expires_in = v["expires_in"].as_u64().and_then(positive)
        .or_else(|| v["expires_in"].as_f64().filter(|e| *e >= 1.0).map(|e| e as u64))
        .or_else(|| v["expires_in"].as_str().and_then(|e| e.trim().parse::<u64>().ok()).and_then(positive))
        .or_else(|| token["exp"].as_u64().map(|exp| exp.saturating_sub(crate::now_ms() / 1000)).and_then(positive))
        // A year at most: a nonsense lifetime must not overflow the expiry.
        .map(|e| e.min(365 * 24 * 3600))
        .context("OpenAI's sign-in answer has no expiry")?;
    if !scope.iter().any(|x| x == DIRECT_SCOPE) {
        note(&format!("the grant lacks {DIRECT_SCOPE} (granted: {})", if scope.is_empty() { "nothing named".into() } else { scope.join(" ") }));
        return Err(Refused("ChatGPT: plan usage wasn't allowed for Heimdall · sign in again and allow it".into()).into());
    }
    Ok(crate::console::Session {
        access, refresh, expires_ms: crate::now_ms() + expires_in * 1000, org_id: None, server: auth_base.into(),
        base_url: None, headers: Default::default(), models: vec![], client_id: Some(client_id.into()),
    })
}

/// Rotates the tokens (the refresh token is single-use). OpenAI's terminal refresh errors mean signed out.
pub async fn refresh(s: &crate::console::Session, timeout: std::time::Duration) -> Result<crate::console::Session> {
    let client_id = s.client_id.as_deref().context("ChatGPT sign-in has no client id")?;
    let r = client()
        .post(format!("{}/api/accounts/oauth/token", s.server))
        .timeout(timeout)
        .form(&[("grant_type", "refresh_token"), ("client_id", client_id), ("refresh_token", s.refresh.as_str()), ("resource", API_BASE)])
        .send()
        .await
        .context("ChatGPT unreachable")?;
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    let code = oauth_code(&v);
    if TERMINAL_REFRESH.contains(&code.as_str()) || status == 401 {
        return Err(SignedOut.into());
    }
    if matches!(code.as_str(), "invalid_client" | "unauthorized_client") { return Err(SignedOut.into()); }
    ensure!((200..300).contains(&status), "ChatGPT refresh failed ({status})");
    session_from(&v, client_id, &s.server)
}

/// The account's models as listing rows, from OpenAI's `{"models":[{slug, display_name, visibility, …}]}` (only
/// `visibility == "list"`); an OpenAI-style `data` list is read too. Each row keeps `id`, `display_name` and what the
/// account states about the model (audit 27 Oct, F2/F11; measured that day: gpt-6.1-sol and gpt-6.1-terra 500,000 /
/// 872,000, gpt-6-luna 272,000 / 872,000, all text and image): `context_window`, `max_context_window`,
/// `input_modalities`, `default_reasoning_level`, and `supported_reasoning_levels` as the effort names.
pub fn listing(body: &Value) -> Vec<Value> {
    let from_models = body["models"].as_array().into_iter().flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|v| v == "list"))
        .filter_map(|m| {
            let id = m["slug"].as_str().or_else(|| m["id"].as_str())?;
            let mut row = serde_json::json!({"id": id, "display_name": m["display_name"].as_str().unwrap_or(id)});
            for k in ["context_window", "max_context_window", "input_modalities", "default_reasoning_level"] {
                if !m[k].is_null() { row[k] = m[k].clone(); }
            }
            let efforts: Vec<&str> = m["supported_reasoning_levels"].as_array().into_iter().flatten()
                .filter_map(|l| l["effort"].as_str().or_else(|| l.as_str())).collect();
            if !efforts.is_empty() { row["supported_reasoning_levels"] = serde_json::json!(efforts); }
            Some(row)
        });
    let from_data = body["data"].as_array().into_iter().flatten().filter(|m| m["id"].is_string()).cloned();
    let mut seen = std::collections::BTreeSet::new();
    from_models.chain(from_data).filter(|m| seen.insert(m["id"].as_str().unwrap_or_default().to_owned())).collect()
}

/// The model list for the signed-in account. 401: signed out.
pub async fn models(api_base: &str, access: &str) -> Result<Vec<Value>> {
    let r = client().get(format!("{api_base}/models")).bearer_auth(access).send().await.context("ChatGPT unreachable")?;
    match r.status().as_u16() {
        401 => return Err(SignedOut.into()),
        s if !(200..300).contains(&s) => {
            let body = r.text().await.unwrap_or_default();
            bail!("{}", explain(&body).unwrap_or_else(|| format!("ChatGPT model list refused ({s})")));
        }
        _ => {}
    }
    let body: Value = r.json().await.context("ChatGPT model list unreadable")?;
    let listed = listing(&body);
    ensure!(!listed.is_empty(), "ChatGPT lists no models for this account");
    Ok(listed)
}

/// One tiny request that tells whether the plan can run Heimdall's requests at all (OpenAI: the model list does not
/// confirm entitlement; a completed response does). `Ok` once `response.completed` arrives.
pub async fn probe(api_base: &str, access: &str, model: &str) -> Result<()> {
    // `input` is a list of messages: the token-sharing route answers a plain string with 400 "Input must be a list"
    // (seen 27 Oct 00:35 and in emergency mode's recovery probe at 01:57, which could then never pass).
    let body = serde_json::json!({"model": model, "instructions": "Reply with OK.", "input": [{"role": "user", "content": [{"type": "input_text", "text": "ping"}]}], "stream": true, "store": false, "reasoning": {"effort": "low"}});
    let r = client().post(format!("{api_base}/responses")).bearer_auth(access).timeout(std::time::Duration::from_secs(60)).json(&body).send().await.context("ChatGPT unreachable")?;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    let code = serde_json::from_str::<Value>(&text).map(|v| safe_code(&oauth_code(&v))).unwrap_or_default();
    note(&format!("test request on {model}: HTTP {status}{}{}", if code.is_empty() { String::new() } else { format!(" ({code})") },
        if (200..300).contains(&status) { if text.contains("response.completed") { ", completed" } else { ", not completed" } } else { "" }));
    if status == 401 && !text.contains("subscription_sharing") { return Err(SignedOut.into()); }
    if !(200..300).contains(&status) {
        bail!("{}", explain(&format!("{status} {text}")).unwrap_or_else(|| format!("ChatGPT refused the request ({status})")));
    }
    if text.contains("response.completed") { return Ok(()); }
    bail!("{}", explain(&text).unwrap_or_else(|| "ChatGPT did not finish the test request".into()))
}

/// OpenAI's documented plan-usage errors in the user's words (`None`: not one of them).
pub fn explain(raw: &str) -> Option<String> {
    let says = |code: &str| raw.contains(code);
    // Pi keeps only the message of an SSE `error` event (26 Oct 23:33), so the words count as much as the code.
    Some(if says("subscription_sharing_usage_limit_exceeded") || says("Subscription Sharing usage limit") {
        "ChatGPT: usage limit reached · see ChatGPT ▸ Settings ▸ Usage".into()
    } else if says("subscription_sharing_user_not_eligible") {
        not_eligible(None)
    } else if says("subscription_sharing_usage_unavailable") || says("subscription_sharing_user_unavailable") {
        "ChatGPT: usage check unavailable · try again shortly".into()
    } else if says("subscription_sharing_invalid_user") {
        "ChatGPT: account check failed · try again shortly".into()
    } else if says("subscription_sharing_unsupported_capability") {
        "ChatGPT: this model or feature isn't available on the plan".into()
    } else if says("subscription_sharing_route_not_supported") || says("chatpass_v2_") {
        "ChatGPT: request not allowed for this sign-in".into()
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::{Form, State}, http::{HeaderMap, StatusCode}, routing::{get, post}, Json, Router};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake { forms: Arc<Mutex<Vec<BTreeMap<String, String>>>>, scope: Arc<Mutex<String>> }

    async fn fake() -> (String, Fake, tokio::task::JoinHandle<()>) {
        let f = Fake::default();
        *f.scope.lock().unwrap() = SCOPE.into();
        let app = Router::new()
            .route("/api/accounts/oauth/token", post(|State(f): State<Fake>, Form(b): Form<BTreeMap<String, String>>| async move {
                f.forms.lock().unwrap().push(b.clone());
                let scope = f.scope.lock().unwrap().clone();
                match (b["grant_type"].as_str(), b.get("refresh_token").map(String::as_str)) {
                    ("authorization_code", _) if b["code"] == "code-1" && b["client_id"] == "issued-1" =>
                        (StatusCode::OK, Json(json!({"access_token":"access-1","refresh_token":"refresh-1","id_token":"x.y.z","expires_in":3600,"scope":scope,"token_type":"Bearer"}))),
                    ("authorization_code", _) if b["code"] == "code-expired" =>
                        (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant","error_description":"The authorization code has expired."}))),
                    ("authorization_code", _) if b["code"] == "code-jwt" => {
                        let part = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
                        let plan = json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "free"}});
                        let access = format!("{}.{}.sig", part(json!({"alg":"none"})), part(json!({"scope": SCOPE, "exp": crate::now_ms() / 1000 + 3600})));
                        let id = format!("{}.{}.sig", part(json!({"alg":"none"})), part(plan));
                        (StatusCode::OK, Json(json!({"access_token": access, "refresh_token":"refresh-1","id_token": id,"expires_in":3600.0,"token_type":"Bearer"})))
                    }
                    ("refresh_token", Some("refresh-1")) => (StatusCode::OK, Json(json!({"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"scope":scope}))),
                    ("refresh_token", _) => (StatusCode::BAD_REQUEST, Json(json!({"error":"refresh_token_reused"}))),
                    _ => (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"}))),
                }
            }))
            .route("/v1/models", get(|h: HeaderMap| async move {
                if h["authorization"] != "Bearer access-1" { return Err(StatusCode::UNAUTHORIZED); }
                Ok(Json(json!({"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list"},{"slug":"internal","display_name":"x","visibility":"hide"}]})))
            }))
            .route("/v1/responses", post(|h: HeaderMap, Json(b): Json<Value>| async move {
                assert_eq!((b["store"].as_bool(), b["stream"].as_bool()), (Some(false), Some(true)), "the route requires store false, stream true");
                assert!(b.get("max_output_tokens").is_none() && b.get("temperature").is_none(), "fields the route rejects");
                if !b["input"].is_array() { return (StatusCode::BAD_REQUEST, json!({"detail":"Input must be a list"}).to_string()); }
                match h["authorization"].to_str().unwrap() {
                    "Bearer access-1" => (StatusCode::OK, "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n".to_string()),
                    "Bearer limited" => (StatusCode::TOO_MANY_REQUESTS, json!({"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}).to_string()),
                    "Bearer free" => (StatusCode::FORBIDDEN, json!({"error":{"code":"subscription_sharing_user_not_eligible"}}).to_string()),
                    _ => (StatusCode::UNAUTHORIZED, json!({"detail":"Unauthorized"}).to_string()),
                }
            }))
            .with_state(f.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (base, f, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    /// The browser's side: follows the redirect a consenting user would land on.
    async fn browser(login_url: &str, query: impl Fn(&str) -> String) -> reqwest::StatusCode {
        let url = url::Url::parse(login_url).unwrap();
        let q: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        let back = format!("{}?{}", q["redirect_uri"], query(&q["state"]));
        reqwest::get(back).await.unwrap().status()
    }

    #[test]
    fn host_id_is_chosen_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = host_id(dir.path()).unwrap();
        assert!(a.starts_with("urn:uuid:") && a.len() == 45, "{a}");
        assert_eq!(host_id(dir.path()).unwrap(), a);
    }

    #[test]
    fn callbacks_are_checked_state_first() {
        assert_eq!(parse_callback("/auth/callback?code=c&state=s&client_id=i", "s"), Callback::Code { code: "c".into(), client_id: Some("i".into()) });
        assert_eq!(parse_callback("/auth/callback?code=c&state=other", "s"), Callback::Ignored, "another attempt's callback");
        assert_eq!(parse_callback("/auth/callback?error=access_denied&state=s", "s"), Callback::Denied("access_denied".into()));
        assert_eq!(parse_callback("/favicon.ico", "s"), Callback::Ignored);
        assert_eq!(parse_callback("/callback?code=c&state=s", "s"), Callback::Ignored, "/callback is not /auth/callback");
    }

    #[tokio::test]
    async fn registration_sign_in_exchange_refresh_and_models_against_a_fake_openai() {
        let (base, f, task) = fake().await;
        let login = begin(&base, "urn:uuid:00000000-0000-4000-8000-000000000000", None, 0).await.unwrap();
        let q: BTreeMap<String, String> = url::Url::parse(&login.url).unwrap().query_pairs().into_owned().collect();
        assert!(login.url.starts_with(&format!("{base}/api/accounts/authorize?")));
        assert_eq!((q["client_id"].as_str(), q["agent_name_hint"].as_str(), q["resource"].as_str(), q["code_challenge_method"].as_str()), (REGISTER_CLIENT, "Heimdall", API_BASE, "S256"));
        assert_eq!(q["scope"], SCOPE);
        assert!(q["redirect_uri"].starts_with("http://127.0.0.1:") && q["redirect_uri"].ends_with("/auth/callback"), "a 127.0.0.1 loopback, not localhost");
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        // A stale attempt's callback does not end the sign-in.
        assert_eq!(browser(&url, |_| "code=code-1&state=stale".into()).await, 404);
        assert_eq!(browser(&url, |s| format!("code=code-1&state={s}&client_id=issued-1")).await, 200);
        let s = pending.await.unwrap().unwrap().session;
        assert_eq!((s.access.as_str(), s.refresh.as_str(), s.client_id.as_deref()), ("access-1", "refresh-1", Some("issued-1")));
        let form = f.forms.lock().unwrap()[0].clone();
        assert_eq!((form["client_id"].as_str(), form["resource"].as_str(), form["redirect_uri"].as_str()), ("issued-1", API_BASE, q["redirect_uri"].as_str()), "the issued id, not dynamic_agent_client");
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(form["code_verifier"].as_bytes())), q["code_challenge"], "PKCE");
        assert!(!format!("{s:?}").contains("access-1"), "no token in debug output");
        // Models: only listed ones; signed out on 401.
        let listed = models(&format!("{base}/v1"), "access-1").await.unwrap();
        assert_eq!(listed, vec![json!({"id":"gpt-6.1-sol","display_name":"GPT-6.1 Sol"})]);
        assert!(models(&format!("{base}/v1"), "revoked").await.unwrap_err().downcast_ref::<SignedOut>().is_some());
        // Refresh rotates; a spent refresh token is signed out.
        let next = refresh(&s, std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!((next.access.as_str(), next.refresh.as_str(), next.client_id.as_deref()), ("access-2", "refresh-2", Some("issued-1")));
        assert!(refresh(&next, std::time::Duration::from_secs(5)).await.unwrap_err().downcast_ref::<SignedOut>().is_some());
        // A grant without plan usage is refused.
        *f.scope.lock().unwrap() = "openid profile email offline_access".into();
        assert_eq!(refresh(&s, std::time::Duration::from_secs(5)).await.unwrap_err().to_string(), "ChatGPT: plan usage wasn't allowed for Heimdall · sign in again and allow it");
        task.abort();
    }

    #[tokio::test]
    async fn declined_cancelled_and_reauthorization() {
        let (base, _f, task) = fake().await;
        let login = begin(&base, "urn:uuid:1", None, 0).await.unwrap();
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        assert_eq!(browser(&url, |s| format!("error=access_denied&state={s}")).await, 200);
        assert_eq!(pending.await.unwrap().unwrap_err().to_string(), "Sign-in was declined");
        let login = begin(&base, "urn:uuid:1", None, 0).await.unwrap();
        assert_eq!(finish(login, || true, std::time::Duration::from_secs(10)).await.unwrap_err().to_string(), "Sign-in cancelled");
        // Reauthorization sends the saved id and accepts a callback without one.
        let login = begin(&base, "urn:uuid:1", Some("issued-1"), 0).await.unwrap();
        let q: BTreeMap<String, String> = url::Url::parse(&login.url).unwrap().query_pairs().into_owned().collect();
        assert!(q["client_id"] == "issued-1" && !q.contains_key("agent_name_hint"));
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        assert_eq!(browser(&url, |s| format!("code=code-1&state={s}")).await, 200);
        assert_eq!(pending.await.unwrap().unwrap().session.client_id.as_deref(), Some("issued-1"));
        // Another app's grant is refused.
        let login = begin(&base, "urn:uuid:1", Some("issued-1"), 0).await.unwrap();
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        browser(&url, |s| format!("code=code-1&state={s}&client_id=someone-else")).await;
        assert_eq!(pending.await.unwrap().unwrap_err().to_string(), "ChatGPT answered for a different app");
        task.abort();
    }

    /// 24 Oct: the tab said "connected" before the exchange, which then failed with no word in the log. Now the tab
    /// waits for the exchange and names its outcome, and the error says what OpenAI answered.
    #[tokio::test]
    async fn the_tab_waits_for_the_exchange_and_a_refused_code_is_named() {
        let (base, _f, task) = fake().await;
        let tab = |url: String, code: &'static str| async move {
            let q: BTreeMap<String, String> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
            reqwest::get(format!("{}?code={code}&state={}&client_id=issued-1", q["redirect_uri"], q["state"])).await.unwrap().text().await.unwrap()
        };
        // An expired code: the tab and the error both say what OpenAI answered.
        let login = begin(&base, "urn:uuid:1", None, 0).await.unwrap();
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        let page = tab(url, "code-expired").await;
        let e = pending.await.unwrap().err().unwrap().to_string();
        assert_eq!(e, "OpenAI refused the sign-in code (invalid_grant: The authorization code has expired.) · try again");
        assert!(page.contains("ChatGPT was not connected: OpenAI refused the sign-in code (invalid_grant") && !page.contains("signed in"), "{page}");
        // A good code: the tab says signed in only after the exchange.
        let login = begin(&base, "urn:uuid:1", None, 0).await.unwrap();
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        assert!(tab(url, "code-1").await.contains("ChatGPT is signed in to Heimdall"));
        assert_eq!(pending.await.unwrap().unwrap().plan, None, "x.y.z names no plan");
        // An answer that names its scope only in the access token, with a float expiry, is kept, and the plan is read.
        let login = begin(&base, "urn:uuid:1", None, 0).await.unwrap();
        let url = login.url.clone();
        let pending = tokio::spawn(finish(login, || false, std::time::Duration::from_secs(10)));
        tab(url, "code-jwt").await;
        let signed = pending.await.unwrap().unwrap();
        assert_eq!(signed.plan.as_deref(), Some("free"));
        assert!(signed.session.expires_ms > crate::now_ms() + 3_500_000);
        assert_eq!(not_eligible(signed.plan.as_deref()), "ChatGPT: your Free plan can't share its usage with apps · OpenAI allows Plus and Pro");
        task.abort();
    }

    /// The token answer's scope and expiry in each shape: string or list scope, the access token's own claim when the
    /// answer has none; integer, float or string `expires_in`, the token's `exp` when it is missing or zero.
    #[test]
    fn token_answers_are_read_in_every_shape() {
        let part = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        let jwt = |claims: Value| format!("{}.{}.sig", part(json!({"alg":"none"})), part(claims));
        let kept = |extra: Value| {
            let mut v = json!({"access_token": "opaque", "refresh_token": "r"});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            session_from(&v, "issued-1", "http://x").map(|s| (s.expires_ms - crate::now_ms()) / 1000)
        };
        let close = |got: Result<u64>, want: u64| { let got = got.unwrap(); assert!(got + 2 >= want && got <= want, "{got} vs {want}"); };
        close(kept(json!({"scope": SCOPE, "expires_in": 3600})), 3600);
        close(kept(json!({"scope": SCOPE, "expires_in": 3600.0})), 3600);
        close(kept(json!({"scope": SCOPE, "expires_in": "1800"})), 1800);
        close(kept(json!({"scope": SCOPE.split(' ').collect::<Vec<_>>(), "expires_in": 60})), 60);
        // No scope and no expiry in the answer: the access token's claims.
        let access = jwt(json!({"scope": SCOPE, "exp": crate::now_ms() / 1000 + 900}));
        close(kept(json!({"access_token": access, "expires_in": 0})), 900);
        let access = jwt(json!({"scp": ["openid", DIRECT_SCOPE]}));
        close(kept(json!({"access_token": access, "expires_in": 3600})), 3600);
        close(kept(json!({"scope": SCOPE, "expires_in": u64::MAX})), 365 * 24 * 3600);
        assert_eq!(kept(json!({"scope": SCOPE})).unwrap_err().to_string(), "OpenAI's sign-in answer has no expiry");
        assert!(kept(json!({"scope": "openid", "expires_in": 3600})).unwrap_err().downcast_ref::<Refused>().is_some());
        assert_eq!(not_eligible(Some("plus")), not_eligible(None), "a Plus or Pro plan is not named as the reason");
    }

    #[test]
    fn the_test_request_runs_on_the_smallest_listed_model() {
        let rows = |ids: &[&str]| ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>();
        assert_eq!(test_model(&rows(&["gpt-6.1-sol", "gpt-6-luna"])), "gpt-6-luna");
        assert_eq!(test_model(&rows(&["gpt-6.1-sol", "gpt-6.2-luna"])), "gpt-6.2-luna");
        assert_eq!(test_model(&rows(&["gpt-x", "gpt-6.1-sol"])), "gpt-6.1-sol");
        assert_eq!(test_model(&rows(&["gpt-x"])), "gpt-x");
        assert_eq!(test_model(&[]), TEST_MODEL, "the list unknown: Luna, the model on every plan");
    }

    #[tokio::test]
    async fn probe_names_the_plan_errors() {
        let (base, _f, task) = fake().await;
        let api = format!("{base}/v1");
        probe(&api, "access-1", DEFAULT_MODEL).await.unwrap();
        assert_eq!(probe(&api, "limited", DEFAULT_MODEL).await.unwrap_err().to_string(), "ChatGPT: usage limit reached · see ChatGPT ▸ Settings ▸ Usage");
        assert_eq!(probe(&api, "free", DEFAULT_MODEL).await.unwrap_err().to_string(), "ChatGPT: this plan can't share its usage with apps · OpenAI allows Plus and Pro");
        assert!(probe(&api, "revoked", DEFAULT_MODEL).await.unwrap_err().downcast_ref::<SignedOut>().is_some());
        task.abort();
    }

    /// Pi's key command path: a ChatGPT session near expiry is refreshed through OpenAI's token endpoint (not
    /// OpenCode's) under the shared lock, kept, and its token printed; the listing and errors read OpenAI's shapes.
    #[tokio::test]
    async fn the_key_command_refreshes_a_chatgpt_session_and_the_list_and_errors_read_openais_shapes() {
        // The lab credential store is shared: the other tests that write it hold this lock too.
        let _lab = crate::provider::LAB_ITEMS_TEST.lock().await;
        let (base, f, task) = fake().await;
        let p = crate::provider::provider(crate::provider::CHATGPT).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        let dir = tempfile::tempdir().unwrap();
        let s = crate::console::Session { access: "access-0".into(), refresh: "refresh-1".into(), expires_ms: crate::now_ms() + 60_000, org_id: None,
            server: base.clone(), base_url: None, headers: Default::default(), models: vec![], client_id: Some("issued-1".into()) };
        crate::provider::save_console_session(p, &s).unwrap();
        let got = crate::console::access_token(p, dir.path(), crate::console::Budget::COMMAND).await.unwrap();
        assert_eq!((got.access.as_str(), got.refresh.as_str()), ("access-2", "refresh-2"));
        assert_eq!(crate::provider::console_session(p).unwrap().refresh, "refresh-2", "the rotated token is kept before it is used");
        assert_eq!(f.forms.lock().unwrap().last().unwrap()["client_id"], "issued-1");
        // The next refresh token is reused upstream: signed out, named for the menu.
        crate::provider::save_console_session(p, &crate::console::Session { expires_ms: crate::now_ms(), refresh: "spent".into(), ..s.clone() }).unwrap();
        let e = crate::console::access_token(p, dir.path(), crate::console::Budget::COMMAND).await.unwrap_err();
        assert!(crate::console::is_signed_out(&e) && e.to_string() == "ChatGPT: sign in again", "{e}");
        // The provider's own list call reads OpenAI's `models` shape.
        let listed = crate::provider::live_models(p, Some("access-1"), None, Some(&format!("{base}/v1/models"))).await.unwrap();
        let rows = crate::provider::merge_catalog(p, &listed, None, None);
        assert_eq!((rows.len(), rows[0].id.as_str(), rows[0].name.as_str(), rows[0].api), (1, "gpt-6.1-sol", "GPT-6.1 Sol", crate::provider::Api::OpenaiResponses));
        assert!(rows[0].unsupported().is_none() && rows[0].pi_definition().unwrap()["baseUrl"] == API_BASE);
        assert_eq!(crate::provider::explain_error(p.id, "OpenAI API error (429): {\"code\":\"subscription_sharing_usage_limit_exceeded\"}").as_deref(), Some("ChatGPT: usage limit reached · see ChatGPT ▸ Settings ▸ Usage"));
        assert_eq!(crate::provider::explain_error(p.id, "The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.").as_deref(), Some("ChatGPT: usage limit reached · see ChatGPT ▸ Settings ▸ Usage"));
        assert_eq!(crate::provider::explain_error(p.id, "403 subscription_sharing_user_not_eligible").as_deref(), Some("ChatGPT: this plan can't share its usage with apps · OpenAI allows Plus and Pro"));
        assert_eq!(crate::provider::explain_error(p.id, "401 Unauthorized").as_deref(), Some("ChatGPT: sign in again"));
        crate::provider::forget_key(p).unwrap();
        task.abort();
    }

    #[test]
    fn a_busy_port_says_what_to_do() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = held.local_addr().unwrap().port();
            let e = begin("http://127.0.0.1:9", "urn:uuid:1", None, port).await.err().unwrap().to_string();
            assert!(e.starts_with(&format!("Port {port} is busy")), "{e}");
        });
    }
}
