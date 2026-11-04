//! The Agent window feed: session facts and history on open, then each streamed entry.
use super::*;

/// Feeds the Agent window: the session's facts and history when it opens, then each entry as it crosses the wire.
pub(super) struct AgentFeed {
    pub(super) ui: Option<ui::Ui>,
    pub(super) sent: bool,
    pub(super) view: Mutex<agent::view::SessionView>,
    pub(super) live: Mutex<agent::view::LiveView>,
    pub(super) session: Mutex<ui::AgentSession>,
}
impl AgentFeed {
    pub(super) fn open(s: &Settings, data: &Path, session: &Path, ui: Option<ui::Ui>) -> Arc<Self> {
        let live = live_session(data);
        // The selected model's name; the session history's model is stale after a switch.
        let selected=s.selected_api_model().map(|m|m.name.clone()).unwrap_or_else(||"Qwen3.8 27B".into());
        let mut model=Some(selected.clone());
        let history_file=if s.dry_run{Some(crate::archive::run_dir(data,session).join("dry-run.jsonl"))}else{active_session(data).ok()}.filter(|p|p.is_file());
        // The session's facts (model, id) come from its start; the window's rows from the bounded tail.
        let (view, _) = history_file
            .clone()
            .and_then(|p| agent::view::read_history(&p, 4 * 1024 * 1024).ok())
            .unwrap_or_default();
        let history: Vec<agent::view::Entry> = history_file
            .as_ref()
            .and_then(|p| bounded_session_history(p).ok())
            .map(|rows| {
                let mut tail = agent::view::SessionView::default();
                rows.iter().flat_map(|row| tail.history_line(row)).collect()
            })
            .unwrap_or_default();
        if !s.dry_run && s.selected_api_model().is_none() {
            model = view.model.clone().or(model);
        }
        let info = ui::AgentSession {
            harness: ui::Harness::Pi,
            model,
            can_open: false,
            session_id: view.session_id.clone().or(live),
            agent_on: !s.dry_run,
        };
        let mut parser = agent::view::SessionView::default();
        parser.model = info.model.clone();
        let feed = Arc::new(Self {
            ui,
            sent: !s.dry_run,
            view: Mutex::new(parser),
            live: Mutex::new(agent::view::LiveView::default()),
            session: Mutex::new(info.clone()),
        });
        if let Some(u) = ui {
            // Persisted Pi entries supply stable editing ids; first-frame state
            // remains supplied even before any new process event arrives. The one
            // conversation shows while the agent is off too; speech captured while
            // off follows it as it arrives ("one long session").
            if let Some(history)=window_history(data){u.send(ui::UiUpdate::AgentWindow(history));}
            u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Session(session_stats(s,data))));
            u.send(ui::UiUpdate::AgentSession(info));
            u.send(ui::UiUpdate::AgentHistory(history.into_iter().map(|e| feed.entry(e)).collect()));
        }
        feed
    }
    pub(super) fn entry(&self, e: agent::view::Entry) -> ui::AgentEntry {
        use agent::view::Kind;
        let kind = match e.kind {
            Kind::Prompt { query } => ui::AgentEntryKind::Prompt { query, sent: self.sent },
            Kind::Reply => ui::AgentEntryKind::Reply,
            Kind::ToolCall { name } => ui::AgentEntryKind::ToolCall { name },
            Kind::ToolResult { name, error } => ui::AgentEntryKind::ToolResult { name, error },
            Kind::Failure => ui::AgentEntryKind::Failure,
        };
        ui::AgentEntry { time: clock_label_ms(e.time_ms), kind, text: e.text }
    }
    pub(super) fn sent(&self, wire: &Value) {
        if let Some(u)=self.ui{let e=self.live.lock().unwrap().sent(wire,agent::now_ms());u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(e.into_iter().map(|e|{let mut e=live_entry(e);e.sent=self.sent;e}).collect())));}
        let entries = self.view.lock().unwrap().sent(wire, agent::now_ms());
        self.publish(entries);
    }
    pub(super) fn received(&self, wire: &Value) {
        if let Some(u)=self.ui{let e=self.live.lock().unwrap().received(wire,agent::now_ms());if !e.is_empty(){u.send(ui::UiUpdate::AgentWindow(ui::AgentWindowUpdate::Entries(e.into_iter().map(live_entry).collect())));}}
        let (entries, model, session_id) = {
            let mut view = self.view.lock().unwrap();
            let entries = view.received(wire, agent::now_ms());
            (entries, view.model.clone(), view.session_id.clone())
        };
        // The harness's init names the model it actually runs.
        let mut session = self.session.lock().unwrap();
        if (model.is_some() && session.model != model)||(session_id.is_some()&&session.session_id!=session_id) {
            if model.is_some(){session.model = model;}
            if session_id.is_some(){session.session_id=session_id;}
            if let Some(u) = self.ui { u.send(ui::UiUpdate::AgentSession(session.clone())); }
        }
        drop(session);
        self.publish(entries);
    }
    pub(super) fn publish(&self, entries: Vec<agent::view::Entry>) {
        if entries.is_empty() { return; }
        if let Some(u) = self.ui {
            u.send(ui::UiUpdate::AgentEntries(entries.into_iter().map(|e| self.entry(e)).collect()));
        }
    }
}
pub(super) fn live_entry(e:agent::view::StreamEntry)->ui::AgentLiveEntry{
    use agent::view::StreamKind as K;use ui::AgentLiveKind as U;
    ui::AgentLiveEntry{id:e.id,session_entry_id:e.session_entry_id,timestamp_ms:e.time_ms,kind:match e.kind{K::Prompt=>U::Prompt,K::Thinking=>U::Thinking,K::Reply=>U::Reply,K::ToolCall=>U::ToolCall,K::ToolResult=>U::ToolResult,K::Compaction=>U::Compaction,K::Failure=>U::Failure,K::Reload=>U::Reload,K::Repair=>U::Repair},text:e.text.clone(),tool_name:e.tool,attachments:e.images.into_iter().map(|(media_type,archive_id)|ui::AgentAttachment{media_type,archive_id}).collect(),finished:e.finished,editable:e.editable,error:e.error,sent:true,queued:false,owner_text:agent::view::typed_text(&e.text).map(str::to_owned),usage:e.usage.map(|u|ui::AgentUsage{input:u.input,output:u.output,cache_read:u.cache_read,cache_write:u.cache_write,started_ms:u.started_ms,first_token_ms:u.first_token_ms})}
}
#[derive(Debug)]
pub(super) struct WindowActionRejected(pub(super) String);
impl std::fmt::Display for WindowActionRejected { fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str(&self.0)} }
impl std::error::Error for WindowActionRejected {}
/// The window's rows: the session's last 500 entries, read backwards from the end of the file. The window counts
/// entries, not bytes: a large entry (an inline image) costs its own read and never pushes older rows out (18 Oct:
/// a 2 MB byte window collapsed the history from 79 rows to 2).
pub(super) fn bounded_session_history(path:&Path)->Result<Vec<Value>> {
    use std::io::{Read,Seek,SeekFrom};
    const ROWS:usize=500; const CHUNK:u64=1024*1024;
    let mut file=std::fs::File::open(path)?;
    let mut end=file.metadata()?.len();
    // The bytes of the line that starts before `begin`: they are read with the next chunk back.
    let mut carry:Vec<u8>=Vec::new();
    let mut rows:Vec<Value>=Vec::new();
    loop{
        let begin=end.saturating_sub(CHUNK);
        file.seek(SeekFrom::Start(begin))?;
        let mut buf=vec![0u8;(end-begin) as usize];
        file.read_exact(&mut buf)?;
        buf.extend_from_slice(&carry);
        let pieces:Vec<&[u8]>=buf.split(|b|*b==b'\n').collect();
        // The first piece starts mid-line unless this chunk reaches the start of the file.
        let candidates:&[&[u8]]=if begin==0 {&pieces} else {&pieces[1..]};
        carry=if begin==0 {Vec::new()} else {pieces[0].to_vec()};
        for piece in candidates.iter().rev(){
            if rows.len()>=ROWS {break;}
            if let Ok(row)=serde_json::from_slice::<Value>(piece){rows.push(row);}
        }
        if begin==0 || rows.len()>=ROWS {break;}
        end=begin;
    }
    rows.reverse();
    Ok(rows)
}
/// The Agent window's first frame, on or off: the Pi conversation's settled history (bounded read), never a
/// per-launch dry-run log.
pub(super) fn window_history(data:&Path)->Option<ui::AgentWindowUpdate>{
    let path=active_session(data).ok().filter(|p|p.is_file())?;
    settled_history(&path).ok()
}
pub(super) fn settled_history(path:&Path)->Result<ui::AgentWindowUpdate>{
    let rows=bounded_session_history(path)?;
    Ok(ui::AgentWindowUpdate::History(live_history(&json!({"entries":rows}))))
}
pub(super) fn live_history(entries:&Value)->Vec<ui::AgentLiveEntry>{
    let all=entries["entries"].as_array().cloned().unwrap_or_default();
    let leaf=entries["leafId"].as_str().or_else(||all.last().and_then(|v|v["id"].as_str()));
    let selected=if let Some(leaf)=leaf{
        let map:HashMap<_,_>=all.iter().filter_map(|v|v["id"].as_str().map(|id|(id,v))).collect();let mut branch=vec![];let mut seen=HashSet::new();let mut current=Some(leaf);
        while let Some(id)=current{if !seen.insert(id){break;}let Some(v)=map.get(id)else{break;};branch.push((*v).clone());current=v["parentId"].as_str();}branch.reverse();branch
    }else{all};
    let mut view=agent::view::LiveView::default();selected.iter().flat_map(|v|view.history(v)).map(live_entry).collect()
}
/// The Debug Agent window's totals for the whole conversation (the window's rows hold only its last entries), and
/// the window a request may fill before history compacts: the context slider's budget inside the model's own window.
pub(super) fn session_stats(s:&Settings,data:&Path)->ui::AgentSessionStats{
    let stats=active_session(data).ok().filter(|p|p.is_file()).and_then(|p|agent::view::session_stats(&p).map_err(|e|eprintln!("Debug Agent: could not read session totals: {e:#}")).ok()).unwrap_or_default();
    session_stats_ui(stats,s.selected_api_model().map(|m|m.budget()))
}
pub(super) fn session_stats_ui(x:agent::view::SessionStats,window_tokens:Option<u64>)->ui::AgentSessionStats{
    ui::AgentSessionStats{since_ms:x.since_ms,through_ms:x.through_ms,requests:x.requests,input:x.input,output:x.output,cache_read:x.cache_read,cache_write:x.cache_write,last_prompt:x.last_prompt,
        compactions:x.compactions,last_compaction_ms:x.last_compaction_ms,tool_calls:x.tool_calls,tool_failures:x.tool_failures,failed_edits:x.failed_edits,errors:x.errors,last_error_ms:x.last_error_ms,window_tokens}
}
/// The live provider session Heimdall has delivered to, if any (dry runs never touch it).
pub(super) fn live_session(data:&Path)->Option<String>{
    let spool=data.join("pi-spool.sqlite");if !spool.is_file()||!spool_has_deliveries(&spool).ok()?{return None;}
    let c=rusqlite::Connection::open_with_flags(spool,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    c.query_row("SELECT value FROM meta WHERE key='session'",[],|r|r.get::<_,String>(0)).ok()
}
pub(super) fn clock_label_ms(ms: u64) -> String {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = (ms / 1000) as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    /// Bug note 2026-10-28T19-41-00 (every untouched model runs its whole window): the Debug Agent window's figure is
    /// the budget requests really stay inside, not a 128K stand-in for an untouched slider.
    #[test]
    fn the_session_window_is_the_models_budget_whole_until_the_slider_moves() {
        use agent::provider::{Api, ApiModel, AgentModel, ANTHROPIC};
        let t=tempfile::tempdir().unwrap();
        let mut s=Settings::default();
        let opus=|budget:Option<u64>| ApiModel{provider:ANTHROPIC.into(),id:"claude-opus-5-5".into(),name:"Claude Opus 5.5".into(),api:Api::AnthropicMessages,context:1_000_000,max_output:128_000,
            images:true,reasoning:true,cost:None,context_budget:budget,thinking:None,base_url:None,efforts:vec![]};
        s.api_models.push(opus(None));
        s.agent_model=AgentModel::Api{provider:ANTHROPIC.into(),id:"claude-opus-5-5".into()};
        assert_eq!(session_stats(&s,t.path()).window_tokens,Some(1_000_000));
        s.api_models[0]=opus(Some(252_571));
        assert_eq!(session_stats(&s,t.path()).window_tokens,Some(s.api_models[0].budget()));
    }
}
