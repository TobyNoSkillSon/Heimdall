//! The relay end to end without a model (docs/claude-relay.md, docs/emergency.md): a fake T3, a fake probe, the real
//! controller, the real scoped MCP server and the real `heimdall-emergency` CLI; then the real supervisor with the
//! fixture Pi on a spent quota.
use super::*;
use agent::emergency::OPENING;
use std::sync::atomic::{AtomicBool, Ordering};

const QUOTA_LINE: &str = "ChatGPT's five-hour limit is used up; it resets at 03:04.";

#[derive(Default)]
struct FakeT3 {
    /// Instances whose threads fail at once (a quota-dead harness).
    failing: Mutex<Vec<String>>,
    log: Mutex<Vec<String>>,
    /// The model each launch asked for, in order.
    models: Mutex<Vec<String>>,
    first: Mutex<Vec<(String, String)>>,
    sent: Mutex<Vec<(String, String)>>,
    /// The answer wait lasts while this is set (the cover agent is already at work through its CLI).
    hold: AtomicBool,
    /// Runs complete with the reply (else they keep working).
    complete: AtomicBool,
    /// No assistant message yet (a thread that has not written anything).
    quiet: AtomicBool,
    /// Each thread's timeline, as `activity` serves it.
    items: Mutex<Vec<(String, Item)>>,
    /// Sends fail (T3 unreachable for a moment): relayed input waits to be delivered.
    send_fails: AtomicBool,
    /// More provider instances after the three (a second Claude account).
    extra: Mutex<Vec<Value>>,
    /// Each account's live quota, as the quick scan reads it.
    quotas: Mutex<agent::routing::Quotas>,
    /// What a failing thread's run reports (T3's notice, error and reply); default: Claude Code's old reply.
    fail_text: Mutex<Option<String>>,
}
#[async_trait]
impl CoverT3 for FakeT3 {
    async fn quotas(&self) -> agent::routing::Quotas { self.quotas.lock().unwrap().clone() }
    async fn catalog(&self) -> Result<Value> {
        let mut c = json!({"providers":[
            {"providerInstanceId":"codex","driverKind":"codex","displayName":"Codex","models":[{"id":"codex-a"}],"constraints":[]},
            {"providerInstanceId":"grok","driverKind":"grok","displayName":"Grok","models":[{"id":"grok-a"}],"runtimeModes":["full-access"],"constraints":[]},
            {"providerInstanceId":"claudeAgent","driverKind":"claudeAgent","displayName":"Claude","models":[{"id":"claude-a","label":"Claude A"},{"id":"claude-b","label":"Claude B"}],"constraints":[]}
        ]});
        c["providers"].as_array_mut().unwrap().extend(self.extra.lock().unwrap().iter().cloned());
        Ok(c)
    }
    async fn launch(&self, _: &str, message: &str, h: &Harness) -> Result<(String, Option<String>)> {
        self.log.lock().unwrap().push(format!("launch:{}", h.instance));
        self.models.lock().unwrap().push(h.model.clone());
        let thread = format!("thread-{}", h.instance);
        self.first.lock().unwrap().push((thread.clone(), message.into()));
        Ok((thread, Some("run-1".into())))
    }
    async fn send(&self, thread: &str, message: &str) -> Result<Option<String>> {
        anyhow::ensure!(!self.send_fails.load(Ordering::SeqCst), "T3 is not answering");
        self.sent.lock().unwrap().push((thread.into(), message.into()));
        Ok(Some("run-2".into()))
    }
    async fn outcome(&self, thread: &str, _: Option<&str>, _: Duration) -> Result<Run> {
        while self.hold.load(Ordering::SeqCst) { tokio::time::sleep(Duration::from_millis(10)).await; }
        Ok(if self.failing.lock().unwrap().iter().any(|i| thread == format!("thread-{i}")) {
            Run::Failed(self.fail_text.lock().unwrap().clone().unwrap_or_else(|| "API Error: Claude usage limit reached".into()))
        } else if self.complete.load(Ordering::SeqCst) { Run::Completed(Some("Told User about the quota.".into())) } else { Run::Working })
    }
    async fn latest_reply(&self, _: &str) -> Result<Option<String>> { Ok((!self.quiet.load(Ordering::SeqCst)).then(|| "Told User about the quota.".into())) }
    async fn archive(&self, thread: &str) -> Result<()> { self.log.lock().unwrap().push(format!("archive:{thread}")); Ok(()) }
    async fn interrupt(&self, thread: &str) -> Result<()> { self.log.lock().unwrap().push(format!("interrupt:{thread}")); Ok(()) }
    async fn activity(&self, thread: &str, after: u64) -> Result<Vec<Item>> {
        Ok(self.items.lock().unwrap().iter().filter(|(t, i)| t == thread && i.position > after).map(|(_, i)| i.clone()).collect())
    }
}
struct FakeProbe(AtomicBool);
#[async_trait]
impl MainProbe for FakeProbe {
    async fn probe(&self, _: Option<&agent::provider::ApiModel>) -> std::result::Result<(), String> {
        if self.0.load(Ordering::SeqCst) { Ok(()) } else { Err("ChatGPT: usage limit reached".into()) }
    }
}
#[derive(Default)]
struct FakeSide { cues: Mutex<Vec<ui::HealthCue>>, said: Mutex<Vec<String>>, records: Mutex<Vec<(String, String)>>, released: Mutex<Vec<String>>, quiet_releases: Mutex<u32>, late: Mutex<Vec<String>>,
    indicators: Mutex<Vec<ui::RelayIndicator>>, sent: Mutex<u32>, working: Mutex<Vec<bool>>, replies: Mutex<Vec<String>>, pauses: Mutex<Vec<Option<Option<u64>>>>,
    no_relay: Mutex<Vec<Option<u64>>>, restored: Mutex<Vec<String>>, relay_models: Mutex<Vec<Vec<ui::RelayModel>>>,
    notices: Mutex<Vec<ui::agent_notice::AgentNotice>>, dismissed: Mutex<Vec<String>>, kept: Mutex<Vec<crate::emergency_recovery::Route>>, local: Mutex<Option<crate::emergency_recovery::Local>>,
    spent: Mutex<Vec<(String, Option<u64>)>> }
