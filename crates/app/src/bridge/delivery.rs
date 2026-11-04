//! Delivery and notice glue: typed sends, outbox notices, agent state published to the window and island.
use super::*;

/// Background MCP outbox callback only: bounded acknowledgement from the durable
/// sink. UI callbacks use window_action and never wait for this path.
pub(super) fn notice_to(
    tx: &mpsc::UnboundedSender<Command>,
    id: String,
    text: String,
    permit: &agent::delivery::DeliveryPermit,
) -> Result<()> {
    notice_to_within(tx, id, text, permit, std::time::Duration::from_secs(10))
}
fn notice_to_within(
    tx: &mpsc::UnboundedSender<Command>,
    id: String,
    text: String,
    permit: &agent::delivery::DeliveryPermit,
    wait: std::time::Duration,
) -> Result<()> {
    permit.check()?;
    let (ack, done) = std::sync::mpsc::channel();
    tx.send(Command::Notice {
        id,
        text,
        permit: permit.clone(),
        ack,
    })
    .map_err(|_| anyhow::anyhow!("Agent supervisor stopped"))?;
    // The T3 watcher and the heartbeat call this from tasks on the agent runtime, whose two workers also run the
    // supervisor and Pi's output reader. Waiting in place held a worker for the whole wait, and the reader task queued
    // behind it: Pi's answer to the notice's own append arrived 10 s late, the control request timed out and Heimdall
    // killed Pi (21 Oct 04:00:52–04:01:02, and 20 Oct 22:08:44 by 16 ms). The wait gives the worker's tasks away first.
    let acknowledged = off_runtime(|| done.recv_timeout(wait));
    acknowledged
        .context("Agent notice was not durably acknowledged")?
        // A notice too long for the agent stays recognisable, so the outbox sends its short form instead.
        .map_err(|e| if e.contains(&agent::MessageRejected.to_string()) {anyhow::Error::new(agent::MessageRejected)} else {anyhow::Error::msg(e)})?;
    permit.check()
}
/// Runs a blocking wait without holding a worker of a multi-thread tokio runtime (its queued tasks move to another
/// thread first); elsewhere it just waits.
pub(super) fn off_runtime<T>(wait: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(wait),
        _ => wait(),
    }
}
pub(super) fn window_command(action:ui::AgentWindowAction)->Command {
    match action {
            ui::AgentWindowAction::Send { text, steer } => Command::Send { text, steer },
            ui::AgentWindowAction::Stop => Command::Cancel,
            ui::AgentWindowAction::EditAndResend {
                session_entry_id,
                text,
            } => Command::EditAndResend {
                session_entry_id,
                text,
            },
            ui::AgentWindowAction::Reload { reason, abort } => Command::Reload {
                reason,
                abort,
                permit: None,
            },
            ui::AgentWindowAction::ResolveDelivery { id, completed } => {
                Command::ResolveDelivery { id, completed }
            }
        }
}
/// The island's word on a conversation repair (`agent::history`, `runtime/history-repair.mjs`); the Agent window shows
/// the details from the session's own `heimdall_history_repair` entry. 20 Oct 19:20–19:24 the conversation was broken
/// and nothing said so.
pub(super) fn history_repair_cue(u:ui::Ui,state:&str) {
    let label=match state {
        "gave_up"=>"Conversation broken: the model refuses it even after repair",
        "restarted_from_summary"=>"Conversation rebuilt from its summary; answering again",
        "repaired_at_start"=>"Conversation repaired at start",
        _=>"Conversation repaired; answering again",
    };
    u.send(ui::UiUpdate::Health(ui::HealthCue::Notice{part:ui::Part::Agent,label:label.into()}));
}
pub(crate) fn window_problem(u:ui::Ui,text:String) {
    u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(vec![ui::AgentLiveEntry{id:format!("message-error-{}",agent::now_ms()),session_entry_id:None,timestamp_ms:agent::now_ms(),kind:ui::AgentLiveKind::Failure,text,tool_name:None,attachments:vec![],finished:true,editable:false,error:true,sent:false,queued:false,owner_text:None,usage:None}])));
}
pub(super) fn validate_message(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len()>1024*1024 {return Err(WindowActionRejected("Empty or oversized message".into()).into());}
    Ok(())
}
/// The user's typed message as the Agent window's bubble, before any database or relay work (bug note 2026-11-01T00-20-27:
/// with the archive held, Enter to bubble took seconds). The same ID throughout, so each state replaces the last; the
/// session's copy of his words replaces it in turn (`ui::agent_view`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Echo {
    /// On its way: "the user · queued".
    Pending,
    /// Handed to the agent: "the user".
    Sent,
    /// Refused: "the user · not sent".
    Failed,
}
pub(super) fn typed_echo(id: &str, text: &str, state: Echo) -> ui::UiUpdate {
    ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(vec![ui::AgentLiveEntry {
        id: id.to_string(), session_entry_id: None, timestamp_ms: agent::now_ms(), kind: ui::AgentLiveKind::Prompt,
        text: format!("[Heimdall typed message]\n{text}"), tool_name: None, attachments: vec![], finished: true, editable: false,
        error: state == Echo::Failed, sent: state == Echo::Sent, queued: state == Echo::Pending, owner_text: Some(text.to_string()), usage: None,
    }]))
}
/// `his_words`: typed by him, so they may change who Heimdall listens to; a drop onto the notch authorises nothing.
pub(super) async fn typed_delivery<T>(tools:&Arc<Tools>,text:&str,his_words:bool,delivery:impl std::future::Future<Output=Result<T>>) -> Result<T> {
    let through=tools.send_context.lock().unwrap().snapshot().through;
    let at=capture::host_now_ns();
    // His message arrived: speech stops, the ring, a short ack (bridge/typed_arrival.rs).
    tools.typed_arrived(text);
    // His typed words may change who Heimdall listens to (pipeline/listen_to.rs): known before the run can act.
    if his_words { tools.speech_policy.owner_woke(text,at); }
    let result=delivery.await?;
    tools.typed_context(text,through,at)?;
    Ok(result)
}
/// `key`: the delivery ID (the bubble's, `Command::Typed`), else a new one.
pub(super) async fn send_typed_message(
    b: &mut agent::Bridge<RecordedAdapter>,
    text: &str,
    steer: bool,
    permit: Option<agent::delivery::DeliveryPermit>,
    key: Option<&str>,
) -> Result<Option<String>> {
    // The bridge spools before Pi's readiness check. EngineUnavailable leaves
    // that same delivery queued: publish it normally, without a failed turn.
    match b.send_text(key, &format!("[Heimdall typed message]\n{text}"), steer, permit).await {
        Err(e) if e.downcast_ref::<agent::harness::EngineUnavailable>().is_some() => Ok(None),
        result => result,
    }
}
/// A drop onto the notch alone: typed like the user's own message (the Agent window and the island's conversation show it
/// as his), with its images.
pub(super) async fn send_dropped(
    b: &mut agent::Bridge<RecordedAdapter>,
    text: &str,
    images: &[Attachment],
    permit: Option<agent::delivery::DeliveryPermit>,
) -> Result<Option<String>> {
    match b.send_text_with_images(None, &format!("[Heimdall typed message]\n{text}"), images, false, permit).await {
        Err(e) if e.downcast_ref::<agent::harness::EngineUnavailable>().is_some() => Ok(None),
        result => result,
    }
}
pub(super) async fn drain_ready(b: &mut agent::Bridge<RecordedAdapter>, tools:&Tools) -> Result<()> {
    while let Ok(event) = tokio::time::timeout(std::time::Duration::from_millis(1), b.next()).await
    {
        let Some(event)=event? else {break;};
        if let Event::Other(value)=&event {
            if value["type"]=="heimdall_final_answer" {tools.agent_final_answer(value["text"].as_str().context("final answer text missing")?,value["stop_reason"].as_str().unwrap_or_default())?;}
        }
        if let Event::Complete {query:true,..}=&event {tools.agent_working(false)?;tools.replies.lock().unwrap().settled();}
        // speech-policy (night 3): only the last completion of a settled run ends the user's address; a turn his name-wake
        // restarted (night pass H2) is still running when the aborted turn's completions arrive.
        if matches!(&event,Event::Complete {query:true,..}) && !b.turn_running() {tools.speech_policy.run_settled();}
    }
    Ok(())
}
pub(super) fn publish_agent_state(
    b: &agent::Bridge<RecordedAdapter>,
    diagnostics: &heimdall_archive::records::Writer,
    s: &Settings,
    failures: u32,
    reload: Option<&str>,
    stopping: Option<bool>,
    ui: Option<ui::Ui>,
) {
    crate::diagnostics::agent_record(diagnostics, &stopping_window_state(b, s, failures, reload, stopping));
    let Some(u) = ui else {
        return;
    };
    use ui::AgentRunState as S;
    let window = stopping_window_state(b, s, failures, reload, stopping);
    let state = window.state.clone();
    u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::State(window)));
    let session = match state {
        S::Off => ui::SessionState::Stopped,
        S::Ready => ui::SessionState::Running {
            since: clock_label(),
        },
        S::Working | S::Compacting | S::Stopping { .. } => ui::SessionState::Working,
        _ => ui::SessionState::Failed(b.status.line()),
    };
    u.send(ui::UiUpdate::Agent(ui::AgentState {
        harness: ui::Harness::Pi,
        session,
    }));
    u.send(ui::UiUpdate::ComponentStatus {
        component: "agent".into(),
        // The Agent caption says "Stopping after compaction…" itself (ui menu); no second line under the tiles.
        message: if matches!(state, S::Ready | S::Working | S::Stopping { .. }) {
            None
        } else {
            Some(b.status.line())
        },
    });
}

