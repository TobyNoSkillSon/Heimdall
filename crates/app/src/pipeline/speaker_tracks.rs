//! Transcript lines and speaker identity: tracks, speaker events, enrollment store, Discord accessibility.
use super::*;

impl Pipeline {
    pub(super) fn line(&self, s: &str, words: &[Word], final_: bool, speaker: Option<String>) {
        let u = &self.active[s];
        let mut segments = vec![ui::Segment::Words(
            listen::Transcript::from_words(words.to_vec()).text,
        )];
        segments.extend(
            u.notes
                .iter()
                .map(|s| ui::Segment::Note(s.trim_matches(['[', ']']).into())),
        );
        self.ui(ui::UiUpdate::Transcript(ui::TranscriptLine {
            id: u.id,
            label: Some(ui::Label {
                source: self.sources.get(s).map_or_else(|| display_name_from_id(s), display_name),
                speaker: speaker.unwrap_or_else(|| {
                    if self.user_source(s) {
                        "User".into()
                    } else if self.is_call(s) {
                        "Speaker ?".into()
                    } else {
                        "audio".into()
                    }
                }),
            }),
            segments,
            is_final: final_,
        }));
    }
    pub(super) fn track_for(&mut self, s: &str, start: u64, end: u64) -> Result<Option<TrackId>> {
        let Some(d) = self.discord.get_mut(s) else {
            return Ok(None);
        };
        if end <= d.origin { return Ok(None); }
        let a = start.saturating_sub(d.origin) as f64 / 1e9;
        let b = end.saturating_sub(d.origin) as f64 / 1e9;
        let mut totals = HashMap::<i32, f64>::new();
        for seg in &d.segments {
            *totals.entry(seg.speaker).or_default() +=
                (seg.end_time.min(b) - seg.start_time.max(a)).max(0.);
        }
        let speaker = totals
            .into_iter()
            .filter(|(_, v)| *v > 0.)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(s, _)| s);
        Ok(speaker.map(|speaker| TrackId {
            source: s.into(),
            session: d.session,
            speaker,
        }))
    }
    pub(super) fn speaker_events(&mut self, events: Vec<SpeakerEvent>) -> Result<()> {
        let had_events = !events.is_empty();
        for e in events {
            let track = match &e {
                SpeakerEvent::VoiceCoded { track, .. }
                | SpeakerEvent::LabelChanged { track, .. } => track,
            };
            let ids: Vec<String> = self
                .labels
                .iter()
                .filter(|(_, (_, t))| t.as_ref() == Some(track))
                .map(|(id, _)| {
                    format!(
                        "{}:{}",
                        self.session.file_name().unwrap().to_string_lossy(),
                        id
                    )
                })
                .collect();
            // Night 8 Nov log sweep, finding 4: a note, never a wait for its commit on the live loop (a rename
            // sends one per track).
            self.index.evidence.note(
                "speaker_name",
                &track.source,
                &ids,
                capture::host_now_ns(),
                serde_json::to_value(&e)?,
            )?;
            // A voice no name matches carries its code label like a name: never a prompt.
            let (track, name) = match e {
                SpeakerEvent::VoiceCoded { track, code, new, near } => {
                    self.coded_tracks.insert(track.clone(), (new, near.as_ref().map(|n| n.name.clone())));
                    if new {
                        self.coded_voices += 1;
                    }
                    if let Some(near) = near {
                        self.voice_question(&track.source, &code, &near);
                    }
                    (track, code)
                }
                SpeakerEvent::LabelChanged { track, name } => {
                    self.coded_tracks.remove(&track);
                    (track, name)
                }
            };
            self.speaker_names.insert(track.clone(), name.clone());
            self.remember_call_name(&track,&name);
            self.invalidate_call_labels(&track.source);
            let mut ids = vec![];
            let attributed=self.call_final_ids();
            for (id, (line, t)) in &mut self.labels {
                if t.as_ref() == Some(&track) {
                    if let Some(l) = line.label.as_mut() {
                        l.speaker = name.clone();
                    }
                    if attributed.contains(id) {continue;}
                    if let Some(u) = self.ui {
                        u.send(ui::UiUpdate::Transcript(line.clone()));
                    }
                    ids.push(format!(
                        "{}:{id}",
                        self.session.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
            self.index.relabel(&track.source, &ids, &name)?;
        }
        // Every diarization frame lands here, mostly with no events: re-read the store only when it changed on disk
        // (bug note 2026-11-04T02-17-22, a 785 KB parse per frame on the app thread).
        if had_events || self.speakers_changed() {
            self.send_speakers();
        }
        let calls:Vec<_>=self.discord.keys().cloned().collect();
        for source in calls {self.project_call(&source)?;}
        Ok(())
    }
    pub(super) fn load_store(&self) -> Result<EnrollmentStore> {
        let p = self.data.join("speakers.json");
        if p.exists() {
            EnrollmentStore::load(&p)
        } else {
            Ok(EnrollmentStore::default())
        }
    }
    /// Whether `speakers.json` differs from the one the last Speakers list was built from (or none was sent yet).
    pub(super) fn speakers_changed(&self) -> bool {
        let now = StoreStamp::of(&self.data.join("speakers.json"));
        self.speakers_sent.lock().unwrap().as_ref() != Some(&now)
    }
    pub(super) fn send_speakers(&self) {
        // Stamp before reading: a write landing between the two is seen as a change on the next frame.
        let stamp = StoreStamp::of(&self.data.join("speakers.json"));
        if let Ok(store) = self.load_store() {
            *self.speakers_sent.lock().unwrap() = Some(stamp);
            // The listened guests follow the file whoever wrote it (pipeline/listen_to.rs).
            *self.listened.lock().unwrap() = store.listened();
            self.tools.speech_policy.set_listening_guests(!self.listened.lock().unwrap().is_empty());
            let owner = |name: &str| name.eq_ignore_ascii_case("User");
            self.ui(ui::UiUpdate::Speakers(
                // The user first: always listened to, his switch locked on.
                std::iter::once(ui::KnownSpeaker {
                    id: "User".into(),
                    name: "User".into(),
                    heard_in: "Mic".into(),
                    clips: store.names.iter().find(|(n, _)| owner(n)).map_or(0, |(_, e)| e.embeddings.len() as u32),
                    last_heard: String::new(),
                    ignored: false,
                    listen: ui::Listen::Always,
                })
                .chain(store.names.iter().filter(|(n, _)| !owner(n)).map(|(name, e)| ui::KnownSpeaker {
                        id: name.clone(),
                        name: name.clone(),
                        heard_in: "Discord".into(),
                        clips: e.embeddings.len() as u32,
                        last_heard: String::new(),
                        ignored: false,
                        listen: if speakers::identity::NOT_GUESTS.iter().any(|g| g.eq_ignore_ascii_case(name)) { ui::Listen::None }
                            else if e.listening { ui::Listen::On } else { ui::Listen::Off },
                    }))
                    // Voices the user chose not to be asked about, after the named ones (Ask Again undoes it).
                    .chain(store.ignored.iter().map(|(id, v)| ui::KnownSpeaker {
                        id: id.clone(),
                        name: ignored_title(&v.quote),
                        heard_in: v.heard_in.clone(),
                        clips: v.embeddings.len() as u32,
                        last_heard: ignored_date(v.at_ms),
                        ignored: true,
                        listen: ui::Listen::None,
                    }))
                    // Coded voices the user chose Forget for on the voice card, by their code (Ask Again undoes it).
                    .chain(store.voices.iter().filter(|(_, v)| v.irrelevant).map(|(code, v)| ui::KnownSpeaker {
                        id: code.clone(),
                        name: code.clone(),
                        heard_in: v.heard_in.clone(),
                        clips: v.embeddings.len() as u32,
                        last_heard: ignored_date(v.last_ms),
                        ignored: true,
                        listen: ui::Listen::None,
                    }))
                    .collect(),
            ));
        }
    }
}

/// What identifies one version of `speakers.json` without reading it: saves are atomic renames, so a new save has
/// a new inode as well as a new mtime and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StoreStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}
impl StoreStamp {
    pub(super) fn of(path: &std::path::Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: m.modified().ok(),
            len: m.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&m),
        })
    }
}