impl Side for FakeSide {
    fn indicator(&self, state: ui::RelayIndicator) { self.indicators.lock().unwrap().push(state); }
    fn sent(&self) { *self.sent.lock().unwrap() += 1; }
    fn working(&self, on: bool) { self.working.lock().unwrap().push(on); }
    fn reply(&self, text: &str) { self.replies.lock().unwrap().push(text.into()); }
    fn pause_background(&self, reset: Option<Option<u64>>) { self.pauses.lock().unwrap().push(reset); }
    fn no_relay(&self, reset: Option<u64>) { self.no_relay.lock().unwrap().push(reset); }
    fn restore_model(&self, model: &agent::provider::ApiModel) { self.restored.lock().unwrap().push(model.id.clone()); }
    fn relay_models(&self, models: Vec<ui::RelayModel>) { self.relay_models.lock().unwrap().push(models); }
    fn cue(&self, cue: ui::HealthCue) { self.cues.lock().unwrap().push(cue); }
    fn say(&self, text: &str) { self.said.lock().unwrap().push(text.into()); }
    fn record(&self, id: &str, text: &str) { self.records.lock().unwrap().push((id.into(), text.into())); }
    fn release(&self, back: Option<String>) { match back { Some(b) => self.released.lock().unwrap().push(b), None => *self.quiet_releases.lock().unwrap() += 1 } }
    fn late(&self, r: &Relayed) { self.late.lock().unwrap().push(r.text.clone()); }
    fn notice(&self, card: ui::agent_notice::AgentNotice) { self.notices.lock().unwrap().push(card); }
    fn dismiss(&self, id: &str) { self.dismissed.lock().unwrap().push(id.into()); }
    fn local_engine(&self) -> Option<crate::emergency_recovery::Local> { self.local.lock().unwrap().clone() }
    fn account_spent(&self, instance: &str, reset_ms: Option<u64>) { self.spent.lock().unwrap().push((instance.into(), reset_ms)); }
    fn keep_relay(&self, route: &crate::emergency_recovery::Route) { self.kept.lock().unwrap().push(route.clone()); }
    fn unfinished(&self, r: &Relayed) { self.late.lock().unwrap().push(format!("unfinished: {}", r.text)); }
    fn context(&self, _: usize) -> (String, String, String) { ("[23:33] To Heimdall: what's on the plan tonight?".into(), "To do:\n- Ship the build".into(), "- task t1 · 'Fix the menu' · running".into()) }
}
#[derive(Default)]
struct FakeHost(Mutex<Vec<String>>);
#[async_trait]
impl ToolHost for FakeHost {
    fn speak(&self, text: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> { self.0.lock().unwrap().push(format!("speak:{text}")); Ok(1) }
    async fn action(&self, name: &str, _: Value, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<String> { self.0.lock().unwrap().push(name.into()); Ok(format!("{name} done.")) }
}

struct Lab {
    _dir: tempfile::TempDir,
    data: PathBuf,
    em: Arc<Emergency>,
    t3: Arc<FakeT3>,
    probe: Arc<FakeProbe>,
    side: Arc<FakeSide>,
    host: Arc<FakeHost>,
    /// The agent's delivery gate, as production passes it (`tools.agent_enabled`): Agent off closes it.
    gate: Arc<agent::delivery::DeliveryGate>,
    stop: tokio::sync::watch::Sender<bool>,
    rt: tokio::runtime::Runtime,
}
impl Lab {
    fn new(failing: &[&str]) -> Self { Self::at(tempfile::tempdir().unwrap(), failing) }
    fn at(dir: tempfile::TempDir, failing: &[&str]) -> Self {
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let em = Emergency::new(&data, format!("heimdall-emergency --data-dir {}", data.display()), "/code/Heimdall/docs/emergency.md".into());
        let t3 = Arc::new(FakeT3::default());
        *t3.failing.lock().unwrap() = failing.iter().map(|s| s.to_string()).collect();
        let probe = Arc::new(FakeProbe(AtomicBool::new(false)));
        let side = Arc::new(FakeSide::default());
        let host = Arc::new(FakeHost::default());
        let (stop, rx) = tokio::sync::watch::channel(false);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
        rt.spawn(em.clone().run(t3.clone(), probe.clone(), side.clone(), host.clone(), Arc::new(agent::mcp::UnavailableArchive), gate.clone(), rx));
        Self { _dir: dir, data, em, t3, probe, side, host, gate, stop, rt }
    }
    /// The CLI as the relay thread runs it (the grant's thread in its environment).
    fn cli(&self, args: &[&str]) -> (i32, String) {
        let thread = self.connection().and_then(|c| c["thread"].as_str().map(str::to_owned));
        heimdall_emergency::run_as(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), Some(self.data.clone()), thread.as_deref())
    }
    /// One tool by its MCP name, through the CLI.
    fn call(&self, tool: &str, args: Value) -> (i32, String) { self.cli(&["call", tool, &args.to_string()]) }
    fn until(&self, what: &str, test: impl Fn(&Self) -> bool) {
        let began = std::time::Instant::now();
        while !test(self) {
            assert!(began.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn connection(&self) -> Option<Value> {
        std::fs::read(self.data.join(em::DIR).join(em::CONNECTION)).ok().map(|b| serde_json::from_slice(&b).unwrap())
    }
    fn poke(&self) { let _ = self.em.tx.lock().unwrap().send(Msg::Answered); }
    /// The newest card shown with this request id prefix and not dismissed: its id and option labels.
    fn card(&self, prefix: &str) -> Option<(String, Vec<String>)> {
        let dismissed = self.side.dismissed.lock().unwrap().clone();
        self.side.notices.lock().unwrap().iter().rev().find(|n| n.id.starts_with(prefix) && !dismissed.contains(&n.id)).and_then(|n| match &n.action {
            Some(ui::agent_notice::NoticeAction::Ask { request_id, questions, .. }) => Some((request_id.clone(), questions[0].options.iter().map(|o| o.label.clone()).collect())),
            _ => None,
        })
    }
    /// The user clicks the option starting with `label` on the open card with this prefix; the local model's id when it was that.
    fn pick(&self, prefix: &str, label: &str) -> Option<String> {
        self.until("the card is open", |l| l.card(prefix).is_some());
        let (id, options) = self.card(prefix).unwrap();
        let option = options.iter().find(|o| o.starts_with(label)).unwrap_or_else(|| panic!("{label} in {options:?}")).clone();
        self.em.answer(&id, &option)
    }
    /// The original works again: the user is asked, and goes back.
    fn go_back(&self) {
        // Never over live work: the relay's runs end first, then the next check asks.
        self.t3.complete.store(true, Ordering::SeqCst);
        self.until("the relay's runs end", |l| l.em.runs.load(Ordering::Acquire) == 0);
        if self.card(crate::emergency_recovery::RETURN_PREFIX).is_none() { self.em.set(|s| s.next_probe_ms = 0); self.poke(); }
        self.until("User is asked to go back", |l| l.card(crate::emergency_recovery::RETURN_PREFIX).is_some());
        self.pick(crate::emergency_recovery::RETURN_PREFIX, "Back to");
    }
}
fn chatgpt() -> agent::provider::ApiModel {
    serde_json::from_value(json!({"provider":agent::provider::CHATGPT,"id":"gpt-model","name":"GPT","api":serde_json::to_value(agent::provider::Api::OpenaiResponses).unwrap(),"context":0,"max_output":0})).unwrap()
}
fn quota() -> Cause { Cause::Quota { reset_ms: None, line: Some(QUOTA_LINE.into()) } }

/// The 26 Oct 23:33 outage: the ChatGPT quota fails the agent. Claude is tried first and is dead too, Codex is skipped
/// (the same ChatGPT plan), Grok answers. Its first message is the user's words, the cause, the CLI, the docs and the
/// context; its CLI reaches the cover tools only and every call enters Heimdall's record; held input reaches it in
/// order with its images; `end` hands back only when the agent answers, and the token dies with the hand-back.
#[test]
fn failover_skips_the_failed_subscription_relays_scopes_the_token_and_hands_back() {
    let lab = Lab::new(&["claudeAgent"]);
    // No emergency: the CLI refuses.
    let (code, out) = lab.call("speak", json!({"text": "hi"}));
    assert_eq!(code, 2);
    assert!(out.starts_with(heimdall_emergency::OFF), "{out}");
    lab.em.enter(quota(), Some(chatgpt()));
    assert!(lab.em.holding() || lab.em.phase() == Phase::Off);
    lab.until("a harness covers", |l| l.em.phase() == Phase::Covering);
    assert_eq!(*lab.t3.log.lock().unwrap(), ["launch:claudeAgent", "archive:thread-claudeAgent", "launch:grok"]);
    let first = lab.t3.first.lock().unwrap()[1].1.clone();
    // The relay's whole prompt first (Heimdall's, with Pi's fills and the CLI line), then the protocol.
    let (prompt, rest) = first.strip_prefix(em::PROMPT_START).and_then(|f| f.split_once(em::PROMPT_END)).unwrap();
    assert!(rest.trim_start().starts_with(OPENING), "{first}");
    assert!(prompt.contains(agent::harness::PROMPT.lines().next().unwrap()) && prompt.contains(&format!("`heimdall-emergency --data-dir {} tools`", lab.data.display())) && !prompt.contains(agent::relay_prompt::CLI_PLACEHOLDER), "{prompt}");
    for part in [QUOTA_LINE, "--data-dir", "/code/Heimdall/docs/emergency.md", "what's on the plan tonight?", "Ship the build"] { assert!(first.contains(part), "{part}"); }
    // The island: down, why, covered, nothing to do.
    assert_eq!(lab.side.cues.lock().unwrap()[0], ui::HealthCue::Notice { part: ui::Part::Agent, label: "Agent out of quota · backup agent on it, nothing to do".into() });
    let conn = lab.connection().unwrap();
    assert_eq!(conn["thread"], "thread-grok");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(lab.data.join(em::DIR).join(em::CONNECTION)).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(lab.side.records.lock().unwrap().iter().any(|(id, t)| id.starts_with(agent::notice::EMERGENCY_COVER) && t.contains("A relay agent, Grok (grok-a), took over in T3 thread thread-grok")));
    // The user, 19:50: the takeover carries the conversation, the plan and the tasks in a private file; 19:52: the triangle
    // and the background pause from the first moment.
    let context = first.lines().find_map(|l| l.split("read your context, in pages: ").nth(1)).map(|r| r.split(' ').next().unwrap().to_owned()).unwrap();
    let body = std::fs::read_to_string(&context).unwrap();
    for part in [QUOTA_LINE, "what's on the plan tonight?", "Ship the build", "Fix the menu"] { assert!(body.contains(part), "{part} in {body}"); }
    assert_eq!(std::fs::metadata(&context).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(lab.side.indicators.lock().unwrap().contains(&ui::RelayIndicator::Emergency { reset_ms: None }));
    assert_eq!(lab.side.pauses.lock().unwrap()[0], Some(None));

    // Relay: what would have woken Heimdall, with its screenshot as a private file.
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "[23:40 Mic · User] Are you there?".into(), images: vec![agent::protocol::Attachment { media_type: "image/png".into(), bytes: vec![1, 2, 3] }] });
    (lab.em.relay())(Relayed { id: "n1".into(), kind: "result".into(), text: "Task done".into(), images: vec![] });
    lab.until("both relayed", |l| l.t3.sent.lock().unwrap().len() == 2);
    // Bug note 2026-10-27 19:48: the user's relayed words animate the send arrow; a task result does not.
    assert_eq!(*lab.side.sent.lock().unwrap(), 1);
    assert!(lab.side.working.lock().unwrap().first() == Some(&true), "a relay run works on the island");
    let sent = lab.t3.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, "thread-grok");
    assert!(sent[0].1.starts_with("[For Heimdall, User said] [23:40 Mic · User] Are you there?"), "{}", sent[0].1);
    let image = sent[0].1.lines().find_map(|l| l.strip_prefix("Image: ")).unwrap().to_owned();
    assert_eq!(std::fs::read(&image).unwrap(), [1, 2, 3]);
    assert_eq!(std::fs::metadata(&image).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(sent[1].1.starts_with("[For Heimdall, a task finished] Task done"));

    // The CLI: every tool Heimdall's agent has (audit 27 Oct: the cover had 15 of 35), each call recorded.
    let (code, list) = lab.cli(&["tools"]);
    assert_eq!(code, 0);
    let contract = agent::mcp_contract::tools();
    let names: Vec<&str> = contract.as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect::<Vec<_>>();
    for name in &names { assert!(list.lines().any(|l| l.starts_with(&format!("{name}:"))), "{name} listed"); }
    assert!(list.contains("emergency_status:"));
    assert_eq!(lab.call("speak", json!({"text": "Heimdall's agent is out of quota; I'm covering."})), (0, "Speech queued: 1.".into()));
    // Bug notes: personal edition only; the public cover token refuses them (docs/bugs.md).
    let bug = lab.call("bug_note", json!({"text": "Agent quota ran out at 23:33"}));
    if cfg!(feature = "public") { assert_ne!(bug.0, 0, "{bug:?}") } else { assert_eq!(bug.0, 0) }
    let (code, out) = lab.cli(&["call", "no_such_tool", "{}"]);
    assert_eq!(code, 1);
    assert!(out.contains("not available to this agent"), "{out}");
    let (code, out) = lab.call("emergency_status", json!({}));
    assert!(code == 0 && out.contains(QUOTA_LINE) && out.contains("thread-grok"), "{out}");
    let ran: &[&str] = if cfg!(feature = "public") { &["speak:Heimdall's agent is out of quota; I'm covering."] } else { &["speak:Heimdall's agent is out of quota; I'm covering.", "bug_note"] };
    assert_eq!(*lab.host.0.lock().unwrap(), ran);
    let records: Vec<String> = lab.side.records.lock().unwrap().iter().map(|(_, t)| t.clone()).collect();
    assert!(records.iter().any(|t| t == "[Relay agent] said to User: \"Heimdall's agent is out of quota; I'm covering.\" → Speech queued: 1."), "{records:#?}");
    assert_eq!(records.iter().any(|t| t.starts_with("[Relay agent] filed a bug note: Agent quota ran out")), !cfg!(feature = "public"));

    // Hand back: refused while the agent still fails, accepted once it answers; then the token is dead.
    let (code, out) = lab.call("emergency_end", json!({"summary": "Told User."}));
    assert!(code == 0 && out.starts_with("Heimdall's agent still fails (ChatGPT: usage limit reached). Keep covering"), "{out}");
    assert_eq!(lab.em.phase(), Phase::Covering);
    lab.probe.0.store(true, Ordering::SeqCst);
    // The user, 06:34: going back is his choice. The relay's `end` makes Heimdall ask him; he goes back.
    let (code, out) = lab.call("emergency_end", json!({"summary": "Told User and filed a bug note."}));
    assert_eq!((code, out.as_str()), (0, "Heimdall's agent answers again. User will be asked whether to go back to it once your run ends; keep acting for him until you are told."));
    assert_eq!(lab.em.phase(), Phase::Covering, "nothing switched by itself");
    lab.go_back();
    assert!(lab.side.said.lock().unwrap().iter().any(|l| l.starts_with("Your built-in agent works again")), "asked by voice");
    lab.until("the hold is released", |l| !l.side.released.lock().unwrap().is_empty());
    assert_eq!(lab.em.phase(), Phase::Trial);
    assert!(!lab.em.holding());
    let back = lab.side.released.lock().unwrap()[0].clone();
    // The user, 20:44: no hand-off summary: the relay's conversation is already Pi's.
    assert!(!back.contains("Its hand-back") && !back.contains("What it did"), "{back}");
    for part in ["You are back", QUOTA_LINE, "Grok (grok-a) acted as you in T3 thread thread-grok", "in your conversation above, in order"] {
        assert!(back.contains(part), "{part} in {back}");
    }
    assert!(lab.connection().is_none(), "the token file goes with the hand-back");
    let (code, out) = lab.call("speak", json!({"text": "still here"}));
    assert_eq!(code, 2);
    assert!(out.starts_with(heimdall_emergency::OFF), "{out}");
    assert_eq!(lab.host.0.lock().unwrap().len(), ran.len(), "nothing ran after the revoke");
    // The agent's hand-back turn answers: the user hears one line.
    lab.em.answered();
    lab.until("emergency mode is off", |l| l.em.phase() == Phase::Off);
    let said = lab.side.said.lock().unwrap().clone();
    assert!(said[0].starts_with("Your built-in agent on GPT is out of quota. In a minute I'll switch to "), "the card, said: {said:?}");
    assert_eq!(said[1..], ["Your built-in agent works again. Do you want to go back to it, or stay where we are?", "I'm back."]);
    assert!(lab.side.cues.lock().unwrap().contains(&ui::HealthCue::Recovered { key: "emergency".into(), part: ui::Part::Agent, label: "Agent back".into() }));
    assert!(!lab.data.join(em::DIR).join("images").exists());
    let _ = lab.stop.send(true);
}

/// A probe passes but the hand-back turn fails again: the same thread covers again with a new token, and is told so;
/// no new thread is launched.
#[test]
fn a_failed_hand_back_puts_the_same_cover_thread_back_on_duty() {
    let lab = Lab::new(&[]);
    lab.em.enter(Cause::NoAnswer, None);
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
    let token = lab.connection().unwrap()["token"].clone();
    lab.probe.0.store(true, Ordering::SeqCst);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.go_back();
    lab.until("the probe passed and the hold is released", |l| l.em.phase() == Phase::Trial && !l.side.released.lock().unwrap().is_empty());
    assert!(lab.t3.sent.lock().unwrap().iter().any(|(t, m)| t == "thread-claudeAgent" && m.starts_with("Heimdall's agent answers again. Wrap up")));
    assert!(lab.side.released.lock().unwrap()[0].contains("in your conversation above"));
    lab.em.enter(quota(), None);
    lab.until("covering again and told so", |l| l.em.phase() == Phase::Covering && l.connection().is_some()
        && l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.starts_with("Heimdall's agent failed again right after it seemed back")));
    assert_eq!(lab.t3.log.lock().unwrap().iter().filter(|l| l.starts_with("launch:")).count(), 1);
    assert_ne!(lab.connection().unwrap()["token"], token);
    assert!(lab.t3.sent.lock().unwrap().iter().any(|(_, m)| m.starts_with("Heimdall's agent failed again right after it seemed back")));
    assert!(!lab.side.said.lock().unwrap().iter().any(|l| l == "I'm back."), "not back yet");
    let _ = lab.stop.send(true);
}

/// No harness answers: the island says so and launch is retried; held input waits and reaches the first thread that
/// answers. Agent off stands it down and the token dies.
#[test]
fn without_a_cover_input_waits_for_one_and_agent_off_stands_it_down() {
    let lab = Lab::new(&["claudeAgent", "grok"]);
    lab.em.enter(quota(), Some(chatgpt()));
    lab.until("no cover", |l| l.em.phase() == Phase::NoCover);
    // The user, 19:55: no relay can drive Heimdall: the triangle and the local engine's offer.
    assert_eq!(*lab.side.no_relay.lock().unwrap(), [None]);
    assert!(lab.connection().is_none(), "no live grant without a thread");
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "Hello?".into(), images: vec![] });
    lab.t3.failing.lock().unwrap().retain(|i| i != "grok");
    lab.until("a relaunch covers", |l| l.em.phase() == Phase::Covering);
    lab.until("the held words arrive", |l| l.t3.sent.lock().unwrap().iter().any(|(t, m)| t == "thread-grok" && m.contains("Hello?")));
    lab.em.end("User switched Heimdall's agent off");
    lab.until("off", |l| l.em.phase() == Phase::Off);
    assert!(lab.t3.sent.lock().unwrap().last().unwrap().1.starts_with("Stop acting now: User switched Heimdall's agent off"));
    // Only `end` for `END_GRACE_MS`, then nothing.
    lab.until("the token dies", |l| l.connection().is_none());
    assert_eq!(lab.side.said.lock().unwrap().len(), 1, "only the card's line: {:?}", lab.side.said.lock().unwrap());
    let _ = lab.stop.send(true);
}

/// Heimdall restarts in the middle of an emergency: input is held from the first moment, the same thread covers with
/// a new token, and the agent is checked within a minute.
#[test]
fn a_restart_keeps_the_emergency_and_its_thread() {
    let lab = Lab::new(&[]);
    lab.em.enter(quota(), None);
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
    let token = lab.connection().unwrap()["token"].clone();
    let _ = lab.stop.send(true);
    lab.until("the token dies with the process", |l| l.connection().is_none());
    let Lab { _dir, rt, .. } = lab;
    rt.shutdown_timeout(Duration::from_secs(2));
    let again = Emergency::new(&_dir.path().join("data"), "cli".into(), "docs".into());
    assert!(again.holding(), "held before the controller runs");
    let lab = Lab::at(_dir, &[]);
    lab.until("covering after the restart", |l| l.em.phase() == Phase::Covering && l.connection().is_some());
    let conn = lab.connection().unwrap();
    assert_eq!(conn["thread"], "thread-claudeAgent");
    assert_ne!(conn["token"], token);
    assert!(lab.em.state.lock().unwrap().next_probe_ms <= agent::now_ms() + 30_000);
    assert!(lab.t3.log.lock().unwrap().is_empty(), "no new launch");
    let _ = lab.stop.send(true);
}

/// Bug note 2026-11-02T23-28-44: the relay thread outlives Heimdall's restarts and keeps the tool definitions it loaded,
/// so after an update it called plan_edit as it was before `quote`. Its next input after the restart names the tools
/// that changed and how to load them again, once; a thread that took over on this build is told nothing.
#[test]
fn after_an_update_the_relay_thread_is_told_once_which_tools_changed() {
    let lab = Lab::new(&[]);
    lab.em.enter(quota(), None);
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
    (lab.em.relay())(Relayed { id: "w0".into(), kind: "wake".into(), text: "Before".into(), images: vec![] });
    lab.until("relayed", |l| l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.contains("Before")));
    assert!(lab.t3.sent.lock().unwrap().iter().all(|(_, m)| !m.contains("[Heimdall was updated]")), "a thread on this build learned the tools at takeover");
    let _ = lab.stop.send(true);
    lab.until("the token dies with the process", |l| l.connection().is_none());
    let Lab { _dir, rt, .. } = lab;
    rt.shutdown_timeout(Duration::from_secs(2));
    // The thread learned an older plan_edit (the build before the update).
    let file = _dir.path().join("data").join(em::DIR).join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert!(state["tools"]["plan_edit"].is_string(), "the takeover recorded the tools: {}", state["tools"]);
    state["tools"]["plan_edit"] = json!("0000000000000000");
    std::fs::write(&file, state.to_string()).unwrap();
    let lab = Lab::at(_dir, &[]);
    lab.until("covering after the restart", |l| l.em.phase() == Phase::Covering && l.connection().is_some());
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "typed".into(), text: "schedule website work tomorrow".into(), images: vec![] });
    (lab.em.relay())(Relayed { id: "w2".into(), kind: "typed".into(), text: "thanks".into(), images: vec![] });
    lab.until("both relayed", |l| l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.contains("thanks")));
    let sent = lab.t3.sent.lock().unwrap().clone();
    let first = sent.iter().find(|(_, m)| m.contains("schedule website work")).unwrap().1.clone();
    assert!(first.starts_with("[Heimdall was updated] These tools changed: plan_edit. ") && first.contains("select:mcp__heimdall__plan_edit)"), "{first}");
    assert!(first.ends_with("[For Heimdall, User typed] schedule website work tomorrow"), "{first}");
    let second = sent.iter().find(|(_, m)| m.contains("thanks")).unwrap().1.clone();
    assert!(second.starts_with("[For Heimdall, User typed] thanks"), "told once: {second}");
    // Heimdall's own message: the mirror leaves it out (the input is in Pi's session already).
    let item = Item { position: 9, kind: "user_message".into(), text: first.clone(), status: "completed".into() };
    assert_eq!(super::mirror_text(&item, "heimdall-emergency"), None, "{first}");
    let _ = lab.stop.send(true);
}

#[test]
fn the_tools_note_names_what_changed_and_says_nothing_otherwise() {
    let now = super::tool_digests();
    assert!(now.contains_key("plan_edit") && now.len() > 20, "{now:?}");
    assert_eq!(super::tools_note(&now, &now), None);
    let mut old = now.clone();
    old.remove("taste_note");
    old.insert("plan_edit".into(), "x".into());
    old.insert("gone_tool".into(), "y".into());
    let note = super::tools_note(&old, &now).unwrap();
    assert!(note.contains("These tools changed: plan_edit, taste_note.") && note.contains("select:mcp__heimdall__plan_edit,mcp__heimdall__taste_note)"), "{note}");
    assert!(super::tools_note(&Default::default(), &now).unwrap().contains("may have changed"));
    assert!(note.contains("Without ToolSearch"), "a non-Claude relay can act on it too: {note}");
}

