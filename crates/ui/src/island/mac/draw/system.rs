//! The system profile: physical facts about this Mac that macOS does not hand out cheaply or at all, measured once
//! per build (the first launch after an install) and read from one file afterwards ("whenever we
//! install Heimdall … it's just doing some kind of system check so that later we can very easily pull this
//! information from Heimdall itself … if anything else needs some kind of system information, then it should take
//! it from that place"). Every component reads system facts through `current()`; nothing re-measures when it opens.
//! A display the profile has not seen (a new monitor) is measured once and added.
//!
//! Only physical facts are stored (millimetres, notch presence). Values that follow the display settings (points per
//! millimetre, the notch's size in points) are read live from NSScreen, which costs nothing.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2_app_kit::NSScreen;
use objc2_core_graphics::{CGDisplayIsBuiltin, CGDisplayModelNumber, CGDisplayScreenSize, CGDisplaySerialNumber, CGDisplayVendorNumber};
use objc2_foundation::MainThreadMarker;
use serde::{Deserialize, Serialize};

/// The rounded top corners of the notched MacBook panels (MacBook Pro 14/16 from 2021, MacBook Air from M2), in
/// millimetres of glass. ESTIMATED: Apple publishes no figure and macOS reports none (4 mm ≈ 20 pt on a 16-inch at
/// the default scaling); confirm on the glass. It is the profile's one estimated value and the only place it lives.
/// Displays without a notch (external monitors, older MacBooks) have square corners.
pub(crate) const NOTCHED_PANEL_CORNER_MM: f64 = 4.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DisplayProfile {
    /// vendor-model-serial, stable across reconnects and display-ID changes.
    pub key: String,
    pub builtin: bool,
    pub notch: bool,
    pub width_mm: f64,
    pub height_mm: f64,
    /// Radius of the top corners of the visible glass; 0 when square. Bottom corners are square on every Mac panel.
    pub top_corner_mm: f64,
    /// `top_corner_mm` is an estimate, not a measurement.
    #[serde(default)]
    pub top_corner_estimated: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemProfile {
    /// The build that measured it: a new build measures again on its first launch.
    pub build: String,
    pub model: String,
    pub displays: Vec<DisplayProfile>,
}

thread_local! {
    static CURRENT: RefCell<Option<(PathBuf, SystemProfile)>> = const { RefCell::new(None) };
}

/// This build's identity: the version and the executable's modification time, so each install measures once.
pub(crate) fn build_id() -> String {
    let stamp = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    format!("{}-{stamp}", env!("CARGO_PKG_VERSION"))
}

pub fn display_key(id: u32) -> String {
    format!("{:x}-{:x}-{:x}", CGDisplayVendorNumber(id), CGDisplayModelNumber(id), CGDisplaySerialNumber(id))
}

fn model() -> String {
    let mut size = 0usize;
    let name = c"hw.model";
    unsafe {
        if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) != 0 || size == 0 {
            return String::new();
        }
        let mut buffer = vec![0u8; size];
        libc::sysctlbyname(name.as_ptr(), buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0);
        String::from_utf8_lossy(&buffer).trim_end_matches('\0').to_string()
    }
}

/// Measures one display.
pub fn measure(screen: &NSScreen) -> DisplayProfile {
    let id = screen.CGDirectDisplayID();
    let builtin = CGDisplayIsBuiltin(id);
    let notch = screen.safeAreaInsets().top > 0.0;
    let mm = CGDisplayScreenSize(id);
    let rounded = builtin && notch;
    DisplayProfile {
        key: display_key(id),
        builtin,
        notch,
        width_mm: mm.width,
        height_mm: mm.height,
        top_corner_mm: if rounded { NOTCHED_PANEL_CORNER_MM } else { 0.0 },
        top_corner_estimated: rounded,
    }
}

/// Reads the profile at `path`; a profile from another build is measured again, and any connected display it does
/// not know is measured and added. Writes it back when it changed.
pub fn load_or_measure(path: &Path, build: &str, mtm: MainThreadMarker) -> SystemProfile {
    let stored: Option<SystemProfile> = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let mut profile = stored.clone().filter(|p| p.build == build).unwrap_or_default();
    if profile.build.is_empty() {
        profile = SystemProfile { build: build.into(), model: model(), displays: vec![] };
    }
    for screen in NSScreen::screens(mtm).iter() {
        let key = display_key(screen.CGDirectDisplayID());
        if !profile.displays.iter().any(|d| d.key == key) {
            profile.displays.push(measure(&screen));
        }
    }
    if stored.as_ref() != Some(&profile) {
        save(path, &profile);
    }
    profile
}

fn save(path: &Path, profile: &SystemProfile) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(json) = serde_json::to_vec_pretty(profile) {
        let tmp = path.with_extension("json.part");
        if std::fs::write(&tmp, json).is_ok() {
            std::fs::rename(&tmp, path).ok();
        }
    }
}

/// Loads (or measures) the profile once at launch; `current` reads it from memory afterwards.
pub(crate) fn install(path: &Path, mtm: MainThreadMarker) {
    let profile = load_or_measure(path, &build_id(), mtm);
    CURRENT.with(|c| *c.borrow_mut() = Some((path.to_path_buf(), profile)));
}

/// The profile in memory. A display it has not seen is measured once now and saved; without an installed profile
/// (renders) the screen is measured on the spot and nothing is written.
pub(crate) fn current(screen: &NSScreen) -> DisplayProfile {
    let key = display_key(screen.CGDirectDisplayID());
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let Some((path, profile)) = c.as_mut() else { return measure(screen) };
        if let Some(d) = profile.displays.iter().find(|d| d.key == key) {
            return d.clone();
        }
        let d = measure(screen);
        profile.displays.push(d.clone());
        save(path, profile);
        d
    })
}

/// The top-corner radius on `screen` in its current points (follows "More Space" and other scalings).
pub(crate) fn top_corner_points(screen: &NSScreen) -> f64 {
    let d = current(screen);
    if d.top_corner_mm > 0.0 && d.width_mm > 0.0 { d.top_corner_mm * screen.frame().size.width / d.width_mm } else { 0.0 }
}
