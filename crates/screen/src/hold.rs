//! Spoken commands pin a recording range at detection, retaining its original
//! encoded tier under the archive lifetime lock until the decision. Holds keep
//! no RGB history. Native supplements are acquired at detection as before;
//! approved attachments decode the phrase-time recording and write PNGs.
use crate::{
    buffer::{causal_ref, FrameRef},
    commands::{write_ring_action, write_shot},
    Action, Frame, FrameSet, PhraseMatch, ScreenBuffer, ShotKind,
};
use anyhow::{ensure, Result};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Instant,
};

/// A detection whose phrase start is unknown: the matcher bounds a phrase to 4 s, plus
/// detection latency. The hold keeps every ring frame a phrase start in that span could select.
pub const LOOKBACK_NS: u64 = 5_000_000_000;
/// Detections of one occurrence (partial, window, final) land within this of each other.
pub const HOLD_MERGE_NS: u64 = 1_000_000_000;
/// Pending holds across all sources; the oldest is dropped beyond this.
pub const MAX_HOLDS: usize = 32;
/// Native pixels referenced by pending holds. Beyond
/// it, full-resolution supplements are released oldest hold first, then oldest holds.
pub const MAX_HELD_BYTES: usize = 512 * 1024 * 1024;

/// A full-resolution grab this long after the phrase's trigger word no longer shows the moment the user spoke: the
/// phrase-time frame from the recording is attached beside it, first.
pub const FULL_RESOLUTION_SAME_MOMENT_NS: u64 = 1_500_000_000;
/// True when `shot` is a full-resolution grab taken too late to stand in for the phrase-time frame.
pub fn full_resolution_is_late(shot: &crate::Shot, trigger_ns: u64) -> bool {
    shot.kind == ShotKind::FullResolution && shot.frame_ns > trigger_ns.saturating_add(FULL_RESOLUTION_SAME_MOMENT_NS)
}
/// Native one-shot capture of one display. Injected so tests never touch TCC or a screen.
pub type Grabber = Arc<dyn Fn(u32) -> Result<Frame> + Send + Sync>;
/// A held native frame with its request-to-pixels latency, or why there is none.
type HeldNative = std::result::Result<(Frame, u64), String>;
/// Per-display native results and the total capture time.
type NativeSet = (Vec<(u32, HeldNative)>, u64);

/// The macOS full-resolution grab; `None` where native capture does not exist.
pub fn native_grabber() -> Option<Grabber> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(crate::macos::grab_full_resolution))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

enum Native {
    Absent,
    Ready(NativeSet),
    /// Grabbing on its own thread (75-90 ms per display measured); joined on use.
    Pending(JoinHandle<NativeSet>),
}

pub struct ScreenHold {
    pub source: String,
    /// Detection timestamp: the start of the phrase's last word.
    pub anchor_ns: u64,
    /// Phrase starts in `from_ns..=to_ns` select from the held ring frames.
    from_ns: u64,
    to_ns: u64,
    displays: Vec<u32>,
    ring: BTreeMap<u32, Vec<FrameRef>>,
    native: Native,
}

/// One native frame per display, with the generation checks the ring guarantees.
/// A capture-generation change during the grabs discards every supplemental frame.
fn grab_native(
    buffer: &Arc<Mutex<ScreenBuffer>>,
    epoch: (u64, u64),
    ids: &[u32],
    now_ns: u64,
    grab: &mut dyn FnMut(u32) -> Result<Frame>,
) -> NativeSet {
    let started = Instant::now();
    let mut out = vec![];
    for &id in ids {
        let result = (|| -> Result<_> {
            ensure!(
                buffer.lock().unwrap().attachment_epoch() == epoch,
                "capture generation changed before full-resolution grab"
            );
            let grab_start = Instant::now();
            let frame = grab(id)?;
            let latency = grab_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            ensure!(
                frame.display.id == id && frame.host_time_ns >= now_ns,
                "invalid or stale full-resolution frame"
            );
            ensure!(
                buffer.lock().unwrap().attachment_epoch() == epoch,
                "capture generation changed during full-resolution grab"
            );
            Ok((frame, latency))
        })();
        out.push((id, result.map_err(|e| format!("{e:#}"))));
    }
    if buffer.lock().unwrap().attachment_epoch() != epoch {
        for (_, r) in &mut out {
            *r = Err("capture generation changed; discarded supplemental frames".into());
        }
    }
    (
        out,
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
    )
}