pub(super) fn agent_window_state(
    b: &agent::Bridge<RecordedAdapter>, s: &Settings, failures: u32, reload: Option<&str>,
) -> ui::AgentWindowState {
    stopping_window_state(b, s, failures, reload, None)
}
/// `agent_window_state` while Agent off waits for crucial work (`Some(true)`: a compaction, `Some(false)`: a state
/// write; supervisor `Stopping`): the menu and window say "Stopping after compaction…", and nothing can be sent.
pub(super) fn stopping_window_state(
    b: &agent::Bridge<RecordedAdapter>, s: &Settings, failures: u32, reload: Option<&str>, stopping: Option<bool>,
) -> ui::AgentWindowState {
    relay_window_state(b, s, failures, reload, stopping, crate::emergency::RELAY_WORKING.load(std::sync::atomic::Ordering::Acquire))
}
/// `stopping_window_state` with the relay's run given (`relay_working`: a T3 relay run works for the agent).
pub(super) fn relay_window_state(
    b: &agent::Bridge<RecordedAdapter>, s: &Settings, failures: u32, reload: Option<&str>, stopping: Option<bool>, relay_working: bool,
) -> ui::AgentWindowState {
    use ui::AgentRunState as S;
    let state = if let Some(compaction) = stopping {
        S::Stopping { compaction }
    } else if s.dry_run {
        S::Off
    } else if failures >= 4 {
        S::Failed
    } else if reload.is_some() {
        S::ReloadPending
    } else {
        match b.status {
            agent::Status::ReloadPending => S::ReloadPending,
            // A relay run works for the agent (docs/claude-relay.md): Working, as for a Pi turn.
            agent::Status::Idle if relay_working => S::Working,
            agent::Status::Idle => S::Ready,
            agent::Status::Working => S::Working,
            agent::Status::WaitingEngine | agent::Status::RateLimited { .. } => S::WaitingEngine,
            agent::Status::WaitingProvider(_) => S::WaitingProvider,
            agent::Status::NeedsReconciliation(_) => S::NeedsReview,
            agent::Status::AuthBlocked(_) => S::Restarting,
            agent::Status::Stopped => S::Off,
        }
    };
    let working = matches!(&b.harness.inner,Adapter::Pi(pi) if pi.active()) && stopping.is_none();
    let state = if stopping.is_none() && matches!(&b.harness.inner,Adapter::Pi(pi) if pi.compacting()) {
        S::Compacting
    } else {
        state
    };
    ui::AgentWindowState {
        state: state.clone(),
        can_send: !s.dry_run
            && stopping.is_none()
            && failures < 4
            // Typed text waits in the queue behind an uncertain delivery; it is never refused for it.
            && !matches!(state, S::ReloadPending),
        can_stop: working,
        can_reload: !s.dry_run && stopping.is_none(),
        problem: match (&state, &b.status) {
            (S::Failed, _) => Some("The agent could not recover".into()),
            (S::NeedsReview, _) => Some("Check the interrupted wake before continuing".into()),
            (S::WaitingProvider, agent::Status::WaitingProvider(reason)) => Some(reason.clone()),
            _ => None,
        },
        unresolved: b.inflight().unwrap_or_default(),
        queued:b.queued_count().unwrap_or(0),
        reviews: if state == S::NeedsReview { b.delivery_reviews().unwrap_or_else(|e| {
            eprintln!("Could not load durable delivery review: {e:#}"); vec![]
        }).into_iter().map(delivery_review).collect() } else { vec![] },
        reload: reload.map(reload_context),
    }
}
pub(super) fn delivery_review(r: agent::DeliveryReview) -> ui::AgentDeliveryReview {
    ui::AgentDeliveryReview { id: r.id, text: r.text, timestamp_ms: r.timestamp_ms,
        attachments: r.attachments.into_iter().map(|(media_type, archive_id)| ui::AgentAttachment { media_type, archive_id }).collect(),
        text_truncated: r.text_truncated, attachments_truncated: r.attachments_truncated }
}
pub(super) fn reload_context(reason: &str) -> ui::AgentReloadPending {
    let pending = PendingReload::accepted(reason.into(), 0);
    let reason = pending.reason.split("; also requested ").map(|part| {
        if let Some((id, text)) = part.split_once(':') {
            if pending.receipts.iter().any(|receipt| receipt == id.trim()) {
                return text.trim();
            }
        }
        part
    }).collect::<Vec<_>>().join("; also requested ");
    ui::AgentReloadPending { reason, receipt_ids: pending.receipts }
}
pub(crate) fn connection_notice_ids(client: &t3::Client) -> Vec<String> {
    let key = client.connection_notice_key();
    vec![format!("t3-connect-expiry:{key}"), format!("t3-connect-401:{key}")]
}

