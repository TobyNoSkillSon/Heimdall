//! Jarvis gives way to Phonon S when memory runs short (bug note 2026-11-03T22-38-44).
//!
//! "whenever the model needs more RAM or is just not enough, then you should switch to a Phonon
//! model for speaking... you should absolutely by yourself decide to switch to a much lighter speaking model... so
//! that we can lift like 20 GB from RAM." His standing rule keeps the whole Mac under 95 GB in use.
//!
//! The policy (`Policy::decide`) is pure: a sample of memory in use, the memory jobs have declared they are about to
//! take, and the native pressure level go in; whether Jarvis stays, gives way, stays away or comes back comes out.
//! - Jarvis gives way when memory in use plus the declared need would pass `LINE`, or on native WARN/CRITICAL.
//! - He comes back only when memory in use plus the need plus his own size plus `RETURN_MARGIN` fits under the line,
//!   the sidecar's own admission would let him load (`ADMISSION` free + inactive), pressure is normal, and that has
//!   held for the hold (hysteresis: no reload that would at once unload again), and only between replies: nothing
//!   spoken for `QUIET`.
//! - His size is what memory actually freed when he last gave way (never under `JARVIS_BYTES`), and the hold doubles
//!   from `RETURN_HOLD` for each hold that follows a return within `CALM`, up to `HOLD_MAX` (review 4 Nov: assuming
//!   24 GiB for a ~29 GB Jarvis, he flip-flopped with Phonon S about once a minute).
//!
//! Jobs declare a need with a JSON file in `<Heimdall data>/memory-needs/` (`Needs`, docs/drama3-integration.md).
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GIB: u64 = 1 << 30;
/// The user's line: the whole Mac stays under 95 GB in use (Activity Monitor's "Memory Used", which counts GiB).
pub const LINE: u64 = 95 * GIB;
/// What Jarvis takes when he loads: about 22 GB measured resident (bug note 2026-11-03T22-38-44, the 4 Nov 02:18 lag
/// sample) plus the 2 GB word-check decoder, rounded up. The sidecar's own admission still books its 29 GB against
/// free memory (`crates/drama-engine/src/memory.rs` `RESIDENT`).
pub const JARVIS_BYTES: u64 = 24 * GIB;
/// The most a give-way's measured release counts as his size: memory other apps freed in the same seconds must not
/// hold him out for good.
pub const JARVIS_MAX: u64 = 36 * GIB;
/// A give-way's release is measured over this long after he stops being resident (the worker is reaped in seconds).
pub const FREED_WINDOW: Duration = Duration::from_secs(15);
/// The sidecar's own admission (`crates/drama-engine/src/memory.rs` `refusal`): free + inactive must be at least
/// `RESIDENT` (27 GB + 2 GB word check) plus `MARGIN` (2 GiB), or it exits 73. A return it would refuse is no return.
pub const ADMISSION: u64 = 27_000_000_000 + 2_000_000_000 + 2 * GIB;
/// Room left under the line after Jarvis is back, so the next small allocation does not unload him again.
pub const RETURN_MARGIN: u64 = 4 * GIB;
/// Memory must stay back this long before Jarvis reloads, the first time.
pub const RETURN_HOLD: Duration = Duration::from_secs(60);
/// A hold that starts within this long of his last return doubles the one before; after it, the hold is a minute again.
pub const CALM: Duration = Duration::from_secs(15 * 60);
/// The longest hold.
pub const HOLD_MAX: Duration = Duration::from_secs(16 * 60);
/// Jarvis reloads between replies only: nothing spoken for this long (a reply's lines follow each other closely).
pub const QUIET: Duration = Duration::from_secs(3);
/// The policy samples at most this often (the app's tick runs on every capture event).
pub const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// `kern.memorystatus_vm_pressure_level` bits.
pub const WARN: i64 = 2;
pub const CRITICAL: i64 = 4;
/// A need file without `until` lasts this long from its last write; no need file lasts longer than `NEED_MAX`.
pub const NEED_DEFAULT: Duration = Duration::from_secs(2 * 3600);
pub const NEED_MAX: Duration = Duration::from_secs(12 * 3600);

/// One reading of the Mac.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    /// Memory in use, Activity Monitor's way: app memory + wired + compressed.
    pub used: u64,
    /// The sum of the live needs jobs declared (bytes they are about to take).
    pub need: u64,
    /// Who declared them, for the log ("Arsenal locomotive restore").
    pub why: Vec<String>,
    pub pressure: i64,
    /// Free + inactive, as the sidecar's admission counts it; None when not read (fakes).
    pub available: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Nothing to do: Jarvis stays (or, not held for memory, loads as usual).
    Stay,
    /// Unload Jarvis now, for this reason; Phonon S speaks meanwhile.
    GiveWay(String),
    /// Jarvis stays unloaded for memory.
    Hold,
    /// Memory is back and has stayed back: reload Jarvis.
    Return,
}

