mod hal;
mod tap;
mod vpio;
/// Dirty bit: the user chose a different microphone.
pub const MICROPHONE: u32 = hal::MICROPHONE;
/// Dirty bit: the user changed which apps App Audio records.
pub const APPS: u32 = hal::APPS;
use crate::{
    pipeline::{self, Command, Pipeline},
    echo::{EchoTarget, Shared},
    Capture, CaptureConfig, CaptureConsent, CaptureReceiver,
};
use anyhow::Result;
use block2::RcBlock;
use hal::{Candidate, Listener};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_core_audio::*;
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use std::{
    collections::{HashMap, HashSet},
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
struct WakeObservers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}
impl WakeObservers {
    fn new(dirty: Arc<AtomicU32>, wake: thread::Thread) -> Self {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let mut tokens = vec![];
        unsafe {
            for (name, bits) in [
                (NSWorkspaceWillSleepNotification, hal::SLEEP),
                (NSWorkspaceDidWakeNotification, hal::WAKE),
            ] {
                let dirty = dirty.clone();
                let wake = wake.clone();
                let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                    dirty.fetch_or(bits, Ordering::Release);
                    wake.unpark();
                });
                tokens.push(center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    None,
                    &block,
                ));
            }
        }
        Self { center, tokens }
    }
}
impl Drop for WakeObservers {
    fn drop(&mut self) {
        for token in &self.tokens {
            unsafe {
                self.center.removeObserver(
                    &*(Retained::as_ptr(token) as *const objc2::runtime::AnyObject),
                );
            }
        }
    }
}
fn send(p: &Pipeline, c: Command) {
    let _ = p.tx.send(c);
    p.wake.unpark();
}
struct Active {
    candidate: Candidate,
    native: tap::NativeSource,
    started: u64,
    retries: u8,
}
struct Failure {
    attempts: u8,
    retry_ns: u64,
}
pub fn start(
    config: CaptureConfig,
    consent: CaptureConsent,
) -> Result<(Capture, CaptureReceiver)> {
    let archive = Box::new(crate::Archive::create(&config.archive_dir)?);
    start_with_archive(config, consent, archive)
}
pub fn start_with_archive(
    config: CaptureConfig,
    consent: CaptureConsent,
    archive: Box<dyn crate::ArchiveSink>,
) -> Result<(Capture, CaptureReceiver)> {
    let (pipeline, receiver) = pipeline::start_pipeline_with_archive(archive)?;
    let capture = spawn(Some(pipeline), consent.bits(), &config)?;
    Ok((capture, receiver))
}
pub fn discover_only(config: &CaptureConfig) -> Result<Capture> {
    spawn(None, 0, config)
}
pub fn idle(config: CaptureConfig) -> Result<(Capture, CaptureReceiver)> {
    let (pipeline, receiver) = pipeline::start_pipeline(&config)?;
    Ok((spawn(Some(pipeline), 0, &config)?, receiver))
}
struct Mics {
    chosen: Arc<std::sync::Mutex<(Option<String>, Vec<String>)>>,
    report: Option<crossbeam_channel::Sender<crate::MicrophoneState>>,
    /// App Audio keys to tap, and where the apps playing now are reported.
    recorded: Arc<std::sync::Mutex<HashSet<String>>>,
    playing: Option<crossbeam_channel::Sender<Vec<crate::AppIdentity>>>,
}
/// Source generations across every capture run in this process (`mic:<uid>:<generation>`).
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn spawn(pipeline: Option<Pipeline>, initial_access: u32, config: &CaptureConfig) -> Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let access = Arc::new(AtomicU32::new(initial_access));
    let access_worker = access.clone();
    let dirty = Arc::new(AtomicU32::new(hal::DISCOVERY));
    let dirty_worker = dirty.clone();
    let microphone = Arc::new(std::sync::Mutex::new((
        config.microphone.clone(),
        config.extra_microphones.clone(),
    )));
    let recorded_apps = Arc::new(std::sync::Mutex::new(config.recorded_apps.iter().cloned().collect()));
    let mics = Mics {
        chosen: microphone.clone(),
        report: config.microphones.clone(),
        recorded: recorded_apps.clone(),
        playing: config.playing_apps.clone(),
    };
    let echo = Shared::new();
    let echo_worker = echo.clone();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let owner = thread::Builder::new()
        .name("heimdall-hal".into())
        .spawn(move || {
            let result = run(stop_worker, pipeline, access_worker, dirty_worker, mics, echo_worker, &ready_tx);
            if let Err(e) = result {
                let _ = ready_tx.send(Err(format!("{e:#}")));
            }
        })?;
    let wake = owner.thread().clone();
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(Capture {
            stop,
            owner: Some(owner),
            wake,
            access,
            microphone,
            recorded_apps,
            dirty,
            echo: crate::EchoControl(echo),
        }),
        result => {
            echo.closed.store(true, Ordering::Release);
            stop.store(true, Ordering::Release);
            wake.unpark();
            let _ = owner.join();
            anyhow::bail!("HAL discovery startup failed: {result:?}");
        }
    }
}
fn run(
    stop: Arc<AtomicBool>,
    pipeline: Option<Pipeline>,
    access: Arc<AtomicU32>,
    dirty: Arc<AtomicU32>,
    mics: Mics,
    echo: Arc<Shared>,
    ready: &crossbeam_channel::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut published: Option<crate::MicrophoneState> = None;
    let mut published_apps: Option<Vec<crate::AppIdentity>> = None;
    let own_pid = std::process::id();
    let wake = thread::current();
    if let Some(p) = &pipeline {
        if let Ok(mut w) = p.control_wake.lock() {
            *w = Some(wake.clone());
        }
    }
    let changes = Arc::new(crossbeam_queue::ArrayQueue::new(512));
    let _power = WakeObservers::new(dirty.clone(), wake.clone());
    let mut listeners: HashMap<(u32, u32, u32), Listener> = HashMap::new();
    let global = kAudioObjectPropertyScopeGlobal;
    for (selector, bits) in [
        (kAudioHardwarePropertyDevices, hal::DISCOVERY),
        (kAudioHardwarePropertyProcessObjectList, hal::DISCOVERY),
        (kAudioHardwarePropertyDefaultInputDevice, hal::DISCOVERY),
        (kAudioHardwarePropertyDefaultOutputDevice, hal::ROUTE),
        (kAudioHardwarePropertyServiceRestarted, hal::WAKE),
    ] {
        listeners.insert(
            (kAudioObjectSystemObject as u32, selector, global),
            Listener::new(
                kAudioObjectSystemObject as u32,
                selector,
                global,
                bits,
                dirty.clone(),
                wake.clone(),
                changes.clone(),
            )?,
        );
    }
    // echo guard (23 Oct): the voice-processing worker exists only when capture records; it never blocks this thread.
    let echo_worker = match &pipeline {
        Some(p) => match vpio::spawn(echo.clone(), p.tx.clone(), p.wake.clone()) {
            Ok(join) => Some(join),
            Err(e) => {
                send(p, Command::Diagnostic(format!("echo cancellation unavailable: {e}")));
                None
            }
        },
        None => None,
    };
    let mut published_echo: Option<EchoTarget> = None;
    let mut own_devices: Option<(u32, Vec<String>)> = None;
    let mut active: HashMap<String, Active> = HashMap::new();
    let mut failed: HashMap<String, Failure> = HashMap::new();
    let mut seen_errors = HashSet::new();
    let mut sleeping = false;
    let mut candidates = vec![];
    let mut announced = false;
    let mut discovery_failures = 0u8;
    let mut next_discovery = 0u64;
    let result = (|| -> Result<()> {
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if pipeline
                .as_ref()
                .is_some_and(|p| !p.alive.load(Ordering::Acquire))
            {
                anyhow::bail!(
                    "archive intake stopped; capture is stopping to avoid unarchived recording"
                );
            }
            let mut bits = dirty.swap(0, Ordering::AcqRel);
            let mut changed = HashSet::new();
            let mut popped = 0;
            while let Some(c) = changes.pop() {
                changed.insert(c);
                popped += 1;
            }
            // A full queue may have lost events: then no ROUTE is ever treated as the echo unit's own noise.
            let overflowed = popped >= changes.capacity();
            let now = crate::host_now_ns();
            if bits & hal::SLEEP != 0 {
                sleeping = true;
            }
            if bits & hal::WAKE != 0 {
                sleeping = false;
                failed.clear();
                listeners.retain(|(object, _, _), _| *object == kAudioObjectSystemObject as u32);
            }
            if bits != 0
                || (discovery_failures > 0 && discovery_failures < 4 && now >= next_discovery)
            {
                let recorded = mics.recorded.lock().map(|r| r.clone()).unwrap_or_default();
                let snapshot = match hal::snapshot().map(|mut snapshot| {
                    // App Audio filters here, before any tap exists: unrecorded apps are only listed.
                    let playing = playing_objects(&snapshot);
                    include_dormant_processes(&mut snapshot, &|app, pid| {
                        own_process(pid, app, own_pid) || recorded.contains(&app.key)
                    });
                    let apps = keep_recorded(&mut snapshot, &playing, own_pid, &recorded);
                    (snapshot, apps)
                }) {
                    Ok((snapshot, apps)) => {
                        discovery_failures = 0;
                        if published_apps.as_ref() != Some(&apps) {
                            if let Some(report) = &mics.playing {
                                let _ = report.send(apps.clone());
                            }
                            published_apps = Some(apps);
                        }
                        snapshot
                    }
                    Err(error) => {
                        discovery_failures = discovery_failures.saturating_add(1);
                        next_discovery = now + (1u64 << discovery_failures.min(4)) * 1_000_000_000;
                        hal::Snapshot {errors:vec![format!("HAL discovery unavailable: {error:#}; retry {discovery_failures}/4, then wait for a HAL event")],..Default::default()}
                    }
                };
                // echo guard (23 Oct): the voice-processing unit joining or leaving Heimdall's own device list is
                // not a route change; restarting every source for it would cut the mic mid-reply.
                if own_route_noise(bits, &changed, overflowed, &own_devices, &snapshot.own_devices) {
                    bits &= !hal::ROUTE;
                }
                own_devices = snapshot.own_devices.clone();
                candidates = snapshot.candidates;
                let mut wanted: HashSet<_> = listeners
                    .keys()
                    .filter(|(object, _, _)| *object == kAudioObjectSystemObject as u32)
                    .copied()
                    .collect();
                for (object, selector, scope, flags) in snapshot.watches {
                    let key = (object, selector, scope);
                    wanted.insert(key);
                    if let std::collections::hash_map::Entry::Vacant(entry) = listeners.entry(key) {
                        match Listener::new(
                            object,
                            selector,
                            scope,
                            flags,
                            dirty.clone(),
                            wake.clone(),
                            changes.clone(),
                        ) {
                            Ok(l) => {
                                entry.insert(l);
                                // The snapshot read this property before installing
                                // its listener. Reconcile once after registration so
                                // a change in that interval cannot remain invisible
                                // until an unrelated HAL event. This is bounded by
                                // new watches, not an idle discovery poll.
                                dirty.fetch_or(hal::DISCOVERY, Ordering::Release);
                            }
                            Err(e) => {
                                if let Some(p) = &pipeline {
                                    send(p,Command::Diagnostic(format!("HAL listener unavailable ({object}/{selector}): {e}")));
                                }
                            }
                        }
                    }
                }
                listeners.retain(|key, _| wanted.contains(key));
                for error in snapshot.errors {
                    if seen_errors.insert(error.clone()) {
                        if let Some(p) = &pipeline {
                            send(p, Command::Diagnostic(error));
                        }
                    }
                }
                // A fresh hardware/activity event releases capped recovery only for
                // that event; no busy retry of a process which has exited.
                failed.retain(|key, f| {
                    if f.attempts < 4 {
                        return true;
                    }
                    let Some(c) = candidates.iter().find(|c| &c.key == key) else {
                        return false;
                    };
                    let object = match c.target {
                        hal::Target::Mic(d) => d,
                        hal::Target::Tap { object, .. } => object,
                    };
                    !changed.iter().any(|(obj, sel)| {
                        *obj == object
                            && (*sel == kAudioProcessPropertyIsRunningOutput
                                || *sel == kAudioProcessPropertyIsRunning
                                || *sel == kAudioDevicePropertyDeviceIsAlive)
                    })
                });
            }
            if bits & hal::ROUTE != 0 {
                failed.clear();
            }
            let grants = access.fetch_and(!4, Ordering::AcqRel);
            if grants & 4 != 0 { failed.clear(); }
            // One primary microphone (the chosen one, else built-in) plus only the extras
            // the user switched on. An explicit choice retries a mic whose recovery was exhausted.
            if bits & hal::MICROPHONE != 0 {
                failed.retain(|key, _| !key.starts_with("mic:"));
            }
            let (chosen, extras) = mics.chosen.lock().map(|m| m.clone()).unwrap_or_default();
            let keep_failure: HashSet<String> =
                chosen.iter().chain(&extras).map(|uid| format!("mic:{uid}")).collect();
            let inputs = candidates
                .iter()
                .filter_map(|c| match &c.kind {
                    crate::SourceKind::InputDevice { name, uid } => Some(crate::InputDevice {
                        uid: uid.clone(),
                        name: name.clone(),
                        built_in: c.built_in,
                    }),
                    crate::SourceKind::Process { .. } => None,
                })
                .collect();
            let mic_state = crate::MicrophoneState::new(inputs, chosen, extras, |d| {
                mic_usable(failed.get(&format!("mic:{}", d.uid)), now)
            });
            let mic_keys: HashSet<String> =
                mic_state.open.iter().map(|uid| format!("mic:{uid}")).collect();
            if published.as_ref() != Some(&mic_state) {
                if let Some(report) = &mics.report {
                    let _ = report.send(mic_state.clone());
                }
                published = Some(mic_state.clone());
            }
            if let Some(p) = pipeline.as_ref() {
                let permitted = |c: &&Candidate| match c.kind {
                    crate::SourceKind::InputDevice { .. } => {
                        grants & 1 != 0 && mic_keys.contains(&c.key)
                    }
                    crate::SourceKind::Process { .. } => grants & 2 != 0,
                };
                let wanted: HashMap<_, _> = candidates.iter().filter(permitted).map(|c| (c.key.clone(), c)).collect();
                let remove: Vec<_> = active
                    .iter()
                    .filter(|(key, a)| {
                        sleeping
                            || bits & (hal::ROUTE | hal::WAKE) != 0
                            || wanted.get(*key).is_none_or(|c| **c != a.candidate)
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in remove {
                    if let Some(mut a) = active.remove(&key) {
                        let id = a.native.raw.info.id.clone();
                        for error in a.native.stop_checked() {
                            send(p, Command::Gap(Some(a.native.raw.info.id.clone()), error));
                        }
                        drop(a);
                        let switched = key.starts_with("mic:")
                            && !mic_keys.contains(&key)
                            && candidates.iter().any(|c| c.key == key);
                        let unrecorded = bits & hal::APPS != 0
                            && key.starts_with("pid:")
                            && !wanted.contains_key(&key);
                        let primary = mic_state.in_use.as_ref().and_then(|uid|mic_state.devices.iter().find(|d| &d.uid==uid));
                        let reason = removal_reason(sleeping, key.starts_with("mic:") && !candidates.iter().any(|c| c.key==key), unrecorded, switched, primary.map(|d|d.name.as_str()));
                        send(p, Command::Remove(id, reason));
                    }
                }
                // A chosen or switched-on mic keeps its exhausted-recovery record while it
                // is skipped, so it is not reopened every pass; a HAL event for it releases it.
                failed.retain(|key, _| wanted.contains_key(key) || keep_failure.contains(key));
                if !sleeping {
                    for candidate in candidates.iter().filter(permitted) {
                        if active.contains_key(&candidate.key) {
                            continue;
                        }
                        let retries = failed.get(&candidate.key).map(|f| f.attempts).unwrap_or(0);
                        if failed
                            .get(&candidate.key)
                            .is_some_and(|f| now < f.retry_ns || f.attempts >= 4)
                        {
                            continue;
                        }
                        // Process-wide: a source opened by a restarted capture gets a new id, so the stopped capture's
                        // late removal of the same device (it stops off the live loop) never removes the new one.
                        let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
                        match tap::NativeSource::create(candidate, generation, p.wake.clone(), wake.clone()) {
                            Ok(mut native) => {
                                // Lifecycle message precedes callbacks even on immediate start.
                                send(p, Command::Add(native.raw.clone()));
                                match native.start() {
                                    Ok(()) => {
                                        failed.remove(&candidate.key);
                                        active.insert(
                                            candidate.key.clone(),
                                            Active {
                                                candidate: candidate.clone(),
                                                native,
                                                started: now,
                                                retries,
                                            },
                                        );
                                    }
                                    Err(e) => {
                                        let id = native.raw.info.id.clone();
                                        for error in native.stop_checked() {
                                            send(p, Command::Gap(Some(id.clone()), error));
                                        }
                                        drop(native);
                                        send(p, Command::Remove(id, "native start failed".into()));
                                        record_failure(&mut failed, candidate, retries, now, &e, p);
                                    }
                                }
                            }
                            Err(e) => record_failure(&mut failed, candidate, retries, now, &e, p),
                        }
                    }
                    let stalled: Vec<_> = active
                        .iter()
                        .filter(|(_, a)| {
                            if let hal::Target::Tap { object, .. } = a.candidate.target {
                                if hal::property::<u32>(object, kAudioProcessPropertyIsRunningOutput, global).unwrap_or(0) == 0 {
                                    return false;
                                }
                            }
                            now.saturating_sub(
                                a.native
                                    .raw
                                    .last_callback
                                    .load(Ordering::Acquire)
                                    .max(a.started),
                            ) > 3_000_000_000
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    for key in stalled {
                        if let Some(mut a) = active.remove(&key) {
                            // A dormant tap is pre-armed for its first output, not
                            // stalled. Recheck activity before bounded recovery.
                            let active_output = match &a.candidate.target {
                                hal::Target::Tap { object, .. } => {
                                    hal::property::<u32>(
                                        *object,
                                        kAudioProcessPropertyIsRunningOutput,
                                        global,
                                    )
                                    .unwrap_or(0)
                                        != 0
                                }
                                hal::Target::Mic(_) => true,
                            };
                            let id = a.native.raw.info.id.clone();
                            let candidate = a.candidate.clone();
                            let retries = a.retries;
                            for error in a.native.stop_checked() {
                                send(p, Command::Gap(Some(a.native.raw.info.id.clone()), error));
                            }
                            drop(a);
                            send(
                                p,
                                Command::Remove(
                                    id,
                                    "no callbacks; reconciling active source".into(),
                                ),
                            );
                            if active_output {
                                record_failure(
                                    &mut failed,
                                    &candidate,
                                    retries,
                                    now,
                                    &anyhow::anyhow!("active source stopped delivering callbacks"),
                                    p,
                                );
                            } else {
                                dirty.fetch_or(hal::DISCOVERY, Ordering::Release);
                            }
                        }
                    }
                }
            }
            // The primary mic's source (its device and ring) is what a voice-processing session may clean. A new
            // source, a switch or sleep publishes a new target (or none), which ends a running session.
            let target = mic_state
                .in_use
                .as_ref()
                .and_then(|uid| active.get(&format!("mic:{uid}")))
                .and_then(|a| {
                    let (hal::Target::Mic(device), crate::SourceKind::InputDevice { uid, name }) =
                        (&a.candidate.target, &a.candidate.kind)
                    else {
                        return None;
                    };
                    Some(EchoTarget {
                        device: *device,
                        name: format!("{name} [{uid}]"),
                        built_in: a.candidate.built_in,
                        rate: a.native.raw.info.format.sample_rate,
                        ring: a.native.echo.clone()?,
                    })
                });
            let changed = match (&published_echo, &target) {
                (Some(a), Some(b)) => !a.same(b),
                (None, None) => false,
                _ => true,
            };
            if changed {
                if let Ok(mut t) = echo.target.lock() {
                    *t = target.clone();
                }
                published_echo = target;
                echo.wake();
            }
            if !announced {
                let _ = ready.send(Ok(()));
                announced = true;
            }
            if dirty.load(Ordering::Acquire) != 0 {
                continue;
            }
            if (discovery_failures > 0 && discovery_failures < 4)
                || (pipeline.is_some()
                    && (active.values().any(|a| matches!(a.candidate.target, hal::Target::Mic(_))
                        || a.native.raw.last_callback.load(Ordering::Acquire) != 0)
                        || failed.values().any(|f| f.attempts < 4)))
            {
                thread::park_timeout(Duration::from_secs(3));
            } else {
                thread::park();
            }
        }
        Ok(())
    })();
    // The unit goes before the sources it feeds (its last diagnostics still reach the archive).
    // Bounded: a unit stuck in a cold Initialize must not hold up shutdown. A worker still busy after 500 ms is
    // detached; it sees `closed` when its call returns and disposes its own unit (the ring and state are Arcs).
    echo.closed.store(true, Ordering::Release);
    echo.wake();
    if let Some(join) = echo_worker {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while !join.is_finished() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if join.is_finished() {
            let _ = join.join();
        } else if let Some(p) = &pipeline {
            send(p, Command::Diagnostic("echo cancellation: worker still busy at shutdown; it disposes its unit when its call returns".into()));
        }
    }
    for (_, mut a) in active.drain() {
        for error in a.native.stop_checked() {
            if let Some(p) = &pipeline {
                send(p, Command::Gap(Some(a.native.raw.info.id.clone()), error));
            }
        }
        drop(a);
    }
    drop(listeners);
    drop(_power);
    if let Some(mut p) = pipeline {
        if let Err(e) = &result {
            send(
                &p,
                Command::Gap(None, format!("HAL control stopped: {e:#}")),
            );
        }
        send(&p, Command::Stop);
        p.close();
    }
    result
}
/// True when the only ROUTE events this pass were Heimdall's own process device list, and that list (voice-processing
/// plumbing left out) is what it was: the echo unit's aggregate came or went. Any other ROUTE-capable selector, a
/// different own list, a new own process object or a possibly overflowed event queue keeps the ROUTE.
fn own_route_noise(
    bits: u32,
    changed: &HashSet<(u32, u32)>,
    overflowed: bool,
    before: &Option<(u32, Vec<String>)>,
    after: &Option<(u32, Vec<String>)>,
) -> bool {
    let (Some((object, was)), Some((now_object, now))) = (before, after) else {
        return false;
    };
    let route_selectors = [
        kAudioProcessPropertyDevices,
        kAudioDevicePropertyNominalSampleRate,
        kAudioDevicePropertyStreams,
        kAudioStreamPropertyVirtualFormat,
        kAudioHardwarePropertyDefaultOutputDevice,
    ];
    let route_events: Vec<_> = changed.iter().filter(|(_, sel)| route_selectors.contains(sel)).collect();
    bits & hal::ROUTE != 0
        && !overflowed
        && object == now_object
        && was == now
        && !route_events.is_empty()
        && route_events
            .iter()
            .all(|(o, sel)| *o == *object && *sel == kAudioProcessPropertyDevices)
}
#[cfg(test)]
mod own_route_tests {
    use super::*;
    #[test]
    fn the_echo_units_aggregate_in_heimdalls_device_list_restarts_nothing() {
        let me = 77;
        let shure = "AppleUSBAudioEngine:Shure Inc:Shure MV7i:2,3".to_string();
        let speakers = "BuiltInSpeakerDevice".to_string();
        // `hal::Snapshot::own_devices` as two snapshots report it (sorted, voice-processing devices already dropped).
        let snap = |object: u32, uids: &[&String]| Some((object, uids.iter().map(|u| u.to_string()).collect()));
        let before = snap(me, &[&shure]);
        // The unit's aggregate joined: the snapshot filters it, so the list is the same and only our list fired.
        let devices_event = HashSet::from([(me, kAudioProcessPropertyDevices)]);
        assert!(own_route_noise(hal::ROUTE, &devices_event, false, &before, &snap(me, &[&shure])));
        // DISCOVERY-only selectors alongside (IsRunning) do not matter.
        let with_running = HashSet::from([(me, kAudioProcessPropertyDevices), (me, kAudioProcessPropertyIsRunning)]);
        assert!(own_route_noise(hal::ROUTE | hal::DISCOVERY, &with_running, false, &before, &snap(me, &[&shure])));
        // A real change in our list (playback opened the speakers) is a route change.
        assert!(!own_route_noise(hal::ROUTE, &devices_event, false, &before, &snap(me, &[&shure, &speakers])));
        // Another ROUTE source in the same pass, another process's list, a lost-event queue, a new process object,
        // or no previous snapshot: the ROUTE stands.
        let default_output = HashSet::from([
            (me, kAudioProcessPropertyDevices),
            (kAudioObjectSystemObject as u32, kAudioHardwarePropertyDefaultOutputDevice),
        ]);
        assert!(!own_route_noise(hal::ROUTE, &default_output, false, &before, &snap(me, &[&shure])));
        let other = HashSet::from([(me, kAudioProcessPropertyDevices), (90, kAudioProcessPropertyDevices)]);
        assert!(!own_route_noise(hal::ROUTE, &other, false, &before, &snap(me, &[&shure])));
        assert!(!own_route_noise(hal::ROUTE, &devices_event, true, &before, &snap(me, &[&shure])));
        assert!(!own_route_noise(hal::ROUTE, &devices_event, false, &before, &snap(78, &[&shure])));
        assert!(!own_route_noise(hal::ROUTE, &devices_event, false, &None, &snap(me, &[&shure])));
        assert!(!own_route_noise(hal::DISCOVERY, &devices_event, false, &before, &snap(me, &[&shure])));
    }
}
/// A microphone is usable unless its recovery is exhausted or waiting for its next attempt. While a chosen mic
/// waits (2, 4, 8, 16 s after each failure), the built-in microphone records instead of nothing; at the retry the
/// chosen one is opened again and, once it delivers, stays.
fn mic_usable(failure: Option<&Failure>, now: u64) -> bool {
    failure.is_none_or(|f| f.attempts < 4 && now >= f.retry_ns)
}
fn record_failure(
    failed: &mut HashMap<String, Failure>,
    candidate: &Candidate,
    retries: u8,
    now: u64,
    error: &anyhow::Error,
    p: &Pipeline,
) {
    let attempts = retries + 1;
    let delay = 1u64 << attempts.min(4);
    failed.insert(
        candidate.key.clone(),
        Failure {
            attempts,
            retry_ns: now + delay * 1_000_000_000,
        },
    );
    send(
        p,
        Command::Diagnostic(
            format!(
                "{}: {error:#}; recovery attempt {attempts}/4{}",
                candidate.key,
                if attempts >= 4 {
                    "; waiting for a HAL activity/route/wake event"
                } else {
                    ""
                }
            ),
        ),
    );
}

/// HAL publishes process objects before their first output. Arm their include-only
/// taps on that event; waiting for IsRunningOutput discards audio before setup.
/// Tap auto-start gates delivery in Core Audio, without an activity polling loop.
/// Only processes `wanted` accepts (recorded apps and Heimdall) are armed: an unrecorded app costs
/// no queue, no tap and no aggregate device.
fn include_dormant_processes(
    snapshot: &mut hal::Snapshot,
    wanted: &dyn Fn(&crate::AppIdentity, u32) -> bool,
) {
    let global = kAudioObjectPropertyScopeGlobal;
    let objects: Vec<_> = snapshot
        .watches
        .iter()
        .filter(|(_, selector, _, _)| *selector == kAudioProcessPropertyIsRunningOutput)
        .map(|(object, _, _, _)| *object)
        .collect();
    for object in objects {
        if snapshot
            .candidates
            .iter()
            .any(|c| matches!(c.target,hal::Target::Tap { object: id, .. } if id == object))
        {
            continue;
        }
        let Ok(pid) = hal::property::<i32>(object, kAudioProcessPropertyPID, global) else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        let Some((birth, path)) = hal::process_identity(pid) else {
            continue;
        };
        let name = std::path::Path::new(&path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let bundle_id = hal::string(object, kAudioProcessPropertyBundleID);
        let owner = hal::owner(&path);
        let app = crate::app_identity(&path, bundle_id.as_deref(), owner.as_ref());
        if !wanted(&app, pid as u32) {
            continue;
        }
        let mut routes = vec![];
        for (device, uid) in hal::route_devices(object) {
            for (index, stream) in hal::list(
                device,
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
            )
            .unwrap_or_default()
            .iter()
            .enumerate()
            {
                snapshot.watches.push((
                    *stream,
                    kAudioStreamPropertyVirtualFormat,
                    global,
                    hal::ROUTE,
                ));
                routes.push((Some(uid.clone()), index as u32));
            }
        }
        if routes.is_empty() {
            routes.push((None, 0));
        }
        for (uid, stream) in routes {
            snapshot.candidates.push(Candidate {
                key: format!(
                    "pid:{pid}:{birth}:{}:{stream}",
                    uid.as_deref().unwrap_or("all")
                ),
                kind: crate::SourceKind::Process {
                    name: name.clone(),
                    bundle_id: bundle_id.clone(),
                    pid: pid as u32,
                    birth_us: birth,
                    owner: owner.clone(),
                    output_devices: uid
                        .iter()
                        .map(|u| format!("{u} / stream {stream}"))
                        .collect(),
                },
                target: hal::Target::Tap {
                    object,
                    device_uid: uid,
                    stream,
                },
                format: None,
                encoding: None,
                built_in: false,
                app: Some(app.clone()),
            });
        }
    }
}

/// Heimdall's own output is archived for its speech context whatever App Audio says.
fn own_process(pid: u32, app: &crate::AppIdentity, own_pid: u32) -> bool {
    pid == own_pid || app.key == "dev.heimdall.app"
}

/// `hal::snapshot` lists only processes whose output is running; their objects are the apps playing now.
fn playing_objects(snapshot: &hal::Snapshot) -> HashSet<u32> {
    snapshot
        .candidates
        .iter()
        .filter_map(|c| match c.target {
            hal::Target::Tap { object, .. } => Some(object),
            hal::Target::Mic(_) => None,
        })
        .collect()
}

/// App Audio's filter at discovery: process candidates survive only for recorded apps and Heimdall itself,
/// so nothing else is ever tapped. Returns the apps playing now (Heimdall excluded), sorted, for the menu.
fn keep_recorded(
    snapshot: &mut hal::Snapshot,
    playing: &HashSet<u32>,
    own_pid: u32,
    recorded: &HashSet<String>,
) -> Vec<crate::AppIdentity> {
    let mut apps = std::collections::BTreeSet::new();
    snapshot.candidates.retain(|c| {
        let (crate::SourceKind::Process { pid, .. }, Some(app), hal::Target::Tap { object, .. }) =
            (&c.kind, &c.app, &c.target)
        else {
            return true;
        };
        let mine = own_process(*pid, app, own_pid);
        if !mine && playing.contains(object) {
            apps.insert(app.clone());
        }
        mine || recorded.contains(&app.key)
    });
    apps.into_iter().collect()
}

#[cfg(test)]
mod app_audio_tests {
    use super::*;
    fn process(object: u32, pid: u32, key: &str, name: &str) -> Candidate {
        Candidate {
            key: format!("pid:{pid}:1:all:0"),
            kind: crate::SourceKind::Process {
                name: name.into(),
                bundle_id: None,
                pid,
                birth_us: 1,
                owner: None,
                output_devices: vec![],
            },
            target: hal::Target::Tap { object, device_uid: None, stream: 0 },
            format: None,
            encoding: None,
            built_in: false,
            app: Some(crate::AppIdentity { key: key.into(), name: name.into(), path: format!("/Applications/{name}.app") }),
        }
    }
    fn mic() -> Candidate {
        Candidate {
            key: "mic:BuiltIn".into(),
            kind: crate::SourceKind::InputDevice { name: "MacBook Pro Microphone".into(), uid: "BuiltIn".into() },
            target: hal::Target::Mic(1),
            format: None,
            encoding: None,
            built_in: true,
            app: None,
        }
    }
    fn snapshot() -> hal::Snapshot {
        hal::Snapshot {
            candidates: vec![
                mic(),
                process(10, 100, "com.hnc.Discord", "Discord"),
                process(11, 101, "com.hnc.Discord", "Discord"), // a helper, same app
                process(12, 102, "com.spotify.client", "Spotify"),
                process(13, 103, "com.google.Chrome", "Chrome"),
                process(14, 999, "/tmp/heimdall", "heimdall"), // Heimdall itself (unbundled lab binary)
                process(15, 104, "dev.heimdall.app", "Heimdall"), // Heimdall's bundle
            ],
            ..Default::default()
        }
    }
    fn kept(s: &hal::Snapshot) -> Vec<String> {
        s.candidates.iter().map(|c| c.key.clone()).collect()
    }

    #[test]
    fn default_records_no_app_but_keeps_mic_and_heimdall() {
        let mut s = snapshot();
        let playing = HashSet::from([10, 12, 14]);
        let apps = keep_recorded(&mut s, &playing, 999, &HashSet::new());
        assert_eq!(kept(&s), ["mic:BuiltIn", "pid:999:1:all:0", "pid:104:1:all:0"]);
        // Playing apps are listed (sorted, one row per app), Heimdall never.
        let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Discord", "Spotify"]);
    }

    #[test]
    fn a_recorded_app_keeps_every_process_dormant_or_playing() {
        let mut s = snapshot();
        let recorded = HashSet::from(["com.hnc.Discord".to_string()]);
        let apps = keep_recorded(&mut s, &HashSet::from([13]), 999, &recorded);
        assert_eq!(
            kept(&s),
            ["mic:BuiltIn", "pid:100:1:all:0", "pid:101:1:all:0", "pid:999:1:all:0", "pid:104:1:all:0"]
        );
        assert_eq!(apps.iter().map(|a| a.key.as_str()).collect::<Vec<_>>(), ["com.google.Chrome"]);
    }

    #[test]
    fn nothing_playing_lists_nothing() {
        let mut s = snapshot();
        assert!(keep_recorded(&mut s, &HashSet::new(), 999, &HashSet::new()).is_empty());
    }
}

fn removal_reason(sleeping:bool, disconnected:bool, unrecorded:bool, switched:bool, primary:Option<&str>) -> String {
    if sleeping {"system sleep".into()}
    else if disconnected {"input device disconnected".into()}
    else if unrecorded {"app audio switched off".into()}
    else if switched {format!("microphone switched off (primary: {})",primary.unwrap_or("none"))}
    else {"source stopped / route or format changed".into()}
}
#[cfg(test)]
mod fallback_tests {
    use super::{mic_usable, Failure};
    use crate::{InputDevice, MicrophoneState};
    #[test]
    fn a_failing_chosen_mic_falls_back_to_built_in_between_retries() {
        let devices = vec![
            InputDevice { uid: "BuiltIn".into(), name: "MacBook Pro Microphone".into(), built_in: true },
            InputDevice { uid: "AirPods".into(), name: "AirPods Pro".into(), built_in: false },
        ];
        let state = |failure: Option<&Failure>, now: u64| {
            MicrophoneState::new(devices.clone(), Some("AirPods".into()), vec![], |d| {
                mic_usable(if d.uid == "AirPods" { failure } else { None }, now)
            })
            .in_use
        };
        assert_eq!(state(None, 0).as_deref(), Some("AirPods"));
        // First failure: retry in 2 s. Meanwhile the built-in records (before: nothing until the chosen recovered or
        // gave up after four attempts, up to ~40 s of silence).
        let first = Failure { attempts: 1, retry_ns: 2_000_000_000 };
        assert_eq!(state(Some(&first), 1_000_000_000).as_deref(), Some("BuiltIn"));
        assert_eq!(state(Some(&first), 2_000_000_000).as_deref(), Some("AirPods"));
        // Exhausted: built-in until a HAL event or the user's choice releases it.
        let exhausted = Failure { attempts: 4, retry_ns: 0 };
        assert_eq!(state(Some(&exhausted), 99_000_000_000).as_deref(), Some("BuiltIn"));
    }
}
#[cfg(test)]
mod disconnection_regression {
    #[test]
    fn physical_removal_is_distinct_from_route_changes_and_chosen_switches() {
        assert_eq!(super::removal_reason(false,true,false,false,Some("Built-in")),"input device disconnected");
        assert_eq!(super::removal_reason(false,false,false,true,Some("Built-in")),"microphone switched off (primary: Built-in)");
        assert_eq!(super::removal_reason(false,false,false,false,None),"source stopped / route or format changed");
        assert_eq!(super::removal_reason(true,true,false,false,None),"system sleep");
    }
}
