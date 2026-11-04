//! The user's live labels for the decider (29 Oct 22:32): "I don't want to call your name every time … Backslash: if you
//! haven't woken up, I press it and you are forced to wake up, so I decide for the decider, and that goes to the
//! training data. Quote: … you make a suggestion from the current conversation. So I'm labelling in real time when you
//! should have woken up or suggested."
//!
//! - ⌃⌘\ (`UiAction::ForceWake`): everything said since the last wake goes to the agent at once, as his own wake, and
//!   the latest unsent line (his, else anyone's) is labelled `send` with the exact decider input V5 saw for it.
//! - ⌃⌘' (`UiAction::ForceSuggest`): a suggestion pass starts now, past the pacing, and the latest decided line is
//!   labelled `suggest` with V5's exact suggest input for it (asked once more on the snapshot V5 saw).
//!
//! Labels are JSON lines in the archive's `decider-v6-labels.jsonl` (`LABELS_FILE`; format in docs/decision-chain.md).
//! A line whose send decision is still running when a key is pressed is labelled when it lands.
use super::*;
use serde_json::{json, Value};

/// In the archive store, beside the index: the Decider V6 labels the user gives with the two keys.
pub(crate) const LABELS_FILE: &str = "decider-v6-labels.jsonl";
/// The labelled source, as the training data names it.
pub(crate) const LABEL_SOURCE: &str = "owner-hotkey";
/// Decided lines kept for a key press (a minute or two of talk).
const KEPT: usize = 16;

/// One line's send decision, as V5 saw it.
#[derive(Clone)]
pub(super) struct Decided {
    seq: u64,
    id: String,
    source: String,
    speaker: String,
    text: String,
    record: Value,
    snapshot: decide::send::Snapshot,
}

#[derive(Default)]
pub(super) struct Hotkeys {
    decided: VecDeque<Decided>,
    /// Wake labels waiting for their line's send decision: the line, whether the wake went out, the other lines it carried.
    wake_waits: VecDeque<(u64, bool, Vec<String>)>,
    /// Suggest labels waiting for their line's send decision.
    suggest_waits: VecDeque<u64>,
}

