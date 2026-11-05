//! The MCP tools Heimdall serves to its agent (`Tools`, `agent::mcp::ToolHost`): screen, archive, island, speech,
//! T3 and settings tools.
use super::*;

/// What a card's answers say in a line: "Yes", "Warm and Dark; Other: \"soft\"" for two questions. A question left
/// unanswered says nothing.
pub(crate) fn answer_summary(answers: &[ui::agent_notice::QuestionAnswer]) -> String {
    answers.iter().filter(|a| !a.labels.is_empty() || a.other.is_some()).map(|a| {
        let mut parts = a.labels.clone();
        parts.extend(a.other.as_ref().map(|o| format!("Other: \"{o}\"")));
        parts.join(" and ")
    }).collect::<Vec<_>>().join("; ")
}

/// Commits the answers a spoken choice completed (ask_voice.rs). The island's card is retracted with his words only
/// once the ledger has the answer; a refused commit is an error and the card stays up (R8).
pub(super) fn commit_voice_choice(actions: &agent::actions::Actions, ui: Option<ui::Ui>, id: &str, picks: &[agent::actions::AskAnswer], permit: &agent::delivery::DeliveryPermit, words: &str) -> Result<()> {
    match actions.answer_card(id, picks, permit)? {
        agent::actions::Clicked::Delivered => {
            if let Some(ui) = ui { ui.send(ui::UiUpdate::CloseQuestion { id: id.into(), answer: words.into() }); }
            eprintln!("Question {id} answered by User's voice: {words:?}");
        }
        // A click or a voice close got there first, and retracted the card itself.
        other => eprintln!("Question {id} answered by voice, but {other:?}"),
    }
    Ok(())
}

/// An `ask` card on the island (questions.rs).
pub(crate) struct OpenQuestion {
    pub(super) permit: agent::delivery::DeliveryPermit,
    /// The card's questions as asked, joined (his words are checked against them).
    pub(super) question: String,
    /// Host clock, when it was shown.
    pub(super) shown_ns: u64,
    /// Every question of the card, in order (a spoken choice answers the first one still open).
    pub(super) items: Vec<agent::actions::AskItem>,
    /// When the card expires (ms since the epoch): an expired card takes no words and no answer (R8).
    pub(super) expires_ms: u64,
    /// The user's picks so far, one per question he has answered (a Pick from the card, or a spoken choice).
    pub(super) picks: Vec<agent::actions::AskAnswer>,
}
/// What the island's Stop cut (`Tools::stop_pressed`): `playing`, Heimdall was speaking at the press (not merely a
/// line held for the user); `run`, the run whose reply was cut, if a run's reply was.
#[derive(Clone, Debug, Default)]
pub(crate) struct StopPress {
    pub(crate) playing: bool,
    pub(crate) run: Option<super::speech_policy::RunTag>,
}
pub struct Tools {
    pub(crate) send_context: Mutex<decide::send::Conversation>,
    pub(super) speech_text: Mutex<HashMap<u64,String>>,
    /// The run each queued reply belongs to (`run_speech`): the island's Stop labels its waking line and silences the
    /// rest of that run. Speech that is no run's reply (a quota line, emergency mode) has none.
    pub(super) speech_runs: Mutex<HashMap<u64,super::speech_policy::RunTag>>,
    /// Agent speech jobs stopped because Speaking went off (not because the user talked): shown as text when their
    /// cancellation arrives.
    pub(super) unheard_by_mode: Mutex<std::collections::HashSet<u64>>,
    /// The reply path (`agent::reply`): what `speak` said this turn, so the final reply never says it again.
    pub(crate) replies: Mutex<agent::reply::Replies>,
    pub(super) reload_data: PathBuf,
    pub(super) reload_intents: Mutex<()>,
    /// 0: no manager (lab fallback), 1: waiting/off, 2: ready or idle.
    pub(crate) engine_signal: Arc<AtomicU8>,
    /// The built-in engine is loading its model (its state is Starting).
    pub(crate) engine_loading: AtomicBool,
    pub(crate) agent_commands: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    /// Inference Engine ▸ API Providers: keys, catalog and the menu's rows.
    pub api: Arc<crate::api_providers::Hub>,
    pub mcp_actions: crate::mcp_actions::Host,
    /// Open `ask` cards: the permit they were asked under, their words, and when they were shown (questions.rs).
    pub(super) pending_questions: Mutex<HashMap<String, OpenQuestion>>,
    /// Recent clicks that answered a card, for the voice-override check (questions.rs).
    pub(super) card_clicks: Mutex<Vec<super::questions::CardClick>>,
    /// The user's utterances (send-context seq) that d1, his name or "send it" sent to Heimdall: the only speech that
    /// closes a card or overrides a click (questions.rs).
    pub(super) card_speech: Mutex<std::collections::BTreeSet<u64>>,
    pub(super) notice_ui: Mutex<Option<ui::Ui>>,
    pub(super) notice_actions_available: AtomicBool,
    pub(super) watcher_running: AtomicBool,
    pub(super) queue: Mutex<Option<Speech>>,
    /// The Speed slider's sample: its own queue, so it plays whatever the agent's state, and a new sample
    /// replaces the one playing.
    pub(super) sample_queue: Mutex<Option<speak::SpeechQueue>>,
    /// Keeps Jarvis resident for the Speed slider's sample until then (`SampleVoice`).
    pub(crate) sample_hold: Arc<Mutex<Option<std::time::Instant>>>,
    pub(super) sample_generation: Arc<AtomicU64>,
    /// Speaking ▸ Voice: each voice's cached sample, played on a click (`voice_preview`).
    pub(super) voice_previews: super::voice_preview::Previews,
    /// Voice speed in percent of natural (settings `voice_speed_percent`).
    pub voice_speed: Arc<AtomicU16>,
    pub enabled: AtomicBool,
    pub(super) speech_status: Mutex<Option<String>>,
    /// Spoken lines failed in a row since the last one played, and when the last failed (`speech_failed`).
    pub(super) speech_failures: Mutex<(u32, Option<std::time::Instant>)>,
    pub(super) screenshot_status: Mutex<Option<String>>,
    pub archive_floor: Arc<AtomicU64>,
    pub agent_enabled: Arc<agent::delivery::DeliveryGate>,
    /// Listening's own ordered on/off intent (`StopReason::Heimdall`), persisted like Agent's but never part of
    /// delivery: Listening off stops capture and transcription only; the agent keeps running.
    /// Only its reason bit and generation are used; no permit is ever taken from it.
    pub listen_latch: Arc<agent::delivery::DeliveryGate>,
    // Agent/Heimdall menu intents issued by the UI callback, not yet processed.
    pub(super) menu_pending: [AtomicU64; 2],
    // Short enqueue/exit ordering only; never held during disk or helper I/O.
    pub(super) intent_acceptance: Mutex<IntentAcceptance>,
    pub(super) intent_path: PathBuf,
    pub(super) intent_writer: std::result::Result<IntentWriter, String>,
    pub tts_backend: Mutex<String>,
    pub drama3: Arc<speak::drama3::Service>,
    /// Speaking ▸ the voice: route, Phonon S, ElevenLabs (night 1022 menu).
    pub speech_voices: Arc<crate::speech_voice::Voices>,
    /// The loaded speech model, shared by replies and the bank fill; `shutdown` releases it.
    pub(super) tts_model: Arc<Mutex<Option<speak::Supertonic>>>,
    /// Seeing ▸ off: screenshot tools fail plainly (`SCREEN_OFF`), never naming the switch.
    pub screen_off: AtomicBool,
    pub(super) voices: Mutex<HashSet<String>>,
    pub(super) models: PathBuf,
    pub buffer: Mutex<Option<Arc<Mutex<screen::ScreenBuffer>>>>,
    pub(super) shots: PathBuf,
    pub archive_session: Mutex<Option<PathBuf>>,
    /// Pre-generated reply lines and the store of every generated voice clip.
    pub voice_bank: Arc<crate::voice_bank::Bank>,
    pub embeddings: Arc<heimdall_archive::embedding::Service>,
    /// Fast reading: messages waiting for the user, and his reading-speed memory.
    pub fast_reads: crate::fast_read::Reads,
    /// When a microphone last started or stopped speaking (the bank fill waits for quiet).
    pub(super) voice_changed: Mutex<std::time::Instant>,
    /// What Heimdall said recently (agent speech, bank lines), newest last: the barge-in
    /// decision reads it, and tells Heimdall's own voice on the microphone from the user's.
    pub(super) spoken: Mutex<VecDeque<(std::time::Instant, String)>>,
    /// The instant acknowledgement said for the user's words through this pending turn, and its line for the wake
    /// (`[Heimdall already said: "On it." at 16:04:10]`, pipeline/ack.rs).
    pub(crate) ack_said: Mutex<Option<(u64, String)>>,
    /// The user's turn on the microphone: agent speech waits for it to end (`hold`, night pass V1).
    pub(crate) owner: Arc<super::hold::OwnerVoice>,
    /// Agent speech jobs the barge-in cut because the user talked over them: the agent learns what he missed.
    pub(super) cut_by_owner: Mutex<HashSet<u64>>,
    /// Lines cut that way so far, with whether he heard their start: one note for all of them once every cut job has
    /// settled (night review 20 Oct: one note per reply, not per line).
    pub(super) missed: Mutex<Vec<(String, bool)>>,
    /// Lines skipped because the user was talking when they would have played (`hold`, night pass 1022): never said, and
    /// shown as one island line once the skipped reply is over (bug note 2026-10-26T18-35-49); one note then, or
    /// inside the wake that carries his words.
    pub(super) skipped: Mutex<Vec<String>>,
    /// The once-only wake that answers a skipped reply when no wake of the user's carried its note (night 1022 review 1).
    pub(super) skip_retry: Arc<Mutex<SkipRetry>>,
    /// Speech jobs queued in a run the user's address by name woke (`SpeechPolicy::addressed_now` at `speak`).
    pub(super) addressed_jobs: Mutex<HashSet<u64>>,
    /// A line of the skipped reply was his answer: its retry may be spoken in a call (bug notes 2026-11-04T20-52-24,
    /// 2026-11-05T00-14-40: the retry's fresh answer was held for minutes because the run's address had settled).
    pub(super) skipped_addressed: AtomicBool,
    /// The island line showing the skipped reply's lines as they are skipped (bug note 2026-11-05T02-32-37): its notice
    /// id and text, updated as more lines are skipped, until the reply is over.
    pub(crate) skipped_notice: Mutex<Option<(String, String)>>,
    /// The lines cut now were cut by a guest on the call talking, not the user (`guest_talked_over`): the note says so.
    pub(super) cut_by_guest: AtomicBool,
    /// Tests: Heimdall's voice counts as audible (the speaking hook is not installed in unit tests).
    #[cfg(test)]
    pub(crate) test_audible: AtomicBool,
    /// Tests: each voice level `apply_call_level` chose, oldest first (the speak crate's level is one for the process).
    #[cfg(test)]
    pub(crate) test_levels: Mutex<Vec<f32>>,
    /// The last final reply sent to the island's pull-down, with its speech jobs: withdrawn if they are dropped unheard
    /// for a wake of his (night 1022 review 4); one skipped at the speaker stays (bug note 2026-10-26T18-35-49).
    /// The third part is the replies the user had probably not read when it came, shown above it (`unread`): what the
    /// pull-down keeps when it is withdrawn.
    pub(super) final_shown: Mutex<Option<(HashSet<u64>, String, Option<String>)>>,
    /// Final replies shown as text whose reading time has not passed (bug note 22 Oct 15:13): a reply arriving in it
    /// keeps them in the pull-down, above it, and the agent learns he probably missed them.
    pub(super) unread: Mutex<super::unread::Unread>,
    /// The agent is in a run, as the supervisor last reported (`agent_working`): a new run releases a skip latch
    /// left by an earlier one (night 1022 review 3).
    pub(super) run_active: AtomicBool,
    /// The wake sound and conversations: when Heimdall makes a sound and when it keeps quiet (speech_policy.rs).
    pub(crate) speech_policy: super::speech_policy::SpeechPolicy,
    /// Emergency mode (docs/emergency.md): the controller of the cover thread while the agent is down.
    pub emergency: Arc<crate::emergency::Emergency>,
    /// What Agent ▸ Model ▸ Claude Code subscription offers: T3's Claude models and accounts (`crate::relay_catalog`).
    pub relay_catalog: Arc<crate::relay_catalog::RelayCatalog>,
    /// Tests replace the native full-resolution grab, which would ask for Screen Recording.
    #[cfg(test)]
    pub(crate) test_grabber: Mutex<Option<screen::Grabber>>,
    /// The pull-down's updates (`reply_to_island`), as tests see them.
    #[cfg(test)]
    pub(crate) test_island: Mutex<Vec<ui::UiUpdate>>,
}
/// The wake that answers a skipped reply (`Tools::after_skip`): one per skipped reply, `spent` until a line reaches
/// the user or his words reach the agent, so a reply skipped again is told as context only (no loop of retries).
#[derive(Default)]
pub(crate) struct SkipRetry {
    /// The skipped lines waiting for the user's pause to end; a wake carrying his words takes them first.
    pending: Option<Vec<String>>,
    spent: bool,
    /// The skipped reply answered his address by name: the retry carries it (`after_skip`).
    addressed: bool,
}
/// The id prefix of that wake (an `Event` notice: it starts a turn).
pub(crate) const SKIPPED_RETRY: &str = "skipped-reply";
/// Quiet needed past the end of the user's pause (`hold::QUIET_AFTER`) before the retry wakes the agent: his own words, if
/// the decider sends them, carry the note first.
const RETRY_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
/// The retry never waits longer than this for his pause: it then wakes anyway (its answer is skipped again if he is
/// still talking, and told as context). Was 300 s; bug note 2026-11-05T02-32-37: an answer minutes
/// later has no point, so it is a minute, as for a held line (`speech_policy::HELD_FOR`).
const RETRY_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
/// Failed spoken lines in a row before the menu says Heimdall can't speak (`Tools::speech_failed`).
pub(crate) const SPEECH_FAILURES_SHOWN: u32 = 2;
/// How long that line stays after the last failure without a new one.
pub(crate) const SPEECH_FAILURE_SHOWN_FOR: std::time::Duration = std::time::Duration::from_secs(600);
/// The note for lines skipped because the user was talking (night pass 1022, bug note 21 Oct 18:42): never said and never
/// replayed, but shown on the island as text (bug note 2026-10-26T18-35-49); the agent answers fresh.
pub(crate) fn skipped_note(lines:&[String]) -> String {
    format!("This was not said: User was talking when it would have played, so it was shown on the island as text instead, and it will not be replayed. Answer fresh in your next turn without repeating it, folding in what still matters with his new words: {}",lines.join(" "))
}
/// The note for lines waiting out his pause that his words made stale (`drop_held_speech`): dropped unheard and
/// unshown; the agent answers his words fresh.
pub(crate) fn dropped_note(lines:&[String]) -> String {
    format!("This never reached User: his words came while it waited for his pause, so it was neither said nor shown, and it will not be replayed. Answer fresh in your next turn, folding in what still matters with his new words: {}",lines.join(" "))
}
impl Tools {
    pub(crate) fn persist_send_context(&self, context: &decide::send::Conversation) -> Result<()> {
        if let Some(session) = self.archive_session.lock().unwrap().as_ref() {
            crate::index::live_event(session, "send_context", "heimdall", &[], capture::host_now_ns(), serde_json::to_value(context)?)?;
        }
        Ok(())
    }
    /// Recover through the archive API, including compacted sessions.
    pub(crate) fn restore_send_context(&self) -> Result<()> {
        let root = self.reload_data.join("archive");
        if !heimdall_archive::store::exists(&root) { return Ok(()); }
        if let Some(record)=heimdall_archive::View::latest_send_context(&root)? {
            let mut context=serde_json::from_value::<decide::send::Conversation>(record.payload)?;
            context.finished_ns=context.finished_ns.and_then(|at| {
                let end_wall=record.wall_ns.checked_sub(record.host_ns.checked_sub(at)?)?;
                let ago=heimdall_archive::wall_ns().checked_sub(end_wall)?;
                (ago<=decide::send::FOLLOW_UP_NS).then(||capture::host_now_ns().saturating_sub(ago))
            });
            context.start_clock_epoch();
            *self.send_context.lock().unwrap()=context;
        }
        Ok(())
    }
    pub(super) fn typed_context(&self,text:&str,through:u64,at:u64)->Result<()> {
        let snapshot={
            let mut context=self.send_context.lock().unwrap();
            context.append_at("User (typed)",text,false,at);
            context.sent(through);
            self.persist_send_context(&context)?;
            context.snapshot()
        };
        if let Some(session)=self.archive_session.lock().unwrap().as_ref() {
            crate::index::live_event(session,"send_decision","typed",&[],capture::host_now_ns(),json!({
                "schema":1,"trigger":"typed","outcome":"send","context":snapshot.turns,"pending":snapshot.pending,
                "prompt":null,"input_tokens":null,"probabilities":null,"latency_ms":0.,"audio":null,
                "inference":"bypassed: typed messages send immediately"}))?;
        }
        Ok(())
    }
    /// MCP reload_me queues this action and returns immediately; the calling tool
    /// must finish before the supervisor replaces its process and MCP runtime.
    pub fn reload(&self, reason: impl Into<String>, permit:&agent::delivery::DeliveryPermit) -> Result<()> {
        permit.check()?;
        let tx=self.agent_commands.lock().unwrap().as_ref().context("Agent supervisor is not running")?.clone();
        let reason=self.persist_reload(reason.into(),permit)?;
        tx.send(Command::Reload {reason,abort:false,permit:Some(permit.clone())}).map_err(|_|anyhow::anyhow!("Agent supervisor stopped"))
    }