/// Fresh review, 3 Nov: a thread reused within REUSE_MS keeps the tools it learned, so an update between its stand-down
/// and its reuse is still told; a new thread learns them as they are.
#[test]
fn a_reused_thread_keeps_the_tools_it_learned() {
    let mut s = State::default();
    s.tools_thread = Some("thread-1".into());
    s.tools.insert("plan_edit".into(), "old".into());
    s.learn_tools("thread-1");
    assert_eq!(s.tools["plan_edit"], "old", "reused: what it learned stays");
    s.learn_tools("thread-2");
    assert_eq!(s.tools, super::tool_digests(), "new: as they are now");
}

/// The real supervisor and the fixture Pi on a spent ChatGPT quota: the failure turns emergency mode on at once, and
/// the user's next words become context in Pi's session (marked held) without starting a turn.
#[test]
fn a_spent_quota_holds_the_agent_s_turns_and_keeps_its_record() -> Result<()> {
    use crate::bridge::{AgentRuntime, Command, Tools};
    let d = tempfile::tempdir()?;
    let data = d.path().join("data");
    let session = data.join("archive/session-emergency");
    std::fs::create_dir_all(&data)?;
    let tools = Arc::new(Tools::new(d.path(), &data));
    crate::bridge::test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
    let runtime = AgentRuntime::start(crate::settings::Settings { dry_run: false, ..Default::default() }, data.clone(), session.clone(), tools.clone(), None);
    crate::bridge::test_harness(None);
    let runtime = runtime?;
    runtime.barrier()?;
    std::fs::write(data.join("provider-quota"), "")?;
    runtime.send(Command::Send { text: "Heimdall, what's on the plan?".into(), steer: false })?;
    let records = |kind: &str| -> Result<Vec<heimdall_archive::Record>> { Ok(heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().filter(|r| r.kind == kind).collect()) };
    let began = std::time::Instant::now();
    while records("agent_emergency")?.is_empty() {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(10), "emergency mode did not turn on");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(records("agent_emergency")?[0].payload["cause"]["kind"], "quota");
    let sends = records("agent_send")?.len();
    runtime.send(Command::Send { text: "Are you there?".into(), steer: false })?;
    let pi = data.join("pi-session.jsonl");
    let began = std::time::Instant::now();
    while !std::fs::read_to_string(&pi).unwrap_or_default().lines().any(|l| l.contains("heimdall_context") && l.contains("this reached the relay agent") && l.contains("Are you there?")) {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(10), "the held words never reached Pi's session");
        std::thread::sleep(Duration::from_millis(20));
    }
    runtime.barrier()?;
    assert!(!records("agent_send")?.iter().skip(sends).any(|r| r.payload.to_string().contains("Are you there?")), "a held message starts no turn");
    assert!(tools.emergency.holding());
    assert!(tools.test_island.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::Health(ui::HealthCue::Notice { label, .. }) if label.starts_with("Agent out of quota"))));
    drop(runtime);
    Ok(())
}

/// Bug note 2026-10-28T17-29-59: relay mode on, a notch drop with no speech (`Pipeline::send_drop_alone`'s command)
/// reaches the real supervisor's hold: one held context entry in Pi's session with its image counted (the same
/// `hold_input` hands it to the relay once), a `notch_drop` journal record, and no turn of Pi's.
#[test]
fn relay_mode_holds_a_notch_drop_alone_as_relayed_context_not_a_turn() -> Result<()> {
    use crate::bridge::{AgentRuntime, Command, Tools};
    let d = tempfile::tempdir()?;
    let data = d.path().join("data");
    let session = data.join("archive/session-relay-drop");
    std::fs::create_dir_all(&data)?;
    let tools = Arc::new(Tools::new(d.path(), &data));
    crate::bridge::test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
    let runtime = AgentRuntime::start(crate::settings::Settings { dry_run: false, agent_relay: true, ..Default::default() }, data.clone(), session.clone(), tools.clone(), None);
    crate::bridge::test_harness(None);
    let runtime = runtime?;
    runtime.barrier()?;
    assert!(tools.emergency.holding() && tools.emergency.reason() == Reason::Chosen, "relay mode holds from the start");
    let records = |kind: &str| -> Result<Vec<heimdall_archive::Record>> { Ok(heimdall_archive::Journal::open_read_only(&session)?.records()?.into_iter().filter(|r| r.kind == kind).collect()) };
    let sends = records("agent_send")?.len();
    let text = "User dropped onto the notch:\n- link: https://huggingface.co/LiquidAI/d1-2B";
    runtime.send(Command::Dropped { text: text.into(), images: vec![agent::protocol::Attachment { media_type: "image/png".into(), bytes: vec![1, 2, 3] }] })?;
    let pi = data.join("pi-session.jsonl");
    let held = || std::fs::read_to_string(&pi).unwrap_or_default().lines().filter(|l| l.contains("heimdall_context") && l.contains("this reached the relay agent") && l.contains("User dropped onto the notch")).map(str::to_owned).collect::<Vec<_>>();
    let began = std::time::Instant::now();
    while held().is_empty() {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(10), "the drop never reached Pi's session as held context");
        std::thread::sleep(Duration::from_millis(20));
    }
    runtime.barrier()?;
    let held = held();
    assert_eq!(held.len(), 1, "once: {held:#?}");
    assert!(held[0].contains("huggingface.co/LiquidAI/d1-2B") && held[0].contains("[1 image(s), shown to the cover agent]"), "{}", held[0]);
    assert_eq!(records("notch_drop")?.len(), 1);
    assert!(!records("agent_send")?.iter().skip(sends).any(|r| r.payload.to_string().contains("User dropped onto the notch")), "a held drop starts no turn");
    drop(runtime);
    Ok(())
}

/// Live lab (never in ordinary runs): the real T3 Code on this Mac, the real controller, scoped server and the built
/// `heimdall-emergency` CLI, with a recording host (nothing is said aloud) and isolated data. The failure is the
/// detector fed the 26 Oct 23:33 quota error, as the supervisor feeds it (the supervisor path itself is covered with
/// the fixture Pi above). The cover thread's first message is prefixed with a drill notice so the real agent only runs
/// the CLI. T3 pairs this lab into memory (a "Heimdall" session in T3 ▸ Connections the operator revokes after).
///
/// Run: HEIMDALL_EMERGENCY_LIVE_LAB=<abs empty dir> HEIMDALL_EMERGENCY_CLI=<abs heimdall-emergency> \
///      cargo test -p app --bin heimdall -- --ignored --nocapture emergency_live_lab
#[test]
#[ignore]
fn emergency_live_lab() {
    struct Memory(Mutex<Option<t3::client::Credential>>);
    impl t3::CredentialStore for Memory {
        fn load(&self) -> Result<Option<t3::client::Credential>> { Ok(self.0.lock().unwrap().clone()) }
        fn save(&self, c: &t3::client::Credential) -> Result<()> { *self.0.lock().unwrap() = Some(c.clone()); Ok(()) }
    }
    /// The real T3, with the drill notice on top of the first message.
    struct Drill { inner: T3Cover, cli: String, threads: Mutex<Vec<String>> }
    #[async_trait]
    impl CoverT3 for Drill {
        async fn catalog(&self) -> Result<Value> { self.inner.catalog().await }
        async fn launch(&self, title: &str, message: &str, h: &Harness) -> Result<(String, Option<String>)> {
            let drill = format!("LAB DRILL. This is an automated test of Heimdall's emergency mode in an isolated lab; Heimdall and User are fine. Do exactly this and nothing else: call Heimdall's speak with the text \"Lab drill: the cover agent is online.\", then emergency_status (as mcp__heimdall__ tools, or `{cli} call speak '{{\"text\":\"Lab drill: the cover agent is online.\"}}'` and `{cli} call emergency_status`), reply with their two outputs, and stop. Touch no files and no other threads; speak to nobody else.\n\nThe real protocol message follows for the record:\n\n", cli = self.cli);
            let r = self.inner.launch(&format!("{title} (lab drill)"), &(drill + message), h).await?;
            self.threads.lock().unwrap().push(r.0.clone());
            Ok(r)
        }
        async fn send(&self, thread: &str, message: &str) -> Result<Option<String>> { self.inner.send(thread, message).await }
        async fn outcome(&self, thread: &str, run: Option<&str>, wait: Duration) -> Result<Run> { self.inner.outcome(thread, run, wait).await }
        async fn latest_reply(&self, thread: &str) -> Result<Option<String>> { self.inner.latest_reply(thread).await }
        async fn archive(&self, thread: &str) -> Result<()> { self.inner.archive(thread).await }
        async fn interrupt(&self, thread: &str) -> Result<()> { self.inner.interrupt(thread).await }
        async fn activity(&self, thread: &str, after: u64) -> Result<Vec<Item>> { self.inner.activity(thread, after).await }
    }
    let Some(root) = std::env::var_os("HEIMDALL_EMERGENCY_LIVE_LAB").map(PathBuf::from) else { return };
    let bin = std::env::var("HEIMDALL_EMERGENCY_CLI").expect("HEIMDALL_EMERGENCY_CLI");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let cli = format!("{bin} --data-dir {}", data.display());
    let client = Arc::new(t3::Client::local(Arc::new(Memory(Mutex::new(None)))).unwrap());
    let t3 = Arc::new(Drill { inner: T3Cover(client), cli: cli.clone(), threads: Mutex::new(vec![]) });
    let em = Emergency::new(&data, cli, concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/emergency.md").into());
    let probe = Arc::new(FakeProbe(AtomicBool::new(false)));
    let side = Arc::new(FakeSide::default());
    let host = Arc::new(FakeHost::default());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.spawn(em.clone().run(t3.clone(), probe.clone(), side.clone(), host.clone(), Arc::new(agent::mcp::UnavailableArchive), Arc::new(agent::delivery::DeliveryGate::new(true)), rx));
    let wait = |what: &str, secs: u64, test: &dyn Fn() -> bool| {
        let began = std::time::Instant::now();
        while !test() { assert!(began.elapsed() < Duration::from_secs(secs), "{what}"); std::thread::sleep(Duration::from_millis(250)); }
        println!("lab: {what} after {:.1} s", began.elapsed().as_secs_f64());
    };
    let mut detector = em::Detector::default();
    let cause = detector.failed("The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.", agent::now_ms()).expect("a spent quota fails the agent");
    em.enter(cause, None);
    wait("a T3 harness covers", 240, &|| em.phase() == Phase::Covering);
    let s = em.state.lock().unwrap().clone();
    println!("lab: cover {} in thread {}", s.harness.unwrap(), s.thread.clone().unwrap());
    wait("the cover agent speaks through the CLI", 300, &|| host.0.lock().unwrap().iter().any(|c| c.starts_with("speak:")));
    println!("lab: host calls {:?}", host.0.lock().unwrap());
    println!("lab: recorded {:?}", side.records.lock().unwrap().iter().map(|(_, t)| t.clone()).collect::<Vec<_>>());
    (em.relay())(Relayed { id: "lab-1".into(), kind: "wake".into(), text: "[lab] User said: this is a relayed drill line; no action needed.".into(), images: vec![] });
    std::thread::sleep(Duration::from_secs(3));
    probe.0.store(true, Ordering::SeqCst);
    em.set(|s| s.next_probe_ms = 0);
    let _ = em.tx.lock().unwrap().send(Msg::Answered);
    wait("the probe passes and the hold is released", 60, &|| em.phase() == Phase::Trial);
    assert!(std::fs::metadata(data.join(em::DIR).join(em::CONNECTION)).is_err(), "token revoked");
    let (code, out) = heimdall_emergency::run_as(&["call".to_string(), "emergency_status".to_string()], Some(data.clone()), None);
    println!("lab: CLI after hand-back: {code} {out}");
    assert_eq!(code, 2);
    em.answered();
    wait("emergency mode ends", 30, &|| em.phase() == Phase::Off);
    println!("lab: said {:?}", side.said.lock().unwrap());
    println!("lab: back notice: {}", side.released.lock().unwrap()[0]);
    for thread in t3.threads.lock().unwrap().iter() {
        let archived = rt.block_on(t3.archive(thread));
        println!("lab: archived {thread}: {}", if archived.is_ok() { "now" } else { "already" });
    }
    let _ = stop.send(true);
}

/// Review 27 Oct: Agent off lifts the hold (no hand-back turn); input relayed after emergency mode ended reaches the
/// agent as late input; `end` outside covering is refused; a failed hand-back turn re-enters without counting.
#[test]
fn standing_down_releases_the_hold_late_input_wakes_the_agent_and_a_failed_trial_re_enters() {
    let lab = Lab::new(&[]);
    lab.em.enter(quota(), None);
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
    // Agent off as the supervisor sees it: the agent's gate closes, then emergency mode ends (review 27 Oct: the
    // grace server checked this gate, so every `end` got 503).
    lab.gate.intent(agent::delivery::StopReason::Agent, false);
    lab.em.end("User switched Heimdall's agent off");
    lab.until("off, hold lifted", |l| l.em.phase() == Phase::Off && *l.side.quiet_releases.lock().unwrap() == 1);
    assert!(lab.side.released.lock().unwrap().is_empty(), "no hand-back turn");
    // Bug note 2026-10-27T02-16-00: the cover's last `end` was refused by a dead token. For `END_GRACE_MS` it is
    // recorded; nothing else runs.
    let (code, out) = lab.call("speak", json!({"text": "still covering"}));
    assert_eq!(code, 1, "{out}");
    assert_eq!(lab.call("emergency_end", json!({"summary": "Told User the quota resets at 03:04."})), (0, "Recorded for Heimdall's agent. The relay is over; you are done.".into()));
    assert!(lab.side.records.lock().unwrap().iter().any(|(id, t)| id.starts_with(agent::notice::EMERGENCY_COVER) && t == "[Relay agent, after standing down] Told User the quota resets at 03:04."));
    assert!(lab.host.0.lock().unwrap().is_empty());
    lab.until("the grace token dies", |l| l.connection().is_none());
    (lab.em.relay())(Relayed { id: "w7".into(), kind: "wake".into(), text: "Anyone?".into(), images: vec![] });
    lab.until("late input passed on", |l| l.side.late.lock().unwrap().iter().any(|t| t == "Anyone?"));
    let (tx, rx) = oneshot::channel();
    let _ = lab.em.tx.lock().unwrap().send(Msg::HandBack("done".into(), tx));
    let answer = lab.rt.block_on(rx).unwrap();
    assert!(answer.starts_with("Nothing to hand back now"), "{answer}");
    // A trial's failed turn: one 503 is enough.
    lab.em.set(|s| s.phase = Phase::Trial);
    let mut d = em::Detector::default();
    let failed = agent::protocol::Event::Complete { id: None, query: true, error: true, text: "503 Service Unavailable".into() };
    assert_eq!(signal(&mut d, &failed, &lab.em), Some(Cause::Provider("503 Service Unavailable".into())));
    lab.em.set(|s| s.phase = Phase::Off);
    assert_eq!(signal(&mut d, &failed, &lab.em), None, "outside a trial one 503 is a blip");
    let _ = lab.stop.send(true);
}

/// Bug note 2026-10-27T02-16-00 parts 2 and 3: the cover agent speaks through its CLI while Heimdall still waits to
/// see it answer. `status` named it "?" then, and `covering` re-issued the token, retiring the server mid-speech
/// ("Speech failed: MCP server retired", 02:12:11). The server it was given stays, and only learns the thread.
#[test]
fn the_cover_keeps_its_server_when_it_is_confirmed_and_status_names_it_meanwhile() {
    let lab = Lab::new(&[]);
    lab.t3.hold.store(true, Ordering::SeqCst);
    lab.em.enter(quota(), None);
    // The grant names the thread as soon as T3 returns it, before the thread shows it answers (its MCP server serves
    // only that thread).
    lab.until("a thread is being tried", |l| l.connection().is_some_and(|c| c["thread"] == "thread-claudeAgent") && l.em.state.lock().unwrap().trying.is_some());
    assert_eq!(lab.em.phase(), Phase::Starting, "named while still being tried");
    let given = lab.connection().unwrap();
    let (code, out) = lab.call("emergency_status", json!({}));
    assert!(code == 0 && out.contains("You cover as Claude (claude-a) in T3 thread thread-claudeAgent."), "{out}");
    assert!(out.contains("reset time is unknown; the next check is not the reset"), "{out}");
    assert_eq!(lab.call("speak", json!({"text": "I'm covering."})), (0, "Speech queued: 1.".into()));
    lab.t3.hold.store(false, Ordering::SeqCst);
    lab.until("covering", |l| l.em.phase() == Phase::Covering && l.connection().is_some_and(|c| c["thread"] == "thread-claudeAgent"));
    let kept = lab.connection().unwrap();
    assert_eq!((&kept["url"], &kept["token"]), (&given["url"], &given["token"]), "the server the cover was given");
    assert_eq!(lab.call("speak", json!({"text": "Still here."})), (0, "Speech queued: 1.".into()));
    assert_eq!(*lab.host.0.lock().unwrap(), ["speak:I'm covering.", "speak:Still here."]);
    let _ = lab.stop.send(true);
}

/// Bug note 2026-10-27T02-16-00 part 1 (night audit H6): Quit closed the delivery gate and the supervisor took it for
/// Agent off, ending emergency mode. Only Agent off stands the cover down; on Quit the emergency survives the restart.
#[test]
fn quit_keeps_emergency_mode_and_agent_off_ends_it() -> Result<()> {
    use crate::bridge::{AgentRuntime, Command, Tools};
    let d = tempfile::tempdir()?;
    let data = d.path().join("data");
    let session = data.join("archive/session-emergency");
    std::fs::create_dir_all(&data)?;
    let tools = Arc::new(Tools::new(d.path(), &data));
    crate::bridge::test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
    let runtime = AgentRuntime::start(crate::settings::Settings { dry_run: false, ..Default::default() }, data.clone(), session.clone(), tools.clone(), None);
    crate::bridge::test_harness(None);
    let runtime = runtime?;
    runtime.barrier()?;
    std::fs::write(data.join("provider-quota"), "")?;
    runtime.send(Command::Send { text: "Heimdall, what's on the plan?".into(), steer: false })?;
    let began = std::time::Instant::now();
    while tools.emergency.phase() == Phase::Off {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(10), "emergency mode did not turn on");
        std::thread::sleep(Duration::from_millis(20));
    }
    tools.agent_enabled.intent(agent::delivery::StopReason::Quit, false);
    runtime.barrier()?;
    std::thread::sleep(Duration::from_millis(500));
    assert_ne!(tools.emergency.phase(), Phase::Off, "Quit is not Agent off");
    let saved: Value = serde_json::from_slice(&std::fs::read(data.join(em::DIR).join("state.json"))?)?;
    assert_ne!(saved["phase"], "off", "the next launch resumes it");
    tools.agent_enabled.intent(agent::delivery::StopReason::Agent, false);
    let began = std::time::Instant::now();
    while tools.emergency.phase() != Phase::Off {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(10), "Agent off did not end emergency mode");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(runtime);
    Ok(())
}

