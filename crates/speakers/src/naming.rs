//! Quiet speaker naming (01:35/01:37). A call voice no enrolled name matches gets a code label
//! ("Speaker Fri 23 Oct 21h #3") and its clean samples are kept with it, so the same voice is recognised in later
//! calls under the same label. Nobody is asked to name it here: the app suggests a name on the island
//! (`crates/app/src/voice_suggest.rs`, `voice_namer.rs`).
use crate::{
    ax::AxSnapshot,
    embedding::Embedding,
    identity::{EnrollmentStore, Identity},
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackId {
    pub source: String,
    pub session: u64,
    pub speaker: i32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SpeakerEvent {
    /// The track's voice is the coded voice `code` (`new`: first heard now). The host labels the track's lines with
    /// `code` exactly as for `LabelChanged`.
    VoiceCoded {
        track: TrackId,
        code: String,
        new: bool,
    },
    LabelChanged {
        track: TrackId,
        name: String,
    },
}
/// When a voice is coded: Unix milliseconds, the label's hour ("Fri 23 Oct 21h") and the local day ("2026-10-23").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTime {
    pub ms: u64,
    pub stamp: String,
    pub day: String,
}
/// The local wall clock.
pub fn local_now() -> VoiceTime {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    voice_time(ms)
}
/// `ms` in local time.
pub fn voice_time(ms: u64) -> VoiceTime {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    VoiceTime {
        ms,
        stamp: format!("{} {} {} {}h", DAYS[tm.tm_wday.clamp(0, 6) as usize], tm.tm_mday, MONTHS[tm.tm_mon.clamp(0, 11) as usize], tm.tm_hour),
        day: format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday),
    }
}
/// Session-scoped track labels over the shared store. A guess never becomes a name: only the host names a voice.
pub struct NamingFlow {
    pub store: EnrollmentStore,
    labels: BTreeMap<TrackId, String>,
    ax: VecDeque<AxSnapshot>,
    discord_sources: BTreeSet<String>,
    /// Tracks of this session whose voice is ignored (Don't Ask Again on the old prompt): left unlabelled.
    ignored_tracks: BTreeSet<TrackId>,
    /// Where coded voices are saved, and the host's lock over that file (`persist_to`). None: in memory only.
    persist: Option<(PathBuf, Arc<Mutex<()>>)>,
    clock: fn() -> VoiceTime,
}
impl NamingFlow {
    pub fn new(store: EnrollmentStore) -> Self {
        Self {
            store,
            labels: BTreeMap::new(),
            ax: VecDeque::new(),
            discord_sources: BTreeSet::new(),
            ignored_tracks: BTreeSet::new(),
            persist: None,
            clock: local_now,
        }
    }
    /// Coded voices are written to `path` (re-read under `lock` first, so another call's writes are kept).
    pub fn persist_to(mut self, path: PathBuf, lock: Arc<Mutex<()>>) -> Self {
        self.persist = Some((path, lock));
        self
    }
    pub fn set_clock(&mut self, clock: fn() -> VoiceTime) {
        self.clock = clock;
    }
    /// The shared store changed (a name, a rename, Forget elsewhere). Ignored tracks are looked at again.
    pub fn set_store(&mut self, store: EnrollmentStore) {
        self.store = store;
        self.ignored_tracks.clear();
    }
    pub fn label(&self, track: &TrackId) -> String {
        self.labels
            .get(track)
            .cloned()
            .unwrap_or_else(|| format!("Speaker {}", track.speaker))
    }
    /// The host classified this capture source as Discord; IDs need not be bundle IDs.
    pub fn enable_discord_ax(&mut self, source: String) {
        self.discord_sources.insert(source);
    }
    /// Discord accessibility snapshots, kept (bounded) for the participant list. They are not naming evidence.
    pub fn observe_ax(&mut self, snapshot: AxSnapshot) {
        if !snapshot.observed_at.is_finite() {
            return;
        }
        self.ax.push_back(snapshot);
        while self.ax.len() > 32 {
            self.ax.pop_front();
        }
    }
    /// Final transcript text of a track. Names are no longer guessed in the call (the voice namer reads the
    /// archived transcript), so nothing changes here.
    pub fn observe_transcript(&mut self, _from: TrackId, _end: f64, _text: &str) -> Vec<SpeakerEvent> {
        vec![]
    }
    pub fn clean_voice(
        &mut self,
        track: TrackId,
        at: f64,
        embedding: Embedding,
        clean_seconds: f64,
        quote: &str,
    ) -> Vec<SpeakerEvent> {
        self.clean_voice_samples(track, at, vec![embedding], clean_seconds, quote)
    }
    /// A track's clean windows: a known name labels it; otherwise it is the coded voice they match, or a new one.
    /// The latest window is the query; do not choose the luckiest of several scores.
    pub(crate) fn clean_voice_samples(
        &mut self,
        track: TrackId,
        at: f64,
        embeddings: Vec<Embedding>,
        clean_seconds: f64,
        quote: &str,
    ) -> Vec<SpeakerEvent> {
        let Some(embedding) = embeddings.last().cloned() else {
            return vec![];
        };
        if !at.is_finite()
            || !clean_seconds.is_finite()
            || clean_seconds < 3.0
            || !(1..=8).contains(&track.speaker)
            || track.source.is_empty()
            || embeddings.iter().any(|e| e.validate().is_err())
            || self.labels.contains_key(&track)
            || self.ignored_tracks.contains(&track)
        {
            return vec![];
        }
        if let Identity::Known { name, .. } = self.store.identify(&embedding) {
            self.labels.insert(track.clone(), name.clone());
            return vec![SpeakerEvent::LabelChanged { track, name }];
        }
        // A voice the user said not to ask about on the old prompt: left as it was (a named voice above still wins).
        if self.store.ignored_match(&embedding).is_some() {
            self.ignored_tracks.insert(track);
            return vec![];
        }
        let now = (self.clock)();
        let heard_in = if self.is_discord(&track.source) { "Discord" } else { "Call" };
        let result = self.write(|store| {
            // The file is re-read here: a name given meanwhile (a rename in another part of the app) wins, and a
            // voice another call coded is matched, not coded twice.
            if let Identity::Known { name, .. } = store.identify(&embedding) {
                return Ok(Err(name));
            }
            if let Some(code) = store.voice_match(&embedding) {
                store.heard_voice(&code, embeddings.clone(), now.ms, &now.day)?;
                Ok(Ok((code, false)))
            } else {
                Ok(Ok((store.code_voice(embeddings.clone(), heard_in, quote, now.ms, &now.stamp, &now.day)?, true)))
            }
        });
        match result {
            Ok(Ok((code, new))) => {
                self.labels.insert(track.clone(), code.clone());
                vec![SpeakerEvent::VoiceCoded { track, code, new }]
            }
            Ok(Err(name)) => {
                self.labels.insert(track.clone(), name.clone());
                vec![SpeakerEvent::LabelChanged { track, name }]
            }
            Err(e) => {
                // The track stays "Speaker N"; its next clean windows try again.
                eprintln!("Speakers: voice not saved: {e:#}");
                vec![]
            }
        }
    }
    fn is_discord(&self, source: &str) -> bool {
        self.discord_sources.contains(source) || source.eq_ignore_ascii_case("Discord") || source.starts_with("com.hnc.Discord")
    }
    /// One change to the store: under the host's lock, on the file's current contents, saved before it is used.
    fn write<T>(&mut self, change: impl FnOnce(&mut EnrollmentStore) -> Result<T>) -> Result<T> {
        let Some((path, lock)) = self.persist.clone() else {
            return change(&mut self.store);
        };
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut store = if path.exists() { EnrollmentStore::load(&path)? } else { self.store.clone() };
        let out = change(&mut store)?;
        store.save(&path)?;
        self.store = store;
        Ok(out)
    }
    /// This call's tracks labelled `code` (a coded voice), for the host's rename.
    pub fn tracks_of(&self, label: &str) -> Vec<TrackId> {
        self.labels.iter().filter(|(_, l)| *l == label).map(|(t, _)| t.clone()).collect()
    }
    pub fn forget(&mut self, name: &str, path: &std::path::Path) -> Result<Vec<SpeakerEvent>> {
        let mut store = self.store.clone();
        ensure!(store.names.remove(name).is_some(), "unknown enrollment");
        store.save(path)?;
        self.store = store;
        Ok(self.clear_name(name))
    }
    /// Host already persisted removal; clear call-local labels without writing an
    /// older enrollment snapshot back over another source's confirmation.
    pub fn clear_name(&mut self, name: &str) -> Vec<SpeakerEvent> {
        let tracks: Vec<_> = self
            .labels
            .iter()
            .filter(|(_, v)| *v == name)
            .map(|(t, _)| t.clone())
            .collect();
        tracks
            .into_iter()
            .map(|track| {
                self.labels.remove(&track);
                SpeakerEvent::LabelChanged {
                    name: format!("Speaker {}", track.speaker),
                    track,
                }
            })
            .collect()
    }
    /// Host persisted a rename (a name, or a coded voice named); keep current call labels in sync without another
    /// write.
    pub fn rename_name(&mut self, from: &str, to: &str) -> Vec<SpeakerEvent> {
        self.labels
            .iter_mut()
            .filter_map(|(track, name)| {
                if name == from {
                    *name = to.into();
                    Some(SpeakerEvent::LabelChanged {
                        track: track.clone(),
                        name: to.into(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn close_session(&mut self, source: &str, session: u64) {
        self.labels
            .retain(|t, _| t.source != source || t.session != session);
        self.ignored_tracks
            .retain(|t| t.source != source || t.session != session);
    }
    /// Whether this track already has its label (a name or a code): its clean speech is not collected again.
    pub fn labelled(&self, track: &TrackId) -> bool {
        self.labels.contains_key(track)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::DIMENSION;
    fn track(i: i32) -> TrackId {
        TrackId { source: "Discord".into(), session: 1, speaker: i }
    }
    fn in_session(session: u64, speaker: i32) -> TrackId {
        TrackId { source: "Discord".into(), session, speaker }
    }
    fn emb() -> Embedding {
        Embedding::new(vec![1.; DIMENSION]).unwrap()
    }
    fn other() -> Embedding {
        Embedding::new((0..DIMENSION).map(|i| if i % 2 == 0 { 1. } else { -1. }).collect()).unwrap()
    }
    fn third() -> Embedding {
        Embedding::new((0..DIMENSION).map(|i| if i % 3 == 0 { 1. } else { -0.5 }).collect()).unwrap()
    }
    fn friday() -> VoiceTime {
        VoiceTime { ms: 1_792_782_000_000, stamp: "Fri 23 Oct 21h".into(), day: "2026-10-23".into() }
    }
    fn saturday() -> VoiceTime {
        VoiceTime { ms: 1_792_868_400_000, stamp: "Sat 24 Oct 21h".into(), day: "2026-10-24".into() }
    }
    fn flow(store: EnrollmentStore) -> NamingFlow {
        let mut f = NamingFlow::new(store);
        f.set_clock(friday);
        f
    }
    fn temp(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!("heimdall-{name}-{}-{}.json", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)))
    }
    fn code(events: &[SpeakerEvent]) -> (String, bool) {
        match &events[..] {
            [SpeakerEvent::VoiceCoded { code, new, .. }] => (code.clone(), *new),
            other => panic!("expected one coded voice, got {other:?}"),
        }
    }
    #[test]
    fn an_unknown_voice_gets_a_code_label_and_never_a_prompt() {
        let mut f = flow(EnrollmentStore::default());
        let events = f.clean_voice_samples(track(2), 10., vec![emb(), emb()], 12., "Hi all");
        assert_eq!(code(&events), ("Speaker Fri 23 Oct 21h #1".to_string(), true));
        assert_eq!(f.label(&track(2)), "Speaker Fri 23 Oct 21h #1");
        let v = &f.store.voices["Speaker Fri 23 Oct 21h #1"];
        assert_eq!((v.embeddings.len(), v.quote.as_str(), v.heard_in.as_str(), v.calls), (2, "Hi all", "Discord", 1));
        assert!(f.store.names.is_empty(), "a code is not a name");
        // Labelled once per call track.
        assert!(f.clean_voice(track(2), 30., emb(), 3., "again").is_empty());
        // A second new voice in the same hour is #2.
        assert_eq!(code(&f.clean_voice(track(3), 40., other(), 3., "")).0, "Speaker Fri 23 Oct 21h #2");
    }
    #[test]
    fn the_same_voice_in_a_later_call_keeps_its_code_and_samples_build_up() {
        let path = temp("coded");
        let lock = Arc::new(Mutex::new(()));
        let mut a = flow(EnrollmentStore::default()).persist_to(path.clone(), lock.clone());
        let (first, _) = code(&a.clean_voice_samples(track(1), 1., vec![emb(), emb()], 12., "Yo"));
        // A restart, the next day: the store read back recognises the voice.
        let mut b = NamingFlow::new(EnrollmentStore::load(&path).unwrap()).persist_to(path.clone(), lock.clone());
        b.set_clock(saturday);
        assert_eq!(code(&b.clean_voice_samples(in_session(2, 4), 1., vec![emb(), emb()], 12., "Back")), (first.clone(), false));
        let saved = EnrollmentStore::load(&path).unwrap();
        let v = &saved.voices[&first];
        assert_eq!((v.calls, v.embeddings.len(), v.days.len()), (2, 4, 2));
        // Samples stay bounded: the first two and the newest.
        for s in 3..10 {
            b.clean_voice_samples(in_session(s, 1), 1., vec![emb(), emb()], 12., "");
        }
        assert_eq!(EnrollmentStore::load(&path).unwrap().voices[&first].embeddings.len(), crate::identity::VOICE_SAMPLES);
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn another_calls_write_is_kept_and_its_voice_matched() {
        let path = temp("shared");
        let lock = Arc::new(Mutex::new(()));
        let mut a = flow(EnrollmentStore::default()).persist_to(path.clone(), lock.clone());
        let mut b = flow(EnrollmentStore::default()).persist_to(path.clone(), lock.clone());
        let (x, _) = code(&a.clean_voice(track(1), 1., emb(), 3., ""));
        // b's in-memory store is stale; the file is re-read, so the voice is matched, not coded twice.
        assert_eq!(code(&b.clean_voice(in_session(9, 1), 1., emb(), 3., "")), (x, false));
        let (y, _) = code(&b.clean_voice(in_session(9, 2), 1., third(), 3., ""));
        assert_eq!(EnrollmentStore::load(&path).unwrap().voices.len(), 2);
        assert_ne!(y, "Speaker Fri 23 Oct 21h #1");
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn a_known_name_wins_and_naming_a_code_moves_its_samples() {
        let mut f = flow(EnrollmentStore::default());
        let (c, _) = code(&f.clean_voice_samples(track(1), 1., vec![emb(), emb()], 12., ""));
        let mut store = f.store.clone();
        store.name_voice(&c, "Dave").unwrap();
        assert!(store.voices.is_empty());
        assert_eq!(store.names["Dave"].embeddings.len(), 2);
        assert!(store.name_voice(&c, "Dave").is_err(), "the code is retired");
        f.set_store(store);
        assert_eq!(f.rename_name(&c, "Dave").len(), 1);
        let events = f.clean_voice(in_session(2, 3), 1., emb(), 3., "");
        assert!(matches!(&events[..], [SpeakerEvent::LabelChanged { name, .. }] if name == "Dave"));
    }
    #[test]
    fn a_name_saved_meanwhile_wins_over_coding() {
        let path = temp("named-meanwhile");
        let lock = Arc::new(Mutex::new(()));
        let mut f = flow(EnrollmentStore::default()).persist_to(path.clone(), lock);
        let mut other = EnrollmentStore::default();
        other.enroll("Dave", emb()).unwrap();
        other.save(&path).unwrap();
        // f's in-memory store has no names yet (its Work::Store has not arrived): the file's name still wins.
        assert!(matches!(&f.clean_voice(track(1), 1., emb(), 3., "")[..], [SpeakerEvent::LabelChanged { name, .. }] if name == "Dave"));
        assert!(EnrollmentStore::load(&path).unwrap().voices.is_empty());
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn a_forgotten_voice_keeps_its_code() {
        let mut f = flow(EnrollmentStore::default());
        let (c, _) = code(&f.clean_voice(track(1), 1., emb(), 3., ""));
        assert!(f.store.set_irrelevant(&c, true));
        assert!(!f.store.set_irrelevant("Speaker nobody", true));
        assert_eq!(code(&f.clean_voice(in_session(2, 1), 1., emb(), 3., "")), (c.clone(), false));
        assert!(f.store.voices[&c].irrelevant);
    }
    #[test]
    fn an_old_ignored_voice_is_left_unlabelled() {
        let mut store = EnrollmentStore::default();
        store.ignore(vec![emb()], "Discord", "Subscribe", 1).unwrap();
        let mut f = flow(store);
        assert!(f.clean_voice(track(2), 1., emb(), 3., "").is_empty());
        assert_eq!(f.label(&track(2)), "Speaker 2");
        assert!(f.store.voices.is_empty());
    }
    #[test]
    fn incomplete_clean_evidence_codes_nothing() {
        let mut f = flow(EnrollmentStore::default());
        assert!(f.clean_voice(track(1), 1.0, emb(), 2.0, "").is_empty());
        assert!(f.clean_voice(track(1), f64::NAN, emb(), 3.0, "").is_empty());
        assert!(f.store.voices.is_empty());
    }
    #[test]
    fn host_rename_and_removal_preserve_enrollments() {
        let mut f = NamingFlow::new(EnrollmentStore::default());
        f.store.enroll("Alice", emb()).unwrap();
        f.store.enroll("Other", emb()).unwrap();
        f.labels.insert(track(1), "Alice".into());
        f.labels.insert(track(2), "Alice".into());
        assert_eq!(f.rename_name("Alice", "Bob").len(), 2);
        assert_eq!(f.label(&track(1)), "Bob");
        assert_eq!(f.tracks_of("Bob").len(), 2);
        assert_eq!(f.clear_name("Bob").len(), 2);
        assert_eq!(f.label(&track(1)), "Speaker 1");
        assert_eq!(f.store.names.len(), 2);
    }
    #[test]
    fn stores_without_coded_voices_read_and_write_as_before() {
        let path = temp("coded-compat");
        let mut store = EnrollmentStore::default();
        store.enroll("Dave", emb()).unwrap();
        store.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ignored") && !text.contains("voices"), "no new keys while empty");
        let back = EnrollmentStore::load(&path).unwrap();
        assert!(back.voices.is_empty() && back.names.contains_key("Dave"));
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["voices"] = serde_json::json!({"Speaker x #1": {"embeddings": []}});
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(EnrollmentStore::load(&path).is_err(), "a coded voice with no samples is refused");
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn local_time_labels_read_like_the_spec() {
        let t = voice_time(1_792_782_000_000);
        assert!(t.stamp.contains("Oct") && t.stamp.ends_with('h'), "{}", t.stamp);
        assert_eq!(t.day.len(), 10);
    }
}