impl ScreenHold {
    /// Copy (by reference) the ring frames a phrase start can select: the exact
    /// start when known, else the lookback. The ring mutex is released before any native request.
    fn ring_only(
        buffer: &Arc<Mutex<ScreenBuffer>>,
        source: &str,
        anchor_ns: u64,
        phrase_start_ns: Option<u64>,
        now_ns: u64,
    ) -> (Self, (u64, u64)) {
        let (from_ns, to_ns) = match phrase_start_ns {
            Some(start) => (start, start),
            None => (anchor_ns.saturating_sub(LOOKBACK_NS), anchor_ns),
        };
        let (displays, ring, epoch) = {
            let mut b = buffer.lock().unwrap();
            let epoch = b.attachment_epoch();
            let (displays, ring) = b.pin(from_ns, to_ns, now_ns);
            (displays, ring, epoch)
        };
        let mut hold = Self {
            source: source.into(),
            anchor_ns,
            from_ns,
            to_ns: to_ns.min(now_ns),
            displays,
            ring,
            native: Native::Absent,
        };
        if let Some(start) = phrase_start_ns {
            hold.narrow(start);
        }
        (hold, epoch)
    }
    /// No native request for a display the ring cannot attach (screen off, no permission, stale).
    fn native_ids(&self) -> Vec<u32> {
        self.ring
            .iter()
            .filter(|(_, frames)| !frames.is_empty())
            .map(|(id, _)| *id)
            .collect()
    }
    /// Screenshot hold, grabbing on the caller's thread (a decision already waiting on it).
    pub fn pin(
        buffer: &Arc<Mutex<ScreenBuffer>>,
        source: &str,
        anchor_ns: u64,
        phrase_start_ns: Option<u64>,
        now_ns: u64,
        grab: Option<&mut dyn FnMut(u32) -> Result<Frame>>,
    ) -> Self {
        let (mut hold, epoch) =
            Self::ring_only(buffer, source, anchor_ns, phrase_start_ns, now_ns);
        let ids = hold.native_ids();
        if let (Some(grab), false) = (grab, ids.is_empty()) {
            hold.native = Native::Ready(grab_native(buffer, epoch, &ids, now_ns, grab));
        }
        hold
    }
    /// Screenshot hold for a detection mid-turn: the native grab runs on its own
    /// thread, so the detecting (pipeline) thread only copies ring references.
    fn pin_async(
        buffer: &Arc<Mutex<ScreenBuffer>>,
        source: &str,
        anchor_ns: u64,
        phrase_start_ns: Option<u64>,
        now_ns: u64,
        grab: &Grabber,
    ) -> Self {
        let (mut hold, epoch) =
            Self::ring_only(buffer, source, anchor_ns, phrase_start_ns, now_ns);
        let ids = hold.native_ids();
        if !ids.is_empty() {
            let (buffer, grab, grab_ids) = (buffer.clone(), grab.clone(), ids.clone());
            let thread = std::thread::Builder::new()
                .name("heimdall-screen-hold".into())
                .spawn(move || grab_native(&buffer, epoch, &grab_ids, now_ns, &mut |id| grab(id)));
            hold.native = match thread {
                Ok(handle) => Native::Pending(handle),
                Err(e) => Native::Ready((
                    ids.iter()
                        .map(|id| (*id, Err(format!("grab thread: {e}"))))
                        .collect(),
                    0,
                )),
            };
        }
        hold
    }
    pub fn covers(&self, phrase_start_ns: u64) -> bool {
        (self.from_ns..=self.to_ns).contains(&phrase_start_ns)
    }
    /// The exact phrase start is known: keep only the frame it selects per display.
    pub fn narrow(&mut self, phrase_start_ns: u64) {
        if !self.covers(phrase_start_ns) {
            return;
        }
        for frames in self.ring.values_mut() {
            *frames = causal_ref(frames.iter(), phrase_start_ns)
                .cloned()
                .into_iter()
                .collect();
        }
        (self.from_ns, self.to_ns) = (phrase_start_ns, phrase_start_ns);
    }
    /// Resolve the phrase time from the pinned recording, over the known displays.
    pub fn select(&self, phrase_start_ns: u64) -> FrameSet {
        let covered = self.covers(phrase_start_ns);
        let mut frames = vec![];
        let mut missing_displays = vec![];
        let mut errors = vec![];
        for id in &self.displays {
            match self
                .ring
                .get(id)
                .filter(|_| covered)
                .and_then(|q| causal_ref(q.iter(), phrase_start_ns))
            {
                Some(f) => match f.decode(phrase_start_ns) {
                    Ok(frame) => frames.push(frame),
                    Err(error) => { missing_displays.push(*id); errors.push(format!("display {id}: {error:#}")); },
                },
                None => missing_displays.push(*id),
            }
        }
        FrameSet {
            trigger_ns: phrase_start_ns,
            frames,
            missing_displays,
            errors,
        }
    }
    fn join(&mut self) {
        if let Native::Pending(_) = self.native {
            let Native::Pending(handle) = std::mem::replace(&mut self.native, Native::Absent)
            else {
                unreachable!()
            };
            self.native = Native::Ready(handle.join().unwrap_or_else(|_| {
                (
                    self.native_ids()
                        .into_iter()
                        .map(|id| (id, Err("full-resolution grab panicked".into())))
                        .collect(),
                    0,
                )
            }));
        }
    }
    fn poll(&mut self) {
        if matches!(&self.native, Native::Pending(h) if h.is_finished()) {
            self.join();
        }
    }
    fn native_frames(&self) -> impl Iterator<Item = &Frame> + '_ {
        match &self.native {
            Native::Ready((set, _)) => Some(
                set.iter()
                    .filter_map(|(_, r)| r.as_ref().ok().map(|(f, _)| f)),
            ),
            _ => None,
        }
        .into_iter()
        .flatten()
    }
    fn buffers(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.native_frames()
            .map(|f| (Arc::as_ptr(&f.rgb) as *const u8 as usize, f.rgb.len()))
    }
    fn release_fullres(&mut self, why: &str) -> bool {
        let Native::Ready((set, _)) = &mut self.native else {
            return false;
        };
        let mut released = false;
        for (_, r) in set {
            if r.is_ok() {
                *r = Err(why.into());
                released = true;
            }
        }
        released
    }
}