/// Bug note 2026-10-27T19-42-14 (the 19:40 notice leak): the hold relayed the wake the failed request left unanswered
/// before the controller had heard of the emergency; it was taken for late input and started a Pi turn at 19:40:30.
/// Now the phase is `Starting` the moment `enter` returns, so input relayed right after it is held for the relay.
#[test]
fn input_relayed_right_after_the_failure_reaches_the_relay_not_the_agent() {
    let lab = Lab::new(&[]);
    lab.em.enter(quota(), Some(chatgpt()));
    assert!(lab.em.holding(), "held before the controller runs");
    (lab.em.relay())(Relayed { id: "w0".into(), kind: "wake".into(), text: "Hai mdelholo?".into(), images: vec![] });
    lab.until("the relay takes it", |l| l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.contains("Hai mdelholo?")));
    assert!(lab.side.late.lock().unwrap().is_empty(), "nothing woke the agent");
    let _ = lab.stop.send(true);
}

/// Bug note 2026-10-27T19-42-14 (90 s to be accepted): a thread that has written nothing yet but calls its CLI has
/// answered; it is not made to wait out `ANSWER_WAIT`.
#[test]
fn a_thread_answers_at_its_first_sign_of_life() {
    let lab = Lab::new(&[]);
    lab.t3.quiet.store(true, Ordering::SeqCst);
    lab.em.enter(quota(), None);
    lab.until("a thread is being tried", |l| l.connection().is_some() && l.em.state.lock().unwrap().trying.is_some());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lab.em.phase(), Phase::Starting, "silent and idle: not yet");
    let began = std::time::Instant::now();
    assert_eq!(lab.call("emergency_status", json!({})).0, 0);
    lab.until("covering after its first call", |l| l.em.phase() == Phase::Covering);
    assert!(began.elapsed() < Duration::from_secs(3), "{:?}", began.elapsed());
    let _ = lab.stop.send(true);
}

/// Relay mode (the user, 19:44-19:50): the same path as emergency mode, by choice. The thread gets the relay opening, the
/// whole toolset, no probes and no hand-back; its replies are spoken like Pi's; the user's Stop interrupts its run; Pi's
/// provider failing changes nothing; switching back hands over quietly.
#[test]
fn relay_mode_is_the_same_path_by_choice() {
    let lab = Lab::new(&[]);
    lab.em.choose(true);
    assert!(lab.em.holding(), "held at once");
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    assert_eq!(lab.em.reason(), Reason::Chosen);
    let first = lab.t3.first.lock().unwrap()[0].1.clone();
    let rest = first.split_once(em::PROMPT_END).unwrap().1.trim_start();
    assert!(first.starts_with(em::PROMPT_START) && rest.starts_with(em::OPENING_RELAY) && !first.contains("What failed"), "{first}");
    assert!(lab.side.cues.lock().unwrap().is_empty(), "no failure line");
    assert!(lab.side.pauses.lock().unwrap().is_empty(), "background work keeps running in relay mode");
    assert_eq!(lab.side.indicators.lock().unwrap().last(), Some(&ui::RelayIndicator::Relay));
    let (code, out) = lab.call("emergency_status", json!({}));
    assert!(code == 0 && out.starts_with("Relay mode since"), "{out}");
    assert!(lab.call("emergency_end", json!({"summary": "done"})).1.starts_with("Relay mode is User's choice"));
    // Replies: the run's final text goes the way Pi's does, once per run.
    lab.t3.complete.store(true, Ordering::SeqCst);
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "What's next?".into(), images: vec![] });
    lab.until("the reply is passed on", |l| l.side.replies.lock().unwrap().len() == 2);
    assert!(lab.side.replies.lock().unwrap().iter().all(|r| r == "Told User about the quota."));
    lab.until("the island stops working", |l| l.side.working.lock().unwrap().last() == Some(&false));
    // Stop interrupts the run.
    (lab.em.relay())(Relayed { id: format!("stop-{}", agent::now_ms()), kind: "stop".into(), text: "Stop.".into(), images: vec![] });
    lab.until("interrupted", |l| l.t3.log.lock().unwrap().iter().any(|e| e == "interrupt:thread-claudeAgent"));
    // Pi's quota running out does not turn this into emergency mode, and nothing is probed.
    lab.em.enter(quota(), Some(chatgpt()));
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!((lab.em.phase(), lab.em.reason()), (Phase::Covering, Reason::Chosen));
    assert!(lab.side.released.lock().unwrap().is_empty());
    lab.em.choose(false);
    lab.until("off, quietly", |l| l.em.phase() == Phase::Off && *l.side.quiet_releases.lock().unwrap() == 1);
    assert!(lab.t3.sent.lock().unwrap().iter().any(|(_, m)| m.starts_with("User switched Heimdall back to his built-in agent")));
    assert!(lab.connection().is_none() && lab.side.said.lock().unwrap().is_empty());
    let _ = lab.stop.send(true);
}

/// "I want both to have exactly the same tool surface", and 02:30: the MCP server is the one source.
/// The relay thread's MCP server (`heimdall-emergency mcp`, as Claude runs it) lists exactly the tools Pi's agent gets,
/// definition for definition, plus the relay's own two; the CLI lists the same names, so the two never diverge; a call
/// runs and is recorded. Any other T3 thread sees none of them, and its calls (MCP or CLI) never reach Heimdall.
#[test]
fn the_relay_has_pi_s_whole_tool_surface_and_no_other_thread_can_drive_heimdall() {
    use heimdall_emergency::mcp;
    let lab = Lab::new(&[]);
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    let relay = lab.connection().unwrap()["thread"].as_str().unwrap().to_owned();
    assert_eq!(relay, "thread-claudeAgent", "the grant names the thread, not `pending`");
    let rpc = |method: &str, params: Value, caller: &str| mcp::handle(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}), &lab.data, Some(caller)).unwrap()["result"].clone();

    // Pi's tools are the main server's list (`mcp_contract::tools()`, registered by heimdall.ts as mcp__heimdall__<name>).
    let pi = agent::mcp_contract::tools().as_array().unwrap().clone();
    let listed = rpc("tools/list", json!({}), &relay)["tools"].as_array().unwrap().clone();
    assert_eq!(listed[..pi.len()], pi[..], "every Pi tool, the same definition, in Pi's order");
    let extra: Vec<&str> = listed[pi.len()..].iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(extra, ["emergency_status", "emergency_end"]);
    // The CLI's list is the same server's: the same names, no more, no fewer.
    let (code, cli) = lab.cli(&["tools"]);
    assert_eq!(code, 0);
    let cli_names: Vec<&str> = cli.lines().filter(|l| !l.starts_with(' ')).filter_map(|l| l.split(':').next()).collect();
    let names: Vec<&str> = listed.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(cli_names, names);
    // The relay calls one by Pi's name; it runs and enters Heimdall's record.
    let said = rpc("tools/call", json!({"name":"speak","arguments":{"text":"On it."}}), &relay);
    assert_eq!(said["content"][0]["text"], "Speech queued: 1.", "{said}");
    assert!(lab.side.records.lock().unwrap().iter().any(|(_, t)| t == "[Relay agent] said to User: \"On it.\" → Speech queued: 1."));

    // Another thread (a worker task, a sub-agent's own session) has the same server in its settings.
    assert_eq!(rpc("tools/list", json!({}), "thread-worker")["tools"], json!([]));
    let refused = rpc("tools/call", json!({"name":"speak","arguments":{"text":"Hi User."}}), "thread-worker");
    assert!(refused["isError"] == true && refused["content"][0]["text"] == mcp::NOT_RELAY, "{refused}");
    let cli_as = |caller: &str| heimdall_emergency::run_as(&["call".into(), "speak".into(), json!({"text":"Hi User."}).to_string()], Some(lab.data.clone()), Some(caller));
    assert_eq!(cli_as("thread-worker"), (2, mcp::NOT_RELAY.into()));
    assert_eq!(*lab.host.0.lock().unwrap(), ["speak:On it."], "nothing the other thread asked for ran");
    // Its rule says to leave them alone; the relay's prompt says it must use them.
    let init = mcp::handle(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}), &lab.data, Some("thread-worker")).unwrap();
    assert_eq!(init["result"]["instructions"], mcp::RULE);
    let first = lab.t3.first.lock().unwrap()[0].1.clone();
    assert!(first.contains("you must use Heimdall's MCP tools, `mcp__heimdall__*`") && first.contains("mcp__heimdall__<tool>"), "{first}");
    assert!(agent::harness::WORKER_PROMPT.contains("Heimdall's own MCP tools (`mcp__heimdall__*`) may be listed") && agent::harness::WORKER_PROMPT.contains("explicit T3 Code relay"));
    let _ = lab.stop.send(true);
}

/// "switching between relay mode and the Pi agent, in either direction and mid-conversation, must
/// break nothing". Pi to relay mid-conversation: the relay starts from Pi's conversation, plan and followed tasks, has
/// Pi's tools on the same host (speech, islands, wakes, tasks), and everything that would have woken Pi reaches it:
/// The user's words, his answer to an ask Pi opened, a scheduled wake Pi set, a result of a task Pi started.
#[test]
fn pi_to_relay_mid_conversation_carries_the_conversation_and_every_kind_of_input() {
    let lab = Lab::new(&[]);
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    let first = lab.t3.first.lock().unwrap()[0].1.clone();
    for part in ["what's on the plan tonight?", "Ship the build"] { assert!(first.contains(part), "{part} in the first message"); }
    let context = first.lines().find_map(|l| l.split("read your context, in pages: ").nth(1)).map(|r| r.split(' ').next().unwrap().to_owned()).unwrap();
    assert!(std::fs::read_to_string(&context).unwrap().contains("Fix the menu"), "the T3 tasks Pi follows");
    // Heimdall's state is the app's, not the agent's: the relay acts on the same host Pi does.
    for (tool, args) in [("speak", json!({"text":"Carrying on."})), ("islands_put", json!({"id":"build","title":"Build"})), ("schedule_wake", json!({"message":"check the build","in_minutes":30})), ("task_status", json!({}))] {
        let (code, out) = lab.call(tool, args);
        assert_eq!(code, 0, "{tool}: {out}");
    }
    assert_eq!(*lab.host.0.lock().unwrap(), ["speak:Carrying on.", "islands_put", "schedule_wake", "task_status"]);
    // Input Pi's own work set up arrives here now, each as what it is.
    for (id, kind, text) in [("a1", "answer", "q1: Opus"), ("s1", "scheduled", "Check the build"), ("r1", "result", "Fix the menu: done"), ("t1", "typed", "and the logs?")] {
        (lab.em.relay())(Relayed { id: id.into(), kind: kind.into(), text: text.into(), images: vec![] });
    }
    lab.until("all relayed", |l| l.t3.sent.lock().unwrap().len() == 4);
    let sent: Vec<String> = lab.t3.sent.lock().unwrap().iter().map(|(_, m)| m.clone()).collect();
    assert_eq!(sent, ["[For Heimdall, User clicked] q1: Opus", "[For Heimdall, a scheduled wake] Check the build", "[For Heimdall, a task finished] Fix the menu: done", "[For Heimdall, User typed] and the logs?"]);
    assert!(lab.side.late.lock().unwrap().is_empty() && lab.side.released.lock().unwrap().is_empty(), "nothing woke Pi meanwhile");
    let _ = lab.stop.send(true);
}