    /// UI admission happens before the pipeline queue and any actor preparation.
    /// His typed message shows at once, in the Agent window and the pulled-down notch, before the supervisor takes it
    /// (bug note 2026-11-01T00-20-27); a send refused here or later marks it not sent.
    pub fn window_action(&self,action:ui::AgentWindowAction)->Result<()> {
        let permit=self.agent_enabled.permit()?;
        let (command,echo)=match action {
            ui::AgentWindowAction::Send{text,steer} if validate_message(&text).is_ok() => {
                let echo=uuid::Uuid::new_v4().to_string();
                self.window_update(typed_echo(&echo,&text,Echo::Pending));
                crate::chat_history::typed_pending(&echo,&text);
                (Command::Typed{text:text.clone(),steer,echo:echo.clone()},Some((echo,text)))
            }
            action=>(window_command(action),None),
        };
        let queued=self.enqueue_agent_action(Command::Scoped{command:Box::new(command),permit});
        if let (Err(_),Some((echo,text)))=(&queued,&echo) {self.window_update(typed_echo(echo,text,Echo::Failed));crate::chat_history::typed_failed(echo);}
        queued
    }
    /// An Agent window update from outside the supervisor (the UI's own thread).
    fn window_update(&self,update:ui::UiUpdate) {
        #[cfg(test)] self.test_island.lock().unwrap().push(update.clone());
        if let Some(u)=*self.notice_ui.lock().unwrap(){u.send(update);}
    }
    pub fn notice_action(&self,action:ui::agent_notice::AgentNoticeAction)->Result<()> {
        let permit=self.agent_enabled.permit()?;
        self.enqueue_agent_action(Command::NoticeAction{action,permit})
    }
    pub(super) fn enqueue_agent_action(&self,command:Command)->Result<()> {
        self.agent_commands.lock().unwrap().as_ref().context("Agent supervisor is not running")?.send(command).map_err(|_|anyhow::anyhow!("Agent supervisor stopped"))
    }
    pub(super) fn persist_reload(&self,reason:String,permit:&agent::delivery::DeliveryPermit)->Result<String> {
        let _serial=self.reload_intents.lock().unwrap();permit.check()?;
        let mut reasons=pending_reload(&self.reload_data,&self.agent_enabled)?.map(|p|p.reason.split("; also requested ").map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
        for reason in reason.split("; also requested "){if !reasons.iter().any(|r|r==reason){reasons.push(reason.into());}}
        let reason=reasons.join("; also requested ");
        permit.with(|| agent::runtime::atomic_write(&self.reload_data.join("agent/pending-reload.json"),&serde_json::to_vec(&PendingReload::accepted(reason.clone(),permit.generation()))?))?;
        Ok(reason)
    }
    pub fn notice(&self,id:impl Into<String>,text:impl Into<String>,permit:&agent::delivery::DeliveryPermit)->Result<()> {
        permit.check()?;
        let tx=self.agent_commands.lock().unwrap().as_ref().context("Agent supervisor is not running")?.clone();
        notice_to(&tx,id.into(),text.into(),permit)
    }
    /// Emergency mode's island line (`crate::emergency::AppSide`).
    pub(crate) fn emergency_cue(&self,cue:ui::HealthCue){
        #[cfg(test)] self.test_island.lock().unwrap().push(ui::UiUpdate::Health(cue.clone()));
        if let Some(u)=*self.notice_ui.lock().unwrap(){u.send(ui::UiUpdate::Health(cue));}
    }
    /// One island notice from the supervisor about the agent itself (the compaction fallback, bug note 2026-10-28T19-41-00).
    pub(crate) fn agent_notice(&self,label:String){self.emergency_cue(ui::HealthCue::Notice{part:ui::Part::Agent,label});}
    /// One line in Heimdall's voice ("I'm back."), on the path every reply takes.
    pub(crate) fn emergency_say(&self,text:&str)->Result<()>{
        let permit=self.agent_enabled.permit()?;
        self.mcp_actions.speak(self,text,"en",&format!("emergency-say:{}",uuid::Uuid::new_v4()),&permit).map(|_|())
    }
    /// Context in the agent's conversation through the outbox (durable, delivered by the heartbeat; never a turn).
    /// With Agent off (the relay's last `end` after a stand-down) it waits in a private file for the next start
    /// (`emergency_flush_records`; review 27 Oct: it was only logged).
    pub(crate) fn emergency_record(&self,id:&str,text:&str)->Result<()>{
        let permit=match self.agent_enabled.permit(){
            Ok(p)=>p,
            Err(_)=>{
                let dir=self.reload_data.join(agent::emergency::DIR);
                std::fs::create_dir_all(&dir)?;
                use std::io::Write;use std::os::unix::fs::OpenOptionsExt;
                let mut f=std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(dir.join("pending-records.jsonl"))?;
                return Ok(writeln!(f,"{}",json!({"id":id,"text":text}))?);
            }
        };
        self.mcp_actions.actions.as_ref().map_err(|e|anyhow::anyhow!(e.clone()))?.enqueue(id,text,&permit)
    }
    /// Records that waited for Agent on go into the conversation now.
    pub(crate) fn emergency_flush_records(&self){
        let path=self.reload_data.join(agent::emergency::DIR).join("pending-records.jsonl");
        let Ok(body)=std::fs::read_to_string(&path) else {return};
        let _=std::fs::remove_file(&path);
        for line in body.lines() {
            let Ok(v)=serde_json::from_str::<Value>(line) else {continue};
            if let (Some(id),Some(text))=(v["id"].as_str(),v["text"].as_str()) { if let Err(e)=self.emergency_record(id,text){eprintln!("emergency: a waiting record was not kept: {e:#}");} }
        }
    }
    /// The relay's island updates: the notch indicator, the send arrow (`crate::emergency::AppSide`).
    pub(crate) fn emergency_ui(&self,update:ui::UiUpdate){
        #[cfg(test)] self.test_island.lock().unwrap().push(update.clone());
        if let Some(u)=*self.notice_ui.lock().unwrap(){u.send(update);}
    }
    /// T3's Claude models and accounts asked again; the menu and the `settings` tool get them (`crate::relay_catalog`).
    pub(crate) fn refresh_relay_catalog(&self){
        let ui=*self.notice_ui.lock().unwrap();
        self.relay_catalog.refresh(move|update|{if let Some(u)=ui{u.send(update);}});
    }
    /// A relay run works or every one ended: the agent shows Working like a Pi turn (the supervisor republishes it).
    pub(crate) fn emergency_working(&self,on:bool){
        crate::emergency::RELAY_WORKING.store(on,Ordering::Release);
        if let Err(e)=self.agent_working(on){eprintln!("emergency: working state not kept: {e:#}");}
        // A relay run settles like a Pi turn (bug note 2026-10-27T20-20-25): the reply window, the user's address and the
        // skip latch end with it, so the next run's speech is judged, held and cut the same way.
        if !on{self.replies.lock().unwrap().settled();self.speech_policy.run_settled();}
        let _=self.enqueue_agent_action(Command::RelayState);
    }
    /// The T3 work Heimdall follows, one line each, for the relay's context file.
    pub(crate) fn emergency_tasks(&self)->Option<String>{
        let workers=self.mcp_actions.workers.lock().unwrap().clone()?;
        workers.restart_notice(true).ok().flatten()
    }
    /// A menu action the user chose on a recovery card ("Stay on …", `emergency::Side::keep_relay`), applied as his own
    /// click; nothing calls it to switch the agent by itself (30 Oct 01:2x).
    pub(crate) fn emergency_select(&self,action:ui::UiAction){
        match self.mcp_actions.menu.lock().unwrap().clone() { Some(menu)=>menu(action), None=>eprintln!("emergency: the menu is not attached; the choice is not applied") }
    }
    /// The supervisor ends the hold and starts the agent's turn with `back`.
    pub(crate) fn emergency_release(&self,back:Option<String>){
        if let Err(e)=self.enqueue_agent_action(Command::EmergencyRelease{back}){eprintln!("emergency: release not sent: {e:#}");}
    }
    /// The user's pick on a suggestion card (suggest.rs): a notice that wakes the agent (`NoticeKind::classify`: an answer).
    pub fn notice_pick(&self,id:String,text:String)->Result<()> {
        let permit=self.agent_enabled.permit()?;
        self.notice(id,text,&permit)
    }

    pub fn new(models: &Path, data: &Path) -> Self {
        let agent_enabled = Arc::new(agent::delivery::DeliveryGate::new(true));
        let listen_latch = Arc::new(agent::delivery::DeliveryGate::new(true));
        let intent_path = data.join("stop-intent.json");
        let intent_writer = IntentWriter::new(intent_path.clone(), agent_enabled.clone(), listen_latch.clone()).map_err(|e| format!("{e:#}"));
        let drama3=speak::drama3::Service::new(models.join("drama3"));
        // The memory line (bug note 2026-11-03T22-38-44): over 95 GB in use with a job's declared need, Jarvis gives
        // way to Phonon S; tests never read this Mac's memory.
        if !cfg!(test) { drama3.set_memory_gate(speak::memory_line::Gate::native(data)); }
        // Jarvis's Rust scheduler counts its lead in the user's listening time (review 1 Nov: 160 % read as 1x).
        let voice_speed=Arc::new(AtomicU16::new(crate::settings::DEFAULT_VOICE_SPEED_PERCENT));
        drama3.set_pace(voice_speed.clone());
        if let Err(error)=crate::drama3_verify::install(&drama3,models.join("parakeet-ultra")) {eprintln!("Speech verifier unavailable: {error:#}");}
        Self {
            engine_signal: Arc::new(AtomicU8::new(0)),
            engine_loading: AtomicBool::new(false),
            agent_commands: Mutex::new(None),
            api: crate::api_providers::Hub::new(None, data.into(), Default::default()),
            send_context: Mutex::new(Default::default()),
            speech_text: Mutex::new(HashMap::new()),
            speech_runs: Mutex::new(HashMap::new()),
            unheard_by_mode: Mutex::new(Default::default()),
            replies: Mutex::new(Default::default()),
            reload_data: data.into(),
            reload_intents: Mutex::new(()),
            mcp_actions: crate::mcp_actions::Host::new(data),
            pending_questions: Mutex::new(HashMap::new()),
            card_clicks: Mutex::new(Vec::new()),
            card_speech: Mutex::new(Default::default()),
            notice_ui: Mutex::new(None),
            notice_actions_available: AtomicBool::new(ui::agent_notice::ACTIONS_RENDERED),
            watcher_running: AtomicBool::new(false),
            queue: Mutex::new(None),
            sample_queue: Mutex::new(None),
            sample_hold: Arc::new(Mutex::new(None)),
            sample_generation: Arc::new(AtomicU64::new(0)),
            voice_previews: super::voice_preview::Previews::new(data),
            speech_voices: Arc::new(crate::speech_voice::Voices::new(models)),
            voice_speed,
            enabled: AtomicBool::new(true),
            speech_status: Mutex::new(None),
            speech_failures: Mutex::new((0, None)),
            screenshot_status: Mutex::new(None),
            archive_floor: Arc::new(AtomicU64::new(10 * 1024 * 1024 * 1024)),
            agent_enabled,
            listen_latch,
            menu_pending: [AtomicU64::new(0), AtomicU64::new(0)],
            intent_acceptance: Mutex::new(IntentAcceptance::default()),
            intent_path,
            intent_writer,
            tts_backend: Mutex::new("auto".into()),
            drama3,
            tts_model: Arc::new(Mutex::new(None)),
            screen_off: AtomicBool::new(false),
            voices: Mutex::new(HashSet::new()),
            models: models.join("supertonic-3"),
            buffer: Mutex::new(None),
            shots: data.join("shots"),
            archive_session: Mutex::new(None),
            voice_bank: Arc::new(crate::voice_bank::Bank::new(&data.join("archive"), &models.join("decide"))),
            embeddings: Arc::new(heimdall_archive::embedding::Service::with_state(models, Some(data.join("embedding")))),
            fast_reads: crate::fast_read::Reads::new(data),
            voice_changed: Mutex::new(std::time::Instant::now()),
            spoken: Mutex::new(VecDeque::new()),
            ack_said: Mutex::new(None),
            owner: Default::default(),
            cut_by_owner: Mutex::new(HashSet::new()),
            missed: Mutex::new(Vec::new()),
            skipped: Mutex::new(Vec::new()),
            skip_retry: Default::default(),
            addressed_jobs: Default::default(),
            skipped_addressed: AtomicBool::new(false),
            skipped_notice: Mutex::new(None),
            cut_by_guest: AtomicBool::new(false),
            #[cfg(test)]
            test_audible: AtomicBool::new(false),
            #[cfg(test)]
            test_levels: Mutex::new(Vec::new()),
            final_shown: Mutex::new(None),
            unread: Mutex::new(Default::default()),
            run_active: AtomicBool::new(false),
            speech_policy: Default::default(),
            emergency: crate::emergency::Emergency::new(data, crate::emergency::Emergency::cli_for(data), crate::emergency::docs_path()),
            relay_catalog: Default::default(),
            #[cfg(test)]
            test_grabber: Mutex::new(None),
            #[cfg(test)]
            test_island: Mutex::new(Vec::new()),
        }
    }
    pub(super) fn show_question(&self, question: &agent::actions::Question, permit: &agent::delivery::DeliveryPermit) -> Result<()> {
        permit.check()?;
        anyhow::ensure!(self.notice_actions_available.load(Ordering::Acquire), "UI_UNAVAILABLE: question buttons are not rendered yet");
        let ui = self.notice_ui.lock().unwrap().context("UI_UNAVAILABLE: island is unavailable")?;
        let asked = question.questions.iter().map(|i| i.question.as_str()).collect::<Vec<_>>().join(" ");
        let title = question.questions.first().map(|i| i.question.clone()).unwrap_or_default();
        permit.with(|| {
            self.pending_questions.lock().unwrap().insert(question.id.clone(), OpenQuestion {
                permit: permit.clone(),
                question: asked.clone(),
                shown_ns: capture::host_now_ns(),
                items: question.questions.clone(),
                expires_ms: question.expires_ms,
                picks: Vec::new(),
            });
            ui.send(ui::UiUpdate::AgentNotice(ui::agent_notice::AgentNotice {
                action: Some(ui::agent_notice::NoticeAction::Ask {
                    request_id: question.id.clone(),
                    questions: question.questions.iter().map(crate::mcp_actions::island_question).collect(),
                    expires_ms: question.expires_ms,
                    countdown: false,
                }),
                ..ui::agent_notice::AgentNotice::line(question.id.clone(), title.clone(), ui::agent_notice::NoticeKind::Info)
            })); Ok(())
        })
    }
    /// An update for the notch that is not an agent `ask` (a bound thread's open request, islands/binding.rs).
    pub(crate) fn send_notch(&self, update: ui::UiUpdate) {
        if let Some(ui) = *self.notice_ui.lock().unwrap() { ui.send(update) }
    }
    pub(super) fn clear_questions(&self) {
        let ids:Vec<_>=self.pending_questions.lock().unwrap().drain().map(|(id,_)|id).collect();
        self.card_clicks.lock().unwrap().clear();
        self.card_speech.lock().unwrap().clear();
        if let Ok(actions)=&self.mcp_actions.actions { for id in ids { if let Err(e)=actions.cancel_question(&id){eprintln!("Could not retire question receipt: {e}");} } }
        if let Some(ui) = *self.notice_ui.lock().unwrap() { ui.send(ui::UiUpdate::ClearAgentNotices); }
        // The cleared notch lost the bound threads' open requests too: the next sync shows them again (R1).
        if let Some(host) = crate::islands::host() { host.bindings.forget_questions(); }
        self.fast_reads.withdraw_all();
    }
    /// The user completed card `id`: one answer per question (a click on a single-question card, or Send). A card of a
    /// bound thread (`t3:` id) goes back to T3 (islands/binding.rs); an `ask` card is committed once in the ledger.
    /// The user dismissed open question `id` (the x or Esc): the agent gets "dismissed" as it would an answer, through the
    /// same close as his spoken `ask_close`. A T3 card closes on the island only: T3 has no dismiss to send.
    pub(super) fn dismiss_question(&self, id: &str) {
        if crate::islands::binding::parse_card_id(id).is_some() {
            return;
        }
        let Some(open) = self.pending_questions.lock().unwrap().remove(id) else { return };
        let Ok(actions) = self.mcp_actions.actions.as_ref() else { return };
        match actions.close(id, "dismissed", &open.permit) {
            Ok(_) => eprintln!("Question {id} dismissed by User"),
            Err(e) => eprintln!("Question {id} not dismissed: {e:#}"),
        }
    }
    /// `by_hand`: the picks the user's own pointer clicked (`ui::island::click_source`); only these write a grant.
    pub(super) fn answer_question(&self, id: &str, answers: &[ui::agent_notice::QuestionAnswer], by_hand: &[ui::agent_notice::QuestionAnswer]) -> Result<()> {
        if let Some((island, request)) = crate::islands::binding::parse_card_id(id) {
            let host = crate::islands::host().context("ISLANDS_UNAVAILABLE: the thread bindings are not running")?;
            let picks = answers.iter().map(|a| crate::islands::binding::Pick { question_id: a.question_id.clone(), labels: a.labels.clone(), other: a.other.clone() }).collect();
            host.t3_job(crate::islands::binding::Job::Answer { island, request, picks });
            return Ok(());
        }
        let choice = answer_summary(answers);
        let Some(open) = self.pending_questions.lock().unwrap().remove(id) else {
            let state = self.mcp_actions.actions.as_ref().ok().and_then(|a| a.state(id).ok());
            // His voice closed it first (questions.rs): the click is dropped, never a second answer.
            if state.as_deref() == Some("closed") {
                self.click_dropped(id, &choice);
                return Ok(());
            }
            // Already answered (a click racing a voice commit, or a second Send): the first answer stands (R16).
            if state.as_deref() == Some("answered") {
                eprintln!("Question {id} is already answered; {choice:?} is not sent again");
                return Ok(());
            }
            anyhow::bail!("ASK_NOT_FOUND: question is no longer active; ask again");
        };
        let permit = open.permit.clone();
        if let Err(e)=permit.check() {if let Ok(actions)=&self.mcp_actions.actions {actions.cancel_question(id)?;}return Err(e);}
        let clicked_ns = capture::host_now_ns();
        let ledger: Vec<agent::actions::AskAnswer> = answers.iter().map(crate::mcp_actions::ask_answer).collect();
        let outcome = match self.mcp_actions.answer_card(id, &ledger, &permit) {
            Err(e) if self.answered_despite(id, &e) => Ok(agent::actions::Clicked::Delivered),
            other => other,
        };
        match outcome {
            Err(e) => {
                if permit.check().is_ok() {if let Ok(actions)=&self.mcp_actions.actions {if actions.state(id).is_ok_and(|s|s=="shown"){self.pending_questions.lock().unwrap().insert(id.into(),open);}}}
                return Err(e);
            }
            Ok(agent::actions::Clicked::ClosedByVoice) => { self.click_dropped(id, &choice); return Ok(()); }
            Ok(agent::actions::Clicked::AlreadyAnswered) => return Ok(()),
            Ok(agent::actions::Clicked::Delivered) => {
                let by_hand: Vec<agent::actions::AskAnswer> = by_hand.iter().map(crate::mcp_actions::ask_answer).collect();
                self.write_grants(id, &open.items, &ledger, &by_hand);
                let mut clicks = self.card_clicks.lock().unwrap();
                clicks.retain(|c| clicked_ns.saturating_sub(c.at_ns) < super::questions::CLICK_MEMORY_NS);
                clicks.push(super::questions::CardClick { id: id.into(), question: open.question, choice: choice.clone(), shown_ns: open.shown_ns, at_ns: clicked_ns });
            }
        }
        if let Some(ui) = *self.notice_ui.lock().unwrap() { notice_outcome(ui,id.into(),ui::agent_notice::NoticeOutcome::Answered { choice }); ui.send(ui::UiUpdate::DismissAgentNotice(id.into())); }
        Ok(())
    }
    /// The user's click on options that carry a grant (`agent::actions::AskGrant`): the deny-list hook lets exactly what they
    /// name through for 30 minutes (`heimdall_emergency::grants`, bug notes 2026-11-03T23-04-19 and
    /// 2026-11-03T18-04-11). Only this click path writes one, and only for options his own pointer clicked (`by_hand`);
    /// a spoken choice (`commit_voice_choice`), a key, an accessibility press or another process's event never does.
    fn write_grants(&self, id: &str, items: &[agent::actions::AskItem], answers: &[agent::actions::AskAnswer], by_hand: &[agent::actions::AskAnswer]) {
        let grants = agent::actions::card_grants(items, answers, by_hand);
        for (question, label, _) in agent::actions::card_grants(items, answers, answers) {
            if !grants.iter().any(|(q, l, _)| *q == question && *l == label) {
                eprintln!("Question {id}: {label:?} was picked, but not by User's own click (a key, his voice, an accessibility press or another process's event): it grants nothing");
            }
        }
        if grants.is_empty() { return; }
        let Some(user) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
        let vault = heimdall_home::home_dir();
        for (question, label, g) in grants {
            // Signed like the rest; a keychain grant (bug note 2026-11-04T23-01-22) is checked new again at the click.
            match heimdall_emergency::grants::write_with(&self.reload_data, &user, &vault, id, &question, &label, &g.delete, &g.login_items, g.keychain.as_deref(), agent::now_ms()) {
                Ok(_) => eprintln!("Question {id}: User's click on {label:?} grants deleting {:?}, removing login items {:?} and making keychain {:?} for 30 minutes", g.delete, g.login_items, g.keychain),
                Err(e) => eprintln!("Question {id}: the grant on {label:?} was not written: {e}"),
            }
        }
    }
    /// `answer_card` commits the answer before it delivers it to the agent: a committed answer whose delivery failed
    /// is answered (its delivery waits in the outbox), not unanswered.
    fn answered_despite(&self, id: &str, e: &anyhow::Error) -> bool {
        let answered = self.mcp_actions.actions.as_ref().is_ok_and(|a| a.state(id).is_ok_and(|s| s == "answered"));
        if answered { eprintln!("Question {id} answered; its delivery waits: {e:#}"); }
        answered
    }
    /// The user's pick for one question of an open card (`Pick` from the island): kept so a spoken choice can finish the
    /// card (`voice_choice`). Nothing is committed here.
    pub(super) fn record_pick(&self, id: &str, answer: &ui::agent_notice::QuestionAnswer) {
        if let Some(open) = self.pending_questions.lock().unwrap().get_mut(id) {
            open.picks.retain(|p| p.question_id != answer.question_id);
            if !answer.labels.is_empty() || answer.other.is_some() {
                open.picks.push(crate::mcp_actions::ask_answer(answer));
            }
        }
    }
    /// The cards still shown on the island, oldest first. A card the agent closed (`ask_close`), clicked, cancelled
    /// or expired leaves the list here, so it never takes his next words or triggers the lower d1 score (questions.rs).
    pub(super) fn open_cards(&self) -> Vec<super::questions::OpenCard> {
        let actions = self.mcp_actions.actions.as_ref().ok();
        let state = |id: &str| actions.map(|a| a.state(id).unwrap_or_default());
        let mut pending = self.pending_questions.lock().unwrap();
        // "pending": the island is drawing it right now (`Actions::ask` marks it shown after `show_question`). An expired
        // card takes no words (R8): the ledger refuses its answer, so it leaves the list here.
        let now = agent::now_ms();
        pending.retain(|id, q| q.expires_ms > now && state(id).is_none_or(|s| matches!(s.as_str(), "pending" | "shown")));
        let mut cards: Vec<_> = pending.iter()
            .filter(|(id, _)| state(id).is_none_or(|s| s == "shown"))
            .map(|(id, q)| super::questions::OpenCard { id: id.clone(), question: q.question.clone(), shown_ns: q.shown_ns, click_only: actions.is_some_and(|a| a.needs_click(id)) })
            .collect();
        cards.sort_by_key(|c| c.shown_ns);
        cards
    }
    /// The cards on screen as the user's utterance (`start_ns`..`end_ns`, host clock) sees them: whether one is up and
    /// whether the utterance falls in its answer window (questions.rs).
    pub(crate) fn card_state(&self, start_ns: u64, end_ns: u64) -> super::questions::CardState {
        super::questions::card_state(self.open_cards().into_iter().map(|c| c.shown_ns), start_ns, end_ns)
    }
    /// d1, his name or "send it" sent the user's utterance `seq` to Heimdall: it may close a card or override a click.
    pub(crate) fn spoke_to_heimdall(&self, seq: u64) {
        let mut speech = self.card_speech.lock().unwrap();
        speech.insert(seq);
        while speech.len() > 256 { speech.pop_first(); }
    }
    /// The agent closed card `id` itself (`ask_close`): it no longer takes the user's words.
    pub(crate) fn question_settled(&self, id: &str) {
        self.pending_questions.lock().unwrap().remove(id);
    }
    #[cfg(test)]
    /// A shown `ask` card at `shown_ns` (host clock) without the island; returns its id.
    pub(crate) fn test_open_card(&self, question: &str, shown_ns: u64) -> String {
        self.test_open_card_with(question, vec![agent::actions::AskItem::one(question, &["Yes", "No"])], shown_ns)
    }
    /// A shown `ask` card of `items` at `shown_ns` without the island; returns its id.
    pub(crate) fn test_open_card_with(&self, question: &str, items: Vec<agent::actions::AskItem>, shown_ns: u64) -> String {
        let permit = self.agent_enabled.permit().unwrap();
        let actions = self.mcp_actions.actions.as_ref().unwrap();
        let body = json!({"questions": items}).to_string();
        let id = actions.receipt(&format!("test-card:{question}"), "ask", &body, Some(agent::now_ms() + agent::actions::QUESTION_LIFETIME_MS), &permit).unwrap();
        actions.set_state(&id, "shown", &permit).unwrap();
        let expires_ms = agent::now_ms() + agent::actions::QUESTION_LIFETIME_MS;
        self.pending_questions.lock().unwrap().insert(id.clone(), OpenQuestion { permit, question: question.into(), shown_ns, items, expires_ms, picks: Vec::new() });
        id
    }
    fn click_dropped(&self, id: &str, choice: &str) {
        eprintln!("Question {id}: click {choice:?} dropped; User's voice answered it first");
        self.record_diagnostic("health", json!({"component":"agent-question","state":"click_superseded_by_voice","request_id":id,"choice":choice}));
    }
    /// A wake carrying the user's speech (supervisor): the card he was answering closes now, showing his words, and the
    /// wake tells the agent so (questions.rs). Only his lines that were for Heimdall count (`spoke_to_heimdall`), and
    /// never a filler. Returns the lines for the wake instruction.
    pub(super) fn voice_closes(&self, through: Option<u64>) -> Vec<String> {
        use super::questions::Spoken;
        let counted = {
            let mut speech = self.card_speech.lock().unwrap();
            let counted: Vec<u64> = speech.iter().copied().filter(|s| through.is_none_or(|n| *s <= n)).collect();
            if let Some(n) = through { speech.retain(|s| *s > n); }
            counted
        };
        let spoken: Vec<Spoken> = {
            let context = self.send_context.lock().unwrap();
            context.snapshot().pending.into_iter()
                .filter(|t| through.is_none_or(|n| t.seq <= n) && t.speaker == "User" && counted.contains(&t.seq) && !super::questions::filler_only(&t.text))
                .map(|t| Spoken { start_ns: t.observation.as_ref().and_then(|o| o["start_ns"].as_u64()).unwrap_or(t.at_ns).min(t.at_ns), end_ns: t.at_ns, text: t.text })
                .collect()
        };
        if spoken.is_empty() { return vec![]; }
        if self.mcp_actions.actions.is_err() { return vec![]; }
        let open = self.open_cards();
        let plan = super::questions::plan(&open, &spoken);
        // His words that are a choice on the card (ask_voice.rs) answer it. Other talk does not close the card: it stays
        // up, with the agent's thinking dots above it, until his click, a spoken choice, or its expiry.
        let choice = plan.close.as_ref().is_some_and(|(id, words)| self.voice_choice(id, &plan.close_lines, words).is_some());
        super::questions::notes(&plan, &open, choice)
    }
    /// The user's words as the choice card `id` is waiting for (ask_voice.rs): the first question still open takes them.
    /// A card that is complete is committed and retracts with his words; a partial one keeps the pick, shows it, and
    /// stays open for the rest. `None` when the words are not a choice, so they close the card as before.
    fn voice_choice(&self, id: &str, lines: &[String], words: &str) -> Option<()> {
        if lines.is_empty() { return None; }
        let (permit, items, picks) = {
            let pending = self.pending_questions.lock().unwrap();
            let open = pending.get(id)?;
            (open.permit.clone(), open.items.clone(), open.picks.clone())
        };
        let current = items.iter().find(|i| !picks.iter().any(|p| p.question_id == i.id))?;
        let labels: Vec<String> = current.options.iter().map(|o| o.label.clone()).collect();
        // Each of the wake's utterances is parsed on its own, in the order the user said them (R9). One that is not a choice
        // makes the wake talk: the card closes with his words, as before.
        let mut heard: Vec<Vec<usize>> = Vec::with_capacity(lines.len());
        for line in lines { heard.push(super::ask_voice::parse(line, &labels, current.multi_select)?); }
        // A multi-select keeps every pick, in option order, each once; a single select keeps the last choice only.
        let chosen: Vec<usize> = if current.multi_select {
            let mut all: Vec<usize> = heard.concat();
            all.sort_unstable();
            all.dedup();
            all
        } else {
            heard.pop()?
        };
        let answer = ui::agent_notice::QuestionAnswer {
            question_id: current.id.clone(),
            labels: chosen.iter().map(|&i| labels[i].clone()).collect(),
            other: None,
        };
        let mut picks = picks;
        picks.push(crate::mcp_actions::ask_answer(&answer));
        let ui = *self.notice_ui.lock().unwrap();
        if picks.len() < items.len() {
            if let Some(open) = self.pending_questions.lock().unwrap().get_mut(id) { open.picks = picks; }
            if let Some(ui) = ui { ui.send(ui::UiUpdate::AskSet { id: id.into(), answer }); }
            return Some(());
        }
        // Committed off this thread: the agent's delivery is served by the supervisor loop this wake runs on, so a drain
        // here would wait on itself (the click path runs in a blocking task for the same reason). The card stays up until
        // the commit is in: a refused commit leaves it answerable instead of retracting it (R8).
        let actions = self.mcp_actions.actions.clone().ok()?;
        if actions.state(id).ok()? != "shown" { return None; }
        let (id, words) = (id.to_owned(), words.to_owned());
        let spawned = std::thread::Builder::new().name("ask-voice-commit".into()).spawn(move || {
            if let Err(e) = commit_voice_choice(&actions, ui, &id, &picks, &permit, &words) {
                eprintln!("Question {id} not answered by voice; the card stays up: {e:#}");
            }
        });
        spawned.ok().map(|_| ())
    }
    pub fn voice(&self, source: &str, on: bool) {
        // The wake sound on the microphone is Heimdall audible too, not the user's turn (speech_policy.rs).
        let heimdall = super::hold::audible() || self.voice_bank.line_playing() || self.speech_policy.cue_sounding(capture::host_now_ns());
        self.owner.set(source, on, heimdall);
        *self.voice_changed.lock().unwrap() = std::time::Instant::now();
        let mut voices = self.voices.lock().unwrap();
        if on {
            voices.insert(source.into());
        } else {
            voices.remove(source);
        }
    }
    pub fn is_speaking(&self) -> bool {
        !self.voices.lock().unwrap().is_empty()
    }
    /// Seconds since a microphone last stopped speaking; zero while one speaks.
    pub fn speech_quiet_seconds(&self) -> f64 {
        if self.is_speaking() { 0. } else { self.voice_changed.lock().unwrap().elapsed().as_secs_f64() }
    }
    /// Heimdall is talking or about to: with Speaking on, agent speech queued, synthesizing or playing, or a
    /// bank line. Agent speech that waits for the user to finish (`hold`), or will wait for him at its first sample, is
    /// not: no barge-in judges a line he has not heard.
    pub fn heimdall_speaking(&self) -> bool {
        self.enabled.load(Ordering::Acquire) && ((self.agent_speech_busy() && !self.owner.defers()) || self.voice_bank.line_playing())
    }
    /// Remembers a text Heimdall is about to say, for two minutes (at most eight texts).
    pub fn note_spoken(&self, text: &str) {
        let mut spoken = self.spoken.lock().unwrap();
        spoken.push_back((std::time::Instant::now(), text.into()));
        while spoken.len() > 8 || spoken.front().is_some_and(|(at, _)| at.elapsed() > SPOKEN_MEMORY) {
            spoken.pop_front();
        }
    }
    /// What Heimdall said in the last two minutes, oldest first.
    #[cfg(test)]
    pub fn recent_speech(&self) -> String {
        let spoken = self.spoken.lock().unwrap();
        spoken.iter().filter(|(at, _)| at.elapsed() <= SPOKEN_MEMORY).map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" ")
    }
    /// Agent speech queued, synthesizing or playing.
    pub fn agent_speech_busy(&self) -> bool {
        self.queue.lock().unwrap().as_ref().is_some_and(|q| q.busy())
    }
    pub fn bank_identity(&self) -> (String, &'static str) {
        // Playing recorded PCM needs neither the generator nor its resident weights.
        // offer_with_fallback handles a Jarvis bank with no accepted takes yet.
        (speak::drama3::VOICE.into(),speak::drama3::GENERATOR)
    }
    /// The bank fill's generator: the shared Supertonic model, not recorded twice.
    pub(crate) fn fill_generator(&self) -> Result<crate::voice_bank::AppSupertonic> {
        let backend = self.tts_backend.lock().unwrap().clone();
        Ok(crate::voice_bank::AppSupertonic {
            synth: LazySynth {
                drama3: None, // Offline ASR-gated tooling owns Drama 3 bank generation.
                session: self.archive_session.lock().unwrap().clone().context("speech archive session missing")?,
                models: self.models.clone(),
                model: self.tts_model.clone(),
                backend: backend.clone(),
                record: None,
                // Bank lines are kept at natural speed; playback applies the current speed.
                speed: Arc::new(AtomicU16::new(100)),
                sample: None,
                voices: None,
            },
            backend,
        })
    }
    #[cfg(test)]
    pub(crate) fn test_speak(&self, text: &str) -> Result<u64> {
        self.queue.lock().unwrap().as_ref().context("no test queue")?.speak(text, "en")
    }
    #[cfg(test)]
    pub(crate) fn test_quiet_since(&self, at: std::time::Instant) {
        *self.voice_changed.lock().unwrap() = at;
    }
    /// Only before installing the callback. Never called while settings apply.
    pub fn initialize_delivery(&self, settings: &Settings) {
        use agent::delivery::StopReason;
        let intent = match std::fs::read(&self.intent_path) {
            Ok(bytes) => serde_json::from_slice::<StopIntent>(&bytes).context("read stop intent"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StopIntent {
                generation: settings.delivery_generation, agent_off: settings.dry_run, heimdall_off: settings.paused,
            }),
            Err(e) => Err(e.into()),
        }.unwrap_or_else(|e| {
            eprintln!("Stop intent unreadable; Agent stays off: {e:#}");
            StopIntent { generation: settings.delivery_generation, agent_off: true, heimdall_off: settings.paused }
        });
        if let Err(e) = self.agent_enabled.seed_generation(intent.generation.max(settings.delivery_generation)) {
            eprintln!("Delivery epoch unreadable; Agent stays off: {e:#}");
            self.agent_enabled.intent(StopReason::Agent, false);
        }
        // Recovery is stop-only: an unversioned durable On is never authority
        // to reopen delivery over settings edited while the app was closed.
        if settings.dry_run || intent.agent_off { self.agent_enabled.intent(StopReason::Agent, false); }
        if settings.paused || intent.heimdall_off { self.listen_latch.intent(StopReason::Heimdall, false); }
    }
    /// The latch that orders `reason`: Listening (Heimdall) has its own, so it never touches the agent's
    /// delivery epoch; Agent and Quit share the delivery gate.
    fn latch(&self, reason: agent::delivery::StopReason) -> &agent::delivery::DeliveryGate {
        match reason { agent::delivery::StopReason::Heimdall => &self.listen_latch, _ => &self.agent_enabled }
    }
    /// Whether the latest explicit intent for `reason` is Off.
    pub fn stopped_for(&self, reason: agent::delivery::StopReason) -> bool {
        self.latch(reason).stopped_for(reason)
    }
    /// The generations of both latches: any explicit intent changes one of them.
    pub fn intent_generation(&self) -> IntentGeneration {
        IntentGeneration { agent: self.agent_enabled.generation(), listen: self.listen_latch.generation() }
    }
    /// Both latches for a durable settings/intent snapshot: (generations, agent_off, listening_off).
    /// Each value is the latest of its own latch; a change racing the read persists again after it.
    pub fn stop_snapshot(&self) -> (IntentGeneration, bool, bool) {
        stop_snapshot(&self.agent_enabled, &self.listen_latch)
    }
    /// The UI callback's Agent/Heimdall intent. It stays pending until the pipeline
    /// processes its queued action, so no older settings-file value can override it.
    pub fn menu_intent(&self, reason: agent::delivery::StopReason, on: bool) {
        if matches!(reason, agent::delivery::StopReason::Quit) {
            self.agent_enabled.intent(reason, false);
            return;
        }
        let mut acceptance = self.intent_acceptance.lock().unwrap();
        if acceptance.closed && on { return; }
        if let Some(pending) = Self::pending(&self.menu_pending, reason) { pending.fetch_add(1, Ordering::AcqRel); }
        self.latch(reason).intent(reason, on);
        self.persist_intent(&mut acceptance);
    }
    /// Sign-in completion carries its starting generation; it is never a new user On authority.
    pub fn resume_agent_intent(&self, generation: u64) -> bool {
        let mut acceptance = self.intent_acceptance.lock().unwrap();
        if acceptance.closed { return false; }
        use agent::delivery::StopReason;
        if self.agent_enabled.intent_if_current(StopReason::Agent, true, generation).is_none() {
            return false;
        }
        self.menu_pending[0].fetch_add(1, Ordering::AcqRel);
        self.persist_intent(&mut acceptance);
        true
    }
    pub(super) fn persist_intent(&self, acceptance: &mut IntentAcceptance) {
        // Late Off stays asynchronous on the UI thread. Exit must observe its
        // durable acknowledgement, even if it arrived after the final flush.
        let reply = acceptance.closed.then(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            acceptance.stops.push(rx);
            tx
        });
        let result = self.intent_writer.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
            .and_then(|writer| writer.tx.as_ref().context("stop intent writer closed")?.send(IntentCommand::Persist(reply)).map_err(Into::into));
        if let Err(e) = result { eprintln!("Stop intent writer unavailable: {e:#}"); }
    }
    /// Automatic relaunch cannot exit until the independent writer durably has the latest stop intent.
    pub fn flush_intent(&self) -> Result<()> {
        self.intent_writer.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?.wait(None, IntentCommand::Flush)
    }
    /// Close On acceptance briefly, then release the lock for flush/helper I/O.
    /// Off remains accepted; wait only for its acknowledgements (250 ms total).
    /// `exit` must terminate immediately, with no I/O. Holding the short lock at
    /// that point orders termination after every accepted Off. Any return reopens.
    pub fn with_final_intent<T>(&self, prepare: impl FnOnce() -> Result<()>, exit: impl FnOnce() -> Result<T>) -> Result<T> {
        self.intent_acceptance.lock().unwrap().closed = true;
        let result = (|| {
            self.flush_intent()?;
            prepare()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            loop {
                let stops = {
                    let mut acceptance = self.intent_acceptance.lock().unwrap();
                    if acceptance.stops.is_empty() { return exit(); }
                    anyhow::ensure!(std::time::Instant::now() < deadline, "late stop intent did not finish; relaunch cancelled");
                    std::mem::take(&mut acceptance.stops)
                };
                for stop in stops {
                    stop.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                        .context("late stop intent did not finish; relaunch cancelled")?
                        .map_err(anyhow::Error::msg)?;
                }
            }
        })();
        let mut acceptance = self.intent_acceptance.lock().unwrap();
        acceptance.closed = false;
        // A future attempt's full flush covers these generations afresh.
        acceptance.stops.clear();
        result
    }
    /// Retire only the generation settings durably saved. All I/O remains owned by the independent writer.
    pub fn intent_saved_in_settings(&self, generation: IntentGeneration) -> Result<()> {
        self.intent_writer.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?
            .wait(Some(&self.intent_acceptance), |reply| IntentCommand::Saved(generation, reply))
    }
    pub fn menu_intent_processed(&self, reason: agent::delivery::StopReason) {
        if let Some(pending) = Self::pending(&self.menu_pending, reason) {
            let _ = pending.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
    }
    /// A fresh settings.json Off/On, ordered at the latch like a menu intent. It
    /// yields (false) to a menu intent for the same reason that is still pending.
    pub fn file_intent(&self, reason: agent::delivery::StopReason, on: bool) -> bool {
        if matches!(reason, agent::delivery::StopReason::Quit) {
            self.agent_enabled.intent(reason, false);
            return true;
        }
        let mut acceptance = self.intent_acceptance.lock().unwrap();
        if acceptance.closed && on { return false; }
        let pending = Self::pending(&self.menu_pending, reason);
        let applied = self.latch(reason).intent_unless(reason, on, || pending.is_some_and(|p| p.load(Ordering::Acquire) > 0)).is_some();
        if applied { self.persist_intent(&mut acceptance); }
        applied
    }
    pub(super) fn pending(counts: &[AtomicU64; 2], reason: agent::delivery::StopReason) -> Option<&AtomicU64> {
        use agent::delivery::StopReason;
        match reason { StopReason::Agent => Some(&counts[0]), StopReason::Heimdall => Some(&counts[1]), StopReason::Quit => None }
    }
    #[cfg(test)]
    pub fn set_agent_enabled(&self, on: bool) {
        self.agent_enabled.store(on, Ordering::Release);
    }
    /// The barge-in decided the user is talking to Heimdall over its speech: speech stops as for any interrupt, the next
    /// line waits for him (`hold`), and the lines cut before he heard all of them are told to the agent as context,
    /// in one note once their cancellations arrive (night pass V1: 19 Oct, four cut replies were never said and the
    /// agent believed they were).
    pub fn talked_over(&self) {
        self.owner.claim();
        self.speech_status();
        let jobs: Vec<u64> = self.speech_text.lock().unwrap().keys().copied().collect();
        self.cut_by_owner.lock().unwrap().extend(jobs);
        self.interrupt();
    }
    /// The user's Stop (the island's Stop button while Heimdall speaks; bug notes 2026-11-05T00-10-37: a stop button to
    /// stop Heimdall speaking, and 2026-11-05T02-32-17 item 4): speech stops at once as for any interrupt (what plays,
    /// what is queued, held lines), and the press is a training signal: this moment wasn't for Heimdall. It is journalled as `speech_stopped` with the words that woke the run and what was being
    /// said, for Decider V6's labels (docs/decision-chain.md), and the agent is told as context. The agent's turn goes
    /// on (the agent window's Stop ends it).
    ///
    /// The rest of the run whose reply was cut is shown on the island unsaid (its final reply came out loud right after
    /// the Stop, review 5 Nov), until the user's own words reach the agent again (`SpeechPolicy::stop_run`). The answer is
    /// what was speaking at the press, for the label (`Pipeline::stop_press`).
    pub(crate) fn stop_pressed(&self) -> StopPress {
        let now=capture::host_now_ns();
        let mut spoken:Vec<(u64,String)>=self.speech_text.lock().unwrap().iter().map(|(j,t)|(*j,t.clone())).collect();
        spoken.sort_by_key(|(job,_)|*job);
        // The oldest job left is the one playing (or about to).
        let run=spoken.first().and_then(|(job,_)|self.speech_runs.lock().unwrap().get(job).cloned());
        let spoken:Vec<String>=spoken.into_iter().map(|(_,t)|t).collect();
        let playing=self.heimdall_speaking();
        let speaking=playing || !spoken.is_empty();
        self.interrupt();
        if !speaking {return StopPress::default();}
        if let Some(run)=&run {self.speech_policy.stop_run(run.run);}
        eprintln!("Speech: User pressed Stop; Heimdall stopped talking (training signal: this moment wasn't for Heimdall)");
        self.record_diagnostic("speech_stopped",json!({"reason":"stop_button","label":"not_for_heimdall","in_call":self.speech_policy.in_conversation(now),
            "words":self.speech_policy.run_words(),"spoken":spoken}));
        self.context_note("unheard-speech:stopped","User pressed Stop while you were talking: this moment was not for you. Do not say it again unless he asks; keep any result on the island.".into());
        StopPress {playing,run}
    }
    /// The call state now sets how Heimdall sounds (bug note 2026-11-05T00-10-37): in a call his voice is quieter
    /// (`speech_policy::voice_gain`) and queued lines wait for a gap (`hold`, `set_patient`); out of one, full level at
    /// once. The one place both are set, read as speech is queued (`speak`), as a bank line or acknowledgement starts
    /// (`voice_bank::play_line`) and as each sentence starts playing (the spoken-text hook). Review 5 Nov: they were set
    /// only when the agent queued speech, so the in-call name ack played at full level and lines after the call stayed
    /// quiet until the agent spoke again. Returns the level.
    pub(crate) fn apply_call_level(&self) -> f32 {
        let now = capture::host_now_ns();
        let gain = self.speech_policy.voice_gain(now);
        self.owner.set_patient(self.speech_policy.in_conversation(now));
        // App-wide (unit tests never set it: the speak crate's level is one for the whole test process).
        #[cfg(not(test))]
        speak::set_voice_gain(gain);
        #[cfg(test)]
        self.test_levels.lock().unwrap().push(gain);
        gain
    }
    /// Reply `job` was queued for the current run (`mcp_actions::Host::say_as`).
    pub(crate) fn run_speech(&self,job:u64) {
        let tag=self.speech_policy.run_tag();
        let live:HashSet<u64>=self.speech_text.lock().unwrap().keys().copied().collect();
        let mut runs=self.speech_runs.lock().unwrap();
        runs.retain(|j,_|live.contains(j));
        runs.insert(job,tag);
    }
    /// Bug note 2026-11-05T02-32-17 item 5 (listen to the call audio, speak only in a quiet gap, and stop as soon as
    /// anyone talks, guests included): a guest's voice on call source `source`, begun at `at_ns` and still open
    /// `speech_policy::GUEST_CUT_AFTER` later, while the user is in the call and Heimdall is audible, stops Heimdall as
    /// the user's own voice does (`talked_over`), keeping lines held for later; the agent is told what was cut. The
    /// user's turn is not claimed: the next line waits for the gap (`hold`).
    pub(crate) fn guest_talked_over(&self,source:&str,at_ns:u64) {
        // It ended (a click, a cough, a blip of echo) or a newer voice opened since.
        if self.speech_policy.voice_open_since(source)!=Some(at_ns) {return;}
        if !self.speech_policy.in_conversation(capture::host_now_ns()) || !self.heimdall_audible() {return;}
        eprintln!("A guest on the call spoke while Heimdall talked: stopped (guest_voice, {source})");
        self.record_diagnostic("speech_stopped",json!({"reason":"guest_voice","source":source}));
        self.speech_status();
        let jobs:Vec<u64>=self.speech_text.lock().unwrap().keys().copied().collect();
        if !jobs.is_empty() {self.cut_by_guest.store(true,Ordering::Release);}
        self.cut_by_owner.lock().unwrap().extend(jobs);
        self.stop_speech();
    }
    /// Heimdall's voice is reaching the speaker now (agent speech or a bank line), not merely queued.
    fn heimdall_audible(&self)->bool {
        #[cfg(test)]
        if self.test_audible.load(Ordering::Acquire) {return true;}
        super::hold::audible() || self.voice_bank.line_playing()
    }
    pub fn interrupt(&self) {
        // Lines held for after a call are not said once the user stopped or talked over Heimdall (review 4 Nov).
        self.speech_policy.drop_held("dropped: speech was stopped", capture::host_now_ns());
        self.stop_speech();
    }
    /// Stops what plays and what is queued, keeping lines held in a call: a typed message is the user talking to Heimdall,
    /// so the newest held line is said for it (bug 2026-11-04T17-48-24, bridge/speech_policy.rs).
    pub(crate) fn stop_speech(&self) {
        self.speech_status(); // Retain completed playback before retiring cancelled jobs.
        super::speech::cut_speech();
        // The reason is taken now, not when the cancellation event is drained later.
        if !self.enabled.load(Ordering::Acquire) {
            let jobs:Vec<u64>=self.speech_text.lock().unwrap().keys().copied().collect();
            self.unheard_by_mode.lock().unwrap().extend(jobs);
        }
        self.voice_bank.interrupt();
        if let Some(q) = self.queue.lock().unwrap().as_ref() {
            q.interrupt();
        }
        // The Speed sample is speech too: Stop ends it, including one still waiting for Jarvis (E3.1, E8.5).
        if let Some(q) = self.sample_queue.lock().unwrap().as_ref() {
            q.interrupt();
        }
    }
    /// The current voice speed as a rate (1.0 natural).
    pub fn voice_rate(&self) -> f32 {
        self.voice_speed.load(Ordering::Acquire) as f32 / 100.
    }
    /// Speaking ▸ Speed was released: one short sentence at the new speed, always in Jarvis. A Jarvis who is not
    /// resident (Heimdall paused, Speaking off) is loaded for it and kept briefly afterwards; the slider row shows the
    /// load. Not agent speech: no decider turn, no reply window, no bank recording; a newer sample replaces one still
    /// playing.
    pub fn play_voice_sample(&self) -> Result<u64> {
        let mut queue = self.sample_queue.lock().unwrap();
        if let Some(q) = queue.as_ref() { q.interrupt(); }
        self.sample_generation.fetch_add(1, Ordering::AcqRel);
        // One sample at a time: a voice's sample stops for the Speed slider's.
        self.voice_previews.stop();
        // Resident from now until the sample has played (the residency tick honours the hold), loading at once. Only
        // when Jarvis is the chosen voice: another voice says its own sample (night 1022 menu).
        if self.speech_voices.route() == crate::speech_voice::Route::Jarvis {
            *self.sample_hold.lock().unwrap() = Some(std::time::Instant::now() + SAMPLE_LOAD_WAIT + SAMPLE_KEEP);
            self.drama3.configure(true);
        }
        if queue.is_none() {
            *queue = Some(speak::SpeechQueue::new(
                LazySynth {
                    drama3: Some(self.drama3.clone()),
                    session: self.archive_session.lock().unwrap().clone().context("speech archive session missing")?,
                    models: self.models.clone(),
                    model: self.tts_model.clone(),
                    backend: self.tts_backend.lock().unwrap().clone(),
                    record: None,
                    speed: self.voice_speed.clone(),
                    voices: Some(self.speech_voices.clone()),
                    sample: Some(SampleVoice { hold: self.sample_hold.clone(), generation: self.sample_generation.clone(), note: {
                        let ui = *self.notice_ui.lock().unwrap();
                        Arc::new(move |note| if let Some(u) = ui { u.send(ui::UiUpdate::VoiceSample(note)); })
                    } }),
                },
                self.voice_bank.exclusive(speak::CoreAudioOutput),
            ));
        }
        queue.as_ref().unwrap().speak(VOICE_SAMPLE, "en")
    }
    /// Whether the Speed slider's sample needs Jarvis resident now.
    pub fn sample_holds_jarvis(&self) -> bool {
        self.sample_hold.lock().unwrap().is_some_and(|until| std::time::Instant::now() < until)
    }
    pub fn screenshot_status(&self) -> Option<String> { self.screenshot_status.lock().unwrap().clone() }
    pub(super) fn speech_event(&self,event:speak::SpeechEvent) {
        match event {
            speak::SpeechEvent::Failed {job,error}=>{
                let addressed=self.addressed_jobs.lock().unwrap().remove(&job);
                self.unheard_by_mode.lock().unwrap().remove(&job);
                self.cut_by_owner.lock().unwrap().remove(&job);
                let unheard=self.speech_text.lock().unwrap().remove(&job);
                if error==super::hold::HOLD_SKIPPED {
                    // The user was talking when it would have played (night pass 1022): not said; the reply stays in the
                    // pull-down, its lines show as one island line and the agent is told once for the reply, when it
                    // is over (`settle_notes`, bug note 2026-10-26T18-35-49). Speech itself is fine.
                    // His answer (an addressed run): the retry may speak in a call. Shown on the island at once, as
                    // quiet text (bug note 2026-11-05T02-32-37: "held replies sometimes show nothing for minutes").
                    if addressed {self.skipped_addressed.store(true,Ordering::Release);}
                    if let Some(text)=unheard {self.skipped.lock().unwrap().push(text);self.show_skipped_live();}
                } else {
                    eprintln!("Speech failed: {error}");
                    self.speech_failed();
                    if let Some(text)=unheard {self.unheard_line(&text);}
                }
            },
            speak::SpeechEvent::Cancelled {job,heard}=>{
                self.addressed_jobs.lock().unwrap().remove(&job);
                let unheard=self.speech_text.lock().unwrap().remove(&job);
                // Speaking switched off mid-line: the agent was told it was queued, so the user reads it instead.
                // The user talking over it (barge-in) stops it on purpose and shows nothing; if he heard its start, the
                // line still joins the decider's conversation, as the final reply always did before it was spoken
                // through this queue (review 19 Oct). A line cut before any audio was never said.
                let cut=self.cut_by_owner.lock().unwrap().remove(&job);
                if self.unheard_by_mode.lock().unwrap().remove(&job) { if let Some(text)=unheard {self.unheard_line(&text);} }
                else if let Some(text)=unheard {
                    if heard {self.spoken_line(&text,capture::host_now_ns());}
                    if cut {self.missed.lock().unwrap().push((text,heard));}
                }
            }
            speak::SpeechEvent::PlaybackEnded {completed_at,..}=>{
                let at=capture::host_now_ns().saturating_sub(completed_at.elapsed().as_nanos() as u64);
                self.playback_ended(at);
            }
            speak::SpeechEvent::Complete {job,completed_at,..}=>{
                // A line reached the user: a later skipped reply may wake the agent once again.
                self.skip_retry.lock().unwrap().spent=false;
                self.addressed_jobs.lock().unwrap().remove(&job);
                self.unheard_by_mode.lock().unwrap().remove(&job);
                self.cut_by_owner.lock().unwrap().remove(&job);
                if let Some(text)=self.speech_text.lock().unwrap().remove(&job) {
                    let at=capture::host_now_ns().saturating_sub(completed_at.elapsed().as_nanos() as u64);
                    let mut context=self.send_context.lock().unwrap();
                    context.append_at("Heimdall",&text,false,at);
                    if let Err(e)=self.persist_send_context(&context) {eprintln!("Send context evidence failed: {e:#}");}
                }
                self.clear_speech_failures();
            }
        }
        self.settle_notes(false);
    }
    /// Sends the batched speech notes once their jobs have settled: the talked-over lines when every cut job has; the
    /// skipped lines once the skipped reply is over (the agent's turn has ended and no line is left), which also
    /// releases the skip (`hold::OwnerVoice::resume`). `idle`: the speech queue had nothing left, read before its last
    /// events were drained; jobs it never reported (a retired queue) then hold nothing back.
    fn settle_notes(&self,idle:bool) {
        let cut_settled={let mut cut=self.cut_by_owner.lock().unwrap();if idle {cut.clear();}cut.is_empty()};
        if cut_settled {
            let missed=std::mem::take(&mut *self.missed.lock().unwrap());
            if !missed.is_empty() {self.missed_lines(&missed);}
        }
        let working=self.send_context.lock().unwrap().working;
        if self.owner.skipping() && !working && (idle || self.speech_text.lock().unwrap().is_empty()) {
            self.owner.resume();
            let skipped=std::mem::take(&mut *self.skipped.lock().unwrap());
            self.after_skip(skipped);
        }
    }
    /// A skipped reply is over and no wake of the user's carried its note (night 1022 review 1: if his next words never
    /// wake the agent, the call gate or a "wait" verdict, he would never get an answer). The agent is woken once,
    /// after his pause ends, with the note, and answers fresh; what it says then follows the same speech rules. One
    /// retry per skipped reply: if that answer is skipped too, the agent is told as context only, until a line
    /// reaches the user or his words reach the agent.
    ///
    /// Bug notes 2026-11-04T20-52-24 and 2026-11-05T00-14-40 (23:44:09 name wake, 23:44:54 skipped, 23:45:16 retry,
    /// 23:47:23 held, 23:57:23 dropped stale): the retry of a skipped answer to the user's address by name carries that
    /// address (`SpeechPolicy::addressed_flag`), set as the retry wakes the agent, so its fresh answer may be spoken in
    /// the call; the first run settling had cleared it.
    fn after_skip(&self,lines:Vec<String>) {
        let addressed=self.skipped_addressed.swap(false,Ordering::AcqRel);
        if lines.is_empty() {return;}
        self.show_skipped(&lines);
        let mut retry=self.skip_retry.lock().unwrap();
        if let Some(pending)=retry.pending.as_mut() {pending.extend(lines);retry.addressed|=addressed;return;}
        if retry.spent {drop(retry);self.context_note("unheard-speech:skipped",skipped_note(&lines));return;}
        retry.spent=true;
        retry.pending=Some(lines);
        retry.addressed=addressed;
        drop(retry);
        let (owner,retry,gate,address)=(self.owner.clone(),self.skip_retry.clone(),self.agent_enabled.clone(),self.speech_policy.addressed_flag());
        let tx=self.agent_commands.lock().unwrap().clone();
        std::thread::spawn(move || {
            let begun=std::time::Instant::now();
            let mut quiet:Option<std::time::Instant>=None;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(40));
                if retry.lock().unwrap().pending.is_none() {return;}
                if owner.holds() {quiet=None;} else if quiet.get_or_insert_with(std::time::Instant::now).elapsed()>=RETRY_GRACE {break;}
                if begun.elapsed()>=RETRY_LIMIT {break;}
            }
            let (lines,addressed)={let mut r=retry.lock().unwrap();let addressed=std::mem::take(&mut r.addressed);(r.pending.take(),addressed)};
            let Some(lines)=lines else {return};
            let (Ok(permit),Some(tx))=(gate.permit(),tx) else {return};
            let (ack,_)=std::sync::mpsc::channel();
            if addressed {address.store(true,Ordering::Release);}
            eprintln!("Hold: a skipped reply's note woke the agent after User's pause; it answers fresh{}",if addressed {", and may speak in the call (his address carried)"} else {""});
            if tx.send(Command::Notice{id:format!("{SKIPPED_RETRY}:{}",uuid::Uuid::new_v4()),text:skipped_note(&lines),permit,ack}).is_err() {eprintln!("Skipped reply not told to the agent: supervisor stopped");}
        });
    }
    /// A skipped reply's lines, never said, as one island line (bug note 2026-10-26T18-35-49: "I didn't hear anything
    /// and you didn't say it"): the path a line takes with Speaking off, so it also joins the decider's conversation.
    /// Its lines were shown as they were skipped (`show_skipped_live`): that island line gets the reply's whole text
    /// and nothing new is shown.
    fn show_skipped(&self,lines:&[String]) {
        if lines.is_empty() {return;}
        eprintln!("Hold: a reply skipped while User talked is shown on the island ({} line(s))",lines.len());
        let text=lines.join(" ");
        let Some((id,shown))=self.skipped_notice.lock().unwrap().take() else {return self.unheard_line(&text)};
        if shown!=text {self.show_notice(&text,Some(&id));}
        self.island_line_said(&text,capture::host_now_ns());
    }
    /// Bug note 2026-11-05T02-32-37: a reply that is not spoken must still appear on the island right away as a quiet
    /// text island; skipped lines showed only once the reply was over, minutes later in a long turn.
    /// Each skipped line shows at once: one island line for the reply, its text growing as more of it is skipped. It
    /// joins the decider's conversation once, when the reply is over (`show_skipped`).
    fn show_skipped_live(&self) {
        let text=self.skipped.lock().unwrap().join(" ");
        if text.trim().is_empty() {return;}
        let id=self.skipped_notice.lock().unwrap().as_ref().map(|(id,_)|id.clone());
        if let Some(id)=self.show_notice(&text,id.as_deref()) {*self.skipped_notice.lock().unwrap()=Some((id,text));}
    }
    /// An island line of `text`, or `update` of one already shown; its id.
    fn show_notice(&self,text:&str,update:Option<&str>)->Option<String> {
        let shown=self.agent_enabled.permit().and_then(|p| {
            let actions=self.mcp_actions.actions.as_ref().map_err(|e|anyhow::anyhow!(e.clone()))?;
            actions.notice(&format!("unheard-speech:{}",uuid::Uuid::new_v4()),text,"info",update,&p)
        });
        shown.map_err(|e|eprintln!("Skipped reply not shown: {e:#}")).ok()
    }
    /// The final reply's speech jobs among `jobs` never reached the user: it leaves the island's pull-down too.
    fn withdraw_final(&self,jobs:&[u64]) {
        let mut shown=self.final_shown.lock().unwrap();
        if !shown.as_ref().is_some_and(|(final_jobs,_,_)|jobs.iter().any(|j|final_jobs.contains(j))) {return;}
        let (_,text,earlier)=shown.take().unwrap();
        drop(shown);
        crate::chat_history::withdrawn(&text);
        self.reply_to_island(ui::UiUpdate::WithdrawReply(text));
        // The unread replies it was shown below stay (bug note 22 Oct 15:13).
        if let Some(earlier)=earlier {self.reply_to_island(ui::UiUpdate::LastReply(earlier));}
    }
    /// The island's pull-down: the last reply, or its withdrawal.
    fn reply_to_island(&self,update:ui::UiUpdate) {
        #[cfg(test)]
        self.test_island.lock().unwrap().push(update.clone());
        if let Some(ui)=*self.notice_ui.lock().unwrap() {ui.send(update);}
    }
    /// A new run started while a skip latch from an earlier one was still up (its lines not yet settled): the
    /// earlier reply's lines still queued are dropped unheard with the skipped ones, and the latch is released so
    /// the new run's speech plays (night 1022 review 3: a T3 completion arriving right after a skip was silenced).
    fn new_run(&self) {
        if !self.owner.skipping() {return;}
        let mut dropped:Vec<(u64,String)>=vec![];
        if let Some(q)=self.queue.lock().unwrap().as_ref() {
            while let Ok(event)=q.events.try_recv() {self.speech_event(event);}
            if self.owner.skipping() && !self.speech_text.lock().unwrap().is_empty() {
                q.interrupt();
                dropped=self.speech_text.lock().unwrap().drain().collect();
            }
        }
        if !self.owner.skipping() {return;}
        self.owner.resume();
        // The skipped reply stays in the pull-down and its lines are shown (`after_skip`).
        dropped.sort_by_key(|(job,_)|*job);
        let mut skipped=std::mem::take(&mut *self.skipped.lock().unwrap());
        skipped.extend(dropped.into_iter().map(|(_,text)|text));
        eprintln!("Hold: a new run started; the earlier skipped reply is released ({} line(s))",skipped.len());
        self.after_skip(skipped);
    }
    /// A wake carrying the user's words is being delivered (bridge/supervisor.rs): agent speech still waiting for him
    /// (`hold`) is stale, since what he said usually changes what is worth saying (night review 20 Oct). It is dropped
    /// unheard with every line queued behind it, and the wake carries the line telling the agent what he did not hear
    /// (prompt: "the user talked over you…"). Speech he is hearing is the barge-in's; with nothing waiting, nothing happens.
    ///
    /// Night pass 1022: lines skipped because he was talking (`hold`) are told in the same wake, and the skip is
    /// released, so the agent's answer to his words plays.
    /// The acknowledgement line for the wake carrying the user's words through exactly `through` (taken once). A line
    /// left by an earlier wake that never went out is dropped, never told to a later one.
    pub(crate) fn take_ack_line(&self, through: Option<u64>) -> Option<String> {
        let mut said = self.ack_said.lock().unwrap();
        match (&*said, through) {
            (Some((seq, _)), Some(t)) if *seq == t => said.take().map(|(_, line)| line),
            (Some((seq, _)), Some(t)) if *seq < t => { said.take(); None }
            _ => None,
        }
    }

    pub(crate) fn drop_held_speech(&self) -> Option<String> {
        let queue=self.queue.lock().unwrap();
        let q=queue.as_ref()?;
        // Lines that have finished are not dropped: their events are in by the time a later line waits.
        while let Ok(event)=q.events.try_recv() {self.speech_event(event);}
        let skipping=self.owner.skipping();
        let mut dropped:Vec<(u64,String)>=vec![];
        if self.owner.drop_held() || skipping {
            q.interrupt();
            // Still under the queue's lock: a line `speak` queues after the interrupt is not taken for a dropped one.
            dropped=self.speech_text.lock().unwrap().drain().collect();
            eprintln!("Hold: User's words reached the agent; {} waiting line(s) dropped unheard",dropped.len());
        }
        // Released only after the interrupt: a line of the skipped reply still queued never plays.
        self.owner.resume();
        drop(queue);
        dropped.sort_by_key(|(job,_)|*job);
        let jobs:Vec<u64>=dropped.iter().map(|(job,_)|*job).collect();
        self.withdraw_final(&jobs);
        // His words reached the agent: they carry a skipped reply still waiting for its retry, and the next skipped
        // reply may retry again.
        let mut skipped={let mut retry=self.skip_retry.lock().unwrap();retry.spent=false;retry.addressed=false;retry.pending.take().unwrap_or_default()};
        self.skipped_addressed.store(false,Ordering::Release);
        // Lines skipped while he talked show now; the retry's were shown when it took them. The lines dropped here
        // never show (they are stale).
        let unshown=std::mem::take(&mut *self.skipped.lock().unwrap());
        self.show_skipped(&unshown);
        skipped.extend(unshown);
        let dropped:Vec<String>=dropped.into_iter().map(|(_,text)|text).collect();
        if !skipped.is_empty() {eprintln!("Hold: User's words carry the note for {} skipped line(s) to the agent",skipped.len());}
        let notes:Vec<String>=[(!skipped.is_empty()).then(||skipped_note(&skipped)),(!dropped.is_empty()).then(||dropped_note(&dropped))].into_iter().flatten().collect();
        (!notes.is_empty()).then(||notes.join("\n"))
    }
    /// Called once for each reply (`heimdall_final_answer`: each finished model turn, or a run's settlement), with
    /// Pi's stop reason. The reply path
    /// (`agent::reply`): the final text is what the user hears or reads, so what is left of it after this
    /// turn's `speak` lines goes through the same path a `speak` call takes (Heimdall's voice with Speaking on, an
    /// island line with it off), and joins the decider's conversation that way. A silent reply (`NO_REPLY`), an
    /// aborted or failed one says nothing. The whole reply as written goes to the island's pull-down
    /// (`UiUpdate::LastReply`). Playback receipts alone still own the follow-up clock.
    pub(crate) fn agent_final_answer(&self, text: &str, stop_reason: &str) -> Result<()> {
        let reply=self.replies.lock().unwrap().finish(text,stop_reason);
        {
            let mut context=self.send_context.lock().unwrap();
            context.working=false;
            self.persist_send_context(&context)?;
        }
        // Bug note 22 Oct 15:13: a reply never wipes one shown as text that the user has probably not read yet. The pull-down
        // shows those first and this one last, and the agent learns, as context, which one he probably missed.
        let arrival=reply.shown.as_deref().map(|shown|self.unread.lock().unwrap().arrive(shown,std::time::Instant::now()));
        if let Some(arrival)=&arrival {
            if let Some(note)=super::unread::note(&arrival.missed) {self.context_note(super::unread::UNREAD_NOTE,note);}
            self.reply_to_island(ui::UiUpdate::LastReply(arrival.pull_down.clone()));
        }
        *self.final_shown.lock().unwrap()=None;
        // Night pass 1022: a reply already skipped because the user was talking is skipped to its end; it stays in the
        // pull-down as text he has to read, its lines show once it is over and the note follows (`settle_notes`, bug
        // note 2026-10-26T18-35-49).
        if self.owner.skipping() {
            if let Some(shown)=&reply.shown {self.unread.lock().unwrap().shown_as_text(shown,std::time::Instant::now());}
            if let Some(say)=reply.say {
                self.record_diagnostic("speech_skipped",json!({"event":"the rest of a reply skipped: User was talking"}));
                if self.speech_policy.addressed_now() {self.skipped_addressed.store(true,Ordering::Release);}
                self.skipped.lock().unwrap().push(say);
                self.show_skipped_live();
            }
            self.speech_status();
            return Ok(());
        }
        let Some(say)=reply.say else {return Ok(())};
        // Agent Off or Heimdall Off since the turn settled: nothing more is said.
        let Ok(permit)=self.agent_enabled.permit() else {return Ok(())};
        let op=format!("final-reply:{}",uuid::Uuid::new_v4());
        let before:HashSet<u64>=self.speech_text.lock().unwrap().keys().copied().collect();
        let said=self.mcp_actions.say(self,&say,agent::reply::language(&say),&op,&permit);
        if let Err(e)=&said {eprintln!("Final reply not said: {e:#}");}
        // Shown as text, not said: the user has to read it before the next reply may take its place.
        if let (Ok(said),Some(shown))=(&said,&reply.shown) {
            if said==crate::mcp_actions::Host::SHOWN_ON_ISLAND {self.unread.lock().unwrap().shown_as_text(shown,std::time::Instant::now());}
        }
        // Its speech jobs: dropped unheard for a wake of his later, it is withdrawn from the pull-down (`withdraw_final`).
        if let Some(arrival)=arrival {
            let jobs:HashSet<u64>=self.speech_text.lock().unwrap().keys().filter(|j|!before.contains(j)).copied().collect();
            if !jobs.is_empty() {*self.final_shown.lock().unwrap()=Some((jobs,arrival.pull_down,arrival.earlier));}
        }
        Ok(())
    }
    /// A plan usage-limit event from the agent's extension (`agent/runtime/quota.mjs`): a spent limit with its reset, or
    /// the five-hour window past 80%. Every event is logged under `quota:`; a new one is said once through the reply
    /// path (Heimdall's voice, or the island line with Speaking off). The extension marks failures within a window it
    /// already reported as repeats, and the receipt is keyed by the window, so a Pi restart never says it twice.
    pub(crate) fn quota_notice(&self,v:&Value) {
        eprintln!("{}",quota_log(v));
        // The background jobs on that provider wait for its reset or the next success (audit 27 Oct, F8).
        if let Some(p)=v["provider"].as_str() {
            if v["event"]=="warning" { crate::background_gate::quota_warned(p,v["resets_at_ms"].as_u64(),agent::now_ms()); }
            if v["event"]=="limit" { crate::background_gate::quota_spent(p,v["resets_at_ms"].as_u64(),agent::now_ms()); }
        }
        let line=v["line"].as_str().unwrap_or_default();
        if v["repeat"]==true || line.is_empty() {return;}
        let Ok(permit)=self.agent_enabled.permit() else {eprintln!("quota: not said: the agent is off");return};
        let op=format!("quota:{}",v["key"].as_str().unwrap_or(line));
        if let Err(e)=self.mcp_actions.say(self,line,"en",&op,&permit) {eprintln!("quota: not said: {e:#}");}
    }
    /// A line Heimdall said without sound (a `speak` shown as text, or a question card): it joins the decider's
    /// conversation as a Heimdall turn. Only real playback opens the 30-second reply window
    /// (docs/briefs/decider-phase2-text.md), so this never does.
    pub(crate) fn island_line_said(&self,text:&str,at:u64) {
        let mut context=self.send_context.lock().unwrap();
        context.append_at("Heimdall",text,false,at);
        if let Err(e)=self.persist_send_context(&context) {eprintln!("Island line context evidence failed: {e:#}");}
    }
    /// An `ask` question: one Heimdall turn now. When it is also spoken, its playback still opens the reply window
    /// but does not add the turn a second time.
    pub(crate) fn question_asked(&self,text:&str,job:Option<u64>,at:u64) {
        if let Some(job)=job {self.speech_text.lock().unwrap().remove(&job);}
        self.island_line_said(text,at);
    }
    /// The lines the user talked over before he heard all of them (`talked_over`), in one note: context for the agent, no
    /// turn, so it knows what he missed when he finishes. Not replayed by itself: what he said over it usually changes
    /// what is worth saying, so the agent decides with his words in hand (prompt: "Talked over"). The reply as written
    /// stays in the island's pull-down.
    /// Cut by a guest on the call talking (`guest_talked_over`), the note says so.
    pub(super) fn missed_lines(&self,lines:&[(String,bool)]) {
        let started:Vec<&str>=lines.iter().filter(|(_,heard)|*heard).map(|(text,_)|text.as_str()).collect();
        let unheard:Vec<&str>=lines.iter().filter(|(_,heard)|!*heard).map(|(text,_)|text.as_str()).collect();
        let who=if self.cut_by_guest.swap(false,Ordering::AcqRel) {"Someone on the call started talking, so you stopped"} else {"User talked over you"};
        let note=match (started.is_empty(),unheard.is_empty()) {
            (false,true)=>format!("{who}; he heard only the start of: {}",started.join(" ")),
            (true,false)=>format!("{who} before he heard: {}",unheard.join(" ")),
            (false,false)=>format!("{who}; he heard only the start of: {} He did not hear: {}",started.join(" "),unheard.join(" ")),
            (true,true)=>return,
        };
        self.context_note("unheard-speech:cut",note);
    }
    /// A context note for the agent (no turn). Sent without waiting: the supervisor may be the caller.
    pub(crate) fn context_note(&self,kind:&str,text:String) {
        let Ok(permit)=self.agent_enabled.permit() else {return};
        let Some(tx)=self.agent_commands.lock().unwrap().clone() else {return};
        let (ack,_)=std::sync::mpsc::channel();
        if tx.send(Command::Notice{id:format!("{kind}:{}",uuid::Uuid::new_v4()),text,permit,ack}).is_err() {eprintln!("Speech note not told to the agent: supervisor stopped");}
    }
    /// Queued speech that was never heard (synthesis failed, or Speaking went off mid-line): shown once as the
    /// island line, and entered as a Heimdall turn.
    pub(super) fn unheard_line(&self,text:&str) {
        let shown=self.agent_enabled.permit().and_then(|p| {
            let actions=self.mcp_actions.actions.as_ref().map_err(|e|anyhow::anyhow!(e.clone()))?;
            actions.notice(&format!("unheard-speech:{}",uuid::Uuid::new_v4()),text,"info",None,&p)
        });
        if let Err(e)=shown {eprintln!("Unheard speech not shown: {e:#}");return;}
        self.island_line_said(text,capture::host_now_ns());
    }
    pub(crate) fn spoken_line(&self,text:&str,at:u64) {
        let mut context=self.send_context.lock().unwrap();
        context.append_at("Heimdall",text,false,at);
        if let Err(e)=self.persist_send_context(&context) {eprintln!("Spoken line context evidence failed: {e:#}");}
    }
    /// The agent is in a run (its last report).
    pub fn agent_in_run(&self) -> bool {
        self.run_active.load(Ordering::Acquire)
    }
    pub(super) fn agent_working(&self, working: bool) -> Result<()> {
        {
            let mut context=self.send_context.lock().unwrap();
            if context.working!=working {context.working=working;self.persist_send_context(&context)?;}
        }
        if working && !self.run_active.swap(true,Ordering::AcqRel) {self.new_run();}
        if !working {self.run_active.store(false,Ordering::Release);}
        Ok(())
    }
    pub(crate) fn playback_ended(&self, at: u64) {
        let mut context=self.send_context.lock().unwrap();
        context.finished_ns=Some(context.finished_ns.unwrap_or(0).max(at));
        if let Err(e)=self.persist_send_context(&context) {eprintln!("Playback context evidence failed: {e:#}");}
    }
    pub fn speech_status(&self) -> Option<String> {
        // Read before the drain: a queue with nothing left has sent every event by then.
        let idle=!self.agent_speech_busy();
        if let Some(q)=self.queue.lock().unwrap().as_ref() {while let Ok(event)=q.events.try_recv(){self.speech_event(event);}}
        self.settle_notes(idle);
        let stale=self.speech_failures.lock().unwrap().1.is_some_and(|at| at.elapsed()>=SPEECH_FAILURE_SHOWN_FOR);
        if stale {self.clear_speech_failures();}
        self.mcp_actions.replay_held(self); // Lines held in a call, said once it is over (speech_policy.rs).
        self.speech_status.lock().unwrap().clone()
    }
    /// One spoken line failed (synthesis or playback, or the queue could not take it). "Speech unavailable" (the
    /// menu's "Heimdall can't speak right now") shows only from `SPEECH_FAILURES_SHOWN` failures in a row, and clears
    /// with the next line that plays, Speaking off, or `SPEECH_FAILURE_SHOWN_FOR` after the last failure (review
    /// 23 Oct, finding 12: one failed line kept it up until the next spoken reply, maybe hours later).
    fn speech_failed(&self) {
        let mut failures=self.speech_failures.lock().unwrap();
        *failures=(failures.0+1,Some(std::time::Instant::now()));
        if failures.0>=SPEECH_FAILURES_SHOWN {*self.speech_status.lock().unwrap()=Some("Speech unavailable".into());}
    }
    /// A line played, or Speaking was switched off: no speech problem stands.
    pub(crate) fn clear_speech_failures(&self) {
        *self.speech_failures.lock().unwrap()=(0,None);
        self.speech_status.lock().unwrap().take();
    }
    #[cfg(test)]
    pub(crate) fn test_guarded_speech_queue(&self, synth: impl speak::Synthesizer, output: impl speak::AudioOutput) {
        *self.queue.lock().unwrap() = Some(Speech::new(synth, output, self.agent_enabled.permit().unwrap()));
    }
    #[cfg(test)]
    pub(crate) fn test_finish_speech(&self) -> Result<()> {
        loop {
            let event=self.queue.lock().unwrap().as_ref().unwrap().events.recv_timeout(std::time::Duration::from_secs(2))?;
            let terminal=!matches!(event,speak::SpeechEvent::PlaybackEnded {..});
            self.speech_event(event);
            if terminal {return Ok(());}
        }
    }
    #[cfg(test)]
    pub(crate) fn test_sample_speech(&self, synth: impl speak::Synthesizer, output: impl speak::AudioOutput) {
        *self.sample_queue.lock().unwrap() = Some(Speech::new(synth, output, self.agent_enabled.permit().unwrap()).queue);
    }
    #[cfg(test)]
    pub(crate) fn test_sample_says(&self, text: &str) -> Result<()> {
        self.sample_queue.lock().unwrap().as_ref().context("no sample queue")?.speak(text, "en").map(drop)
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_speech_queue(&self, queue: speak::SpeechQueue) {
        *self.queue.lock().unwrap() = Some(Speech { queue, permit: self.agent_enabled.permit().unwrap() });
    }
    /// The watchdog cannot wait for a mutex held by the frozen live loop. Clone the
    /// session address and release it before opening a separate journal connection.
    pub(crate) fn record_diagnostic(&self, kind: &str, payload: Value) {
        let Some(session) = self.archive_session.try_lock().ok().and_then(|s| s.clone()) else {
            eprintln!("{kind} diagnostic not archived: session address unavailable");
            return;
        };
        let kind = kind.to_string();
        let worker_kind = kind.clone();
        let host_ns = capture::host_now_ns();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // SQLite's normal writer timeout is 60 seconds. A freeze/forced Quit must
        // never wait on that same store. Sparse fault records get one independent
        // commit attempt, at most 250 ms on the watchdog; late work may finish if
        // the process stays alive. An unavailable store cannot archive its own failure.
        let spawned = std::thread::Builder::new().name("heimdall-diagnostic".into()).spawn(move || {
            let result = heimdall_archive::Journal::open(&session)
                .and_then(|journal| journal.event(&worker_kind, "heimdall", &[], host_ns, payload));
            let _ = tx.send(result);
        });
        if let Err(e) = spawned { eprintln!("{kind} diagnostic worker failed: {e}"); return; }
        match rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(Ok(())) => {},
            Ok(Err(e)) => eprintln!("{kind} diagnostic not archived: {e:#}"),
            Err(e) => eprintln!("{kind} diagnostic commit pending: {e}; recovery continues"),
        }
    }

    pub fn shutdown(&self) {
        self.drama3.stop();
        // Take and unlock before Drop joins synthesis. Only pipeline/tool worker
        // paths tear down speech; the UI callback touches the atomic latch only.
        self.voice_bank.interrupt();
        let queue = self.queue.lock().unwrap().take();
        drop(queue);
        let sample = self.sample_queue.lock().unwrap().take();
        drop(sample);
        let model = self.tts_model.lock().unwrap().take();
        drop(model);
    }

}
#[async_trait]
impl agent::mcp::ToolHost for Tools {
    fn speak_replay(&self,text:&str,language:&str,id:&str,p:&agent::delivery::DeliveryPermit)->Result<String>{self.mcp_actions.speak(self,text,language,id,p)}
    async fn screen_frames(&self,args:Value,p:&agent::delivery::DeliveryPermit)->Result<Value>{crate::mcp_frames::frames(self,args,p).await}
    async fn action(&self,name:&str,args:Value,id:&str,p:&agent::delivery::DeliveryPermit)->Result<String>{self.mcp_actions.action(self,name,args,id,p).await}
    async fn look(&self,app:Option<&str>,p:&agent::delivery::DeliveryPermit)->Result<Value>{crate::drawing::look(self,app,p).await}
    fn speak(&self, text: &str, lang: &str, permit: &agent::delivery::DeliveryPermit) -> Result<u64> {
        let result = (|| -> Result<u64> {
        permit.check()?;
        // Evidence only: a failed write never costs the user the line (1 Nov 00:17-00:21, three replies lost to "database
        // is locked"). Through the store's live writer, as `persist_send_context`.
        let session = self.archive_session.lock().unwrap().clone();
        if let Some(session) = session {
            let payload = json!({"text":text,"language":lang,"enabled":self.enabled.load(Ordering::Acquire)});
            if let Err(e) = crate::index::live_event(&session, "speak_back", "agent", &[], capture::host_now_ns(), payload) {
                eprintln!("speak_back not archived; the line is spoken anyway: {e:#}");
            }
        }
        anyhow::ensure!(self.enabled.load(Ordering::Acquire), "Speak Replies is off");
        let mut queue = self.queue.lock().unwrap();
        let retired = if queue.as_ref().is_some_and(|q| q.permit.check().is_err()) { queue.take() } else { None };
        let result = (|| -> Result<u64> {
        if queue.is_none() {
            *queue = Some(Speech::new(
                super::hold::HeldSynth { owner: self.owner.clone(), inner: LazySynth {
                    drama3: Some(self.drama3.clone()),
                    session: self.archive_session.lock().unwrap().clone().context("speech archive session missing")?,
                    models: self.models.clone(),
                    model: self.tts_model.clone(),
                    backend: self.tts_backend.lock().unwrap().clone(),
                    record: Some((self.voice_bank.clone(), voicebank::Origin::Agent)),
                    speed: self.voice_speed.clone(),
                    sample: None,
                    voices: Some(self.speech_voices.clone()),
                } },
                // One voice at a time: agent speech and bank lines share the playback lock. Nothing starts while the user
                // is talking (`hold`).
                super::hold::HeldOutput { inner: self.voice_bank.exclusive(speak::CoreAudioOutput), owner: self.owner.clone() },
                permit.clone(),
            ));
        }
        // In a call (bug notes 2026-11-04T20-52-24, 2026-11-05T00-14-40, 2026-11-05T00-10-37): only his answers reach
        // here then; they wait for a gap instead of being skipped while anyone talks (`hold`), and play quieter.
        self.apply_call_level();
        let addressed = self.speech_policy.addressed_now();
        let id = permit.with(|| queue.as_ref().unwrap().speak(text, lang))?;
        if addressed { self.addressed_jobs.lock().unwrap().insert(id); }
        crate::latency::speak_queued(); // Latency map (lab/latency).
        self.note_spoken(text);
        self.speech_text.lock().unwrap().insert(id,text.into());
        Ok(id)
        })();
        drop(queue);
        if let Some(retired) = retired {
            // A held native synth must not stall the next MCP speech request.
            std::thread::spawn(move || drop(retired));
        }
        result
        })();
        if result.is_err() { if self.enabled.load(Ordering::Acquire) { self.speech_failed(); } else { *self.speech_status.lock().unwrap() = Some("Speech replies off".into()); } }
        result
    }
    async fn screenshot_now(&self, permit: &agent::delivery::DeliveryPermit) -> Result<agent::mcp::ScreenshotResult> {
        permit.check()?;
        if self.screen_off.load(Ordering::Acquire) {
            *self.screenshot_status.lock().unwrap() = None;
            return Ok(agent::mcp::ScreenshotResult { images: vec![], warnings: vec![], notes: vec![SCREEN_OFF.into()] });
        }
        let work_permit = permit.clone();
        let result = permit.run(async {
        let guard = crate::archive::DiskGuard {
            live_floor: None,
            dir: self.shots.parent().context("screenshot root")?.into(),
            floor: self.archive_floor.load(Ordering::Acquire),
            status: Arc::new(Mutex::new(None)),
        };
        anyhow::ensure!(
            guard.allows(256 * 1024 * 1024),
            "screenshot archival paused below disk free-space floor"
        );
        let b = self
            .buffer
            .lock()
            .unwrap()
            .clone()
            .context("screen capture is unavailable")?;
        let shots = self.shots.clone();
        let session = self.archive_session.lock().unwrap().clone();
        #[cfg(test)]
        let grab = self.test_grabber.lock().unwrap().clone();
        tokio::task::spawn_blocking(move || -> Result<agent::mcp::ScreenshotResult> {
            work_permit.check()?;
            let now = capture::host_now_ns();
            let m = screen::PhraseMatch {
                source: "agent".into(),
                phrase: "screenshot_now".into(),
                start_ns: now,
                trigger_ns: now,
            };
            #[cfg(test)]
            let a = match grab {
                Some(g) => screen::attach_command_with_grabber(&b, &m, now, &shots, |id| g(id))?,
                None => screen::attach_command_screenshot(&b, &m, now, &shots)?,
            };
            #[cfg(not(test))]
            let a = screen::attach_command_screenshot(&b, &m, now, &shots)?;
            // While drawing mode is up, the screenshot carries the user's strokes.
            crate::drawing::mark_shots(&a);
            if let Err(e) = work_permit.check() {
                for shot in &a.shots { let _ = std::fs::remove_file(&shot.path); }
                return Err(e);
            }
            let warnings = a.full_resolution_errors.clone();
            for warning in &warnings { eprintln!("Full-resolution screenshot unavailable: {warning}"); }
            let images = if let Some(session) = session {
                let journal = heimdall_archive::Journal::open(&session)?;
                let (images, captured) = archive_screenshot_action(&journal, &a, "agent", &[], now)?;
                journal.event("agent_screenshot", "agent", &[], now, json!({"command":m,"action":a,"shots":captured}))?;
                images
            } else {
                a.shots.iter().map(|shot| context_attachment(std::fs::read(&shot.path)?)).collect::<Result<Vec<_>>>()?
            };
            Ok(agent::mcp::ScreenshotResult { images, warnings, notes: vec![] })
        })
        .await?
        }).await;
        *self.screenshot_status.lock().unwrap() = match &result {
            Err(_) => Some("Screenshot unavailable".into()),
            Ok(shots) if !shots.warnings.is_empty() => Some("Full-resolution screenshot unavailable".into()),
            Ok(_) => None,
        };
        result
    }

}
pub(super) const SPOKEN_MEMORY: std::time::Duration = std::time::Duration::from_secs(120);
/// What the agent hears when it asks for a screenshot while Seeing is off: a plain failure. The agent is never told
/// which switch is off ("you shouldn't know").
pub const SCREEN_OFF: &str = "No screenshot was taken: screen capture is unavailable.";