/// Approval: write the held phrase-time frames as the command's attachment.
/// Missing displays fail explicitly, exactly as live selection does.
pub fn attach_held(
    mut hold: ScreenHold,
    matched: &PhraseMatch,
    now_ns: u64,
    dir: &Path,
) -> Result<Action> {
    let mut action = write_ring_action(hold.select(matched.start_ns), matched, now_ns, dir)?;
    hold.join();
    let started = Instant::now();
    let ids: Vec<_> = action.shots.iter().map(|s| s.display_id).collect();
    let Native::Ready((set, grab_ns)) = hold.native else {
        return Ok(action);
    };
    let mut fullres = vec![];
    for (id, held) in set {
        if !ids.contains(&id) {
            continue;
        }
        let shot = held.and_then(|(frame, latency)| {
            write_shot(
                &frame,
                matched.start_ns,
                ShotKind::FullResolution,
                Some(latency),
                dir,
            )
            .map_err(|e| format!("{e:#}"))
        });
        match shot {
            Ok(shot) => fullres.push(shot),
            Err(error) => action
                .full_resolution_errors
                .push(format!("display {id}: {error}")),
        }
    }
    // Bug note 2026-10-25T21-31-53: a final-only detection grabs at the decision, seconds after "look at the screen",
    // when a menu or dialog he meant may be gone. Its full-resolution frame then joins the phrase-time one instead of
    // replacing it (`full_resolution_is_late`), and the note says which is which.
    if let Some(late) = fullres.iter().filter(|s| full_resolution_is_late(s, matched.trigger_ns)).map(|s| s.frame_ns).min() {
        action.note = format!(
            "[screenshot {} attached, and a sharper one from {}]",
            crate::commands::wall_label(matched.start_ns, now_ns),
            crate::commands::wall_label(late, now_ns.max(late))
        );
    }
    action.shots.extend(fullres);
    action.full_resolution_elapsed_ns =
        Some(grab_ns.saturating_add(started.elapsed().as_nanos().min(u64::MAX as u128) as u64));
    Ok(action)
}