/// Relay to Pi mid-conversation: what the relay did is already in Pi's conversation; the input it had not answered
/// yet (a run still working, a message T3 had not taken) wakes Pi in order instead of being lost; the relay's late
/// reply is not spoken; its grant is gone; and input after the switch (an ask the relay opened, answered now) wakes Pi.
#[test]
fn relay_to_pi_mid_conversation_hands_pi_what_the_relay_had_not_answered() {
    let lab = Lab::new(&[]);
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    assert_eq!(lab.call("speak", json!({"text":"Looking."})).0, 0);
    assert!(lab.side.records.lock().unwrap().iter().any(|(_, t)| t.starts_with("[Relay agent] said to User: \"Looking.\"")), "Pi's record has it as it happened");
    // One input the relay is still working on, one T3 has not taken yet.
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "Is the build green?".into(), images: vec![] });
    lab.until("the first is with the relay", |l| l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.contains("Is the build green?")));
    lab.t3.send_fails.store(true, Ordering::SeqCst);
    (lab.em.relay())(Relayed { id: "t2".into(), kind: "typed".into(), text: "and the logs?".into(), images: vec![] });
    std::thread::sleep(Duration::from_millis(200));
    let replies = lab.side.replies.lock().unwrap().len();
    lab.em.choose(false);
    // The hold lifts first and the unanswered input follows it (`relay_off`): wait for both, not the hold alone (a race
    // the merge's slower test build showed, 30 Oct).
    lab.until("Pi answers again", |l| l.em.phase() == Phase::Off && *l.side.quiet_releases.lock().unwrap() == 1 && l.side.late.lock().unwrap().len() >= 2);
    assert_eq!(*lab.side.late.lock().unwrap(), ["unfinished: Is the build green?", "unfinished: and the logs?"], "unanswered input wakes Pi, in order");
    assert!(lab.side.records.lock().unwrap().iter().any(|(_, t)| t.starts_with("[Relay mode over]")));
    assert!(lab.connection().is_none());
    assert_eq!(lab.call("speak", json!({"text":"still me"})).0, 2, "the relay's tools are off");
    // The relay finishes its run anyway: its reply is not spoken over Pi.
    lab.t3.send_fails.store(false, Ordering::SeqCst);
    lab.t3.complete.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lab.side.replies.lock().unwrap().len(), replies);
    // The user answers an ask the relay opened: it wakes Pi.
    (lab.em.relay())(Relayed { id: "a2".into(), kind: "answer".into(), text: "q1: Ship it".into(), images: vec![] });
    lab.until("the answer reaches Pi", |l| l.side.late.lock().unwrap().len() == 3);
    assert_eq!(lab.side.late.lock().unwrap()[2], "q1: Ship it");
    let _ = lab.stop.send(true);
}

/// What the relay answered before the switch is not asked of Pi again; what the user stopped is not handed over.
#[test]
fn relay_to_pi_hands_over_neither_answered_nor_stopped_input() {
    let lab = Lab::new(&[]);
    lab.t3.complete.store(true, Ordering::SeqCst);
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "Is the build green?".into(), images: vec![] });
    lab.until("its run ends and the island is ready", |l| l.side.working.lock().unwrap().last() == Some(&false) && l.t3.sent.lock().unwrap().len() == 1);
    assert!(!lab.side.replies.lock().unwrap().is_empty(), "the relay answered it");
    // Then T3 stops taking messages: the user types, then presses Stop.
    lab.t3.send_fails.store(true, Ordering::SeqCst);
    (lab.em.relay())(Relayed { id: "t2".into(), kind: "typed".into(), text: "and the logs?".into(), images: vec![] });
    (lab.em.relay())(Relayed { id: format!("stop-{}", agent::now_ms()), kind: "stop".into(), text: "Stop.".into(), images: vec![] });
    std::thread::sleep(Duration::from_millis(200));
    lab.em.choose(false);
    lab.until("Pi answers again", |l| l.em.phase() == Phase::Off && *l.side.quiet_releases.lock().unwrap() == 1);
    assert!(lab.side.late.lock().unwrap().is_empty(), "{:?}", lab.side.late.lock().unwrap());
    let _ = lab.stop.send(true);
}

/// Back and forth: Pi, relay, Pi, relay. Each relay starts clean (nothing left open from the last one), and the second
/// switch back hands Pi only what the second relay left.
#[test]
fn switching_back_and_forth_leaves_nothing_behind() {
    let lab = Lab::new(&[]);
    for round in 0..2 {
        lab.em.choose(true);
        // The second round reuses the recent thread; its first call is its sign of life.
        lab.until("the grant names the thread", |l| l.connection().is_some_and(|c| c["thread"] == "thread-claudeAgent"));
        assert_eq!(lab.call("speak", json!({"text": format!("round {round}")})), (0, "Speech queued: 1.".into()));
        lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
        (lab.em.relay())(Relayed { id: format!("w{round}"), kind: "wake".into(), text: format!("question {round}"), images: vec![] });
        lab.until("relayed", |l| l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.ends_with(&format!("question {round}"))));
        lab.em.choose(false);
        lab.until("Pi again", |l| l.em.phase() == Phase::Off && *l.side.quiet_releases.lock().unwrap() == round + 1 && l.side.late.lock().unwrap().len() == round as usize + 1);
    }
    assert_eq!(*lab.side.late.lock().unwrap(), ["unfinished: question 0", "unfinished: question 1"]);
    assert_eq!(*lab.host.0.lock().unwrap(), ["speak:round 0", "speak:round 1"]);
    let _ = lab.stop.send(true);
}

/// "Claude in T3 should also have a collapsible bar, and then we can choose a model, for example
/// Opus." Relay mode runs the T3 model chosen; a new choice moves the relay to a thread on it; T3's Claude models reach
/// the menu. While the relay answers, Pi's provider problems stay out of the menu (17:27:55: the relay's compaction on
/// Opus by API key showed "Anthropic credits used up" while Claude ran in T3).
#[test]
fn relay_mode_runs_the_chosen_t3_model_and_moves_when_it_changes() {
    let lab = Lab::new(&[]);
    assert!(crate::emergency::reports_provider_problems(&lab.em), "Pi's problems show while it is the agent");
    let relay = |m: &str| Options { order: em::default_order(false), skip_relay: false, relay_model: Some(m.into()), relay_account: None };
    lab.em.configure(relay("claude-b"));
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    assert_eq!(lab.t3.models.lock().unwrap().as_slice(), ["claude-b"], "the chosen model, not T3's first");
    assert!(!crate::emergency::reports_provider_problems(&lab.em), "relay mode: Pi's provider problems are not the agent's");
    lab.em.configure(relay("claude-a"));
    lab.em.choose(true);
    lab.until("moved to claude-a", |l| l.t3.models.lock().unwrap().last().map(String::as_str) == Some("claude-a") && l.em.phase() == Phase::Covering);
    assert!(lab.t3.sent.lock().unwrap().iter().any(|(_, m)| m.starts_with("User switched the relay to another model")));
    // The same choice again changes nothing.
    lab.em.choose(true);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(lab.t3.models.lock().unwrap().len(), 2);
    lab.em.refresh_relay_models();
    lab.until("T3's Claude models reach the menu", |l| l.side.relay_models.lock().unwrap().last()
        .is_some_and(|m| m.iter().map(|m| (m.id.as_str(), m.name.as_str())).collect::<Vec<_>>() == [("claude-a", "Claude A"), ("claude-b", "Claude B")]));
    lab.em.choose(false);
    lab.until("off", |l| l.em.phase() == Phase::Off);
    assert!(crate::emergency::reports_provider_problems(&lab.em));
    let _ = lab.stop.send(true);
}

/// The user, 19:55: when no relay can drive Heimdall the local engine is offered; taken, it answers the hand-back, and the
/// main model is probed until it can take over again. `relay_skip` goes straight to the offer.
#[test]
fn without_a_relay_the_local_engine_stands_in_until_the_main_model_is_back() {
    let lab = Lab::new(&[]);
    lab.em.configure(Options { order: em::default_order(false), skip_relay: true, relay_model: None, relay_account: None });
    let reset = agent::now_ms() + 3_600_000;
    lab.em.enter(Cause::Quota { reset_ms: Some(reset), line: None }, Some(chatgpt()));
    lab.until("the offer", |l| !l.side.no_relay.lock().unwrap().is_empty());
    assert_eq!(*lab.side.no_relay.lock().unwrap(), [Some(reset)]);
    assert!(lab.t3.log.lock().unwrap().is_empty(), "no relay tried");
    assert_eq!(lab.side.pauses.lock().unwrap()[0], Some(Some(reset)));
    // The user says yes: the agent switches to the local engine (no API model), which the probe passes.
    lab.em.local_fallback(Some(chatgpt()));
    lab.probe.0.store(true, Ordering::SeqCst);
    lab.em.model(None);
    lab.until("handed back to the local engine", |l| l.em.phase() == Phase::Trial);
    lab.em.answered();
    lab.until("the local engine stands in", |l| l.em.phase() == Phase::Local);
    assert_eq!(lab.side.pauses.lock().unwrap().last(), Some(&None), "background work resumes");
    assert!(!lab.em.holding());
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    // The main model answers: the user is asked (30 Oct: never switched by itself); the stand-in lasts until he answers.
    lab.until("asked whether to go back", |l| l.side.restored.lock().unwrap().as_slice() == ["gpt-model"]);
    assert_eq!(lab.em.phase(), Phase::Local, "nothing switched while the question waits");
    // Unanswered, the next good check asks again.
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.until("asked again", |l| l.side.restored.lock().unwrap().len() == 2);
    // His No ends the stand-in on the local model.
    lab.em.keep_local();
    assert_eq!(lab.em.phase(), Phase::Off);
    // The relay's own local pick records no API model to go back to (review 30 Oct).
    lab.em.local_fallback(Some(chatgpt()));
    lab.em.no_fallback();
    assert!(lab.em.state.lock().unwrap().fallback.is_none());
    let _ = lab.stop.send(true);
}

/// A controller that died (a panic took its inbox) is replaced: `respawn` gives it a new inbox and the emergency it
/// missed is picked up from the saved state.
#[test]
fn a_replaced_controller_picks_up_the_emergency_it_missed() {
    let lab = Lab::new(&[]);
    let _ = lab.stop.send(true);
    lab.until("the first controller stopped", |l| l.em.rx.lock().unwrap().is_some());
    *lab.em.rx.lock().unwrap() = None; // as a panic leaves it
    lab.em.enter(quota(), None); // logged: nobody reads the inbox
    assert!(lab.em.holding());
    lab.em.respawn();
    let (_stop, rx) = tokio::sync::watch::channel(false);
    lab.rt.spawn(lab.em.clone().run(lab.t3.clone(), lab.probe.clone(), lab.side.clone(), lab.host.clone(), Arc::new(agent::mcp::UnavailableArchive), lab.gate.clone(), rx));
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
}

/// Bug note 2026-10-27T19-45-00: launched with Agent off, the controller never ran, so turning Agent on later and
/// running out of quota (19:29, 19:31, 19:38) went nowhere. Now: off at launch, then on, then a spent quota: emergency
/// mode turns on and a relay is looked for.
#[test]
fn agent_off_at_launch_then_on_still_gets_emergency_mode() -> Result<()> {
    use crate::bridge::{AgentRuntime, Command, Tools};
    let d = tempfile::tempdir()?;
    let data = d.path().join("data");
    let session = data.join("archive/session-emergency");
    std::fs::create_dir_all(&data)?;
    let tools = Arc::new(Tools::new(d.path(), &data));
    tools.agent_enabled.intent(agent::delivery::StopReason::Agent, false);
    crate::bridge::test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
    let runtime = AgentRuntime::start(crate::settings::Settings { dry_run: true, ..Default::default() }, data.clone(), session.clone(), tools.clone(), None);
    crate::bridge::test_harness(None);
    let runtime = runtime?;
    runtime.barrier()?;
    tools.agent_enabled.intent(agent::delivery::StopReason::Agent, true);
    runtime.send(Command::Restart(crate::settings::Settings { dry_run: false, ..Default::default() }))?;
    runtime.barrier()?;
    std::fs::write(data.join("provider-quota"), "")?;
    runtime.send(Command::Send { text: "Heimdall, what's on the plan?".into(), steer: false })?;
    let began = std::time::Instant::now();
    while !std::fs::read(data.join(em::DIR).join("state.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v| v["cause"]["kind"] == "quota") {
        anyhow::ensure!(began.elapsed() < Duration::from_secs(15), "emergency mode did not turn on after Agent on");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(tools.emergency.holding());
    drop(runtime);
    Ok(())
}

fn item(position: u64, kind: &str, text: &str) -> Item { Item { position, kind: kind.into(), text: text.into(), status: "completed".into() } }
fn mirrored(lab: &Lab) -> Vec<(String, String)> {
    lab.side.records.lock().unwrap().iter().filter(|(id, _)| id.contains("mirror:")).cloned().collect()
}

/// The user, 20:44-20:45 (total context continuity): what the relay thread was told, said and did goes into Pi's session
/// as it happens: every message whole, each action one plain line with its result, nothing replayed, nothing summed
/// up. Heimdall's own CLI calls reach it as their own lines, the relayed inputs are held there already, and the
/// takeover's first message is Pi's own context. Then hand-back carries no hand-off.
#[test]
fn the_relay_thread_is_mirrored_into_pi_s_session_whole_and_nothing_is_handed_off() {
    let lab = Lab::new(&[]);
    lab.em.enter(quota(), None);
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
    let long = "word ".repeat(40_000); // 200 KB: compacted later by Pi, never cut here
    let t = "thread-claudeAgent".to_string();
    *lab.t3.items.lock().unwrap() = vec![
        (t.clone(), item(1, "user_message", &format!("{}\n…\n{}\n\n{} …", em::PROMPT_START, em::PROMPT_END, em::OPENING))),
        (t.clone(), item(2, "reasoning", "thinking about it")),
        (t.clone(), item(3, "assistant_message", "ChatGPT ran out; I have it. I'm finishing the menu fix you asked for.")),
        (t.clone(), item(4, "command_execution", "$ heimdall-emergency --data-dir /x speak \"hi\"\nSpeech queued: 1.")),
        (t.clone(), item(5, "command_execution", "$ git -C /V/Projects/Heimdall log -1\nabc123 Fix")),
        (t.clone(), item(6, "user_message", "[For Heimdall, User said] Are you there?")),
        (t.clone(), item(7, "assistant_message", &long)),
        (t.clone(), item(8, "file_change", "crates/ui/src/mac/menu.rs")),
        (t.clone(), item(9, "checkpoint", "[]")),
        (t.clone(), item(10, "command_execution", "$ Preparing workspace\nWorkspace preparation completed.")),
        (t.clone(), item(11, "dynamic_tool", "{\"toolName\":\"ToolSearch\",\"input\":{\"query\":\"x\"}}")),
        // Heimdall's own tool through his MCP server: recorded live when it ran, so not mirrored again.
        (t.clone(), item(12, "dynamic_tool", "{\"toolName\":\"mcp__heimdall__speak\",\"input\":{\"text\":\"hi\"}}")),
    ];
    lab.t3.complete.store(true, Ordering::SeqCst);
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "Are you there?".into(), images: vec![] });
    lab.until("mirrored", |l| mirrored(l).iter().any(|(id, _)| id.contains(":11:")));
    let m = mirrored(&lab);
    let texts: Vec<&str> = m.iter().map(|(_, t)| t.as_str()).collect();
    assert!(texts.iter().any(|t| *t == "[Relay agent] ChatGPT ran out; I have it. I'm finishing the menu fix you asked for."), "{texts:#?}");
    assert!(texts.iter().any(|t| *t == "[Relay agent ran] git -C /V/Projects/Heimdall log -1 → completed"));
    assert!(texts.iter().any(|t| t.starts_with("[Relay agent: file change] crates/ui/src/mac/menu.rs")));
    // Live smoke 27 Oct: T3's bookkeeping stays out, a harness tool is its name only.
    assert!(texts.contains(&"[Relay agent used ToolSearch] → completed") && !texts.iter().any(|t| t.contains("checkpoint") || t.contains("Preparing workspace") || t.contains("{")), "{texts:#?}");
    assert!(!texts.iter().any(|t| t.contains("thinking about it") || t.contains("Are you there?") || t.contains(em::OPENING) || t.contains("Speech queued")), "{texts:#?}");
    // The long message whole, in parts.
    let whole: String = m.iter().filter(|(id, _)| id.contains(":7:")).map(|(_, t)| t.split("\n[part ").next().unwrap().to_owned()).collect();
    assert_eq!(whole, format!("[Relay agent] {}", long.trim()));
    // Idempotent: another pass adds nothing.
    let count = m.len();
    (lab.em.relay())(Relayed { id: "w2".into(), kind: "wake".into(), text: "Again".into(), images: vec![] });
    lab.until("the second run is sent", |l| l.t3.sent.lock().unwrap().iter().any(|(_, m)| m.contains("Again")));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(mirrored(&lab).len(), count);
    assert!(!mirrored(&lab).iter().any(|(_, t)| t.contains("mcp__heimdall__")), "Heimdall's tools are recorded as they run, not mirrored");
    // Hand back: no summary of any of it.
    lab.probe.0.store(true, Ordering::SeqCst);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.go_back();
    lab.until("handed back", |l| !l.side.released.lock().unwrap().is_empty());
    let back = lab.side.released.lock().unwrap()[0].clone();
    assert!(!back.contains("menu fix") && back.contains("in your conversation above"), "{back}");
    let _ = lab.stop.send(true);
}