/// GB as Activity Monitor shows them (GiB), the unit of the 95 GB line, in every reason and log line.
pub fn gb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / GIB as f64)
}

/// Why `sample` says Jarvis must go, if it does.
pub fn over_line(sample: &Sample) -> Option<String> {
    if sample.pressure & (WARN | CRITICAL) != 0 {
        let level = if sample.pressure & CRITICAL != 0 { "critical" } else { "warning" };
        return Some(format!("System memory pressure ({level}). Jarvis unloaded to free memory; Phonon S speaks."));
    }
    if sample.used.saturating_add(sample.need) > LINE {
        let need = if sample.need > 0 {
            let why = if sample.why.is_empty() { String::new() } else { format!(" for {}", sample.why.join(", ")) };
            format!(" plus {} GB needed{why}", gb(sample.need))
        } else {
            String::new()
        };
        return Some(format!("Memory would pass 95 GB ({} GB in use{need}). Jarvis unloaded to free memory; Phonon S speaks.",
            gb(sample.used)));
    }
    None
}

/// Why loading Jarvis (`jarvis` bytes) now would itself pass the line, if it would.
pub fn no_room(sample: &Sample, jarvis: u64) -> Option<String> {
    let after = sample.used.saturating_add(sample.need).saturating_add(jarvis);
    (after > LINE).then(|| format!("Not enough room under 95 GB for Jarvis ({} GB in use{}). Phonon S speaks.", gb(sample.used),
        if sample.need > 0 { format!(" plus {} GB needed", gb(sample.need)) } else { String::new() }))
}

/// Memory is back with room for Jarvis (`jarvis` bytes) and the margin, the sidecar would admit him, at normal
/// pressure.
pub fn back(sample: &Sample, jarvis: u64) -> bool {
    sample.pressure & (WARN | CRITICAL) == 0
        && sample.used.saturating_add(sample.need).saturating_add(jarvis).saturating_add(RETURN_MARGIN) <= LINE
        && sample.available.is_none_or(|free| free >= ADMISSION)
}

#[derive(Debug, Default)]
pub struct Policy {
    /// Why Jarvis is held out for memory.
    held: Option<String>,
    /// Since when memory has been back without a break.
    back_since: Option<Instant>,
    /// Holds that each began within `CALM` of his last return: the hold is `RETURN_HOLD` doubled this many times.
    strikes: u32,
    /// When he was last allowed back.
    returned_at: Option<Instant>,
    /// Memory in use at the last reading while he was resident.
    resident_used: Option<u64>,
    /// He stopped being resident while held: memory in use just before, and when.
    unloaded: Option<(u64, Instant)>,
    /// The most memory a give-way freed in this run of holds (reset with the strikes).
    freed: u64,
}