impl Pipeline {
    pub(crate) fn labels_path(&self) -> PathBuf {
        crate::archive::store_root(&self.session).join(LABELS_FILE)
    }
    fn write_label(&self, label: Value) {
        let path = self.labels_path();
        let line = format!("{label}\n");
        let written = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
            use std::io::Write;
            std::fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(line.as_bytes())
        })();
        match written {
            Ok(()) => eprintln!("hotkey: labelled {} {} ({})", label["task"].as_str().unwrap_or(""), label["utterance_id"].as_str().unwrap_or("-"), path.display()),
            Err(e) => eprintln!("hotkey: label not written to {}: {e}", path.display()),
        }
    }
    /// After a line's send decision (`record`, the `send_decision` evidence): kept for a key press, and labelled now if
    /// a key was pressed while it was being decided.
    pub(super) fn hotkey_decided(&mut self, source: &str, snapshot: &decide::send::Snapshot, id: &str, record: &Value) {
        let Some(line) = snapshot.pending.iter().find(|t| t.seq == snapshot.through) else { return };
        let d = Decided { seq: line.seq, id: id.into(), source: source.into(), speaker: line.speaker.clone(), text: line.text.clone(),
            record: record.clone(), snapshot: snapshot.clone() };
        self.hotkeys.decided.push_back(d.clone());
        while self.hotkeys.decided.len() > KEPT { self.hotkeys.decided.pop_front(); }
        // A line decided later than these never will be: their waits are dropped (bounded either way).
        if let Some(i) = self.hotkeys.wake_waits.iter().position(|w| w.0 == d.seq) {
            let (_, woke, others) = self.hotkeys.wake_waits.remove(i).unwrap();
            self.write_label(send_label(Some(&d), &d.text, &d.id, woke, &others));
        }
        self.hotkeys.wake_waits.retain(|w| w.0 > d.seq);
        let suggest = self.hotkeys.suggest_waits.contains(&d.seq);
        self.hotkeys.suggest_waits.retain(|s| *s > d.seq);
        if suggest {
            self.suggest_label(d);
        }
    }
    /// ⌃⌘\: wake the agent with everything unsent, as the user's own wake, and label the latest unsent line `send`.
    pub(super) fn force_wake(&mut self) -> Result<()> {
        let snap = self.tools.send_context.lock().unwrap().snapshot();
        let unsent: Vec<_> = snap.pending.iter().filter(|t| t.seq > self.dispatched_through).cloned().collect();
        let Some(last) = unsent.iter().max_by_key(|t| t.seq).cloned() else {
            eprintln!("hotkey: wake pressed with nothing said since the last wake");
            return Ok(());
        };
        // The line he meant: his latest unsent one, else the latest of anyone's.
        let labelled = unsent.iter().filter(|t| t.speaker == "User").max_by_key(|t| t.seq).cloned().unwrap_or_else(|| last.clone());
        let id_of = |t: &decide::send::TextTurn| t.observation.as_ref().and_then(|o| o["id"].as_str()).unwrap_or_default().to_owned();
        let decided = self.hotkeys.decided.iter().find(|d| d.seq == labelled.seq).cloned();
        let source = decided.as_ref().map(|d| d.source.clone())
            .or_else(|| self.hotkeys.decided.iter().find(|d| d.seq == last.seq).map(|d| d.source.clone()))
            .unwrap_or_default();
        let permit = self.agent.as_ref().and_then(|a| a.wake_permit(self.settings.dry_run).ok());
        let before = self.dispatched_through;
        let ack = self.ack_plan(None);
        self.dispatch_pending(&source, &last.text, last.seq, &id_of(&last), agent::Speaker::Owner, permit, true, ack)?;
        let woke = self.dispatched_through > before;
        let others: Vec<String> = unsent.iter().filter(|t| t.seq != labelled.seq).map(id_of).collect();
        self.index.evidence.event("hotkey", "heimdall", &[id_of(&labelled)], capture::host_now_ns(),
            json!({"schema":1,"key":"wake","woke":woke,"through":last.seq,"lines":unsent.len()}))?;
        if !woke { eprintln!("hotkey: wake pressed; the agent is off or stopped, nothing was sent"); }
        match decided {
            Some(d) => self.write_label(send_label(Some(&d), &d.text, &d.id, woke, &others)),
            // Still being decided: labelled when its decision lands, with exactly what V5 saw.
            None if self.send_worker.as_ref().is_some_and(|o| o.send_busy()) => self.hotkeys.wake_waits.push_back((labelled.seq, woke, others)),
            // Heard before this launch (live 29 Oct 23:00): its decision is only in the archive; labelled now without it.
            None => {
                let mut label = send_label(None, &labelled.text, &id_of(&labelled), woke, &others);
                label["speaker"] = json!(labelled.speaker);
                label["decision"] = json!("unavailable: heard before this launch");
                self.write_label(label);
            }
        }
        Ok(())
    }
    /// ⌃⌘': a suggestion pass now, and the current line labelled `suggest`.
    pub(super) fn force_suggest(&mut self) -> Result<()> {
        self.suggest_now();
        let latest = self.tools.send_context.lock().unwrap().snapshot().pending.iter().map(|t| t.seq)
            .chain(self.hotkeys.decided.back().map(|d| d.seq)).max();
        match (self.hotkeys.decided.back().cloned(), latest) {
            (Some(d), Some(seq)) if d.seq >= seq => self.suggest_label(d),
            (_, Some(seq)) => if !self.hotkeys.suggest_waits.contains(&seq) { self.hotkeys.suggest_waits.push_back(seq) },
            (_, None) => eprintln!("hotkey: suggest pressed with no line heard yet; nothing to label"),
        }
        self.index.evidence.event("hotkey", "heimdall", &[], capture::host_now_ns(), json!({"schema":1,"key":"suggest"}))?;
        Ok(())
    }
    /// V5's suggest question once more on the snapshot it saw for `d`, so the label carries its exact input and answer.
    fn suggest_label(&mut self, d: Decided) {
        let snapshot = d.snapshot.clone();
        #[cfg(test)]
        let text = d.text.clone();
        let done = move |p: &mut Pipeline, result: Result<(decide::send::Fitted, decide::suggest::Suggestion)>| -> Result<()> {
            let (input, v5) = match result {
                Ok((fitted, s)) => (fitted.request, json!({"yes":s.yes,"choice":s.choice})),
                Err(e) => (Value::Null, json!({"error":format!("{e:#}")})),
            };
            p.write_label(json!({"schema":1,"task":"suggest","label":"yes","source":LABEL_SOURCE,"at_ms":agent::now_ms(),
                "utterance_id":d.id,"speaker":d.speaker,"text":d.text,"line_source":d.source,"decider_input":input,"v5":v5}));
            Ok(())
        };
        #[cfg(test)]
        if let Some(yes) = self.hooks.suggest.pop_front() {
            let s = decide::suggest::Suggestion { yes, choice: yes >= 0.5, input_tokens: 0, output: json!({"test":true}) };
            let fitted = decide::send::Fitted { request: json!({"test_request":text}), context_tokens: 0, dropped_turns: 0, pending_truncated: false };
            let _ = done(self, Ok((fitted, s)));
            return;
        }
        let queued = self.inference().and_then(|owner| owner.request(0, snapshot.token_bound(),
            move |state| {
                let urgent = state.urgent.clone();
                state.decider().and_then(|d| d.suggest_decision_interruptible(&snapshot, &urgent))
            },
            done,
        ));
        if let Err(e) = queued {
            eprintln!("hotkey: suggest label not asked: {e:#}");
        }
    }
}

/// A `send` label for line `d` (None: its decision never came), and V5's own verdict on it.
fn send_label(d: Option<&Decided>, text: &str, id: &str, woke: bool, others: &[String]) -> Value {
    let r = d.map(|d| &d.record);
    let get = |k: &str| r.map_or(Value::Null, |r| r[k].clone());
    json!({"schema":1,"task":"send","label":"send","source":LABEL_SOURCE,"at_ms":agent::now_ms(),
        "utterance_id":id,"speaker":d.map(|d| d.speaker.as_str()),"text":text,"line_source":d.map(|d| d.source.as_str()),
        "decider_input":get("prompt"),
        "v5":{"choice":get("choice"),"probabilities":get("probabilities"),"outcome":get("outcome"),"trigger":get("trigger"),"d1_gate":get("d1_gate")},
        "woke":woke,"also_sent":others})
}