/// The longest edge of a screenshot the agent receives (18 Oct: a 3456×2234 PNG went into its session as a 7.9 MB
/// line). The archive keeps the capture at full resolution.
pub(crate) const CONTEXT_EDGE_PX: u32 = 2000;
/// A context image at or under this size stays PNG (text stays crisp); larger ones go as JPEG.
const CONTEXT_PNG_BYTES: usize = 1_500_000;
const CONTEXT_JPEG_QUALITY: u8 = 90;

/// The image as the agent receives it: fitted to `CONTEXT_EDGE_PX` on its long edge, PNG when that stays within
/// `CONTEXT_PNG_BYTES`, JPEG otherwise. An image already within both limits passes through unchanged.
pub(crate) fn context_attachment(png: Vec<u8>) -> Result<Attachment> {
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
    let long_edge = decoded.width().max(decoded.height());
    if long_edge <= CONTEXT_EDGE_PX && png.len() <= CONTEXT_PNG_BYTES {
        return Ok(Attachment { media_type: "image/png".into(), bytes: png });
    }
    let fitted = if long_edge > CONTEXT_EDGE_PX {
        decoded.resize(CONTEXT_EDGE_PX, CONTEXT_EDGE_PX, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let mut bytes = Vec::new();
    fitted.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)?;
    if bytes.len() <= CONTEXT_PNG_BYTES {
        return Ok(Attachment { media_type: "image/png".into(), bytes });
    }
    bytes.clear();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, CONTEXT_JPEG_QUALITY)
        .encode_image(&fitted.to_rgb8())?;
    Ok(Attachment { media_type: "image/jpeg".into(), bytes })
}