impl Policy {
    /// `resident`: Jarvis is loaded (or still finishing a line before he gives way). `quiet_for`: how long nothing
    /// has been spoken; None while a line is being spoken.
    pub fn decide(&mut self, sample: &Sample, resident: bool, quiet_for: Option<Duration>, now: Instant) -> Action {
        self.measure(sample, resident, now);
        if resident {
            if let Some(reason) = over_line(sample) {
                self.hold(reason.clone(), now);
                return Action::GiveWay(reason);
            }
            // Resident while held: the give-way is still waiting for his line to end.
            if let Some(reason) = &self.held {
                return Action::GiveWay(reason.clone());
            }
            return Action::Stay;
        }
        if self.held.is_none() {
            // Not loaded and not held: he loads as usual, unless loading him would itself pass the line (or pressure
            // is up), which would only unload him again.
            match over_line(sample).or_else(|| no_room(sample, self.jarvis_bytes())) {
                Some(reason) => self.hold(reason, now),
                None => return Action::Stay,
            }
        }
        if !back(sample, self.jarvis_bytes()) {
            self.back_since = None;
            return Action::Hold;
        }
        let since = *self.back_since.get_or_insert(now);
        if now.duration_since(since) >= self.hold_for() && quiet_for.is_some_and(|q| q >= QUIET) {
            self.held = None;
            self.back_since = None;
            self.returned_at = Some(now);
            return Action::Return;
        }
        Action::Hold
    }
    /// Jarvis left for memory (a give-way, the sidecar's own guard, or a refusal at load): he stays away until memory
    /// is back. A new hold within `CALM` of his last return doubles the hold; after a calm period it is a minute again.
    pub fn hold(&mut self, reason: String, now: Instant) {
        if self.held.is_none() {
            self.back_since = None;
            if self.returned_at.is_some_and(|at| now.duration_since(at) < CALM) {
                self.strikes = self.strikes.saturating_add(1);
            } else {
                self.strikes = 0;
                self.freed = 0;
            }
        }
        self.held = Some(reason);
    }
    pub fn held(&self) -> Option<&str> {
        self.held.as_deref()
    }
    /// How long memory must stay back before he returns from this hold.
    pub fn hold_for(&self) -> Duration {
        RETURN_HOLD.saturating_mul(1u32 << self.strikes.min(10)).min(HOLD_MAX)
    }
    /// What Jarvis is taken to need: the most a give-way freed lately, never under `JARVIS_BYTES`.
    pub fn jarvis_bytes(&self) -> u64 {
        self.freed.max(JARVIS_BYTES)
    }
    /// Memory actually freed when he stops being resident while held: in use just before, less the lowest reading in
    /// the `FREED_WINDOW` after.
    fn measure(&mut self, sample: &Sample, resident: bool, now: Instant) {
        if resident {
            self.resident_used = Some(sample.used);
            self.unloaded = None;
            return;
        }
        if let Some(before) = self.resident_used.take() {
            if self.held.is_some() {
                self.unloaded = Some((before, now));
            }
        }
        if let Some((before, at)) = self.unloaded {
            if now.duration_since(at) > FREED_WINDOW {
                self.unloaded = None;
            } else {
                self.freed = self.freed.max(before.saturating_sub(sample.used).min(JARVIS_MAX));
            }
        }
    }
}

/// Where readings come from; tests inject fakes.
pub trait Source: Send {
    fn sample(&mut self) -> Result<Sample>;
}

/// The policy with its source, sampled at most every `SAMPLE_EVERY`.
pub struct Gate {
    source: Box<dyn Source>,
    pub policy: Policy,
    sampled: Option<(Instant, Sample)>,
    /// A failed reading is logged once; the sidecar's own guard still protects memory (it fails closed).
    pub failing: Option<String>,
    clock: Box<dyn Fn() -> Instant + Send>,
}

impl Gate {
    pub fn new(source: Box<dyn Source>) -> Self {
        Self { source, policy: Policy::default(), sampled: None, failing: None, clock: Box::new(Instant::now) }
    }
    /// Tests: the time the gate's holds are counted in.
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> Instant + Send>) -> Self {
        self.clock = clock;
        self
    }
    pub fn now(&self) -> Instant {
        (self.clock)()
    }
    /// The app's: native readings and the needs folder in Heimdall's data directory.
    pub fn native(data: &Path) -> Self {
        Self::new(Box::new(Native { needs: Needs::new(data.join(NEEDS_DIR)) }))
    }
    pub fn tick(&mut self, resident: bool, quiet_for: Option<Duration>, now: Instant) -> Action {
        let fresh = self.sampled.as_ref().is_some_and(|(at, _)| now.duration_since(*at) < SAMPLE_EVERY);
        if !fresh {
            match self.source.sample() {
                Ok(sample) => {
                    self.failing = None;
                    self.sampled = Some((now, sample));
                }
                Err(error) => {
                    // Fail open: a reading that cannot be taken keeps Jarvis as he is; never a reload on no data.
                    self.failing = Some(format!("{error:#}"));
                    self.sampled = None;
                    return if self.policy.held().is_some() { Action::Hold } else { Action::Stay };
                }
            }
        }
        let sample = self.sampled.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
        self.policy.decide(&sample, resident, quiet_for, now)
    }
    /// The last reading, for the log.
    pub fn last(&self) -> Option<&Sample> {
        self.sampled.as_ref().map(|(_, s)| s)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Declared needs.

/// The folder in Heimdall's data directory where jobs declare memory they are about to take.
pub const NEEDS_DIR: &str = "memory-needs";

/// One declaration: `<name>.json` with `{"gb": 40, "why": "Arsenal locomotive restore", "until": <unix seconds>}`
/// (`bytes` instead of `gb` also works). It counts until `until`, or `NEED_DEFAULT` after its last write, and never
/// longer than `NEED_MAX` after it; delete the file when the job ends. Malformed files are ignored.
pub struct Needs {
    pub dir: PathBuf,
}

impl Needs {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    /// The live needs: (bytes, why).
    pub fn live(&self, now: SystemTime) -> Vec<(u64, String)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(written) = entry.metadata().and_then(|m| m.modified()) else { continue };
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if let Some(need) = parse_need(&text, written, now) {
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("a job").to_owned();
                out.push((need.0, need.1.unwrap_or(name)));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }
}

/// One need file's (bytes, why) while it is live at `now`.
pub fn parse_need(text: &str, written: SystemTime, now: SystemTime) -> Option<(u64, Option<String>)> {
    let value: Value = serde_json::from_str(text).ok()?;
    let bytes = match (value["bytes"].as_u64(), value["gb"].as_f64()) {
        (Some(bytes), _) => bytes,
        // GB as Activity Monitor and the 95 GB line count them (GiB).
        (None, Some(gb)) if gb.is_finite() && gb > 0. => (gb * GIB as f64) as u64,
        _ => return None,
    };
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.);
    let cap = secs(written) + NEED_MAX.as_secs_f64();
    let until = value["until"].as_f64().unwrap_or(secs(written) + NEED_DEFAULT.as_secs_f64()).min(cap);
    if bytes == 0 || secs(now) >= until {
        return None;
    }
    let why = value["why"].as_str().map(|s| s.chars().take(80).collect());
    Some((bytes, why))
}