/// Typed outcomes retain the legacy health text until the UI owner switches to ids.
pub(crate) fn notice_outcome(u: ui::Ui, id: String, outcome: ui::agent_notice::NoticeOutcome) {
    notice_outcomes(u, vec![id], outcome);
}
pub(crate) fn notice_outcomes(u: ui::Ui, ids: Vec<String>, outcome: ui::agent_notice::NoticeOutcome) {
    let Some(id) = ids.first() else { return };
    let notice = outcome_notice(id.clone(), outcome.clone());
    let answered = matches!(notice.outcome, Some(ui::agent_notice::NoticeOutcome::Answered { .. }));
    let label = notice.text.clone();
    for id in ids { u.send(ui::UiUpdate::AgentNotice(outcome_notice(id, outcome.clone()))); }
    // Successful answers already dismiss by request id; no new legacy health message needed.
    if !answered {
        u.send(ui::UiUpdate::Health(ui::HealthCue::Notice { part: ui::Part::Agent, label }));
    }
}
pub(super) fn outcome_notice(id: String, outcome: ui::agent_notice::NoticeOutcome) -> ui::agent_notice::AgentNotice {
    use ui::agent_notice::NoticeOutcome as O;
    let text = match &outcome {
        O::T3Connected => "T3 connected".to_string(),
        O::T3ConnectionFailed => "T3 connection failed; click Connect T3 to retry".into(),
        O::T3NeedsAgent => "Turn Agent on to connect T3".into(),
        O::QuestionUnavailable => "Question unavailable; ask again".into(),
        O::Answered { choice } => format!("Answered: {choice}"),
    };
    ui::agent_notice::AgentNotice::outcome(id, outcome, text)
}


#[cfg(test)]
mod notice_wait_tests {
    use super::*;
    /// 21 Oct 04:00:52 (pi-exit): a T3 notice sent from a task on the agent runtime blocked a worker until its
    /// acknowledgement, and the supervisor work that acknowledges it could not run there. With one worker the
    /// starvation is certain: before the fix the acknowledgement never arrived within the wait.
    #[test]
    fn a_notice_sent_from_a_runtime_task_is_acknowledged_without_starving_the_supervisor() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        let started = std::time::Instant::now();
        let result = rt.block_on(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
            // The supervisor: acknowledges after async work of its own (Pi's answer to the append).
            let supervisor = tokio::spawn(async move {
                while let Some(command) = rx.recv().await {
                    if let Command::Notice { ack, .. } = command {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        let _ = ack.send(Ok(()));
                    }
                }
            });
            // The T3 watcher: a task on the same runtime that sends the notice synchronously (Actions::drain).
            let sent = tokio::spawn(async move {
                notice_to_within(&tx, "terminal:t:r:completed".into(), "Task t completed.".into(), &permit, std::time::Duration::from_secs(2))
            }).await.unwrap();
            supervisor.abort();
            sent
        });
        assert!(result.is_ok(), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }
}