/// The mirror's place survives a restart: the new process goes on from the last mirrored item, without repeats.
#[test]
fn the_mirror_continues_where_it_was_after_a_restart() {
    let lab = Lab::new(&[]);
    lab.em.choose(true);
    lab.until("relaying", |l| l.em.phase() == Phase::Covering);
    let t = "thread-claudeAgent".to_string();
    lab.t3.items.lock().unwrap().push((t.clone(), item(1, "assistant_message", "first")));
    lab.t3.complete.store(true, Ordering::SeqCst);
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "one".into(), images: vec![] });
    lab.until("first mirrored", |l| mirrored(l).iter().any(|(_, t)| t == "[Relay agent] first"));
    let _ = lab.stop.send(true);
    lab.until("stopped", |l| l.connection().is_none());
    let Lab { _dir, rt, t3, .. } = lab;
    rt.shutdown_timeout(Duration::from_secs(2));
    let lab = Lab::at(_dir, &[]);
    *lab.t3.items.lock().unwrap() = t3.items.lock().unwrap().clone();
    lab.t3.items.lock().unwrap().push((t.clone(), item(2, "assistant_message", "second")));
    lab.t3.complete.store(true, Ordering::SeqCst);
    lab.until("relaying again", |l| l.em.phase() == Phase::Covering && l.connection().is_some());
    (lab.em.relay())(Relayed { id: "w2".into(), kind: "wake".into(), text: "two".into(), images: vec![] });
    lab.until("second mirrored", |l| mirrored(l).iter().any(|(_, t)| t == "[Relay agent] second"));
    assert!(!mirrored(&lab).iter().any(|(_, t)| t == "[Relay agent] first"), "not again after the restart");
    // Relay off: the rest goes in, and only a line that the user switched back; no summary.
    lab.em.choose(false);
    lab.until("off", |l| l.em.phase() == Phase::Off);
    assert!(lab.side.records.lock().unwrap().iter().any(|(_, t)| t.starts_with("[Relay mode over]")));
    let _ = lab.stop.send(true);
}

/// Review 27 Oct: relay mode must never be on with nothing held. Chosen during a hand-back trial it starts (and holds);
/// Agent off and straight back on, the stand-down queued first, it comes back on.
#[test]
fn relay_mode_chosen_during_a_trial_or_after_a_quick_off_and_on_acts_and_holds() {
    let lab = Lab::new(&[]);
    lab.em.enter(quota(), None);
    lab.until("covering", |l| l.em.phase() == Phase::Covering);
    lab.probe.0.store(true, Ordering::SeqCst);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.go_back();
    lab.until("trial", |l| l.em.phase() == Phase::Trial);
    lab.em.choose(true);
    lab.until("relay mode acts", |l| l.em.phase() == Phase::Covering && l.em.reason() == Reason::Chosen);
    assert!(lab.em.holding());
    lab.em.end("User switched Heimdall's agent off");
    lab.em.choose(true);
    lab.until("on again after the stand-down", |l| l.em.phase() == Phase::Covering && l.em.reason() == Reason::Chosen);
    assert!(lab.em.holding());
    let _ = lab.stop.send(true);
}

/// Live smoke (never in ordinary runs; real T3, real Claude subscription thread): relay mode end to end against the
/// real T3 Code on this Mac, with isolated data, a recording host (nothing is said aloud, no setting or profile of
/// the user's is touched) and a synthetic conversation. Checks: the connection and the grant (the thread runs the built CLI:
/// `tools` and a harmless `task_status`), acceptance time, reply capture, Pi -> T3 context (a code word only the
/// synthetic context holds), T3 -> Pi mirror (messages whole, actions one line, each position once, no replayed tool
/// call), Stop interrupting a real run, and relay off. Writes its evidence as JSON to `$HEIMDALL_RELAY_LIVE_EVIDENCE`.
///
/// Run: HEIMDALL_RELAY_LIVE_LAB=<abs empty dir> HEIMDALL_EMERGENCY_CLI=<abs heimdall-emergency> \
///      HEIMDALL_RELAY_LIVE_EVIDENCE=<abs file.json> cargo test -p app --bin heimdall -- --ignored --nocapture relay_live_smoke
#[test]
#[ignore]
fn relay_live_smoke() {
    struct Memory(Mutex<Option<t3::client::Credential>>);
    impl t3::CredentialStore for Memory {
        fn load(&self) -> Result<Option<t3::client::Credential>> { Ok(self.0.lock().unwrap().clone()) }
        fn save(&self, c: &t3::client::Credential) -> Result<()> { *self.0.lock().unwrap() = Some(c.clone()); Ok(()) }
    }
    /// The real T3, with a drill notice on top of the first message and every thread kept for the archive at the end.
    struct Drill { inner: T3Cover, cli: String, threads: Mutex<Vec<String>> }
    #[async_trait]
    impl CoverT3 for Drill {
        async fn catalog(&self) -> Result<Value> { self.inner.catalog().await }
        async fn launch(&self, title: &str, message: &str, h: &Harness) -> Result<(String, Option<String>)> {
            let drill = format!("LAB DRILL. An automated test of Heimdall's relay in an isolated lab; Heimdall and User are fine and nobody hears you. Do only this: run `{cli} tools | head -5` and `{cli} call task_status '{{}}'`, then reply in one short sentence that names the code word in Heimdall's context file. Touch no other files, threads or tools. Later messages in this thread are part of the drill; follow them the same way.\n\nThe real first message follows for the record:\n\n", cli = self.cli);
            let r = self.inner.launch(&format!("{title} (lab drill)"), &(drill + message), h).await?;
            self.threads.lock().unwrap().push(r.0.clone());
            Ok(r)
        }
        async fn send(&self, thread: &str, message: &str) -> Result<Option<String>> { self.inner.send(thread, message).await }
        async fn outcome(&self, thread: &str, run: Option<&str>, wait: Duration) -> Result<Run> { self.inner.outcome(thread, run, wait).await }
        async fn latest_reply(&self, thread: &str) -> Result<Option<String>> { self.inner.latest_reply(thread).await }
        async fn archive(&self, thread: &str) -> Result<()> { self.inner.archive(thread).await }
        async fn interrupt(&self, thread: &str) -> Result<()> { self.inner.interrupt(thread).await }
        async fn activity(&self, thread: &str, after: u64) -> Result<Vec<Item>> { self.inner.activity(thread, after).await }
    }
    /// The synthetic conversation the takeover carries: its code word is nowhere else.
    struct LabSide(FakeSide);
    impl Side for LabSide {
        fn cue(&self, c: ui::HealthCue) { self.0.cue(c) }
        fn indicator(&self, s: ui::RelayIndicator) { self.0.indicator(s) }
        fn say(&self, t: &str) { self.0.say(t) }
        fn record(&self, id: &str, t: &str) { self.0.record(id, t) }
        fn release(&self, b: Option<String>) { self.0.release(b) }
        fn late(&self, r: &Relayed) { self.0.late(r) }
        fn unfinished(&self, r: &Relayed) { self.0.unfinished(r) }
        fn context(&self, _: usize) -> (String, String, String) {
            ("[20:01] To Heimdall: Remember the code word for tonight's drill: HERON-42.\n[20:01] Heimdall: Noted, HERON-42.\n[20:02] Heimdall used plan_edit: {\"action\":\"add\",\"item\":\"Lab drill\"} → Added 'Lab drill'".into(),
             "To do:\n- Lab drill".into(), "(none)".into())
        }
        fn sent(&self) { self.0.sent() }
        fn working(&self, on: bool) { self.0.working(on) }
        fn reply(&self, t: &str) { self.0.reply(t) }
        fn pause_background(&self, r: Option<Option<u64>>) { self.0.pause_background(r) }
        fn no_relay(&self, r: Option<u64>) { self.0.no_relay(r) }
        fn restore_model(&self, m: &agent::provider::ApiModel) { self.0.restore_model(m) }
    }
    let Some(root) = std::env::var_os("HEIMDALL_RELAY_LIVE_LAB").map(PathBuf::from) else { return };
    let bin = std::env::var("HEIMDALL_EMERGENCY_CLI").expect("HEIMDALL_EMERGENCY_CLI");
    let evidence_path = std::env::var("HEIMDALL_RELAY_LIVE_EVIDENCE").expect("HEIMDALL_RELAY_LIVE_EVIDENCE");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let cli = format!("{bin} --data-dir {}", data.display());
    let client = Arc::new(t3::Client::local(Arc::new(Memory(Mutex::new(None)))).unwrap());
    let t3 = Arc::new(Drill { inner: T3Cover(client), cli: cli.clone(), threads: Mutex::new(vec![]) });
    let em = Emergency::new(&data, cli, concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/emergency.md").into());
    em.configure(Options { order: vec!["claude".into()], skip_relay: false, relay_model: None, relay_account: None });
    let probe = Arc::new(FakeProbe(AtomicBool::new(false)));
    let side = Arc::new(LabSide(FakeSide::default()));
    let host = Arc::new(FakeHost::default());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.spawn(em.clone().run(t3.clone(), probe.clone(), side.clone(), host.clone(), Arc::new(agent::mcp::UnavailableArchive), Arc::new(agent::delivery::DeliveryGate::new(true)), rx));
    let mut steps: Vec<Value> = Vec::new();
    fn step(steps: &mut Vec<Value>, name: &str, secs: u64, test: &dyn Fn() -> bool) -> bool {
        let began = std::time::Instant::now();
        while !test() {
            if began.elapsed() > Duration::from_secs(secs) { steps.push(json!({"step":name,"ok":false,"after_s":secs})); println!("lab: {name}: TIMED OUT after {secs} s"); return false; }
            std::thread::sleep(Duration::from_millis(250));
        }
        let s = began.elapsed().as_secs_f64();
        println!("lab: {name} after {s:.1} s");
        steps.push(json!({"step":name,"ok":true,"after_s":(s * 10.0).round() / 10.0}));
        true
    }
    let replies = |side: &LabSide| side.0.replies.lock().unwrap().clone();
    em.choose(true);
    step(&mut steps, "a real Claude thread is accepted as the relay", 180, &|| em.phase() == Phase::Covering);
    let thread = em.state.lock().unwrap().thread.clone().unwrap_or_default();
    step(&mut steps, "it reaches Heimdall's tools through the CLI (task_status)", 240, &|| host.0.lock().unwrap().iter().any(|c| c == "task_status"));
    step(&mut steps, "its first reply is captured and names the code word from Pi's context", 240, &|| replies(&side).iter().any(|r| r.contains("HERON-42")));
    (em.relay())(Relayed { id: "lab-w1".into(), kind: "wake".into(), text: "[lab] User said: reply with exactly the word PINEAPPLE and nothing else.".into(), images: vec![] });
    step(&mut steps, "a relayed wake is answered and the reply captured", 240, &|| replies(&side).iter().any(|r| r.contains("PINEAPPLE")));
    step(&mut steps, "the send arrow fired for the relayed wake", 5, &|| *side.0.sent.lock().unwrap() >= 1);
    (em.relay())(Relayed { id: "lab-w2".into(), kind: "wake".into(), text: "[lab] User said: run `python3 -c 'import time; time.sleep(90)'` in the foreground (not in the background; it is the drill's Stop test), then reply DONE.".into(), images: vec![] });
    std::thread::sleep(Duration::from_secs(20));
    let before_stop = rt.block_on(t3.outcome(&thread, None, Duration::from_millis(500))).map(|r| format!("{r:?}")).unwrap_or_else(|e| format!("{e:#}"));
    (em.relay())(Relayed { id: format!("stop-{}", agent::now_ms()), kind: "stop".into(), text: "Stop speaking and stop what you are doing; wait for User.".into(), images: vec![] });
    let stop_line = || rt.block_on(t3.activity(&thread, 0)).unwrap_or_default().iter().any(|i| i.text.starts_with("[For Heimdall, User pressed Stop]"));
    step(&mut steps, "the Stop line reaches the thread at once, not after a 30 s retry", 15, &stop_line);
    std::thread::sleep(Duration::from_secs(5));
    let after_stop = rt.block_on(t3.inner.0.call("t3_thread_read", json!({"threadId":thread,"limit":1,"runLimit":3}))).map(|r| r["recentRuns"].clone()).unwrap_or(Value::Null);
    let done_said = replies(&side).iter().any(|r| r.trim() == "DONE");
    println!("lab: before Stop {before_stop}; runs after Stop {after_stop}; DONE said: {done_said}");
    steps.push(json!({"step":"Stop interrupts the running 90-second foreground run","ok":!done_said && after_stop.to_string().contains("interrupt"),"before":before_stop,"runs_after":after_stop}));
    em.choose(false);
    step(&mut steps, "relay off: the last of the thread is mirrored and Pi is told it switched back", 120, &|| em.phase() == Phase::Off);
    let records = side.0.records.lock().unwrap().clone();
    let mirror: Vec<&(String, String)> = records.iter().filter(|(id, _)| id.contains("mirror:")).collect();
    let mut ids: Vec<&str> = mirror.iter().map(|(id, _)| id.as_str()).collect();
    let total = ids.len(); ids.sort(); ids.dedup();
    let replayed = mirror.iter().any(|(_, t)| t.contains("\"tool_use\"") || t.contains("toolu_") || t.contains("\"type\":\"tool"));
    let cli_actions: Vec<&String> = records.iter().filter(|(id, t)| !id.contains("mirror:") && t.starts_with("[Relay agent] ")).map(|(_, t)| t).collect();
    steps.push(json!({"step":"T3 -> Pi mirror: messages whole, each position once, no replayed tool call","ok": !mirror.is_empty() && total == ids.len() && !replayed,
        "mirrored":mirror.len(),"unique":ids.len(),"replayed_tool_calls":replayed}));
    steps.push(json!({"step":"Heimdall tool actions reach Pi as one line each","ok":!cli_actions.is_empty(),"lines":cli_actions}));
    let ok = steps.iter().all(|s| s["ok"] == true);
    let evidence = json!({"kind":"live smoke, real T3 Code and a real Claude subscription thread (not a mock)","when_ms":agent::now_ms(),
        "source_commit":std::env::var("HEIMDALL_RELAY_LIVE_COMMIT").unwrap_or_default(),"thread":thread,"harness":em.state.lock().unwrap().last_thread.clone(),
        "host_calls":host.0.lock().unwrap().clone(),"replies":replies(&side),"mirror":mirror.iter().map(|(id, t)| json!({"id":id,"text":t.chars().take(400).collect::<String>()})).collect::<Vec<_>>(),
        "steps":steps,"ok":ok,
        "not_covered":"Heimdall's real speech, island and supervisor (a recording host and side stand in), the emergency failover and hand-back against a real outage, voice barge-in"});
    std::fs::write(&evidence_path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    for thread in t3.threads.lock().unwrap().iter() { let _ = rt.block_on(t3.archive(thread)); }
    let _ = stop.send(true);
    println!("lab: evidence at {evidence_path}; ok = {ok}");
}

// ---- Accounts and recovery (Assistant/Plans/relay-account-routing-2026-10-30.md) ----

use crate::emergency_recovery::{PREFIX as RECOVERY, RETURN_PREFIX as RETURN};
use agent::routing::{Quota, Window};

const HOUR: u64 = 3_600_000;
/// A second Claude account in T3, like the user's `claudeAgent_claude_code_2`.
fn second_claude(lab: &Lab) {
    lab.t3.extra.lock().unwrap().push(json!({"providerInstanceId":"claudeAgent_2","driverKind":"claudeAgent","displayName":"Claude 2","models":[{"id":"claude-a"},{"id":"claude-b"}],"constraints":[]}));
}
fn quota_of(five: f64, week: f64, week_reset_ms: u64) -> Quota {
    Quota { five_hour: Window { used_pct: Some(five), reset_ms: Some(agent::now_ms() + HOUR) }, weekly: Window { used_pct: Some(week), reset_ms: Some(week_reset_ms) }, reached: false, error: None }
}
fn set_quota(lab: &Lab, instance: &str, q: Quota) { lab.t3.quotas.lock().unwrap().insert(instance.into(), q); }
fn launches(lab: &Lab) -> Vec<String> { lab.t3.log.lock().unwrap().iter().filter(|l| l.starts_with("launch:")).cloned().collect() }
/// Relay mode on the first Claude account, then that account runs out under a run answering the user (with a screenshot).
fn relay_then_account_runs_out(lab: &Lab) {
    second_claude(lab);
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    assert_eq!(launches(lab), ["launch:claudeAgent"]);
    lab.t3.failing.lock().unwrap().push("claudeAgent".into());
    set_quota(lab, "claudeAgent", quota_of(100.0, 40.0, agent::now_ms() + 50 * HOUR));
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "Is the build green?".into(), images: vec![agent::protocol::Attachment { media_type: "image/png".into(), bytes: vec![7, 7] }] });
    lab.until("the recovery card", |l| l.card(RECOVERY).is_some());
}