/// Shared voice/tool attachment path. CAS owns the durable bytes; temporary
/// private PNGs are removed only after every image and reference is archived.
pub(crate) fn archive_screenshot_action(
    journal: &heimdall_archive::Journal, action: &screen::Action, source: &str,
    utterance_ids: &[String], command_ns: u64,
) -> Result<(Vec<Attachment>, Vec<Value>)> {
    let mut images = Vec::with_capacity(action.shots.len());
    let mut captured = Vec::with_capacity(action.shots.len());
    let mut attached = std::collections::HashSet::new();
    for shot in &action.shots {
        let bytes = std::fs::read(&shot.path)?;
        let id = journal.image(&bytes, "image/png")?;
        let reference = json!({"image_id":id,"command_host_ns":command_ns,"captured_host_ns":shot.frame_ns,"display_id":shot.display_id,"kind":shot.kind,"width":shot.width,"height":shot.height,"grab_latency_ns":shot.grab_latency_ns});
        journal.event("screenshot", source, utterance_ids, shot.frame_ns, reference.clone())?;
        captured.push(reference);
        // A full-resolution frame replaces its display's phrase-time frame only when grabbed at about the same moment
        // (bug note 2026-10-25T21-31-53: a grab at the decision missed what he meant by "look at the screen").
        let preferred = shot.kind == screen::ShotKind::FullResolution || !action.shots.iter().any(|other|
            other.display_id == shot.display_id && other.kind == screen::ShotKind::FullResolution
                && !screen::full_resolution_is_late(other, action.trigger_ns));
        if preferred && attached.insert(id) {
            images.push(context_attachment(bytes)?);
        }
    }
    for shot in &action.shots { std::fs::remove_file(&shot.path)?; }
    Ok((images, captured))
}

/// The `heimdall.log` line for a plan usage-limit event: what happened, the line, the reset in local time and where it
/// came from (`header`, `body` or `none`), and whether it was already said in this window.
pub(crate) fn quota_log(v:&Value)->String {
    let event=match v["event"].as_str() {Some("limit")=>"limit hit",Some("warning")=>"five-hour window past 80%",_=>"event"};
    let reset=v["resets_at_ms"].as_i64().and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t|t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(||"not stated".into());
    let mut out=format!("quota: {} {event}: \"{}\" · reset {reset} · source {}",v["provider"].as_str().unwrap_or("?"),v["line"].as_str().unwrap_or_default(),v["source"].as_str().unwrap_or("?"));
    if let Some(p)=v["used_percent"].as_f64() {out.push_str(&format!(" · {p}% used"));}
    if let Some(m)=v["window_minutes"].as_u64() {out.push_str(&format!(" · {m}-minute window"));}
    if let Some(c)=v["code"].as_str() {out.push_str(&format!(" · code {c}"));}
    if let Some(s)=v["status"].as_u64() {out.push_str(&format!(" · HTTP {s}"));}
    if v["repeat"]==true {out.push_str(" · already said this window");}
    out
}