/// An ignored voice's row title: what it said, quoted and short, or "Unknown voice".
fn ignored_title(quote: &str) -> String {
    let words: Vec<&str> = quote.split_whitespace().collect();
    if words.is_empty() {
        return "Unknown voice".into();
    }
    let mut out = String::new();
    for w in &words {
        if out.chars().count() + w.chars().count() > 40 {
            out.push('\u{2026}');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    format!("\u{201c}{out}\u{201d}")
}

/// When a voice was ignored, as "21 Oct" (local time); nothing when unknown.
fn ignored_date(at_ms: u64) -> String {
    if at_ms == 0 {
        return String::new();
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = (at_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{} {}", tm.tm_mday, MONTHS[tm.tm_mon.clamp(0, 11) as usize])
}

/// "If you're not sure… then you can just ask me verbally" (~08:15). A new voice in a call that sounds
/// somewhat like a person he named (speakers::identity `NearMiss::Ask`) is put to the agent as a notice, at most once
/// per call source per `ASK_EVERY`: the agent may ask him at a pause who it was, and `name_voice` joins the voice to
/// that person's set on his answer. Nothing is said by the app itself.
pub(super) const ASK_EVERY_MS: u64 = 3600_000;
/// The notice for a voice question, or None while one was put for this call within `ASK_EVERY_MS`.
pub(super) fn voice_question_text(code: &str, near: &speakers::naming::NearVoice, now_ms: u64, last_ms: Option<u64>) -> Option<String> {
    if last_ms.is_some_and(|l| now_ms.saturating_sub(l) < ASK_EVERY_MS) {
        return None;
    }
    Some(format!(
        "Not sure who that was: a new voice in the call, labelled \"{code}\", sounds a little like {name} (voice match {c:.2}, below the {t:.2} that counts as the same person; microphones change voices). If it matters, ask {user} at a natural pause, in one short line, whether that was {name}. If he says yes, call name_voice with voice \"{code}\" and name \"{name}\"; if he names someone else, use that name. Do not interrupt the conversation for this, and do not ask again about this voice.",
        name = near.name,
        c = near.cosine,
        t = speakers::identity::EnrollmentStore::default().calibration.map_or(0.4555, |c| c.threshold),
        user = agent::identity::user(),
    ))
}
impl Pipeline {
    fn voice_question(&mut self, source: &str, code: &str, near: &speakers::naming::NearVoice) {
        let now = agent::now_ms();
        let Some(text) = voice_question_text(code, near, now, self.mic_voices.asked.get(source).copied()) else { return };
        self.mic_voices.asked.insert(source.to_owned(), now);
        let _ = self.index.evidence.note("voice_question", source, &[], capture::host_now_ns(), serde_json::json!({"schema":1,"code":code,"near":near}));
        if self.replay {
            return;
        }
        let told = self.tools.agent_enabled.permit().and_then(|permit| self.tools.notice(format!("voice-question:{code}"), text, &permit));
        if let Err(e) = told {
            eprintln!("Speakers: voice question not delivered: {e:#}");
        }
    }
}