/// The relay starts on the eligible account whose week resets soonest, and stays there when the ranking changes.
#[test]
fn relay_mode_starts_on_the_account_whose_quota_resets_soonest_and_stays() {
    let lab = Lab::new(&[]);
    second_claude(&lab);
    let now = agent::now_ms();
    set_quota(&lab, "claudeAgent", quota_of(10.0, 50.0, now + 100 * HOUR));
    set_quota(&lab, "claudeAgent_2", quota_of(60.0, 80.0, now + 20 * HOUR));
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    assert_eq!(launches(&lab), ["launch:claudeAgent_2"], "its week ends first: that quota would go unused");
    // The ranking changes while it works: it stays.
    set_quota(&lab, "claudeAgent", quota_of(0.0, 0.0, now + HOUR));
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "next".into(), images: vec![] });
    lab.until("relayed", |l| l.t3.sent.lock().unwrap().iter().any(|(t, m)| t == "thread-claudeAgent_2" && m.ends_with("next")));
    assert_eq!(launches(&lab), ["launch:claudeAgent_2"]);
    let _ = lab.stop.send(true);
}

/// The relay's account runs out: the card offers the same model on the other account first, other providers and
/// waiting, with a countdown; the user's pick (here not the search's first choice) takes over with the question it was answering (and its image); after the
/// switch it runs normally (no emergency indicator, background work on).
#[test]
fn the_relay_s_account_runs_out_and_owner_picks_the_other_account() {
    let lab = Lab::new(&[]);
    relay_then_account_runs_out(&lab);
    let (_, options) = lab.card(RECOVERY).unwrap();
    assert_eq!(options, ["Claude 2 · A", "Codex · codex-a", "Grok · grok-a", "Wait for Claude · A"], "same model on the other account first, then the other providers");
    let n = lab.side.notices.lock().unwrap().last().cloned().unwrap();
    assert!(matches!(n.action, Some(ui::agent_notice::NoticeAction::Ask { countdown: true, .. })), "a visible countdown");
    assert!(lab.em.holding() && lab.em.phase() == Phase::Choosing);
    assert!(matches!(lab.side.indicators.lock().unwrap().last(), Some(ui::RelayIndicator::Emergency { .. })), "emergency while switching");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(launches(&lab), ["launch:claudeAgent"], "nothing launched before he picks or the minute ends");
    // His pick, not the search's first: Grok.
    lab.pick(RECOVERY, "Grok");
    lab.until("his pick acts", |l| l.em.phase() == Phase::Covering && launches(l).len() == 2);
    assert_eq!(launches(&lab)[1], "launch:grok", "the pick, not the search order");
    lab.until("the open question reaches it", |l| l.t3.sent.lock().unwrap().iter().any(|(t, m)| t == "thread-grok" && m.contains("Is the build green?") && m.contains("Image: ")));
    assert_eq!(lab.t3.sent.lock().unwrap().iter().filter(|(_, m)| m.contains("Is the build green?")).count(), 2, "once to each account, never replayed again");
    assert_eq!(lab.side.indicators.lock().unwrap().last(), Some(&ui::RelayIndicator::Relay), "runs normally after the switch");
    assert_eq!(lab.side.pauses.lock().unwrap().last(), Some(&None));
    assert_eq!(lab.em.reason(), Reason::Chosen, "still User's relay: switching it off works as before");
    let _ = lab.stop.send(true);
}

/// No pick within the minute: Heimdall searches by itself, in the card's order.
#[test]
fn without_a_pick_the_search_starts_by_itself() {
    let lab = Lab::new(&[]);
    relay_then_account_runs_out(&lab);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(launches(&lab).len(), 1, "he has the minute");
    lab.until("searched by itself", |l| l.em.phase() == Phase::Covering && launches(l).len() == 2);
    assert_eq!(launches(&lab)[1], "launch:claudeAgent_2");
    assert_eq!(lab.t3.models.lock().unwrap().last().map(String::as_str), Some("claude-a"), "the same model on the other account");
    assert!(lab.side.dismissed.lock().unwrap().iter().any(|d| d.starts_with(RECOVERY)), "the card closes");
    let _ = lab.stop.send(true);
}

/// The user waits for his account: nothing else starts; once its quota reads as usable again, the relay starts on it.
#[test]
fn waiting_for_the_original_account_resumes_on_it_when_it_works() {
    let lab = Lab::new(&[]);
    relay_then_account_runs_out(&lab);
    lab.pick(RECOVERY, "Wait for");
    lab.until("waiting, with its line", |l| l.em.phase() == Phase::NoCover && l.side.notices.lock().unwrap().iter().any(|n| n.text.starts_with("Waiting for the quota")));
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(launches(&lab), ["launch:claudeAgent"], "still spent: still waiting, nothing else tried");
    lab.t3.failing.lock().unwrap().clear();
    set_quota(&lab, "claudeAgent", quota_of(2.0, 40.0, agent::now_ms() + 50 * HOUR));
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.until("back on it", |l| l.em.phase() == Phase::Covering && launches(l).len() == 2);
    assert_eq!(launches(&lab)[1], "launch:claudeAgent");
    let _ = lab.stop.send(true);
}

/// The original works again: the user is asked once, by voice and card, never while a run works; nothing switches by
/// itself; Stay keeps the replacement as his setting and he is not asked again.
#[test]
fn the_original_back_is_offered_once_and_stay_keeps_the_replacement() {
    let lab = Lab::new(&[]);
    relay_then_account_runs_out(&lab);
    lab.pick(RECOVERY, "Claude 2");
    lab.until("the other account acts", |l| l.em.phase() == Phase::Covering && launches(l).len() == 2);
    lab.t3.failing.lock().unwrap().clear();
    set_quota(&lab, "claudeAgent", quota_of(0.0, 40.0, agent::now_ms() + 50 * HOUR));
    // A run is working (the carried question): no question now.
    lab.until("a run works", |l| l.em.runs.load(Ordering::Acquire) > 0);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    std::thread::sleep(Duration::from_millis(200));
    assert!(lab.card(RETURN).is_none(), "never over live work");
    // The run ends; the next check asks.
    lab.t3.complete.store(true, Ordering::SeqCst);
    lab.until("the run ends", |l| l.em.runs.load(Ordering::Acquire) == 0);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.until("asked", |l| l.card(RETURN).is_some());
    assert!(lab.side.said.lock().unwrap().iter().any(|l| l == "A on Claude works again. Do you want to go back to it, or stay where we are?"));
    assert_eq!(lab.card(RETURN).unwrap().1, ["Back to Claude · A", "Stay on Claude 2 · A"]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(launches(&lab).len(), 2, "nothing switched by itself");
    lab.pick(RETURN, "Stay on");
    lab.until("kept", |l| !l.side.kept.lock().unwrap().is_empty());
    assert_eq!(lab.side.kept.lock().unwrap()[0].instance, "claudeAgent_2", "the replacement becomes his relay setting");
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(lab.side.notices.lock().unwrap().iter().filter(|n| n.id.starts_with(RETURN)).count(), 1, "not asked again");
    assert_eq!(launches(&lab).len(), 2);
    let _ = lab.stop.send(true);
}

/// Back to the original: the relay moves to it, and the conversation carries on there.
#[test]
fn going_back_moves_the_relay_to_the_original_account() {
    let lab = Lab::new(&[]);
    lab.t3.complete.store(true, Ordering::SeqCst);
    relay_then_account_runs_out(&lab);
    lab.pick(RECOVERY, "Claude 2");
    lab.until("the other account acts", |l| l.em.phase() == Phase::Covering && launches(l).len() == 2);
    lab.t3.failing.lock().unwrap().clear();
    set_quota(&lab, "claudeAgent", quota_of(0.0, 40.0, agent::now_ms() + 50 * HOUR));
    lab.until("its runs end", |l| l.em.runs.load(Ordering::Acquire) == 0);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    lab.go_back();
    lab.until("back on the original", |l| l.em.phase() == Phase::Covering && launches(l).len() == 3);
    assert_eq!(launches(&lab)[2], "launch:claudeAgent");
    assert!(lab.t3.sent.lock().unwrap().iter().any(|(t, m)| t == "thread-claudeAgent_2" && m.starts_with("User went back to his original account")));
    let _ = lab.stop.send(true);
}

/// Pi's own quota runs out: the same card, with the local model when it is on this Mac; picking it hands the switch
/// to the menu's path (its model id comes back to the pipeline).
#[test]
fn pi_running_out_offers_accounts_the_local_model_and_waiting() {
    let lab = Lab::new(&[]);
    second_claude(&lab);
    *lab.side.local.lock().unwrap() = Some(crate::emergency_recovery::Local { model: "qwen3.8-27b".into(), label: "Qwen3.8 27B".into(), ready: true, download_bytes: None });
    lab.em.enter(quota(), Some(chatgpt()));
    lab.until("the recovery card", |l| l.card(RECOVERY).is_some());
    // One account of each provider that can take over (the second Claude account stays in the search).
    assert_eq!(lab.card(RECOVERY).unwrap().1, ["Claude · A", "Grok · grok-a", "Local · Qwen3.8 27B", "Wait for GPT · built in"]);
    assert_eq!(lab.pick(RECOVERY, "Local"), Some("qwen3.8-27b".into()));
    lab.until("waiting for the local model", |l| l.em.phase() == Phase::NoCover);
    std::thread::sleep(Duration::from_millis(400));
    assert!(launches(&lab).is_empty(), "no relay searched");
    let _ = lab.stop.send(true);
}

/// Relay mode on an account that is already spent: the recovery card, never a silent skip.
#[test]
fn relay_mode_on_a_spent_account_offers_what_can_take_over() {
    let lab = Lab::new(&[]);
    second_claude(&lab);
    set_quota(&lab, "claudeAgent_2", quota_of(100.0, 30.0, agent::now_ms() + 30 * HOUR));
    lab.em.configure(Options { order: em::default_order(false), skip_relay: false, relay_model: None, relay_account: Some("claudeAgent_2".into()) });
    lab.em.choose(true);
    lab.until("the recovery card", |l| l.card(RECOVERY).is_some());
    assert!(launches(&lab).is_empty());
    assert_eq!(lab.card(RECOVERY).unwrap().1[0], "Claude · A", "the other Claude account first");
    assert!(lab.card(RECOVERY).unwrap().1.last().unwrap().starts_with("Wait for Claude 2"));
    let _ = lab.stop.send(true);
}

/// T3 0.1.6's record of a spent Claude account's run (2 Nov 06:29, thread c920c7d1): its notice (the id carrying the
/// reset), Claude Code's reply and the error item, one per line.
fn spent_text(reset_s: u64) -> String {
    format!("You've hit your weekly limit · resets 1pm (Europe/Warsaw)\nUsage limit reached: Claude API rate limit reached. Try again later. [turn-item:provider:claudeAgent:native-item:terminal-failure]\nClaude usage limit reached. This turn is paused until the 7-day limit resets in 6h 31m. [turn-item:provider:claudeAgent:native-item:usage-limit%3Aprovider-turn%3Aprovider%3AclaudeAgent%3Aseven_day%3A{reset_s}]")
}

/// 2 Nov, bug notes 2026-11-02T14-47-34, -37, -44 and -46. Relay mode on Claude main, its state from an older build:
/// no route, and a stale built-in original with an open "back to GPT?" question. The account runs out with T3's limit
/// report. The original is Claude main (never the built-in agent), the reset is on the card ("out until 13:00"), the
/// spent thread gets no message (no run on the spent account), background helpers hear the account is spent, and the
/// card, its countdown and the switch are announced. When Claude main works again, the user is asked about it.
#[test]
fn the_relay_s_true_original_its_limit_and_reset_and_the_announced_switch() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("data").join(em::DIR);
    std::fs::create_dir_all(&state_dir).unwrap();
    let mut old = State::default();
    old.phase = Phase::Covering; old.reason = Reason::Chosen; old.since_ms = 1; old.failed_probes = 6;
    old.thread = Some("thread-claudeAgent".into()); old.harness = Some("Claude (claude-a)".into());
    old.model = Some(chatgpt()); old.original = Some(Original::Pi { model: Some(chatgpt()) }); old.return_asked = Some("relay-return:1".into());
    std::fs::write(state_dir.join("state.json"), serde_json::to_vec(&old).unwrap()).unwrap();
    let lab = Lab::at(dir, &[]);
    second_claude(&lab);
    {
        let s = lab.em.state.lock().unwrap();
        assert!(s.original.is_none() && s.return_asked.is_none(), "the stale built-in original goes at load");
    }
    lab.until("the relay acts again", |l| l.em.phase() == Phase::Covering && l.connection().is_some());
    let reset_s = agent::now_ms() / 1000 + 3 * 3600;
    let reset = reset_s * 1000;
    *lab.t3.fail_text.lock().unwrap() = Some(spent_text(reset_s));
    lab.t3.failing.lock().unwrap().push("claudeAgent".into());
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "Is the build green?".into(), images: vec![] });
    lab.until("the recovery card", |l| l.card(RECOVERY).is_some());
    let at = agent::limits::when(reset, agent::now_ms());
    let (_, options) = lab.card(RECOVERY).unwrap();
    assert_eq!(options, ["Claude 2 · A", "Codex · codex-a", "Grok · grok-a", "Wait for Claude · A"], "Claude main is the original");
    let card = lab.side.notices.lock().unwrap().iter().rev().find(|n| n.id.starts_with(RECOVERY)).cloned().unwrap();
    assert_eq!(card.text, format!("Claude · A is out until {at}. Switch to?"));
    let Some(ui::agent_notice::NoticeAction::Ask { questions, .. }) = &card.action else { panic!() };
    assert_eq!(questions[0].options[3].description.as_deref(), Some(format!("back at {at}").as_str()));
    let cause = lab.em.state.lock().unwrap().cause.clone().and_then(|c| Cause::from_json(&c)).unwrap();
    assert_eq!(cause, Cause::Quota { reset_ms: Some(reset), line: Some(format!("Claude's weekly limit is used up; it resets at {at}.")) });
    // No "stop acting" message started a run on the spent account.
    let to_spent: Vec<String> = lab.t3.sent.lock().unwrap().iter().filter(|(t, _)| t == "thread-claudeAgent").map(|(_, m)| m.clone()).collect();
    assert_eq!(to_spent.len(), 1, "only the relayed words: {to_spent:?}");
    assert_eq!(*lab.side.spent.lock().unwrap(), [("claudeAgent".to_owned(), Some(reset))], "background helpers move off it");
    // Announced: said (or shown with Speaking off) and noticed on the island.
    assert!(lab.side.said.lock().unwrap().contains(&format!("A on Claude is out of quota until {at}. In a minute I'll switch to A on Claude 2, unless you pick something else on the card.")), "{:?}", lab.side.said.lock().unwrap());
    assert!(lab.side.cues.lock().unwrap().contains(&ui::HealthCue::Notice { part: ui::Part::Agent, label: format!("Claude · A out till {at} · switching in a minute") }));
    lab.pick(RECOVERY, "Claude 2");
    lab.until("the other account acts", |l| l.em.phase() == Phase::Covering && launches(l) == ["launch:claudeAgent_2"]);
    lab.until("the switch is said", |l| l.side.said.lock().unwrap().iter().any(|x| x == "Switched to A on Claude 2."));
    assert!(lab.side.cues.lock().unwrap().contains(&ui::HealthCue::Notice { part: ui::Part::Agent, label: "Now on Claude 2 · A".into() }));
    assert_eq!(lab.em.state.lock().unwrap().next_probe_ms, reset + 60_000, "Claude main is checked at its reset");
    // Claude main's quota reads usable before the reset T3 reported: a limit the reading misses still holds, so
    // nothing asks yet (fresh review, 2 Nov).
    lab.t3.failing.lock().unwrap().clear();
    set_quota(&lab, "claudeAgent", quota_of(0.0, 0.0, agent::now_ms() + 160 * HOUR));
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    std::thread::sleep(Duration::from_millis(200));
    assert!(lab.card(RETURN).is_none(), "not before T3's reset");
    assert!(lab.em.state.lock().unwrap().next_probe_ms >= reset, "checked again at the reset");
    // The reset passes and the quota reads usable: the user is asked about it, by voice and card.
    lab.em.set(|s| for x in s.spent.iter_mut() { x.reset_ms = Some(agent::now_ms() - 1) });
    lab.go_back();
    assert!(lab.side.said.lock().unwrap().iter().any(|l| l == "A on Claude works again. Do you want to go back to it, or stay where we are?"));
    lab.until("back on Claude main", |l| l.em.phase() == Phase::Covering && launches(l).last().map(String::as_str) == Some("launch:claudeAgent"));
    let _ = lab.stop.send(true);
}

