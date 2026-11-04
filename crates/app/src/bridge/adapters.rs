//! Harness adapters: the dry-run logger, the unavailable stand-in, the Pi/dry/unavailable switch and the
//! recording adapter that archives every provider payload before transport.
use super::*;

pub(super) struct LoggedDry {
    pub(super) feed: Arc<AgentFeed>,
    pub(super) file: std::fs::File,
    pub(super) path: PathBuf,
    pub(super) session: PathBuf,
    pub(super) tx: mpsc::UnboundedSender<Event>,
    pub(super) rx: mpsc::UnboundedReceiver<Event>,
}
#[async_trait]
impl Harness for LoggedDry {
    fn observation_append(&self) -> bool {
        true
    }
    async fn send(&mut self, v: Value) -> Result<()> {
        self.feed.sent(&v);
        let record = json!({"dry_run":true,"timestamp_ms":agent::now_ms(),"would_send":v});
        // Log image count/metadata but never base64 pixel payloads in stdout.
        let mut printable = record.clone();
        if let Some(a) = printable["would_send"]["message"]["content"].as_array_mut() {
            for b in a.iter_mut().filter(|b| b["type"] == "image") {
                b["source"]["data"] = json!("<private attachment>");
            }
        }
        writeln!(self.file, "{printable}")?;
        self.file.flush()?;
        if self.file.metadata()?.len() >= 1024 * 1024 { rotate_dry_log(&mut self.file, &self.path, &self.session)?; }
        crate::diagnostics::event_stream(&printable);
        let id = v["uuid"].as_str().unwrap_or("").to_owned();
        self.tx.send(Event::Echo { id: id.clone() })?;
        self.tx.send(Event::Complete {
            id: Some(id),
            query: v["shouldQuery"].as_bool().unwrap_or(false),
            error: false,
            text: String::new(),
        })?;
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        Ok(self.rx.recv().await)
    }
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
/// Keep the hot log below one MiB. Each rotation is archived in the session's store logs
/// (`dry-run-<sha256>.jsonl`, verified, durably committed) before the hot file is truncated.
pub(super) fn rotate_dry_log(file: &mut std::fs::File, path: &Path, session: &Path) -> Result<()> {
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() { return Ok(()); }
    let key = heimdall_archive::hash(&bytes);
    heimdall_archive::store::archive_log(session, &format!("dry-run-{key}.jsonl"), &bytes)?;
    file.set_len(0)?;
    heimdall_archive::full_sync(file)?;
    Ok(())
}

pub(super) struct Unavailable;
#[async_trait]
impl Harness for Unavailable {
    fn observation_append(&self) -> bool { false }
    async fn send(&mut self, _: Value) -> Result<()> { Err(agent::harness::EngineUnavailable.into()) }
    async fn next(&mut self) -> Result<Option<Event>> { std::future::pending().await }
    async fn shutdown(&mut self) -> Result<()> { Ok(()) }
}
pub(super) enum Adapter {
    Unavailable(Unavailable),
    Dry(LoggedDry),
    Pi(Pi),
}
#[async_trait]
impl Harness for Adapter {
    fn max_text_bytes(&self)->usize{match self{Self::Pi(pi)=>pi.max_text_bytes(),_=>9*1024*1024}}
    fn serial_wakes(&self)->bool{matches!(self,Self::Pi(_))}
    fn supports_steering(&self)->bool{match self{Self::Pi(pi)=>pi.supports_steering(),_=>false}}
    fn input_blocked(&self)->Option<&'static str>{match self{Self::Pi(pi)=>pi.input_blocked(),_=>None}}
    fn observation_append(&self) -> bool {
        match self {
            Self::Unavailable(h) => h.observation_append(),
            Self::Dry(h) => h.observation_append(),
            Self::Pi(h) => h.observation_append(),
        }
    }
    async fn send(&mut self, v: Value) -> Result<()> {
        match self {
            Self::Unavailable(h) => h.send(v).await,
            Self::Dry(h) => h.send(v).await,
            Self::Pi(h) => h.send(v).await,
        }
    }
    async fn send_with_permit(&mut self, v: Value, permit: agent::delivery::DeliveryPermit) -> Result<()> {
        match self {
            Self::Pi(h) => h.send_with_permit(v, permit).await,
            Self::Unavailable(h) => permit.run(h.send(v)).await,
            Self::Dry(h) => permit.run(h.send(v)).await,
        }
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        match self {
            Self::Unavailable(h) => h.next().await,
            Self::Dry(h) => h.next().await,
            Self::Pi(h) => h.next().await,
        }
    }
    async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Unavailable(h) => h.shutdown().await,
            Self::Dry(h) => h.shutdown().await,
            Self::Pi(h) => h.shutdown().await,
        }
    }
    fn turn_active(&self) -> bool { matches!(self, Self::Pi(pi) if pi.active()) }
    async fn append_context(&mut self, text: &str) -> Result<()> {
        match self {
            Self::Pi(h) => Harness::append_context(h, text).await,
            Self::Unavailable(h) => h.append_context(text).await,
            Self::Dry(h) => h.append_context(text).await,
        }
    }
    async fn context_tokens(&mut self) -> Result<Option<(Option<u64>, u64)>> {
        match self { Self::Pi(h) => h.context_tokens().await, _ => Ok(None) }
    }
    async fn compact_history(&mut self) -> Result<()> {
        match self { Self::Pi(h) => h.compact_history().await, _ => anyhow::bail!("the agent is not running") }
    }
    fn can_rejoin(&self) -> bool { matches!(self, Self::Pi(pi) if pi.can_rejoin()) }
    async fn rejoin(&mut self) -> Result<bool> {
        match self { Self::Pi(h) => h.rejoin().await, _ => Ok(false) }
    }
    fn last_request(&self) -> Option<agent::maintenance::LastRequest> {
        match self { Self::Pi(h) => h.last_request(), _ => None }
    }
    fn history_end_ms(&self) -> Option<u64> {
        match self { Self::Pi(h) => h.history_end_ms(), _ => None }
    }
}
#[cfg(test)]
thread_local! {
    pub(super) static TEST_BEFORE_DELIVERY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
/// The common provider payload is archived before transport; image bytes use CAS refs.
pub(super) struct RecordedAdapter {
    pub(super) inner: Adapter,
    pub(super) journal: heimdall_archive::Journal,
    pub(super) gate: Option<Arc<agent::delivery::DeliveryGate>>,
    pub(super) ids: Arc<Mutex<HashMap<String, Vec<String>>>>,
    pub(super) queries: HashMap<String, Vec<String>>,
}
impl RecordedAdapter {
    pub(super) fn source(&self) -> &str {
        match self.inner {
            Adapter::Unavailable(_) => "agent:offline",
            Adapter::Dry(_) => "agent:dry",
            Adapter::Pi(_) => "agent:pi",
        }
    }
    pub(super) fn record_event(&mut self, e: &Event) -> Result<()> {
        let (payload, id) = match e {
            Event::Other(v) => (v.clone(), None),
            Event::Deferred { id, reason } => (json!({"kind":"deferred","id":id,"reason":reason}),Some(id)),
            Event::Complete {
                id,
                query,
                error,
                text,
            } => (
                json!({"kind":"complete","id":id,"query":query,"error":error,"text":text}),
                id.as_ref(),
            ),
            Event::Echo { id } => (json!({"kind":"echo","id":id}), Some(id)),
            Event::Consumed { id } => (json!({"kind":"consumed","id":id}), Some(id)),
            Event::Init {
                session,
                capabilities,
                api_key_source,
            } => (
                json!({"kind":"init","session":session,"capabilities":capabilities,"api_key_source":api_key_source}),
                None,
            ),
            Event::Limit {
                rejected,
                reset,
                status,
            } => (
                json!({"kind":"limit","rejected":rejected,"reset":reset,"status":status}),
                None,
            ),
        };
        if matches!(e, Event::Other(v) if v["type"] == "heimdall_loop_stopped") {
            self.journal.event("agent_loop_guard", self.source(), &[], capture::host_now_ns(), payload.clone())?;
        }
        let exact = id.and_then(|id| self.ids.lock().unwrap().get(id).cloned());
        let mut ids = exact
            .clone()
            .unwrap_or_else(|| self.queries.values().flatten().cloned().collect());
        ids.sort();
        ids.dedup();
        self.journal.event("agent_reply",self.source(),&ids,capture::host_now_ns(),json!({"event":payload,"attribution":if exact.is_some(){"exact"}else{"possible_active_queries"}}))?;
        if matches!(e, Event::Complete { .. }) {
            if let Some(id) = id {
                self.ids.lock().unwrap().remove(id);
                self.queries.remove(id);
            }
        }
        Ok(())
    }
}
impl RecordedAdapter {
    pub(super) async fn send_permitted(&mut self, v: Value, permit:Option<agent::delivery::DeliveryPermit>) -> Result<()> {
        if permit.as_ref().is_some_and(|p|p.check().is_err() || self.gate.as_ref().is_some_and(|g|!p.belongs_to(g))) {
            return Err(agent::delivery::DeliveryBlocked{written:0,total:0}.into());
        }
        if v["delivery_epoch"].as_u64().is_some_and(|epoch|permit.as_ref().map(|p|p.generation())!=Some(epoch)) {
            return Err(agent::delivery::DeliveryBlocked{written:0,total:0}.into());
        }
        let mut stored = v.clone();
        if let Some(content) = stored["message"]["content"].as_array_mut() {
            for part in content {
                if part["type"] == "image" {
                    let encoded = part["source"]["data"]
                        .as_str()
                        .context("image payload data")?;
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
                    let mime = part["source"]["media_type"]
                        .as_str()
                        .context("image MIME")?;
                    let id = self.journal.image(&bytes, mime)?;
                    part["source"].as_object_mut().unwrap().remove("data");
                    part["source"]["archive_image_id"] = json!(id);
                }
            }
        }
        let ids = agent::protocol::observation_ids(&v);
        let message = v["uuid"].as_str().unwrap_or("").to_owned();
        self.ids.lock().unwrap().insert(message.clone(), ids.clone());
        if v["shouldQuery"] == true {
            self.queries.insert(message.clone(), ids.clone());
        }
        self.journal.event(
            "agent_send",
            self.source(),
            &ids,
            capture::host_now_ns(),
            json!({"payload":stored,"query":v["shouldQuery"],"message_id":message}),
        )?;
        #[cfg(test)]
        TEST_BEFORE_DELIVERY.with(|hook| { if let Some(hook) = hook.borrow_mut().take() { hook(); } });
        let result = match permit {
            Some(permit) => self.inner.send_with_permit(v, permit).await,
            None => self.inner.send(v).await,
        };
        self.journal.event("agent_delivery",self.source(),&ids,capture::host_now_ns(),json!({"message_id":message,"transport_accepted":result.is_ok(),"error":result.as_ref().err().map(|e|e.to_string())}))?;
        result
    }
}
#[async_trait]
impl Harness for RecordedAdapter {
    fn max_text_bytes(&self)->usize{self.inner.max_text_bytes()}
    fn serial_wakes(&self)->bool{self.inner.serial_wakes()}
    fn supports_steering(&self)->bool{self.inner.supports_steering()}
    fn input_blocked(&self)->Option<&'static str>{self.inner.input_blocked()}
    fn record_queue_fallback(&mut self, wire:&Value, reason:&str)->Result<()> {
        self.journal.event("agent_queue_fallback",self.source(),&agent::protocol::observation_ids(wire),capture::host_now_ns(),json!({"message_id":wire["uuid"],"reason":reason}))
    }
    fn delivery_epoch(&self)->Option<u64>{self.gate.as_ref().map(|g|g.generation())}
    fn observation_append(&self) -> bool {
        self.inner.observation_append()
    }
    async fn send(&mut self,v:Value)->Result<()> {let p=self.gate.as_ref().map(|g|g.permit()).transpose()?;self.send_permitted(v,p).await}
    async fn send_with_permit(&mut self,v:Value,p:agent::delivery::DeliveryPermit)->Result<()> {self.send_permitted(v,Some(p)).await}
    async fn next(&mut self) -> Result<Option<Event>> {
        let event = self.inner.next().await?;
        if let Some(e) = &event {
            self.record_event(e)?;
        }
        Ok(event)
    }
    async fn shutdown(&mut self) -> Result<()> {
        self.inner.shutdown().await
    }
    fn turn_active(&self) -> bool { self.inner.turn_active() }
    async fn append_context(&mut self, text: &str) -> Result<()> {
        self.inner.append_context(text).await?;
        // Context without a turn (catch-up, restart message, notices that are context only) is journalled like a send.
        self.journal.event("agent_context", self.source(), &[], capture::host_now_ns(), json!({"text":text}))
    }
    async fn context_tokens(&mut self) -> Result<Option<(Option<u64>, u64)>> { self.inner.context_tokens().await }
    async fn compact_history(&mut self) -> Result<()> { self.inner.compact_history().await }
    fn can_rejoin(&self) -> bool { self.inner.can_rejoin() }
    /// Night pass H2: the user's words restart a turn that has neither acted nor replied (journalled, like every send);
    /// `restarted` false: it had started writing a reply or tool call by then, so his words steer it (review 4).
    async fn rejoin(&mut self) -> Result<bool> {
        let result = self.inner.rejoin().await;
        self.journal.event("agent_turn_rejoined", self.source(), &[], capture::host_now_ns(), json!({"restarted":result.as_ref().ok(),"error":result.as_ref().err().map(|e|format!("{e:#}"))}))?;
        result
    }
    fn last_request(&self) -> Option<agent::maintenance::LastRequest> { self.inner.last_request() }
    fn history_end_ms(&self) -> Option<u64> { self.inner.history_end_ms() }
}
