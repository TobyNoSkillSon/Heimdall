//! Narrow host attachment points for the Pi supervisor and island UI owners.
use agent::{
    actions::{ActionSink, Actions, Notice, Question},
    delivery::DeliveryPermit,
};
use t3::{Client, Workers};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
/// What a `speak` call is told after the user pressed Stop on this run's speech.
const STOPPED_RUN: &str = "STOPPED: User pressed Stop on your speech in this turn, so the rest of it is shown on the island, not said. Do not repeat it aloud unless they ask.";
type Delivery = Arc<dyn Fn(&str, &str, &DeliveryPermit) -> Result<()> + Send + Sync>;
type Reload = Arc<dyn Fn(&str, &DeliveryPermit) -> Result<()> + Send + Sync>;
type Ask = Arc<dyn Fn(&Question, &DeliveryPermit) -> Result<()> + Send + Sync>;
fn connection_expiry(id: &str) -> Option<u64> {
    id.strip_prefix("t3-connect-expiry:")?.rsplit(':').next()?.parse().ok()
}
fn island_notice(n: &Notice) -> ui::agent_notice::AgentNotice {
    use ui::agent_notice::{AgentNotice, NoticeAction, NoticeKind};
    AgentNotice {
        id: n.id.clone(), text: n.text.clone(), outcome: None,
        expires_ms: connection_expiry(&n.id),
        kind: NoticeKind::parse(&n.kind),
        action: if n.text.starts_with("Connect T3") { Some(NoticeAction::ConnectT3) } else { None },
        detail: n.detail.as_ref().and_then(detail),
    }
}
/// `island_notice.detail` (validated by the contract) for the island's pull-down.
fn detail(v: &Value) -> Option<ui::agent_notice::Detail> {
    use ui::agent_notice::Detail;
    let strings = |a: &Value| -> Option<Vec<String>> { a.as_array()?.iter().map(|x| x.as_str().map(str::to_owned)).collect() };
    match v["type"].as_str()? {
        "text" => Some(Detail::Text(v["text"].as_str()?.to_owned())),
        "table" => Some(Detail::Table { headers: strings(&v["headers"])?, rows: v["rows"].as_array()?.iter().map(strings).collect::<Option<_>>()? }),
        "svg" => Some(Detail::Svg(v["svg"].as_str()?.to_owned())),
        "image" => Some(Detail::Image { archive_id: v["archive_id"].as_str()?.to_owned() }),
        _ => None,
    }
}
#[cfg(test)]
mod ui_gap_tests {
    #[test]
    fn expiry_is_typed_from_the_durable_connection_identity() {
        assert_eq!(super::connection_expiry("t3-connect-expiry:environment:123456"), Some(123456));
        assert_eq!(super::connection_expiry("t3-connect-expiry:unconnected"), None);
        assert_eq!(super::connection_expiry("t3-connect-401:environment:123456"), None);
        assert_eq!(super::connection_expiry("question:123456"), None);
        let notice = super::island_notice(&agent::actions::Notice {
            id: "t3-connect-expiry:environment:123456".into(),
            text: "Connect T3 · click".into(), kind: "info".into(), detail: None,
        });
        assert_eq!(notice.expires_ms, Some(123456));
        assert_eq!(notice.action, Some(ui::agent_notice::NoticeAction::ConnectT3));
        assert_eq!(notice.text, "Connect T3 · click");
    }
    #[test]
    fn only_real_worker_events_say_worker_update() {
        use super::delivery_label;
        for id in ["terminal:t:r:completed", "stuck:t:r:a", "question:t:q", "approval:t:idle"] {
            assert_eq!(delivery_label(id, "Worker t run r completed."), Some("Worker update"), "{id}");
        }
        assert_eq!(delivery_label("t3-connect-401:env", "Connect T3 · click"), Some("Connect T3 · click"));
        // The user's own click on an `ask` card and a fast-read result are not worker updates.
        assert_eq!(delivery_label("ask-1b2c", "Question ask-1b2c: User clicked Yes. Question: Install?"), None);
        assert_eq!(delivery_label("fastread-9", "Fast read: read at 300 wpm."), None);
    }
    #[test]
    fn island_lines_carry_their_kind_and_every_detail_type() {
        use serde_json::json;
        use ui::agent_notice::{Detail, NoticeKind};
        let line = |kind: &str, detail: Option<serde_json::Value>| super::island_notice(&agent::actions::Notice { id: "n".into(), text: "Done".into(), kind: kind.into(), detail });
        assert_eq!(line("working", None).kind, NoticeKind::Working);
        assert_eq!(line("error", None).kind, NoticeKind::Error);
        assert!(line("looking", None).is_line());
        assert_eq!(line("info", Some(json!({"type":"text","text":"a"}))).detail, Some(Detail::Text("a".into())));
        assert_eq!(
            line("success", Some(json!({"type":"table","headers":["A","B"],"rows":[["1","2"],["3"]]}))).detail,
            Some(Detail::Table { headers: vec!["A".into(), "B".into()], rows: vec![vec!["1".into(), "2".into()], vec!["3".into()]] })
        );
        assert_eq!(line("info", Some(json!({"type":"svg","svg":"<svg/>"}))).detail, Some(Detail::Svg("<svg/>".into())));
        assert_eq!(line("looking", Some(json!({"type":"image","archive_id":"f1"}))).detail, Some(Detail::Image { archive_id: "f1".into() }));
    }
}
/// The island line for an outbox delivery to the agent. Only real T3 worker events say "Worker update"; a
/// question click or a fast-read result is the user's own action and gets no line (the card or reader already closed).
fn delivery_label(id: &str, text: &str) -> Option<&'static str> {
    if text.starts_with("Connect T3") { return Some("Connect T3 · click"); }
    ["terminal:", "stuck:", "question:", "approval:"].iter().any(|k| id.starts_with(k)).then_some("Worker update")
}
struct Sink {
    ui: ui::Ui,
    deliver: Delivery,
    reload: Reload,
    ask: Ask,
}
impl ActionSink for Sink {
    fn notice(&self, n: &Notice, p: &DeliveryPermit) -> Result<()> {
        // One update: the island draws a plain line (icon, words, pull-down detail); a Connect T3 notice takes the
        // card. The Agent window lists both.
        p.with(|| {
            let notice = island_notice(n);
            // Kept for pulling out of the notch before the session file records it.
            if let Some(d) = &notice.detail { crate::chat_history::shown_note(&notice.id, &notice.text, d); }
            self.ui.send(ui::UiUpdate::AgentNotice(notice));
            Ok(())
        })
    }
    fn question(&self, q: &Question, p: &DeliveryPermit) -> Result<()> {
        (self.ask)(q, p)
    }
    fn question_closed(&self, id: &str, answer: &str, p: &DeliveryPermit) -> Result<()> {
        p.with(|| {
            self.ui.send(ui::UiUpdate::CloseQuestion { id: id.into(), answer: answer.into() });
            Ok(())
        })
    }
    fn deliver(&self, id: &str, text: &str, p: &DeliveryPermit) -> Result<()> {
        (self.deliver)(id, text, p)?;
        // Monitoring already has its durable island receipt. The outbox carries
        // only the agent delivery, so one outage has one presentation identity.
        if text.starts_with("Worker monitoring") { return Ok(()); }
        let Some(label) = delivery_label(id, text) else { return Ok(()) };
        self.notice(&Notice { id: id.into(), text: label.into(), kind: "info".into(), detail: None }, p)
    }
    fn reload(&self, id: &str, p: &DeliveryPermit) -> Result<()> {
        (self.reload)(id, p)
    }
}
pub struct Host {
    pub actions: std::result::Result<Arc<Actions>, String>,
    pub workers: Mutex<Option<Arc<Workers>>>,
    data: std::path::PathBuf,
    /// Bug notes (bugs.rs): the `bug_note` tool's inbox and island.
    pub bug_notes: crate::bugs::Filing,
    /// Scheduled wakes (`agent::wakes`, selfwake night 21 Oct): the schedule the wake tools, the heartbeat and
    /// `Heimdall --schedule-wake` share.
    pub wakes: std::result::Result<Arc<agent::wakes::Wakes>, String>,
    /// Plan refresher (night 1022, plan_refresher.rs): `refresh_plan` and the 5-minute background pass.
    pub plan: crate::plan_refresher::Refresher,
    /// Quiet speaker naming: `name_voice` runs on the live pipeline, through its internal queue.
    pub voice_names: Mutex<Option<crossbeam_channel::Sender<crate::pipeline::Internal>>>,
    /// The menu's own action callback (main.rs `ui_action_callback`): the `settings` tool's changes go through it, as
    /// a click would.
    pub menu: Mutex<Option<MenuCallback>>,
}
/// The menu's action callback, callable from the agent's thread.
pub type MenuCallback = Arc<dyn Fn(ui::UiAction) + Send + Sync>;
/// A speech receipt's store failed (a SQLite error: a held lock, a full disk): logged, and `None` so the line still
/// goes out. Anything else (Agent off, a replayed operation with other content) stays an error.
fn receipt_soft<T>(result: Result<T>, what: &str) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.downcast_ref::<rusqlite::Error>().is_some() => {
            eprintln!("Speech receipt {what} failed; the line goes out anyway: {e:#}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
/// `Actions::set_state` on a receipt that exists, its store failures logged (`receipt_soft`).
fn set_state_soft(actions: &Actions, id: Option<&str>, state: &str, p: &DeliveryPermit) -> Result<()> {
    let Some(id) = id else { return Ok(()) };
    receipt_soft(actions.set_state(id, state, p), "update").map(drop)
}
impl Host {
    /// Runs `f` on the live pipeline through its internal queue (`name_voice`, `listen_to`): its answer, or None when
    /// none came within `seconds`.
    async fn on_pipeline(&self, p: &DeliveryPermit, seconds: u64, f: impl FnOnce(&mut crate::pipeline::Pipeline) -> Result<String> + Send + 'static) -> Result<Option<std::result::Result<String, String>>> {
        let tx = self.voice_names.lock().unwrap().clone().context("VOICES_UNAVAILABLE: Heimdall is not listening")?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        p.with(|| {
            tx.send(crate::pipeline::Internal::Inference(Box::new(move |pipeline: &mut crate::pipeline::Pipeline| {
                let _ = reply.send(f(pipeline).map_err(|e| format!("{e:#}")));
                Ok(())
            })))
            .map_err(|_| anyhow::anyhow!("VOICES_UNAVAILABLE: the pipeline stopped"))
        })?;
        Ok(tokio::time::timeout(std::time::Duration::from_secs(seconds), answer).await.ok().and_then(Result::ok))
    }
    pub fn new(data: &Path) -> Self {
        Self {
            actions: Actions::open(&data.join("agent/mcp-actions.sqlite"))
                .map(Arc::new)
                .map_err(|_| "Action receipt store is unavailable; check free disk space".into()),
            workers: Mutex::new(None),
            data: data.into(),
            bug_notes: Default::default(),
            wakes: agent::wakes::Wakes::open(&data.join("agent/wakes.sqlite"))
                .map(Arc::new)
                .map_err(|e| format!("WAKES_UNAVAILABLE: the wake schedule could not be opened ({e:#}); check free disk space")),
            plan: Default::default(),
            voice_names: Mutex::new(None),
            menu: Mutex::new(None),
        }
    }
    /// Plan refresher (night 1022): the background pass beside the wake heartbeat, until `stop`. Tests never start
    /// it (no real model calls); `plan_refresher` tests drive the loop against a fake server.
    pub fn spawn_plan_refresher(&self, workers: Arc<Workers>, gate: Arc<agent::delivery::DeliveryGate>, stop: tokio::sync::watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
        if cfg!(test) { return None; }
        let actions = self.actions.as_ref().ok()?.clone();
        crate::plan_refresher::spawn_live(&self.plan, &self.data, actions, workers, gate, stop)
    }
    pub fn attach(
        &self,
        ui: ui::Ui,
        deliver: Delivery,
        reload: Reload,
        ask: Ask,
    ) -> Result<Arc<Workers>> {
        let actions = self
            .actions
            .as_ref()
            .map_err(|e| anyhow::anyhow!(e.clone()))?
            .clone();
        self.bug_notes.set_ui(ui);
        actions.attach(Arc::new(Sink {
            ui,
            deliver,
            reload,
            ask,
        }));
        if let Some(workers) = self.workers.lock().unwrap().clone() { return Ok(workers); }
        #[cfg(not(test))]
        let client = Arc::new(Client::local(Arc::new(t3::Keychain))?.with_consent(crate::setup_wizard::t3_consent()).with_launch_record(self.data.clone()));
        #[cfg(test)]
        let client = Arc::new(Client::new(self.data.join("agent/fake-t3-runtime.json"), Arc::new(TestCredentials))?);
        let workers = Arc::new(Workers::open(
            &self.data.join("agent/t3-workers.sqlite"),
            client,
            actions,
        )?);
        workers.set_brief_preamble(agent::harness::WORKER_PROMPT);
        // Background Tasks ▸ Model on a T3 instance: each call is a thread through this client (background_t3.rs).
        #[cfg(not(test))]
        crate::background_call::t3::set_client(workers.client.clone(), &self.data);
        *self.workers.lock().unwrap() = Some(workers.clone());
        Ok(workers)
    }
    pub fn detach(&self) {
        if let Ok(actions) = &self.actions { actions.detach(); }
        self.workers.lock().unwrap().take();
    }
    /// The `speak` tool: said at once, and remembered for this turn so the final reply never says it again
    /// (`agent::reply`).
    pub fn speak(
        &self,
        tools: &crate::bridge::Tools,
        text: &str,
        language: &str,
        op: &str,
        p: &DeliveryPermit,
    ) -> Result<String> {
        self.say_as(tools, text, language, op, p, true, None)
    }
    /// What `say` answers when the words went out as text on the island instead of Heimdall's voice (Speaking off,
    /// speech held or failed): the user has to read them (`bridge::unread`).
    pub const SHOWN_ON_ISLAND: &'static str = "Shown on the island.";
    /// The path every reply takes to the user, a `speak` call or a turn's final reply (`Tools::agent_final_answer`):
    /// Heimdall's voice with Speaking on, an island line with it off or when speech cannot start. Once per `op`.
    pub fn say(
        &self,
        tools: &crate::bridge::Tools,
        text: &str,
        language: &str,
        op: &str,
        p: &DeliveryPermit,
    ) -> Result<String> {
        self.say_as(tools, text, language, op, p, false, None)
    }
    fn say_as(
        &self,
        tools: &crate::bridge::Tools,
        text: &str,
        language: &str,
        op: &str,
        p: &DeliveryPermit,
        tool_call: bool,
        // A held line said now: when it was held (host clock), and whether the user's words reached the agent after it, so it
        // is said even in a call (he is talking to Heimdall).
        held: Option<(u64, bool)>,
    ) -> Result<String> {
        let actions = self
            .actions
            .as_ref()
            .map_err(|e| anyhow::anyhow!(e.clone()))?;
        // The receipt makes a replayed call say nothing twice. A receipt store that fails (SQLite: a held lock, a full
        // disk) never costs the user the reply: logged, and the line goes out without it (1 Nov 00:17-00:21, three replies
        // lost to "database is locked").
        let id = receipt_soft(actions.receipt(
            op,
            "speech",
            &serde_json::to_string(&(text, language))?,
            None,
            p,
        ), "write")?;
        let state = match &id {
            Some(id) => receipt_soft(actions.state(id), "read")?.unwrap_or_else(|| "pending".into()),
            None => "pending".into(),
        };
        let id = id.as_deref();
        if state == "pending" {
            if tool_call { tools.replies.lock().unwrap().said(text); }
            // One tool in both modes ("completely agnostic to whether the sound is on or off"):
            // Speaking on is Heimdall's voice; off, or speech that cannot start, is the text as an island line.
            if !tools.enabled.load(std::sync::atomic::Ordering::Acquire) {
                return Self::island_line(tools, actions, id, text, op, p);
            }
            // speech-policy (night 3): while the user talks with other people nothing is played unless he addressed
            // Heimdall by name; the text is shown silently and a `speak` call is told why (bridge/speech_policy.rs).
            if !held.is_some_and(|(_, asked)| asked) && tools.speech_policy.speech_held(capture::host_now_ns()) {
                // One record and log line, so a reply the user did not hear can be told from one never sent (bug note
                // 2026-10-26T18-35-49: he was in a Discord call and the log said nothing).
                tools.record_diagnostic("speech_held", serde_json::json!({"event":"in a call, shown on the island","op":op}));
                // Said once the conversation is over, unless it goes stale or is superseded (`replay_held`).
                tools.speech_policy.hold_line(text, language, capture::host_now_ns(), p);
                Self::island_line(tools, actions, id, text, op, p)?;
                return Ok(if tool_call { crate::bridge::speech_policy::HELD_IN_CONVERSATION.into() } else { Self::SHOWN_ON_ISLAND.into() });
            }
            // The agent's run reply (a `speak` call or its final reply), not app lines (quota, emergency mode) or a held
            // line said later.
            let run_reply = held.is_none() && !op.starts_with("emergency-say:") && !op.starts_with("quota:");
            // The user pressed Stop on this run's speech (review 5 Nov: its final reply was said right after): the rest of
            // the run shows on the island, unsaid, until their own words reach the agent again.
            if run_reply && tools.speech_policy.run_stopped() {
                tools.record_diagnostic("speech_held", serde_json::json!({"event":"the user pressed Stop on this run, shown on the island","op":op}));
                Self::island_line(tools, actions, id, text, op, p)?;
                return Ok(if tool_call { STOPPED_RUN.into() } else { Self::SHOWN_ON_ISLAND.into() });
            }
            set_state_soft(actions, id, "submitting", p)?;
            // Written to be read as well as heard: list items and short lines become sentences for the voice.
            match agent::mcp::ToolHost::speak(tools, &agent::reply::for_speech(text), language, p) {
                Ok(job) => {
                    // A newer reply goes out: lines held in a call before it are not said after it. A held line said now
                    // supersedes only those held before it, never one held since (review: the answer to his words).
                    tools.speech_policy.reply_queued(held.map_or_else(capture::host_now_ns, |(at, _)| at));
                    if run_reply { tools.run_speech(job); }
                    set_state_soft(actions, id, &format!("queued:{job}"), p)?;
                    // The words show on the island as each sentence starts playing (`UiUpdate::SpokenText`, from
                    // playback; "I want the text to also appear on the Dynamic Island").
                    return Ok(format!("Speech queued: {}.", id.unwrap_or("unrecorded")));
                }
                Err(e) => {
                    set_state_soft(actions, id, "failed", p)?;
                    eprintln!("Speech failed, shown on the island instead: {e:#}");
                    return Self::island_line(tools, actions, id, text, op, p);
                }
            }
        }
        let id = id.unwrap_or_default();
        if state.starts_with("queued:") {
            return Ok(format!("Speech queued: {id}."));
        }
        if state == "shown" {
            return Ok(Self::SHOWN_ON_ISLAND.into());
        }
        anyhow::bail!(
            "SPEECH_UNCONFIRMED: receipt {id} is {state}; do not automatically repeat speech"
        )
    }
    /// Lines held while the user was in a conversation are said once it is over, and the newest at once when his words
    /// reached the agent after it (bridge/speech_policy.rs): checked from `Tools::speech_status` on every capture event,
    /// so the common case is one lock. Each line goes through the normal speech path with no lead-in, once; stale and
    /// superseded lines are dropped by the policy.
    pub(crate) fn replay_held(&self, tools: &crate::bridge::Tools) {
        let policy = &tools.speech_policy;
        if !policy.has_held() {
            return;
        }
        let now = capture::host_now_ns();
        let may_speak = tools.enabled.load(std::sync::atomic::Ordering::Acquire) && !tools.owner.holds() && !tools.heimdall_speaking();
        let permit = may_speak.then(|| tools.agent_enabled.permit().ok()).flatten();
        for crate::bridge::speech_policy::Due { line, addressed } in policy.due_held(now, permit.is_some()) {
            // Said in the epoch it was held in (`due_held` dropped it if Agent or Heimdall was off since).
            let p = &line.permit;
            let what = if addressed { "said: User spoke to Heimdall after it" } else { "replayed after the call" };
            crate::bridge::speech_policy::held_log(what, now.saturating_sub(line.held_ns));
            let op = format!("held-replay:{}", uuid::Uuid::new_v4());
            match self.say_as(tools, &line.text, &line.language, &op, p, false, Some((line.held_ns, addressed))) {
                Ok(said) if said.starts_with("Speech queued") => {}
                Ok(said) => eprintln!("speech: held line not replayed: {said}"),
                Err(e) => eprintln!("speech: held line not replayed: {e:#}"),
            }
        }
    }
    /// The text of a `speak` call as an island line. It is said at this moment, so it enters the decider's
    /// conversation as a Heimdall turn; only real playback opens the 30-second reply window.
    fn island_line(
        tools: &crate::bridge::Tools,
        actions: &Actions,
        id: Option<&str>,
        text: &str,
        op: &str,
        p: &DeliveryPermit,
    ) -> Result<String> {
        actions.notice(&format!("{op}:island-line"), text, "info", None, p)?;
        set_state_soft(actions, id, "shown", p)?;
        tools.island_line_said(&agent::reply::for_speech(text), capture::host_now_ns());
        Ok(Self::SHOWN_ON_ISLAND.into())
    }
    pub async fn action(
        &self,
        tools: &crate::bridge::Tools,
        name: &str,
        args: Value,
        op: &str,
        p: &DeliveryPermit,
    ) -> Result<String> {
        let actions = self
            .actions
            .as_ref()
            .map_err(|e| anyhow::anyhow!(e.clone()))?;
        match name {
            "island_notice" => Ok(format!(
                "Notice shown: {}.",
                actions.notice_with(
                    op,
                    args["text"].as_str().unwrap(),
                    // No agent status kinds since 19 Oct: the agent's notices are content.
                    "info",
                    args["notice_id"].as_str(),
                    args.get("detail"),
                    p
                )?
            )),
            "ask_close" => {
                let id = args["request_id"].as_str().unwrap();
                let closed = actions.close_question(id, args["answer"].as_str().unwrap(), p)?;
                // Closed, clicked or gone: it no longer takes the user's words (bridge/questions.rs).
                tools.question_settled(id);
                Ok(match closed.as_str() {
                    "closed" => "Question closed.".to_string(),
                    // The ledger says "by click" for every commit, a spoken choice's too (R12): the reply names no source.
                    "already answered by click" => "Question already answered; nothing to close.".to_string(),
                    other => format!("Question {other}; nothing to close."),
                })
            }
            "ask" => {
                // One card for one or more questions (24 Oct): the legacy `question` + `options` is one of them.
                let items = agent::actions::parse_ask(&args)?;
                let id = actions.ask(op, &items, p)?;
                // speech-policy (night 3): a card is never read aloud, in any mode ("the text to speech
                // by default reads those typed questions, which is kind of annoying"; bridge/speech_policy.rs).
                // The decider judges a spoken answer ("yeah go ahead") against the question, in both modes; the
                // question card is the visible form, so it is not repeated as an island line.
                // Once per call: a replayed call finds its context receipt already done.
                let asked = items.iter().map(|i| i.question.as_str()).collect::<Vec<_>>().join(" ");
                let context = actions.receipt(&format!("{op}:question-context"), "context", &asked, None, p)?;
                if actions.state(&context)? == "pending" {
                    tools.replies.lock().unwrap().said(&asked);
                    tools.question_asked(&asked, None, capture::host_now_ns());
                    actions.set_state(&context, "done", p)?;
                }
                Ok(format!(
                    "Question pending: request_id {id}. His click arrives at once; when he talks to you instead, the card stays open, and you call ask_close once his words answer it."
                ))
            }
            "fast_read" => crate::fast_read::start(tools, actions, &args, op, p),
            // Pointing and drawing mode (drawing.rs).
            "point" => crate::drawing::point(tools, &args, p),
            "drawing_mode" => crate::drawing::drawing_mode(tools, &args, p),
            // Bug notes (bugs.rs): Heimdall's own plain-English note for the night fixing round. Personal edition only:
            // the public contract has no `bug_note` (docs/bugs.md).
            #[cfg(not(feature = "public"))]
            "bug_note" => self.bug_notes.file(actions, &args, op, p),
            // Scheduled wakes (agent::wakes): replayed by the call's operation id; the heartbeat delivers them.
            "schedule_wake" | "list_wakes" | "cancel_wake" => {
                let wakes = self.wakes.as_ref().map_err(|e| anyhow::anyhow!(e.clone()))?;
                p.with(|| agent::wakes::tool(wakes, name, &args, op))
            }
            // Archive clips (mcp_clip.rs): a window of the archive as a movie, read-only.
            "archive_clip" => crate::mcp_clip::clip(tools, args, p).await,
            // Taste ledger (agent::taste): one judgement per call, once per operation; search reads the file.
            "taste_note" => {
                // A replayed call answers what the first did (the entry's id, which `follows` needs); a crash between
                // the append and the receipt is caught by the ledger's own repeat check.
                let receipt = actions.receipt(op, "taste_note", &args.to_string(), None, p)?;
                if let Some(line) = actions.state(&receipt)?.strip_prefix("saved:") {
                    return Ok(line.to_owned());
                }
                let ledger = agent::taste::path(&self.data);
                let archive = agent::taste::archive_path(&self.data);
                let line = p.with(|| agent::taste::note(&ledger, &args, Some(&archive)))?;
                actions.set_state(&receipt, &format!("saved:{line}"), p)?;
                Ok(line)
            }
            "taste_search" => agent::taste::search_answer(
                &agent::taste::path(&self.data),
                args["query"].as_str(),
                args["verdict"].as_str(),
                args["since"].as_str(),
                args["limit"].as_u64().unwrap_or(10) as usize,
            ),
            // Plan refresher (plan_refresher.rs): returns at once; the pass runs in the background.
            "refresh_plan" => Ok(self.plan.request()),
            // Quiet speaker naming (pipeline/voice_names.rs): renames on the live pipeline, past lines included.
            "name_voice" => {
                let (voice, name) = (args["voice"].as_str().unwrap().to_owned(), args["name"].as_str().unwrap().to_owned());
                // An archive-wide relabel of a long-heard voice can take a while on the pipeline thread.
                // Joining a voice to a listened guest would listen to it too: only in a run the user woke (listen_to.rs).
                // voice "latest": the unnamed voice that spoke last in the user's last conversation ("I was just talking
                // to Elizabeth"), in one step; the reply says what they said and who else was unnamed there.
                let latest = crate::pipeline::LATEST_VOICE.iter().any(|w| voice.trim().eq_ignore_ascii_case(w));
                match self.on_pipeline(p, 90, move |pipeline| {
                    if !latest {
                        pipeline.agent_may_name(&voice, &name)?;
                        return pipeline.rename_voice(&voice, &name);
                    }
                    let (code, quote, others) = pipeline.latest_unnamed_voice()?;
                    pipeline.agent_may_name(&code, &name)?;
                    let line = pipeline.rename_voice(&code, &name)?;
                    let said = if quote.is_empty() { String::new() } else { format!(" They last said \u{201c}{quote}\u{201d}.") };
                    let rest = if others.is_empty() { String::new() } else { format!(" Also unnamed in that conversation: {}.", others.join(", ")) };
                    Ok(format!("{line}{said}{rest}"))
                }).await? {
                    Some(Ok(line)) => Ok(line),
                    Some(Err(e)) => anyhow::bail!("NOT_NAMED: {e}"),
                    None => anyhow::bail!("VOICES_UNAVAILABLE: no answer from the pipeline yet; check timeline_search before trying again"),
                }
            }
            // Listening to guests (pipeline/listen_to.rs): only in a run the user's own words woke.
            "listen_to" => {
                let (speaker, on) = (args["speaker"].as_str().unwrap().to_owned(), args["on"].as_bool().unwrap());
                tools.speech_policy.listen_change(on.then_some(speaker.as_str()), capture::host_now_ns()).map_err(|e| anyhow::anyhow!(e))?;
                match self.on_pipeline(p, 10, move |pipeline| pipeline.set_listening(&speaker, on, "agent")).await? {
                    Some(Ok(line)) => Ok(line),
                    Some(Err(e)) => anyhow::bail!("NOT_CHANGED: {e}"),
                    None => anyhow::bail!("VOICES_UNAVAILABLE: no answer from the pipeline yet; try once more"),
                }
            }
            // ---- floating islands host (night 1022): islands/tools.rs ----
            "islands_put" | "islands_view" | "islands_remove" | "plan_edit" => {
                let host = crate::islands::host().context("ISLANDS_UNAVAILABLE: Floating Islands are not running in this Heimdall")?;
                p.with(|| match name {
                    "islands_put" => crate::islands::tools::put(&host, &args),
                    "islands_view" => crate::islands::tools::view(&host, &args),
                    "islands_remove" => crate::islands::tools::remove(&host, &args),
                    _ => crate::islands::tools::plan_edit(&host, &args),
                })
            }
            // ---- end floating islands host ----
            // Every menu setting (settings_tool.rs), through the menu's own actions; `personalise` is its older name for the
            // name, the user's name, the mark and the voice.
            "settings" | "personalise" => self.settings_call(name, args, p).await,
            "reload_me" => Ok(format!(
                "Reload scheduled: {}; conversation preserved. End this turn now; do not call reload_me again. The supervisor reloads after the turn ends and adds the resume notice automatically.",
                actions.reload_reason(op, args["reason"].as_str(), p)?
            )),
            _ => {
                let workers = self.workers.lock().unwrap().clone().context(
                    "T3_UNAVAILABLE: worker channel is not attached; Connect T3 · click",
                )?;
                workers.execute(name, args, op, p).await
            }
        }
    }
    /// Reads the pipeline's settings view with `f`; Err when the pipeline does not answer in time.
    async fn on_view(&self, p: &DeliveryPermit, f: impl FnOnce(&crate::settings_tool::View) -> Result<String> + Send + 'static) -> Result<String> {
        match self.on_pipeline(p, 10, move |pipeline| f(&pipeline.settings_view())).await? {
            Some(Ok(line)) => Ok(line),
            Some(Err(e)) => anyhow::bail!("{e}"),
            None => anyhow::bail!("NOT_CONFIRMED: Heimdall is busy; try again in a moment"),
        }
    }
    /// `settings` (list, get, set) and the older `personalise`: a change is planned on the pipeline against what the menu
    /// offers, sent through the menu's callback, then read back until it shows (at most about three seconds).
    async fn settings_call(&self, name: &str, args: Value, p: &DeliveryPermit) -> Result<String> {
        use crate::settings_tool as st;
        let personalise = name == "personalise";
        let action = if personalise {
            if args.as_object().is_none_or(|o| o.keys().all(|k| k == "wake_words")) { "personal" } else { "set" }
        } else {
            args["action"].as_str().unwrap_or("list")
        };
        let key = args["key"].as_str().map(str::to_owned);
        let item = args["item"].as_str().map(str::to_owned);
        let read = {
            let (key, item) = (key.clone(), item.clone());
            move |v: &st::View| -> Result<String> {
                if personalise {
                    return Ok(st::personal_line(v));
                }
                let key = key.as_deref().context("NOT_CHANGED: name a key")?;
                st::key(key)?;
                Ok(st::show(&v.get(key, item.as_deref())?))
            }
        };
        match action {
            "list" => self.on_view(p, |v| Ok(v.list())).await,
            "personal" => self.on_view(p, |v| Ok(format!("{}. {}", st::personal_line(v), crate::identity::choices(&v.voices)))).await,
            "get" => {
                let k = key.clone().context("NOT_CHANGED: get needs a key")?;
                self.on_view(p, move |v| {
                    let now = read(v)?;
                    let choices = v.choices(&k);
                    let what = st::key(&k)?.takes;
                    Ok(if choices.is_empty() { format!("{k}: {now} (takes {what})") } else { format!("{k}: {now} (takes {what}; choices: {choices})") })
                })
                .await
            }
            "set" => {
                let plan = Arc::new(Mutex::new(None));
                let slot = plan.clone();
                let (planned_args, plan_read) = (args.clone(), read.clone());
                let before = self
                    .on_view(p, move |v| {
                        let made = if personalise {
                            st::plan_personalise(v, &planned_args)?
                        } else {
                            let key = planned_args["key"].as_str().context("NOT_CHANGED: set needs a key")?;
                            let value = planned_args.get("value").context("NOT_CHANGED: set needs a value")?;
                            st::plan(v, key, planned_args["item"].as_str(), value, planned_args["owner_asked"].as_str())?
                        };
                        *slot.lock().unwrap() = Some(made);
                        plan_read(v)
                    })
                    .await?;
                let plan: st::Plan = plan.lock().unwrap().take().context("NOT_CHANGED: nothing was planned")?;
                let menu = self.menu.lock().unwrap().clone().context("SETTINGS_UNAVAILABLE: the menu is not running in this Heimdall")?;
                if let Some(words) = &plan.agent_off_by {
                    let (k, w) = (key.clone().unwrap_or_default(), words.clone());
                    let _ = self.on_pipeline(p, 5, move |pipeline| { pipeline.record_agent_off(&k, &w); Ok(String::new()) }).await;
                }
                if !personalise && args.get("value").is_some_and(|v| st::show(v).eq_ignore_ascii_case(&before)) {
                    return Ok(format!("{} is already {before}.", key.unwrap_or_default()));
                }
                let changes = !plan.actions.is_empty();
                let off = plan.agent_off_by.is_some();
                p.with(|| {
                    for a in plan.actions { menu(a); }
                    Ok(())
                })?;
                // Turning itself off closes this run's delivery gate: no read-back can follow.
                if off {
                    return Ok(format!("{}: switching off now, as User asked.", key.unwrap_or_default()));
                }
                // Read back until the value changes and then holds for one more read (a personalise call sends several
                // actions); a read that times out leaves the change sent but unconfirmed.
                let mut now = before.clone();
                let mut confirmed = !changes;
                for _ in 0..15 {
                    if !changes { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let r = read.clone();
                    let Ok(read_now) = self.on_view(p, move |v| r(v)).await else { continue };
                    let settled = read_now != before && read_now == now;
                    now = read_now;
                    if settled { confirmed = true; break; }
                }
                if !confirmed && now == before {
                    let what = key.clone().unwrap_or_else(|| "Personalisation".into());
                    return Ok(format!("{what}: sent to the menu, not yet shown (still {now}); check with get in a moment."));
                }
                let what = if personalise { "Personalisation".to_string() } else { key.unwrap_or_default() + &item.map(|i| format!(" ({i})")).unwrap_or_default() };
                let renamed = personalise && args.get("name").or(args.get("user_name")).is_some() || matches!(args["key"].as_str(), Some("name" | "user_name"));
                let tail = if renamed && now != before { " Your prompt reloads with the new names when this turn ends, conversation kept: end the turn now." } else { "" };
                Ok(if now == before {
                    format!("{what} stays {now}.{tail}")
                } else {
                    format!("{what}: {before} → {now}.{tail}")
                })
            }
            other => anyhow::bail!("NOT_CHANGED: unknown action {other}; list, get or set"),
        }
    }
    /// The user's answers to every question of card `id` (a click, Send, or a spoken choice); committed once.
    pub fn answer_card(&self, id: &str, answers: &[agent::actions::AskAnswer], p: &DeliveryPermit) -> Result<agent::actions::Clicked> {
        self.actions
            .as_ref()
            .map_err(|e| anyhow::anyhow!(e.clone()))?
            .answer_card(id, answers, p)
    }
}

/// One question of an `ask` card as the island draws it.
pub(crate) fn island_question(item: &agent::actions::AskItem) -> ui::agent_notice::AskQuestion {
    ui::agent_notice::AskQuestion {
        id: item.id.clone(),
        header: item.header.clone(),
        text: item.question.clone(),
        multi_select: item.multi_select,
        allow_other: item.allow_other, field: None,
        options: item.options.iter().map(|o| ui::agent_notice::AskOption { label: o.label.clone(), description: o.description.clone(), grant: grant_items(o.grant.as_ref()) }).collect(),
    }
}

/// What an option grants, as the card writes it out (approval grants, 4 Nov): each path as the hook keeps it
/// (`heimdall_emergency::grants::grant_path`: absolute, normalised), with the home folder as `~/` only when the path
/// lies inside it exactly, each login item by its name, and the new keychain file as the hook keeps it
/// (`grants::grant_keychain`, bug note 2026-11-04T23-01-22).
pub(crate) fn grant_items(grant: Option<&agent::actions::AskGrant>) -> Vec<ui::agent_notice::GrantItem> {
    let Some(g) = grant else { return vec![] };
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| "/var/empty".into());
    let vault = heimdall_home::home_dir();
    let shown = |raw: &str| match heimdall_emergency::grants::grant_path(raw, &home, &vault) {
        Ok(p) => match p.strip_prefix(&home) {
            Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
            _ => p.display().to_string(),
        },
        Err(_) => raw.trim().to_owned(),
    };
    let home_form = |p: std::path::PathBuf| match p.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    };
    let keychain = |raw: &str| heimdall_emergency::grants::grant_keychain(raw, &home, &vault).map_or_else(|_| raw.trim().to_owned(), home_form);
    g.delete.iter().map(|p| ui::agent_notice::GrantItem::Delete(shown(p)))
        .chain(g.login_items.iter().map(|n| ui::agent_notice::GrantItem::LoginItem(n.trim().to_owned())))
        .chain(g.keychain.iter().map(|k| ui::agent_notice::GrantItem::Keychain(keychain(k))))
        .collect()
}

/// The island's answer to one question, as the agent's ledger takes it.
pub(crate) fn ask_answer(a: &ui::agent_notice::QuestionAnswer) -> agent::actions::AskAnswer {
    agent::actions::AskAnswer { question_id: a.question_id.clone(), labels: a.labels.clone(), other: a.other.clone() }
}

#[cfg(test)]
struct TestCredentials;
#[cfg(test)]
impl t3::CredentialStore for TestCredentials {
    fn load(&self) -> Result<Option<t3::client::Credential>> {
        // Non-T3 app tests isolate Pi's wake stream from a startup authentication notice.
        // This token is fake and can only discover the lab's fake runtime file.
        Ok(Some(t3::client::Credential { token: "fake-app-test-token".into(), expires_ms: agent::now_ms()+30*24*60*60*1000, environment: "fake-app-test".into(), revoked: false }))
    }
    fn save(&self, _: &t3::client::Credential) -> Result<()> { anyhow::bail!("test credentials are read-only") }
}