// ---------------------------------------------------------------------------------------------------------------
// Native readings.

#[allow(non_upper_case_globals)]
extern "C" {
    fn mach_host_self() -> u32;
    static mach_task_self_: u32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

const HOST_VM_INFO64: i32 = 4;
/// `vm_statistics64` through `internal_page_count` (mach/vm_statistics.h).
const VM_BYTES: usize = 144;
const VM_FREE: usize = 0;
const VM_INACTIVE: usize = 8;
const VM_WIRE: usize = 12;
const VM_PURGEABLE: usize = 88;
const VM_COMPRESSOR: usize = 128;
const VM_INTERNAL: usize = 140;

/// Activity Monitor's "Memory Used" from a `host_statistics64(HOST_VM_INFO64)` result: app memory (internal pages
/// less purgeable ones) + wired + compressed, in bytes.
pub fn used_from(status: i32, raw: &[i32], count: u32, page: u64) -> Result<u64> {
    if status != 0 || (count as usize) * 4 < VM_BYTES {
        bail!("host_statistics64(HOST_VM_INFO64) failed");
    }
    let at = |offset: usize| raw[offset / 4] as u32 as u64;
    let app = at(VM_INTERNAL).saturating_sub(at(VM_PURGEABLE));
    Ok((app + at(VM_WIRE) + at(VM_COMPRESSOR)) * page)
}

/// Free + inactive pages in bytes, as the sidecar's admission counts available memory (psutil's macOS `available`,
/// `crates/drama-engine/src/memory.rs` `available_from`).
pub fn available_from(status: i32, raw: &[i32], count: u32, page: u64) -> Result<u64> {
    if status != 0 || (count as usize) * 4 < VM_INACTIVE + 4 {
        bail!("host_statistics64(HOST_VM_INFO64) failed");
    }
    let at = |offset: usize| raw[offset / 4] as u32 as u64;
    Ok((at(VM_FREE) + at(VM_INACTIVE)) * page)
}

/// Memory in use and available (free + inactive), from one reading.
pub fn vm_bytes() -> Result<(u64, u64)> {
    let mut raw = [0i32; 64];
    let mut count = raw.len() as u32;
    let host = unsafe { mach_host_self() };
    let status = unsafe { host_statistics64(host, HOST_VM_INFO64, raw.as_mut_ptr(), &mut count) };
    unsafe { mach_port_deallocate(mach_task_self_, host) };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Ok((used_from(status, &raw, count, page)?, available_from(status, &raw, count, page)?))
}

pub fn used_bytes() -> Result<u64> {
    Ok(vm_bytes()?.0)
}

pub fn pressure() -> Result<i64> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let failed = unsafe {
        libc::sysctlbyname(c"kern.memorystatus_vm_pressure_level".as_ptr(), &mut value as *mut _ as *mut libc::c_void,
            &mut size, std::ptr::null_mut(), 0)
    };
    if failed != 0 {
        bail!("Cannot read system memory pressure");
    }
    Ok(value as i64)
}

pub struct Native {
    pub needs: Needs,
}

impl Source for Native {
    fn sample(&mut self) -> Result<Sample> {
        let needs = self.needs.live(SystemTime::now());
        let (used, available) = vm_bytes()?;
        Ok(Sample {
            used,
            need: needs.iter().map(|(b, _)| *b).sum(),
            why: needs.into_iter().map(|(_, w)| w).collect(),
            pressure: pressure()?,
            available: Some(available),
        })
    }
}

#[cfg(test)]
#[path = "memory_line_tests.rs"]
mod tests;