/// 2 Nov 13:05: the replacement fails and the search lands on the original account itself (Claude main had reset):
/// the chain is over, so nothing asks to go back to where Heimdall already is.
#[test]
fn a_search_that_lands_on_the_original_ends_the_chain() {
    let lab = Lab::new(&[]);
    second_claude(&lab);
    lab.em.choose(true);
    lab.until("the relay acts", |l| l.em.phase() == Phase::Covering);
    *lab.t3.fail_text.lock().unwrap() = Some(spent_text(agent::now_ms() / 1000 + 3600));
    lab.t3.failing.lock().unwrap().push("claudeAgent".into());
    (lab.em.relay())(Relayed { id: "w1".into(), kind: "wake".into(), text: "one".into(), images: vec![] });
    lab.pick(RECOVERY, "Claude 2");
    lab.until("the other account acts", |l| l.em.phase() == Phase::Covering && launches(l).len() == 2);
    // Claude main resets; Claude 2 runs out. Nobody picks: the search starts on the same model elsewhere, Claude main.
    lab.em.set(|s| s.spent.clear());
    *lab.t3.failing.lock().unwrap() = vec!["claudeAgent_2".into()];
    *lab.t3.fail_text.lock().unwrap() = Some("You've hit your weekly limit · resets Nov 6 at 9am (Europe/Warsaw)".into());
    (lab.em.relay())(Relayed { id: "w2".into(), kind: "wake".into(), text: "two".into(), images: vec![] });
    lab.until("the card", |l| l.em.phase() == Phase::Choosing && l.card(RECOVERY).is_some());
    assert_eq!(lab.card(RECOVERY).unwrap().1[0], "Claude · A");
    assert!(lab.card(RECOVERY).unwrap().1.last().unwrap().starts_with("Wait for Claude · A"), "the original stays Claude main");
    lab.t3.failing.lock().unwrap().clear();
    lab.until("searched by itself onto Claude main", |l| l.em.phase() == Phase::Covering && launches(l).len() == 3);
    assert_eq!(launches(&lab)[2], "launch:claudeAgent");
    assert!(lab.em.state.lock().unwrap().original.is_none(), "back where it began");
    lab.until("said", |l| l.side.said.lock().unwrap().iter().any(|x| x == "Switched to A on Claude."));
    lab.t3.complete.store(true, Ordering::SeqCst);
    lab.until("its runs end", |l| l.em.runs.load(Ordering::Acquire) == 0);
    lab.em.set(|s| s.next_probe_ms = 0);
    lab.poke();
    std::thread::sleep(Duration::from_millis(200));
    assert!(lab.card(RETURN).is_none(), "nothing to go back to");
    let _ = lab.stop.send(true);
}

/// Bug note 2026-11-02T14-47-39: the built-in agent's ChatGPT running out no longer drops Codex when Codex's own
/// quota reads usable.
#[test]
fn codex_with_room_is_offered_when_chatgpt_runs_out() {
    let lab = Lab::new(&[]);
    set_quota(&lab, "codex", quota_of(0.0, 74.0, agent::now_ms() + 50 * HOUR));
    lab.em.enter(quota(), Some(chatgpt()));
    lab.until("the recovery card", |l| l.card(RECOVERY).is_some());
    assert!(lab.card(RECOVERY).unwrap().1.iter().any(|o| o == "Codex · codex-a"), "{:?}", lab.card(RECOVERY));
    let _ = lab.stop.send(true);
}

/// Bug note 2026-11-02T14-47-37: T3's items for a failed run (the live 0.1.6 shapes) become one text that reads as
/// the weekly limit with its exact reset; another run's items stay out.
#[test]
fn a_failed_run_s_items_read_as_its_limit() {
    let items = vec![
        json!({"runId":"r925","type":"system_notice","title":"Claude usage limit reached. This turn is paused until the 7-day limit resets in 6h 31m.","text":"Claude usage limit reached. This turn is paused until the 7-day limit resets in 6h 31m.","itemId":"turn-item:provider:claudeAgent:native-item:usage-limit%3Aprovider-turn%3Aseven_day%3A1793620800"}),
        json!({"runId":"r926","type":"user_message","text":"Your account cannot answer now"}),
        json!({"runId":"r926","type":"assistant_message","text":"You've hit your weekly limit · resets 1pm (Europe/Warsaw)"}),
        json!({"runId":"r926","type":"error","title":"Usage limit reached","text":"Claude API rate limit reached. Try again later.","itemId":"x"}),
    ];
    let text = failure_text(&items, "r926").unwrap();
    assert_eq!(text, "You've hit your weekly limit · resets 1pm (Europe/Warsaw)\nUsage limit reached: Claude API rate limit reached. Try again later. [x]");
    let all = failure_text(&items, "r925").unwrap();
    assert_eq!(agent::limits::parse(&all, 1_793_597_340_000).unwrap().reset_ms, Some(1_793_620_800_000));
    let grok = [json!({"runId":"r3","type":"error","title":"Provider error","text":"API error (status 402 Payment Required): Grok Build usage balance exhausted","itemId":"g"})];
    let cause = em::relay_cause(&failure_text(&grok, "r3").unwrap(), "Grok", 0).unwrap();
    assert_eq!(cause, Cause::Quota { reset_ms: None, line: Some("Grok's prepaid balance is used up; it does not come back by itself.".into()) });
    assert!(failure_text(&items[1..2], "r926").is_none());
    // Review 2 Nov: a working run's prose is no failure, and the user's Stop (interrupted) stays no failure.
    let prose = [json!({"runId":"r9","type":"assistant_message","text":"Your Claude usage limit has plenty of room today."})];
    assert!(failure_text(&prose, "r9").is_none());
    let Run::Failed(stopped) = ended("interrupted", Some("Your usage limit is fine.".into())) else { panic!() };
    assert_eq!(stopped, "run interrupted");
    assert_eq!(em::relay_cause(&stopped, "Claude", 0), None, "Stop never switches accounts");
    let Run::Failed(failed) = ended("failed", Some("Provider error: API error (status 402 Payment Required): balance exhausted [g]".into())) else { panic!() };
    assert!(matches!(em::relay_cause(&failed, "Grok", 0), Some(Cause::Quota { .. })));
}

/// Review 2 Nov: relay mode never takes the built-in agent for its original, even without a route; the built-in
/// agent failing keeps what T3 reported spent.
#[test]
fn relay_mode_s_host_is_never_the_built_in_agent_and_a_pi_failure_keeps_the_spent_accounts() {
    let mut s = State::default();
    s.reason = Reason::Chosen; s.model = Some(chatgpt()); s.harness = Some("Claude (claude-opus-5-5)".into());
    assert!(matches!(s.host(), Original::Relay(r) if r.label() == "Claude · Opus 5.5"));
    s.reason = Reason::Emergency;
    assert!(matches!(s.host(), Original::Pi { .. }));
    let lab = Lab::new(&[]);
    let until = agent::now_ms() + HOUR;
    lab.em.set(|s| s.spent.push(Spent { instance: "claudeAgent".into(), reset_ms: Some(until), line: "spent".into(), at_ms: agent::now_ms(), billing: false }));
    lab.em.enter(quota(), Some(chatgpt()));
    lab.until("the recovery card", |l| l.card(RECOVERY).is_some());
    assert_eq!(lab.em.state.lock().unwrap().spent_until("claudeAgent", agent::now_ms()), Some(until));
    assert!(!lab.card(RECOVERY).unwrap().1.iter().any(|o| o.starts_with("Claude ·")), "the spent account is not offered");
    let _ = lab.stop.send(true);
}

/// Bug note 2026-11-02T14-47-43: the hand-over context holds the latest conversation, up to the switch: in relay mode
/// Pi's session has only context entries (held input relayed, the relay's mirrored lines), and those were dropped,
/// so every context file ended with the last turn Pi ran itself (30 Oct 06:36).
#[test]
fn the_hand_over_context_runs_up_to_the_switch() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    let lines = [
        json!({"type":"message","timestamp":"2026-10-30T05:36:05.909Z","message":{"role":"user","content":"old question"}}),
        json!({"type":"message","timestamp":"2026-10-30T05:36:16.930Z","message":{"role":"assistant","content":[{"type":"text","text":"old answer"}]}}),
        json!({"type":"custom_message","customType":"heimdall_context","timestamp":"2026-11-02T05:45:28.531Z","content":format!("{}\n[Heimdall typed message]\nhey I'm away for an hour", em::HELD)}),
        json!({"type":"custom_message","customType":"heimdall_context","timestamp":"2026-11-02T05:46:00.000Z","content":"[Heimdall notice: emergency-cover:mirror:mcp_c6c1:48:0]\n[Relay agent] Got it. Nothing will be said until you're back."}),
        json!({"type":"custom","customType":"heimdall_loop_guard_other","timestamp":"2026-11-02T05:47:00.000Z"}),
    ];
    std::fs::write(&session, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
    let tail = conversation_tail(&session, 400 * 1024).unwrap();
    let rows: Vec<&str> = tail.lines().collect();
    assert_eq!(rows.len(), 4, "one line each, the typed header gone: {tail}");
    assert!(rows[0].ends_with("To Heimdall: old question") && rows[1].ends_with("Heimdall: old answer"), "{tail}");
    assert!(rows[2].ends_with("To Heimdall (relayed to the relay agent): hey I'm away for an hour"), "{tail}");
    assert!(rows[3].ends_with("[Relay agent] Got it. Nothing will be said until you're back."), "the latest line is the latest entry: {tail}");
}

/// The last relay thread is not reused while its account is reported spent: a send there would start a run on the
/// exhausted account (bug note 2026-11-02T14-47-44; fresh review, 2 Nov).
#[test]
fn the_last_relay_thread_on_a_spent_account_is_not_reused() {
    let now = agent::now_ms();
    let mut s = State::default();
    s.last_route = Some(crate::emergency_recovery::Route { instance: "claudeAgent".into(), driver: "claudeAgent".into(), name: "Claude".into(), model: "claude-a".into() });
    assert!(!s.last_route_spent(now), "nothing reported spent");
    s.spent.push(Spent { instance: "claudeAgent_2".into(), reset_ms: Some(now + HOUR), line: String::new(), at_ms: now, billing: false });
    assert!(!s.last_route_spent(now), "another account");
    s.spent.push(Spent { instance: "claudeAgent".into(), reset_ms: Some(now + HOUR), line: String::new(), at_ms: now, billing: false });
    assert!(s.last_route_spent(now), "its own account is spent");
    assert!(!s.last_route_spent(now + 2 * HOUR), "after its reset it may be reused");
}