/// Pending holds for undecided commands.
pub struct ScreenHolds {
    holds: VecDeque<ScreenHold>,
    grab: Option<Grabber>,
}
impl ScreenHolds {
    pub fn new(grab: Option<Grabber>) -> Self {
        Self {
            holds: VecDeque::new(),
            grab,
        }
    }
    pub fn set_grabber(&mut self, grab: Option<Grabber>) {
        self.grab = grab;
    }
    fn held(&mut self, source: &str, anchor_ns: u64) -> Option<&mut ScreenHold> {
        self.holds
            .iter_mut()
            .find(|h| h.source == source && h.anchor_ns.abs_diff(anchor_ns) <= HOLD_MERGE_NS)
    }
    /// Screenshot: pin unless this occurrence is already held; a later detection with
    /// the exact phrase start narrows an earlier lookback hold to the frame it selects.
    /// The native grab runs off the calling thread.
    pub fn pin(
        &mut self,
        buffer: &Arc<Mutex<ScreenBuffer>>,
        source: &str,
        anchor_ns: u64,
        phrase_start_ns: Option<u64>,
        now_ns: u64,
    ) {
        if let Some(h) = self.held(source, anchor_ns) {
            if let Some(start) = phrase_start_ns {
                h.narrow(start);
            }
            return;
        }
        let hold = match &self.grab {
            Some(grab) => {
                ScreenHold::pin_async(buffer, source, anchor_ns, phrase_start_ns, now_ns, grab)
            }
            None => ScreenHold::pin(buffer, source, anchor_ns, phrase_start_ns, now_ns, None),
        };
        self.holds.push_back(hold);
        self.enforce_bounds();
    }
    /// A phrase first detected at its decision (in the turn's final): pin without
    /// holding, grabbing on this thread because the decision waits for it anyway.
    pub fn pin_now(
        &mut self,
        buffer: &Arc<Mutex<ScreenBuffer>>,
        source: &str,
        anchor_ns: u64,
        phrase_start_ns: u64,
        now_ns: u64,
    ) -> ScreenHold {
        let grab = self.grab.clone();
        let mut call = grab.as_ref().map(|g| move |id| g(id));
        ScreenHold::pin(
            buffer,
            source,
            anchor_ns,
            Some(phrase_start_ns),
            now_ns,
            call.as_mut()
                .map(|g| g as &mut dyn FnMut(u32) -> Result<Frame>),
        )
    }
    /// The decision has come: remove and return the hold for this occurrence (any
    /// detection timestamp within the merge window, covering the phrase start).
    /// Other holds for the same occurrence are dropped.
    pub fn take(
        &mut self,
        source: &str,
        triggers: &[u64],
        phrase_start_ns: u64,
    ) -> Option<ScreenHold> {
        let same = |h: &ScreenHold| {
            h.source == source
                && triggers
                    .iter()
                    .any(|t| t.abs_diff(h.anchor_ns) <= HOLD_MERGE_NS)
        };
        let mut found: Vec<ScreenHold> = vec![];
        let mut kept = VecDeque::with_capacity(self.holds.len());
        for h in self.holds.drain(..) {
            if same(&h) {
                found.push(h);
            } else {
                kept.push_back(h);
            }
        }
        self.holds = kept;
        found.into_iter().find(|h| h.covers(phrase_start_ns))
    }
    /// After a final: holds up to its end that no decided command took.
    pub fn release_through(&mut self, source: &str, end_ns: u64) {
        self.holds
            .retain(|h| h.source != source || h.anchor_ns > end_ns);
    }
    /// Source closed or capture gap: the abandoned path releases everything held for it.
    pub fn reset_source(&mut self, source: &str) {
        self.holds.retain(|h| h.source != source);
    }
    /// Seeing turned off: nothing captured before may attach to a later decision.
    pub fn clear(&mut self) {
        self.holds.clear();
    }
    pub fn len(&self) -> usize {
        self.holds.len()
    }
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty()
    }
    /// Wait for in-flight native grabs (replay and tests advance faster than they run).
    pub fn settle(&mut self) {
        for h in &mut self.holds {
            h.join();
        }
        self.enforce_bounds();
    }
    /// Distinct pixel buffers referenced by pending holds (in-flight grabs excluded).
    pub fn bytes(&self) -> usize {
        let mut seen = HashSet::new();
        self.holds
            .iter()
            .flat_map(ScreenHold::buffers)
            .filter(|(ptr, _)| seen.insert(*ptr))
            .map(|(_, len)| len)
            .sum()
    }
    fn enforce_bounds(&mut self) {
        for h in &mut self.holds {
            h.poll();
        }
        while self.holds.len() > MAX_HOLDS {
            self.holds.pop_front();
        }
        while self.bytes() > MAX_HELD_BYTES {
            let why = "released: pending screenshots exceeded 512 MiB";
            if self.holds.iter_mut().any(|h| h.release_fullres(why)) {
                continue;
            }
            // A single hold is never split; the newest stays.
            if self.holds.len() <= 1 {
                break;
            }
            self.holds.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DisplayInfo, ScreenConfig};
    const S: u64 = 1_000_000_000;
    fn frame(id: u32, t: u64, value: u8, width: u32) -> Frame {
        Frame::new(
            DisplayInfo {
                id,
                x: 0.,
                y: 0.,
                width,
                height: 1,
            },
            t,
            width,
            1,
            vec![value; width as usize * 3],
        )
        .unwrap()
    }
    fn ring(_history_s: u64) -> Arc<Mutex<ScreenBuffer>> {
        Arc::new(Mutex::new(
            ScreenBuffer::new(ScreenConfig {
                fps: 1,
                ..Default::default()
            })
            .unwrap(),
        ))
    }
    fn matched(start: u64) -> PhraseMatch {
        PhraseMatch {
            source: "mic".into(),
            phrase: "look at this".into(),
            start_ns: start,
            trigger_ns: start + 600_000_000,
        }
    }
    #[test]
    fn lookback_hold_outlives_history_and_selects_the_exact_phrase_start() {
        let b = ring(5);
        for t in 0..=3 {
            for id in [1, 2] {
                b.lock()
                    .unwrap()
                    .push(frame(id, 10 * S + t * S, t as u8, 2))
                    .unwrap();
            }
        }
        let mut holds = ScreenHolds::new(None);
        // Detected 1.4 s after "look at this" began at 11.0 s, start unknown.
        holds.pin(&b, "mic", 12_400_000_000, None, 12_400_000_000);
        // Five minutes later, the ring has moved on and the old frames are gone.
        for t in 13..=310 {
            for id in [1, 2] {
                b.lock().unwrap().push(frame(id, t * S, 200, 2)).unwrap();
            }
        }
        assert_eq!(b.lock().unwrap().select(11 * S, 310 * S).frames.len(),2, "recorded history survives long speech");
        let t = tempfile::tempdir().unwrap();
        let hold = holds.take("mic", &[11_600_000_000], 11 * S).unwrap();
        assert!(holds.is_empty());
        let action = attach_held(hold, &matched(11 * S), 310 * S, t.path()).unwrap();
        assert_eq!(
            action
                .shots
                .iter()
                .map(|s| (s.display_id, s.frame_ns))
                .collect::<Vec<_>>(),
            [(1, 11 * S), (2, 11 * S)]
        );
    }
    #[test]
    fn exact_start_narrows_a_lookback_hold_and_never_selects_a_later_frame() {
        let b = ring(20);
        for t in 0..=6 {
            b.lock().unwrap().push(frame(1, t * S, t as u8, 2)).unwrap();
        }
        let mut holds = ScreenHolds::new(None);
        holds.pin(&b, "mic", 6 * S, None, 6 * S);
        assert_eq!(
            holds.holds[0].ring[&1].len(),
            1,
            "one recording range handle covers starts from 1 s"
        );
        // Window decode: the same occurrence, exact start 4.5 s.
        holds.pin(&b, "mic", 5_700_000_000, Some(4_500_000_000), 15 * S);
        assert_eq!(holds.len(), 1);
        assert_eq!(
            holds.holds[0].ring[&1]
                .iter()
                .map(|f| f.host_time_ns)
                .collect::<Vec<_>>(),
            [S]
        );
        let h = holds.take("mic", &[5_700_000_000], 4_500_000_000).unwrap();
        assert_eq!(h.select(4_500_000_000).frames[0].host_time_ns, 4 * S);
        // A start outside the pinned span is never answered with a nearby frame.
        assert_eq!(h.select(6 * S).missing_displays, [1]);
    }
    #[test]
    fn full_resolution_is_grabbed_at_detection_and_dropped_on_rejection() {
        let b = ring(20);
        b.lock().unwrap().push(frame(1, 10 * S, 1, 2)).unwrap();
        let screen = Arc::new(std::sync::atomic::AtomicU64::new(11));
        let grabbed = Arc::new(Mutex::new(vec![]));
        let (s, g) = (screen.clone(), grabbed.clone());
        let mut holds = ScreenHolds::new(Some(Arc::new(move |id| {
            let at = s.load(std::sync::atomic::Ordering::SeqCst);
            let f = frame(id, at * S, at as u8, 4);
            g.lock().unwrap().push(Arc::downgrade(&f.rgb));
            Ok(f)
        })));
        holds.pin(&b, "mic", 10_800_000_000, None, 10_800_000_000);
        holds.settle();
        screen.store(300, std::sync::atomic::Ordering::SeqCst);
        let t = tempfile::tempdir().unwrap();
        let hold = holds.take("mic", &[10_800_000_000], 10 * S).unwrap();
        let action = attach_held(hold, &matched(10 * S), 300 * S, t.path()).unwrap();
        let native: Vec<_> = action
            .shots
            .iter()
            .filter(|s| s.kind == ShotKind::FullResolution)
            .collect();
        assert_eq!(native.len(), 1);
        assert_eq!(
            native[0].frame_ns,
            11 * S,
            "the screen at detection, not at the decision"
        );
        assert!(action.full_resolution_elapsed_ns.is_some());
        // Rejected: the next hold is taken and dropped; nothing written, pixels freed.
        holds.pin(&b, "mic", 10_900_000_000 + 5 * S, None, 16 * S);
        holds.settle();
        assert_eq!(grabbed.lock().unwrap().len(), 2);
        drop(holds.take("mic", &[15_900_000_000], 10 * S + 5 * S));
        assert!(grabbed
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.strong_count() == 0));
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    }
    #[test]
    fn memory_bound_releases_full_resolution_first_then_oldest_holds() {
        let b = ring(600);
        // 1 MiB ring frames, shared between overlapping holds; 100 MiB native frames.
        let ring_w = (1 << 20) / 3;
        for t in 0..=40 {
            b.lock().unwrap().push(frame(1, t * S, 0, ring_w)).unwrap();
        }
        let native_w = (100 << 20) / 3;
        let mut holds = ScreenHolds::new(Some(Arc::new(move |id| {
            Ok(frame(id, 1 << 62, 1, native_w))
        })));
        for i in 0..8 {
            holds.pin(&b, "mic", (i * 5 + 5) * S, None, (i * 5 + 5) * S);
            holds.settle();
        }
        assert_eq!(holds.len(), 8);
        assert!(holds.bytes() <= MAX_HELD_BYTES);
        let with_native = holds
            .holds
            .iter()
            .filter(|h| h.native_frames().count() > 0)
            .count();
        assert_eq!(with_native, 5, "recording references use no RGB budget; oldest native frames go first");
        let Native::Ready((oldest, _)) = &holds.holds[0].native else {
            panic!()
        };
        assert!(oldest[0].1.as_ref().unwrap_err().contains("512 MiB"));
        assert_eq!(holds.holds[7].native_frames().count(), 1);
        // Count bound: the oldest holds go.
        for i in 8..40 {
            holds.pin(&b, "mic", i * S * 10 + 5 * S, None, i * S * 10 + 5 * S);
            holds.settle();
        }
        assert_eq!(holds.len(), MAX_HOLDS);
        assert!(holds.bytes() <= MAX_HELD_BYTES);
        holds.reset_source("mic");
        assert!(holds.is_empty());
    }
    #[test]
    fn several_occurrences_keep_their_own_frames_and_release_after_final() {
        let b = ring(20);
        for t in 0..=40 {
            b.lock().unwrap().push(frame(1, t * S, t as u8, 2)).unwrap();
        }
        let mut holds = ScreenHolds::new(None);
        holds.pin(&b, "mic", 25 * S, Some(24 * S), 40 * S);
        holds.pin(&b, "mic", 32 * S, Some(31 * S), 40 * S);
        holds.pin(&b, "other", 32 * S, Some(31 * S), 40 * S);
        assert_eq!(holds.len(), 3);
        assert_eq!(
            holds
                .take("mic", &[32 * S], 31 * S)
                .unwrap()
                .select(31 * S)
                .frames[0]
                .host_time_ns,
            31 * S
        );
        holds.release_through("mic", 30 * S);
        assert_eq!(holds.len(), 1);
        assert!(holds.take("mic", &[25 * S], 24 * S).is_none());
    }
    /// docs/screen.md measured native grabs at 75-90 ms per display: never on the caller's thread.
    #[test]
    fn a_slow_native_grab_does_not_block_the_detecting_thread() {
        let b = ring(20);
        for id in [1, 2] {
            b.lock().unwrap().push(frame(id, 10 * S, 1, 2)).unwrap();
        }
        let mut holds = ScreenHolds::new(Some(Arc::new(|id| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(frame(id, u64::MAX / 2, 9, 4))
        })));
        let started = Instant::now();
        holds.pin(&b, "mic", 10_800_000_000, None, 10_800_000_000);
        let pinned = started.elapsed();
        eprintln!("pin with a 2 x 300 ms native grab returned in {pinned:?}");
        assert!(
            pinned < std::time::Duration::from_millis(50),
            "pin blocked for {pinned:?}"
        );
        let t = tempfile::tempdir().unwrap();
        let hold = holds.take("mic", &[10_800_000_000], 10 * S).unwrap();
        let action = attach_held(hold, &matched(10 * S), 20 * S, t.path()).unwrap();
        assert_eq!(
            action
                .shots
                .iter()
                .filter(|s| s.kind == ShotKind::FullResolution)
                .count(),
            2
        );
    }
}
