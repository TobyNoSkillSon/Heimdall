use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use url::Url;
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// The prefix of an error whose cause is only the caller's deadline (`call_deadline`): the connection is kept.
pub const TIMEOUT: &str = "T3_TIMEOUT";
pub const PROTOCOL: &str = "2025-06-18";
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub token: String,
    pub expires_ms: u64,
    pub environment: String,
    /// T3 rejected this credential (401: the user revoked it in Settings → Connections). Kept across restarts so neither
    /// renewal nor a relaunch pairs again; only the Connect T3 click replaces it.
    #[serde(default)]
    pub revoked: bool,
}
// Intentionally no Debug on credential/client/transport.
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credential>>;
    fn save(&self, credential: &Credential) -> Result<()>;
}
pub struct Keychain;
/// The Keychain service of the T3 pairing credential. The public edition has its own, so it can never read the
/// personal app's pairing on the same Mac; the personal name never changes.
pub const KEYCHAIN_SERVICE: &str = if cfg!(feature = "public") { "com.tobynoskillson.heimdall.t3" } else { "dev.heimdall.t3" };
impl CredentialStore for Keychain {
    fn load(&self) -> Result<Option<Credential>> {
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, "oauth") {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("T3 credential is invalid; reconnect T3")?,
            )),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => {
                anyhow::bail!("Keychain unavailable; unlock the login Keychain and reconnect T3")
            }
        }
    }
    fn save(&self, c: &Credential) -> Result<()> {
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            "oauth",
            &serde_json::to_vec(c)?,
        )
        .map_err(|_| anyhow::anyhow!("Keychain write failed; reconnect T3"))
    }
}
#[derive(Clone)]
struct Transport {
    origin: String,
    session: Option<String>,
    token: String,
    expires_ms: u64,
    catalog: Value,
}
pub struct Client {
    runtime: PathBuf,
    http: reqwest::Client,
    credentials: Arc<dyn CredentialStore>,
    transport: Mutex<Option<Transport>>,
    connect: Mutex<()>,
    counter: AtomicU64,
    auth_required: AtomicBool,
    /// T3's own CLI (`~/.t3/bin/t3`, written by T3 Code). With it Heimdall connects itself: see `pair`.
    cli: Option<PathBuf>,
    /// No automatic pairing before this time (ms) after a failed attempt.
    pair_after: AtomicU64,
    /// Whether the user has allowed Heimdall to connect to T3 Code (the setup wizard's Connect). While false the
    /// client never reads T3's runtime file, pairs or calls T3, and reports T3 as unavailable.
    consent: Arc<AtomicBool>,
    /// Heimdall's data folder: each `t3_thread_launch` records its first message's digest there first, so Heimdall's
    /// Claude permission hook answers that thread's prompts (`heimdall_emergency::approvals`). None: nothing recorded.
    launches: Option<PathBuf>,
}
/// After a failed automatic connection, wait this long before trying again (a click retries at once).
const PAIR_BACKOFF_MS: u64 = 10 * 60 * 1000;
/// Renew the 30-day OAuth session this close to its expiry.
const RENEW_MS: u64 = 3 * 24 * 60 * 60 * 1000;
impl Client {
    pub fn new(runtime: PathBuf, credentials: Arc<dyn CredentialStore>) -> Result<Self> {
        Ok(Self {
            runtime,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .build()?,
            credentials,
            transport: Mutex::new(None),
            connect: Mutex::new(()),
            counter: AtomicU64::new(1),
            auth_required: AtomicBool::new(false),
            cli: None,
            pair_after: AtomicU64::new(0),
            consent: Arc::new(AtomicBool::new(true)),
            launches: None,
        })
    }
    /// Record every launch's first message under this data folder (see `launches`).
    pub fn with_launch_record(mut self, data: PathBuf) -> Self {
        self.launches = Some(data);
        self
    }
    /// Connect through this T3 CLI without a browser approval (see `pair`).
    pub fn with_cli(mut self, cli: PathBuf) -> Self {
        self.cli = Some(cli);
        self
    }
    /// Connect only while `consent` holds (the public edition's saved Connect choice). Without it: always.
    pub fn with_consent(mut self, consent: Arc<AtomicBool>) -> Self {
        self.consent = consent;
        self
    }
    pub fn local(credentials: Arc<dyn CredentialStore>) -> Result<Self> {
        let home = std::env::var_os("HOME").context("home directory unavailable")?;
        Ok(Self::new(
            PathBuf::from(&home).join(".t3/userdata/server-runtime.json"),
            credentials,
        )?
        .with_cli(PathBuf::from(home).join(".t3/bin/t3")))
    }
    async fn discover(&self) -> Result<(String, String)> {
        let bytes = tokio::fs::read(&self.runtime)
            .await
            .context("T3_UNAVAILABLE: open T3 Code; discovery file is unavailable")?;
        ensure!(bytes.len() < 65536, "T3 discovery file exceeds bound");
        let runtime: Value =
            serde_json::from_slice(&bytes).context("T3 discovery file is invalid")?;
        let port = runtime["port"]
            .as_u64()
            .filter(|p| (1..=65535).contains(p))
            .context("T3 discovery port is invalid")?;
        let origin = format!("http://127.0.0.1:{port}");
        let env = self.public(&origin, "/.well-known/t3/environment").await?;
        let id = env["environmentId"]
            .as_str()
            .context("T3 discovery lacks environment identity")?;
        Ok((origin, id.into()))
    }
    async fn public(&self, origin: &str, path: &str) -> Result<Value> {
        let r = self
            .http
            .get(format!("{origin}{path}"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("T3_UNAVAILABLE: open T3 Code and retry"))?;
        ensure!(
            r.status().is_success(),
            "T3 discovery failed; reopen T3 Code"
        );
        let bytes = bounded(r).await?;
        Ok(serde_json::from_slice(&bytes).context("T3 discovery returned invalid JSON")?)
    }
    /// Non-secret credential generation used to deduplicate reconnect notices.
    pub fn connection_notice_key(&self) -> String {
        self.credentials
            .load()
            .ok()
            .flatten()
            .map(|c| format!("{}:{}", c.environment, c.expires_ms))
            .unwrap_or_else(|| "unconnected".into())
    }
    pub fn expiry_notice(&self) -> Result<bool> {
        Ok(self
            .credentials
            .load()?
            .is_none_or(|c| c.expires_ms <= crate::now_ms() + 3 * 24 * 60 * 60 * 1000))
    }
    pub async fn invalidate(&self) {
        *self.transport.lock().await = None;
    }
    pub async fn catalog(&self) -> Result<Value> {
        self.ensure_connected().await?;
        Ok(self
            .transport
            .lock()
            .await
            .as_ref()
            .unwrap()
            .catalog
            .clone())
    }
    /// Connect when T3 is running, pairing automatically when there is no usable credential or it is about to
    /// expire. `T3_UNAVAILABLE` means T3 Code is not running; `T3_AUTH_REQUIRED` that pairing failed too.
    pub async fn auto_connect(&self) -> Result<()> {
        self.ensure_connected().await
    }
    /// The stored credential was revoked by the user (a 401, persisted).
    fn revoked(&self) -> bool {
        self.credentials.load().ok().flatten().is_some_and(|c| c.revoked)
    }
    /// True when the stored credential cannot be used as is: missing, expiring, or for another T3 environment.
    fn needs_pairing(&self, environment: Option<&str>) -> bool {
        self.credentials.load().ok().flatten().is_none_or(|c| {
                c.expires_ms <= crate::now_ms() + RENEW_MS
                    || environment.is_some_and(|e| e != c.environment)
            })
    }
    /// Automatic pairing, rate limited after a failure. Returns whether a new credential was saved.
    /// A credential T3 rejected (401: revoked in Settings → Connections) is the user's decision, so it is never
    /// replaced automatically; his Connect T3 click pairs again.
    async fn try_pair(&self, origin: &str, environment: &str) -> bool {
        if self.cli.is_none()
            || self.auth_required.load(Ordering::Acquire)
            || self.revoked()
            || crate::now_ms() < self.pair_after.load(Ordering::Acquire)
        {
            return false;
        }
        match self.pair(origin, environment).await {
            Ok(()) => true,
            Err(e) => {
                eprintln!("T3 automatic connection failed: {e:#}");
                self.pair_after.store(crate::now_ms() + PAIR_BACKOFF_MS, Ordering::Release);
                false
            }
        }
    }
    /// Connect without a browser: T3's own CLI issues a five-minute one-time pairing code (the code T3's
    /// sign-in page accepts), Heimdall submits it with its OAuth request for full access, then exchanges the
    /// code for the 30-day bearer as usual. The session appears as "Heimdall" in T3 Settings → Connections and
    /// can be revoked there.
    async fn pair(&self, origin: &str, environment: &str) -> Result<()> {
        let cli = self.cli.as_ref().context("T3 CLI unavailable")?;
        // The data directory that owns this server's runtime file: <home>/userdata/server-runtime.json.
        let home = self.runtime.parent().and_then(|p| p.parent()).context("T3 data directory unknown")?;
        let mut command = tokio::process::Command::new(cli);
        command
            .args(["auth", "pairing", "create", "--json", "--ttl", "5m", "--label", "Heimdall connection"])
            .args(["--scope", "orchestration:read", "--scope", "orchestration:operate"])
            .args(["--scope", "settings:write", "--scope", "source-control:write"])
            .env("T3CODE_HOME", home)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(45), command.output())
            .await
            .context("T3 CLI timed out")?
            .context("T3 CLI did not start; open T3 Code once to reinstall it")?;
        ensure!(output.status.success(), "T3 CLI refused to issue a pairing code");
        let issued: Value = serde_json::from_slice(&output.stdout).context("T3 CLI returned no JSON")?;
        let code = ["credential", "token", "code"]
            .iter()
            .find_map(|k| issued[*k].as_str())
            .context("T3 CLI returned no pairing code")?
            .to_owned();
        let discovery = self.public(origin, "/.well-known/oauth-authorization-server").await?;
        ensure!(discovery["issuer"] == origin, "T3 OAuth issuer mismatch");
        let register = endpoint(&discovery, "registration_endpoint", origin)?;
        let token = endpoint(&discovery, "token_endpoint", origin)?;
        // The approval page posts here; the code is the page's "pairing code" answer.
        let decision = format!("{origin}/oauth/mcp/decision");
        let redirect = "http://127.0.0.1/callback".to_string();
        let registration = self.http.post(register)
            .json(&json!({"client_name":"Heimdall","redirect_uris":[redirect],"token_endpoint_auth_method":"none"}))
            .timeout(Duration::from_secs(10)).send().await
            .map_err(|_| anyhow::anyhow!("T3 OAuth registration failed"))?;
        ensure!(registration.status().is_success(), "T3 OAuth registration rejected");
        let reg: Value = serde_json::from_slice(&bounded(registration).await?)?;
        let client = reg["client_id"].as_str().context("T3 OAuth client ID missing")?.to_owned();
        let verifier = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let state = uuid::Uuid::new_v4().to_string();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let resource = format!("{origin}/mcp");
        let response = self.http.post(decision)
            .json(&json!({"authorization":{"response_type":"code","client_id":client,"redirect_uri":redirect,
                "code_challenge":challenge,"code_challenge_method":"S256","state":state,"resource":resource},
                "decision":{"_tag":"pairing-code","access":"full-access","code":code}}))
            .timeout(Duration::from_secs(10)).send().await
            .map_err(|_| anyhow::anyhow!("T3 approval request failed"))?;
        let status = response.status();
        let body: Value = serde_json::from_slice(&bounded(response).await?).unwrap_or(Value::Null);
        ensure!(status.is_success(), "T3 declined the pairing code: {}", bounded_text(body["message"].as_str().unwrap_or("no reason"), 300));
        let to = Url::parse(body["redirectTo"].as_str().context("T3 approval lacks a redirect")?)?;
        let q = to.query_pairs().collect::<std::collections::HashMap<_, _>>();
        ensure!(!q.contains_key("error"), "T3 declined the sign-in");
        ensure!(q.get("state").is_some_and(|v| v == &state) && q.get("iss").is_some_and(|v| v == origin), "T3 approval state/issuer mismatch");
        let code = q.get("code").context("T3 approval lacks a code")?.to_string();
        self.exchange(&token, &code, &client, &redirect, &verifier, &resource, environment).await?;
        self.invalidate().await;
        self.auth_required.store(false, Ordering::Release);
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn exchange(&self, token: &str, code: &str, client: &str, redirect: &str, verifier: &str, resource: &str, environment: &str) -> Result<()> {
        let response = self
            .http
            .post(token)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", client),
                ("redirect_uri", redirect),
                ("code_verifier", verifier),
                ("resource", resource),
            ])
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("T3 OAuth exchange failed; retry sign-in"))?;
        ensure!(
            response.status().is_success(),
            "T3 OAuth exchange rejected; click Connect T3 again"
        );
        let value: Value = serde_json::from_slice(&bounded(response).await?)?;
        ensure!(
            value["token_type"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case("bearer")),
            "T3 OAuth bearer type missing"
        );
        let seconds = value["expires_in"]
            .as_u64()
            .filter(|n| *n <= 31 * 24 * 60 * 60 && *n > 0)
            .context("T3 token expiry invalid")?;
        self.credentials.save(&Credential {
            token: value["access_token"]
                .as_str()
                .context("T3 access token missing")?
                .into(),
            expires_ms: crate::now_ms() + seconds * 1000,
            environment: environment.into(),
            revoked: false,
        })
    }
    async fn ensure_connected(&self) -> Result<()> {
        // The same answer as a Mac without T3 Code, so every caller already handles it quietly.
        ensure!(self.consent.load(Ordering::Acquire), "T3_UNAVAILABLE: connect T3 Code in Setup first");
        let _lock = self.connect.lock().await;
        let renew = match self.transport.lock().await.as_ref() {
            Some(t) => t.expires_ms <= crate::now_ms() + RENEW_MS,
            None => self.cli.is_some() && self.needs_pairing(None),
        };
        if self.cli.is_some() && renew && crate::now_ms() >= self.pair_after.load(Ordering::Acquire) {
            // T3 must be running to pair; a closed T3 reports T3_UNAVAILABLE, not a sign-in problem.
            let (origin, environment) = self.discover().await?;
            if self.needs_pairing(Some(&environment)) {
                self.try_pair(&origin, &environment).await;
            }
        }
        ensure!(
            !self.auth_required.load(Ordering::Acquire),
            "T3_AUTH_REQUIRED: Connect T3 · click"
        );
        if let Some(t) = self.transport.lock().await.as_ref() {
            ensure!(
                t.expires_ms > crate::now_ms(),
                "T3_AUTH_REQUIRED: Connect T3 · click"
            );
            return Ok(());
        }
        let credential = self
            .credentials
            .load()?
            .context("T3_AUTH_REQUIRED: Connect T3 · click")?;
        ensure!(
            credential.expires_ms > crate::now_ms() && !credential.revoked,
            "T3_AUTH_REQUIRED: Connect T3 · click"
        );
        if self.transport.lock().await.is_some() {
            return Ok(());
        }
        let (origin, environment) = self.discover().await?;
        ensure!(
            credential.environment == environment,
            "T3_AUTH_REQUIRED: environment changed; Connect T3 · click"
        );
        let mut t = Transport {
            origin,
            session: None,
            token: credential.token,
            expires_ms: credential.expires_ms,
            catalog: Value::Null,
        };
        let (init,session)=self.rpc(&t,"initialize",json!({"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"Heimdall","version":env!("CARGO_PKG_VERSION")}}),Duration::from_secs(10)).await?;
        ensure!(
            init["protocolVersion"] == PROTOCOL,
            "T3_PROTOCOL_CHANGED: reconnect after checking the new MCP revision"
        );
        t.session = session;
        let r = self
            .http
            .post(format!("{}/mcp", t.origin))
            .bearer_auth(&t.token)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL)
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
            .timeout(Duration::from_secs(10));
        let r = if let Some(id) = &t.session {
            r.header("Mcp-Session-Id", id)
        } else {
            r
        };
        ensure!(
            r.send()
                .await
                .map_err(|_| anyhow::anyhow!("T3 transport failed; retry connection"))?
                .status()
                .is_success(),
            "T3 initialization notification failed; reconnect"
        );
        t.catalog = self
            .rpc(&t, "tools/list", json!({}), Duration::from_secs(10))
            .await?
            .0;
        ensure!(
            t.catalog["tools"].is_array(),
            "T3 catalog is invalid; check the installed version"
        );
        *self.transport.lock().await = Some(t);
        Ok(())
    }
    /// The orchestrator pass-through: the full MCP result envelope of any live tool. A rejected call
    /// returns T3's own message (bounded) so the agent can correct its arguments.
    pub async fn call_raw(&self, name: &str, args: Value, deadline: Duration) -> Result<Value> {
        self.ensure_connected().await?;
        self.record_launch(name, &args);
        let t = self.transport.lock().await.clone().unwrap();
        ensure!(
            t.catalog["tools"].as_array().unwrap().iter().any(|v| v["name"] == name),
            "T3_TOOL_UNKNOWN: {name} is not in the live catalog; list tools with t3_tools"
        );
        let envelope = match self
            .rpc(&t, "tools/call", json!({"name":name,"arguments":args}), deadline)
            .await
        {
            Ok((v, _)) => v,
            Err(e) => {
                if !e.to_string().starts_with("T3_RPC_ERROR") {
                    self.invalidate().await;
                }
                return Err(e);
            }
        };
        if envelope["isError"] == true {
            let text = envelope["content"].as_array().into_iter().flatten()
                .filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join(" ");
            anyhow::bail!("T3_TOOL_FAILED: {}", bounded_text(&text, 1500));
        }
        self.record_launched(name, &envelope["structuredContent"]);
        Ok(envelope)
    }
    /// A `t3_thread_launch` (Heimdall's tasks, the relay thread, a launch through `t3_call`) records its first message
    /// before the call: the thread's first permission prompt can come within seconds of it (see `launches`).
    fn record_launch(&self, name: &str, args: &Value) {
        if let (Some(data), "t3_thread_launch", Some(message)) = (&self.launches, name, args["message"].as_str()) {
            if let Err(e) = heimdall_emergency::approvals::record(data, message) {
                eprintln!("T3 launch not recorded for the approval hook: {e}");
            }
        }
    }
    /// Records T3 thread ids as Heimdall's for the approval hook (`approvals::THREADS`): a `delegate_task` child of one
    /// of them is answered too (bug note 2026-11-02T14-56-51-2). Without a data folder (tests, probes) nothing happens.
    pub fn record_threads(&self, threads: &[&str]) {
        if let Some(data) = &self.launches {
            if let Err(e) = heimdall_emergency::approvals::record_threads(data, threads) {
                eprintln!("T3 threads not recorded for the approval hook: {e}");
            }
        }
    }
    /// Whether `thread` is in the record of Heimdall's own threads (`approvals::THREADS`, see `record_threads`): it
    /// outlives every relay session. Without a data folder (tests, probes): false.
    pub fn recorded_thread(&self, thread: &str) -> bool {
        let Some(data) = &self.launches else { return false };
        std::fs::read_to_string(data.join(heimdall_emergency::approvals::THREADS)).is_ok_and(|t| t.lines().any(|l| l.trim() == thread))
    }
    /// A launch's new thread id, once T3 answered with it.
    fn record_launched(&self, name: &str, result: &Value) {
        if let ("t3_thread_launch", Some(thread)) = (name, result["threadId"].as_str()) {
            self.record_threads(&[thread]);
        }
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        self.call_deadline(name, args, Duration::from_secs(15))
            .await
    }
    pub async fn call_deadline(
        &self,
        name: &str,
        args: Value,
        deadline: Duration,
    ) -> Result<Value> {
        self.ensure_connected().await?;
        self.record_launch(name, &args);
        let t = self.transport.lock().await.clone().unwrap();
        ensure!(t.catalog["tools"].as_array().unwrap().iter().any(|v|v["name"]==name),"T3_CAPABILITY_MISSING: required tool {name} is absent from the installed T3 Code");
        let result = self
            .rpc(
                &t,
                "tools/call",
                json!({"name":name,"arguments":args}),
                deadline,
            )
            .await;
        let envelope = match result {
            Ok((v, _)) => v,
            Err(e) => {
                // A deadline the caller chose ran out (`T3_TIMEOUT`): the connection itself is fine, and it is shared
                // (a background call's short wait must never drop the main agent's connection). Only a real transport
                // or protocol failure reconnects.
                if !e.to_string().starts_with(TIMEOUT) { self.invalidate().await; }
                return Err(e);
            }
        };
        ensure!(
            envelope["isError"] != true,
            "T3_TOOL_FAILED: {}",
            safe_tool_error(&envelope)
        );
        if let Some(v) = envelope.get("structuredContent") {
            self.record_launched(name, v);
            return Ok(v.clone());
        }
        for c in envelope["content"]
            .as_array()
            .context("T3 result has no content")?
        {
            if c["type"] == "text" {
                if let Ok(v) = serde_json::from_str::<Value>(c["text"].as_str().unwrap_or("")) {
                    self.record_launched(name, &v);
                    return Ok(v);
                }
            }
        }
        anyhow::bail!("T3 result has no structured JSON; check the installed T3 Code version")
    }
    async fn rpc(
        &self,
        t: &Transport,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<(Value, Option<String>)> {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut r = self
            .http
            .post(format!("{}/mcp", t.origin))
            .bearer_auth(&t.token)
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .timeout(deadline);
        if method != "initialize" {
            r = r.header("MCP-Protocol-Version", PROTOCOL);
        }
        if let Some(s) = &t.session {
            r = r.header("Mcp-Session-Id", s);
        }
        let response=r.send().await.map_err(|e|if e.is_timeout()&&!e.is_connect(){anyhow::anyhow!("{TIMEOUT}: no answer within {} ms. A mutation may have been accepted",deadline.as_millis())}else{anyhow::anyhow!("T3_UNAVAILABLE: transport failed; rediscover before retrying. A mutation may have been accepted")})?;
        let status = response.status().as_u16();
        if status == 401 {
            self.auth_required.store(true, Ordering::Release);
            // Persist the revocation for this exact credential (a newer one from a Connect click is untouched).
            if let Ok(Some(mut c)) = self.credentials.load() {
                if c.token == t.token && !c.revoked {
                    c.revoked = true;
                    if let Err(e) = self.credentials.save(&c) { eprintln!("T3 revocation not saved: {e:#}"); }
                }
            }
        }
        ensure!(status != 401, "T3_AUTH_REQUIRED: Connect T3 · click");
        ensure!(status==200,"T3_TRANSPORT_ERROR: HTTP {status}; reconnect before retrying. Do not repeat launch blindly");
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let sse = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.contains("text/event-stream"));
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        let envelope = loop {
            let next = stream.next().await.transpose().map_err(|e| {
                if e.is_timeout() { anyhow::anyhow!("{TIMEOUT}: the answer did not finish within {} ms", deadline.as_millis()) }
                else { anyhow::anyhow!("T3 result stream failed; reconnect before retrying") }
            })?;
            if let Some(chunk) = &next {
                ensure!(
                    bytes.len() + chunk.len() <= MAX_BYTES,
                    "T3 result exceeds transport bound; use smaller pages"
                );
                bytes.extend_from_slice(&chunk);
            }
            if sse {
                if let Some(v) = parse_sse(&bytes, id)? {
                    break v;
                }
            } else if next.is_none() {
                break serde_json::from_slice::<Value>(&bytes)
                    .context("T3 returned invalid RPC JSON")?;
            }
            if next.is_none() {
                anyhow::bail!("T3 stream closed without matching response");
            }
        };
        ensure!(
            envelope["jsonrpc"] == "2.0" && envelope["id"] == id,
            "T3 RPC response identity mismatch"
        );
        if let Some(error) = envelope.get("error") {
            anyhow::bail!(
                "T3_RPC_ERROR: request rejected ({}); refresh catalog and correct arguments",
                bounded_text(error["message"].as_str().unwrap_or("no message"), 800)
            );
        }
        Ok((
            envelope
                .get("result")
                .context("T3 RPC result missing")?
                .clone(),
            session,
        ))
    }
    /// Only the explicit Connect T3 UI action calls this. Never auto-open on a 401.
    pub async fn authorize(&self, open: impl FnOnce(&str) -> Result<()>) -> Result<()> {
        let _connection = self.connect.lock().await;
        let (origin, environment) = self.discover().await?;
        // One click: pair through T3's CLI when it is installed; the browser approval is the fallback.
        if self.cli.is_some() && self.pair(&origin, &environment).await.is_ok() {
            self.pair_after.store(0, Ordering::Release);
            drop(_connection);
            return self.ensure_connected().await;
        }
        let discovery = self
            .public(&origin, "/.well-known/oauth-authorization-server")
            .await?;
        let auth = endpoint(&discovery, "authorization_endpoint", &origin)?;
        let token = endpoint(&discovery, "token_endpoint", &origin)?;
        let register = endpoint(&discovery, "registration_endpoint", &origin)?;
        ensure!(
            discovery["issuer"] == origin,
            "T3 OAuth issuer mismatch; reconnect"
        );
        let protected = self
            .public(&origin, "/.well-known/oauth-protected-resource/mcp")
            .await?;
        ensure!(
            protected["resource"] == format!("{origin}/mcp"),
            "T3 OAuth resource mismatch"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let redirect = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );
        let registration=self.http.post(register).json(&json!({"client_name":"Heimdall","redirect_uris":[redirect],"token_endpoint_auth_method":"none"})).timeout(Duration::from_secs(10)).send().await.map_err(|_|anyhow::anyhow!("T3 OAuth registration failed; retry Connect T3"))?;
        ensure!(
            registration.status().is_success(),
            "T3 OAuth registration rejected; check T3 Connections"
        );
        let reg: Value = serde_json::from_slice(&bounded(registration).await?)?;
        let client = reg["client_id"]
            .as_str()
            .context("T3 OAuth client ID missing")?;
        let verifier = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let state = uuid::Uuid::new_v4().to_string();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = Url::parse(&auth)?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", client),
            ("redirect_uri", &redirect),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("resource", &format!("{origin}/mcp")),
            ("scope", "orchestration:read orchestration:operate"),
        ]);
        open(url.as_str())?;
        let code=tokio::time::timeout(Duration::from_secs(300),async{
            loop {
                let (mut socket,_)=listener.accept().await?;
                let request=tokio::time::timeout(Duration::from_secs(3),async{let mut buf=vec![];let mut one=[0;1024];while buf.len()<8192{let n=socket.read(&mut one).await?;if n==0{break;}buf.extend_from_slice(&one[..n]);if buf.windows(4).any(|x|x==b"\r\n\r\n"){break;}}Ok::<_,anyhow::Error>(buf)}).await;
                let result=request.ok().and_then(Result::ok).and_then(|bytes|callback(&bytes,&state,&origin).ok());
                let ok=result.is_some();let body=if ok{"Connected. Return to Heimdall."}else{"Invalid callback. Continue sign-in in T3."};
                let response=format!("HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",if ok{"200 OK"}else{"400 Bad Request"},body.len());
                let _=tokio::time::timeout(Duration::from_secs(3),socket.write_all(response.as_bytes())).await;
                if let Some(code)=result{return Ok::<_,anyhow::Error>(code);}
            }
        }).await.context("T3 sign-in expired; click Connect T3 again")??;
        self.exchange(&token, &code, client, &redirect, &verifier, &format!("{origin}/mcp"), &environment)
            .await?;
        self.invalidate().await;
        self.auth_required.store(false, Ordering::Release);
        drop(_connection);
        self.ensure_connected().await
    }
}
fn endpoint(v: &Value, key: &str, origin: &str) -> Result<String> {
    let u = Url::parse(v[key].as_str().context("T3 OAuth endpoint missing")?)?;
    ensure!(
        u.origin().ascii_serialization() == origin
            && u.username().is_empty()
            && u.password().is_none(),
        "T3 OAuth endpoint origin mismatch"
    );
    Ok(u.into())
}
fn callback(bytes: &[u8], state: &str, origin: &str) -> Result<String> {
    let request = std::str::from_utf8(bytes)?;
    let line = request.lines().next().context("callback request missing")?;
    let words = line.split_whitespace().collect::<Vec<_>>();
    ensure!(
        words.len() == 3 && words[0] == "GET",
        "invalid callback method"
    );
    let url = Url::parse(&format!("http://127.0.0.1{}", words[1]))?;
    ensure!(url.path() == "/callback", "invalid callback path");
    let p = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    ensure!(
        p.get("state").is_some_and(|v| v == state) && p.get("iss").is_some_and(|v| v == origin),
        "callback state/issuer mismatch"
    );
    ensure!(!p.contains_key("error"), "T3 sign-in declined");
    Ok(p.get("code").context("callback code missing")?.to_string())
}
pub fn parse_sse(bytes: &[u8], id: u64) -> Result<Option<Value>> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    let normalized = text.replace("\r\n", "\n");
    for event in normalized
        .split_inclusive("\n\n")
        .filter(|e| e.ends_with("\n\n"))
    {
        let payload = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:").map(|x| x.trim_start()))
            .collect::<Vec<_>>()
            .join("\n");
        if !payload.is_empty() {
            let v: Value = serde_json::from_str(&payload).context("invalid T3 SSE data")?;
            if v["id"] == id {
                return Ok(Some(v));
            }
        }
    }
    Ok(None)
}
async fn bounded(response: reqwest::Response) -> Result<Vec<u8>> {
    let mut out = vec![];
    let mut stream = response.bytes_stream();
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.map_err(|_| anyhow::anyhow!("T3 response stream failed"))?;
        ensure!(
            out.len() + bytes.len() <= MAX_BYTES,
            "T3 response exceeds bound"
        );
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}
/// At most `max` characters, cut on a character boundary, marked when cut.
pub(crate) fn bounded_text(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.into();
    }
    format!("{}…", text.chars().take(max).collect::<String>())
}
/// T3's own reason for a rejected call, one line, with anything secret-looking removed (day review T2: a generic
/// "request rejected" line sent the agent guessing; the user wants the real reason, 18-52-36).
fn safe_tool_error(v: &Value) -> String {
    let text = v["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join(" ");
    let text = strip_secrets(&text.split_whitespace().collect::<Vec<_>>().join(" "));
    if text.is_empty() {
        return "T3 rejected the call without a reason".into();
    }
    bounded_text(&text, 600)
}
/// Bearer tokens, credential-looking query values and long opaque tokens become `[redacted]`. Ids with colons or
/// percent signs (thread, run and item ids) and UUIDs are kept: the agent needs them.
pub(crate) fn strip_secrets(text: &str) -> String {
    const KEYS: [&str; 8] = ["token", "access_token", "code", "signature", "sig", "secret", "key", "credential"];
    let mut out = vec![];
    let mut bearer = false;
    for word in text.split(' ') {
        if bearer {
            out.push("[redacted]".to_owned());
            bearer = false;
            continue;
        }
        if word.eq_ignore_ascii_case("bearer") {
            bearer = true;
            out.push(word.to_owned());
            continue;
        }
        // URL query values of credential keys.
        let mut w = word.to_owned();
        if let Some(q) = w.find('?') {
            let (base, query) = w.split_at(q + 1);
            let query = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, _)) if KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)) => format!("{k}=[redacted]"),
                    _ => pair.to_owned(),
                })
                .collect::<Vec<_>>()
                .join("&");
            w = format!("{base}{query}");
        }
        // key=value outside a URL.
        if let Some((k, v)) = w.split_once('=') {
            if !v.is_empty() && KEYS.iter().any(|s| k.trim_start_matches(['"', '\'', '(']).eq_ignore_ascii_case(s)) && !w.contains('?') {
                w = format!("{k}=[redacted]");
            }
        }
        let core = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let opaque = core.len() >= 40
            && core.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '='))
            && core.chars().any(|c| c.is_ascii_digit())
            && core.chars().any(|c| c.is_ascii_alphabetic());
        if opaque {
            w = w.replace(core, "[redacted]");
        }
        out.push(w);
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t3_errors_keep_their_reason_and_lose_secrets() {
        let e = |t: &str| safe_tool_error(&json!({"isError":true,"content":[{"type":"text","text":t}]}));
        // The 19 Oct rejection now reads as T3 wrote it.
        assert_eq!(e("Worktree path /Users/t/Notes/Projects/Heimdall is not a git worktree of project 430e3e58-eb61-4f2a-aaa0-7ad485f9bec3."),
            "Worktree path /Users/t/Notes/Projects/Heimdall is not a git worktree of project 430e3e58-eb61-4f2a-aaa0-7ad485f9bec3.");
        assert_eq!(e("runtimeMode full-access rejected by server"), "runtimeMode full-access rejected by server");
        assert_eq!(e(""), "T3 rejected the call without a reason");
        let secret = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtY3AtY2xpZW50In0.c2lnbmF0dXJlMTIzNDU2Nzg5MA";
        let out = e(&format!("Unauthorized: Bearer {secret} expired; upload to /uploads/x?token=abc123&name=a.png failed; key=sk-live-1234 thread mcp:1f3f2e88-a7f5-462a-97a4-e823761ed147 run run:thread:mcp%3A1f3f2e88:ordinal:2"));
        assert!(!out.contains(secret) && !out.contains("abc123") && !out.contains("sk-live-1234"), "{out}");
        assert!(out.contains("Bearer [redacted]") && out.contains("token=[redacted]&name=a.png") && out.contains("key=[redacted]"), "{out}");
        assert!(out.contains("mcp:1f3f2e88-a7f5-462a-97a4-e823761ed147") && out.contains("run:thread:mcp%3A1f3f2e88:ordinal:2"), "ids kept: {out}");
        assert!(e(&"long reason ".repeat(200)).chars().count() <= 601);
    }

    struct NoCredential;
    impl CredentialStore for NoCredential {
        fn load(&self) -> Result<Option<Credential>> { panic!("the consent gate read the credential store") }
        fn save(&self, _: &Credential) -> Result<()> { panic!("the consent gate saved a credential") }
    }

    #[tokio::test]
    async fn t3_consent_gate_blocks_every_connection_until_allowed() {
        let dir = std::env::temp_dir().join(format!("t3-consent-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        // A runtime file that would point at a server: the gate must answer before reading it.
        std::fs::write(dir.join("userdata/server-runtime.json"), "{\"port\":9}").unwrap();
        let consent = Arc::new(AtomicBool::new(false));
        let client = Client::new(dir.join("userdata/server-runtime.json"), Arc::new(NoCredential)).unwrap()
            .with_cli(dir.join("bin/t3"))
            .with_consent(consent.clone());
        let err = client.auto_connect().await.unwrap_err().to_string();
        assert!(err.starts_with("T3_UNAVAILABLE"), "{err}");
        assert!(client.call("orchestrator_capabilities", json!({})).await.unwrap_err().to_string().starts_with("T3_UNAVAILABLE"));
        // Allowed: the client goes on to T3 (and here finds no credential).
        consent.store(true, Ordering::Release);
        let result = std::panic::AssertUnwindSafe(client.auto_connect());
        let reached = futures_util::FutureExt::catch_unwind(result).await.is_err();
        assert!(reached, "with consent the client reads its credential store");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A caller's deadline running out is not a broken connection: the app's one client is shared, and a background call's
/// short wait must never drop Heimdall's main T3 connection (review 30 Oct).
#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Memory(std::sync::Mutex<Option<Credential>>);
    impl CredentialStore for Memory {
        fn load(&self) -> Result<Option<Credential>> { Ok(self.0.lock().unwrap().clone()) }
        fn save(&self, c: &Credential) -> Result<()> { *self.0.lock().unwrap() = Some(c.clone()); Ok(()) }
    }
    async fn mcp(axum::extract::State(inits): axum::extract::State<Arc<AtomicUsize>>, axum::Json(req): axum::Json<Value>) -> axum::response::Response {
        use axum::response::IntoResponse;
        let id = req["id"].clone();
        let result = match req["method"].as_str().unwrap_or("") {
            "initialize" => { inits.fetch_add(1, Ordering::SeqCst); json!({"protocolVersion": PROTOCOL, "capabilities": {}}) }
            "notifications/initialized" => return axum::http::StatusCode::ACCEPTED.into_response(),
            "tools/list" => json!({"tools": [{"name": "slow"}, {"name": "fast"}]}),
            "tools/call" => {
                if req["params"]["name"] == "slow" { tokio::time::sleep(Duration::from_millis(600)).await; }
                json!({"structuredContent": {"ok": true}, "content": []})
            }
            other => panic!("unexpected {other}"),
        };
        axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
    }
    #[tokio::test]
    async fn a_deadline_that_runs_out_keeps_the_shared_connection() {
        let inits = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/.well-known/t3/environment", axum::routing::get(|| async { axum::Json(json!({"environmentId": "e1"})) }))
            .route("/mcp", axum::routing::post(mcp))
            .with_state(inits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("server-runtime.json");
        std::fs::write(&runtime, json!({"port": port}).to_string()).unwrap();
        let credential = Credential { token: "t".into(), expires_ms: crate::now_ms() + 30 * 24 * 3_600_000, environment: "e1".into(), revoked: false };
        let client = Client::new(runtime, Arc::new(Memory(std::sync::Mutex::new(Some(credential))))).unwrap();
        assert_eq!(client.call("fast", json!({})).await.unwrap(), json!({"ok": true}));
        let err = client.call_deadline("slow", json!({}), Duration::from_millis(100)).await.unwrap_err().to_string();
        assert!(err.starts_with(TIMEOUT), "{err}");
        assert!(client.transport.lock().await.is_some(), "the connection is kept");
        assert_eq!(client.call("fast", json!({})).await.unwrap(), json!({"ok": true}));
        assert_eq!(inits.load(Ordering::SeqCst), 1, "no reconnect after a deadline");
    }
}

#[cfg(test)]
mod keychain_name_tests {
    #[test]
    #[cfg(not(feature = "public"))]
    fn keychain_name_of_the_personal_edition_is_unchanged() {
        assert_eq!(super::KEYCHAIN_SERVICE, "dev.heimdall.t3");
    }
    #[test]
    #[cfg(feature = "public")]
    fn the_public_edition_t3_keychain_name_is_its_own() {
        assert_eq!(super::KEYCHAIN_SERVICE, "com.tobynoskillson.heimdall.t3");
    }
}
