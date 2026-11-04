//! The decider the app runs: which trained version (`DeciderModel`, "Install V6", V6 by default with V5
//! kept as the rollback) in which precision ("We definitely want to run the 4-bit… choose which
//! quantization to run the decider in… so we can hot-swap"). Both versions ship in all three precisions side by side in
//! the bundle (`scripts/models.lock.json`), so a swap never downloads. A `Pick` is one of the six folders.
//!
//! The app holds one decider instance (the send worker's d1 owner, `app::pipeline::send_worker`): the memory shown per
//! precision is one instance's. A swap destroys the old instance before the new one loads, so the two never overlap.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 16-bit (BF16, the trained checkpoint), 8-bit or 4-bit (MLX affine group-64 quantization of the text trunk's linear
/// layers; embedding and head stay BF16).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Precision {
    #[serde(rename = "16bit")]
    Bf16,
    #[serde(rename = "8bit")]
    Int8,
    /// The default.
    #[default]
    #[serde(rename = "4bit")]
    Int4,
}

/// Measured bytes on disk of each folder (the sum of its files in `scripts/models.lock.json`): V6 (4 Nov), V5 (30 Oct).
/// V6's calibration.json is 110 bytes longer; the weights are the same size (same architecture). V6 8-bit's
/// config.json has no Python `auto_map` (110 bytes shorter), so that folder matches V5 8-bit.
const DISK: [[u64; 3]; 2] = [[3_854_334_716, 2_100_954_734, 1_367_819_592], [3_854_334_606, 2_100_954_734, 1_367_819_482]];
// Measured in the Rust engine (30 Oct, crates/decide/engine-v5/README.md) on V5: MLX active memory of one instance after
// load, and its peak during a ~1,850-token decision. V6 has V5's architecture and tensor sizes (its night pass measured
// the same peaks for both in Python, runs/v6/report.json), so the same figures hold.
const RESIDENT: [u64; 3] = [3_460_000_000, 2_080_000_000, 1_350_000_000];
const PEAK: [u64; 3] = [4_160_000_000, 3_080_000_000, 2_350_000_000];

impl Precision {
    /// Largest first, as the menu and Setup list them.
    pub const ALL: [Precision; 3] = [Precision::Bf16, Precision::Int8, Precision::Int4];
    pub const DEFAULT: Precision = Precision::Int4;

    fn index(self) -> usize {
        self as usize
    }
    /// "16-bit", "8-bit", "4-bit".
    pub fn label(self) -> &'static str {
        ["16-bit", "8-bit", "4-bit"][self.index()]
    }
    /// Its name in settings files and actions.
    pub fn key(self) -> &'static str {
        ["16bit", "8bit", "4bit"][self.index()]
    }
    pub fn from_key(key: &str) -> Option<Precision> {
        Precision::ALL.into_iter().find(|p| p.key() == key)
    }
    /// Resident weights of one loaded instance (measured).
    pub fn resident_bytes(self) -> u64 {
        RESIDENT[self.index()]
    }
    /// Peak memory of one instance while it decides (measured): what the menu and Setup show as its memory.
    pub fn memory_bytes(self) -> u64 {
        PEAK[self.index()]
    }
    /// The others, nearest first: by memory, so a missing 8-bit falls to the 4-bit before the 16-bit.
    fn nearest(self) -> Vec<Precision> {
        let mut others: Vec<Precision> = Precision::ALL.into_iter().filter(|p| *p != self).collect();
        others.sort_by_key(|p| p.memory_bytes().abs_diff(self.memory_bytes()));
        others
    }
}

/// Which trained decider runs ("Install V6"): V6, the default, or V5, kept in the bundle as the rollback.
/// Its name in settings files is `decider_model` ("v6", "v5").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DeciderModel {
    #[default]
    #[serde(rename = "v6")]
    V6,
    #[serde(rename = "v5")]
    V5,
}

impl DeciderModel {
    /// Newest first: the order a missing one falls back in.
    pub const ALL: [DeciderModel; 2] = [DeciderModel::V6, DeciderModel::V5];
    pub const DEFAULT: DeciderModel = DeciderModel::V6;

    fn index(self) -> usize {
        self as usize
    }
    /// Its name in settings files: "v6", "v5".
    pub fn key(self) -> &'static str {
        ["v6", "v5"][self.index()]
    }
    pub fn from_key(key: &str) -> Option<DeciderModel> {
        DeciderModel::ALL.into_iter().find(|m| m.key() == key)
    }
    /// "Decider V6", "Decider V5".
    pub fn label(self) -> &'static str {
        ["Decider V6", "Decider V5"][self.index()]
    }
}

/// One decider folder: a version in a precision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pick {
    pub model: DeciderModel,
    pub precision: Precision,
}

impl Pick {
    /// V6 in 4-bit.
    pub const DEFAULT: Pick = Pick { model: DeciderModel::DEFAULT, precision: Precision::DEFAULT };

    pub const fn new(model: DeciderModel, precision: Precision) -> Self {
        Self { model, precision }
    }
    /// The checkpoint folder, relative to the decide models folder.
    pub fn folder(self) -> &'static str {
        const FOLDERS: [[&str; 3]; 2] = [
            ["d1/d1-2B-heimdall-v6", "d1/d1-2B-heimdall-v6-8bit", "d1/d1-2B-heimdall-v6-4bit"],
            ["d1/d1-2B-heimdall-v5", "d1/d1-2B-heimdall-v5-8bit", "d1/d1-2B-heimdall-v5-4bit"],
        ];
        FOLDERS[self.model.index()][self.precision.index()]
    }
    /// The folder's own name: "d1-2B-heimdall-v6-4bit".
    pub fn name(self) -> &'static str {
        self.folder().rsplit('/').next().unwrap_or_default()
    }
    /// "V6 4-bit": what the menu's swap reasons name.
    pub fn label(self) -> String {
        format!("{} {}", self.model.label().trim_start_matches("Decider "), self.precision.label())
    }
    /// The decider as the log names it once it loads: "d1-2B-heimdall-v6-4bit (Decider V6, 4-bit)".
    pub fn describe(self) -> String {
        format!("{} ({}, {})", self.name(), self.model.label(), self.precision.label())
    }
    /// Measured bytes on disk.
    pub fn disk_bytes(self) -> u64 {
        DISK[self.model.index()][self.precision.index()]
    }
}

/// Whether a decider's checkpoint is complete in `models` (the decide models folder).
pub fn present(models: &Path, p: Pick) -> bool {
    let dir = models.join(p.folder());
    ["config.json", "tokenizer.json", "model.safetensors"].iter().all(|f| dir.join(f).is_file())
}

/// What loads for a choice: the chosen folder when it is here, else the chosen version's nearest present precision,
/// else the same search in the other version (V6 missing: V5 runs, and `reason` says so). Nothing present: the chosen
/// one, so the load fails with its own reason.
///
/// Falling back rather than failing (4 Nov): a decider that does not load leaves Heimdall deaf to its name ("Commands
/// and replies are off"), while V5 is the decider that ran until today; the log line and the menu name what runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub chosen: Pick,
    pub used: Pick,
}

impl Resolved {
    /// Another precision loads in the chosen one's place (the menu lights it).
    pub fn precision_fell_back(&self) -> bool {
        self.chosen.precision != self.used.precision
    }
    /// The other version loads (the chosen one has no folder here).
    pub fn model_fell_back(&self) -> bool {
        self.chosen.model != self.used.model
    }
    /// Why another folder loads, for the log and the menu; None when the chosen one does.
    pub fn reason(&self) -> Option<String> {
        (self.chosen != self.used).then(|| format!("{} is not on this Mac; {} loads in its place", self.chosen.name(), self.used.name()))
    }
}

pub fn resolve(chosen: Pick, present: impl Fn(Pick) -> bool) -> Resolved {
    let models = std::iter::once(chosen.model).chain(DeciderModel::ALL.into_iter().filter(|m| *m != chosen.model));
    let used = models
        .flat_map(|m| std::iter::once(chosen.precision).chain(chosen.precision.nearest()).map(move |p| Pick::new(m, p)))
        .find(|p| present(*p))
        .unwrap_or(chosen);
    Resolved { chosen, used }
}

/// "Decider V6": the version the folder names (`send_v3::version`).
pub fn version_label(p: Pick) -> String {
    match crate::send_v3::version(Path::new(p.folder())) {
        Some(v) => format!("Decider V{v}"),
        None => "Decider".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_precisions_default_4_bit() {
        assert_eq!(Precision::default(), Precision::Int4);
        assert_eq!(Precision::DEFAULT, Precision::Int4);
        assert_eq!(Precision::ALL.map(Precision::label), ["16-bit", "8-bit", "4-bit"]);
        for p in Precision::ALL {
            assert_eq!(Precision::from_key(p.key()), Some(p));
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{}\"", p.key()));
        }
        assert!(Precision::Int4.memory_bytes() < Precision::Int8.memory_bytes() && Precision::Int8.memory_bytes() < Precision::Bf16.memory_bytes());
        assert!(Precision::ALL.iter().all(|p| p.resident_bytes() < p.memory_bytes()));
    }

    /// ("Install V6"): V6 by default, V5 a setting away, each in its three folders.
    #[test]
    fn v6_is_the_default_and_v5_is_a_setting_away_with_their_folders() {
        assert_eq!(DeciderModel::default(), DeciderModel::V6);
        assert_eq!(Pick::DEFAULT, Pick::new(DeciderModel::V6, Precision::Int4));
        assert_eq!(Pick::DEFAULT.folder(), "d1/d1-2B-heimdall-v6-4bit");
        assert_eq!(DeciderModel::from_key("v5"), Some(DeciderModel::V5));
        assert_eq!(DeciderModel::from_key("v7"), None);
        assert_eq!(serde_json::to_string(&DeciderModel::V6).unwrap(), "\"v6\"");
        assert_eq!(Pick::new(DeciderModel::V6, Precision::Bf16).folder(), crate::DEFAULT_MODEL, "16-bit V6 is the trained checkpoint");
        for (m, n) in [(DeciderModel::V6, 6), (DeciderModel::V5, 5)] {
            assert_eq!(DeciderModel::from_key(m.key()), Some(m));
            for p in Precision::ALL {
                let pick = Pick::new(m, p);
                assert_eq!(crate::send_v3::version(Path::new(pick.folder())), Some(n));
                assert_eq!(crate::send_v3::Generation::of(Path::new(pick.folder())), crate::send_v3::Generation::V5, "V6 asks V5's questions");
                assert_eq!(version_label(pick), m.label());
                assert!(pick.folder().ends_with(pick.name()));
            }
        }
        assert_eq!(Pick::new(DeciderModel::V5, Precision::Int8).folder(), "d1/d1-2B-heimdall-v5-8bit");
        assert_eq!(Pick::DEFAULT.describe(), "d1-2B-heimdall-v6-4bit (Decider V6, 4-bit)");
        assert_eq!(Pick::new(DeciderModel::V5, Precision::Bf16).label(), "V5 16-bit");
    }

    /// The disk sizes are the lock's: the sum of each folder's files (so the menu and Setup show what ships).
    #[test]
    fn disk_sizes_are_the_locks() {
        let lock: serde_json::Value = serde_json::from_str(include_str!("../scripts/models.lock.json")).unwrap();
        for m in DeciderModel::ALL {
            for p in Precision::ALL {
                let pick = Pick::new(m, p);
                let prefix = format!("{}/", pick.folder());
                let rows: Vec<&serde_json::Value> = lock["files"].as_array().unwrap().iter().filter(|r| r["path"].as_str().unwrap().starts_with(&prefix)).collect();
                assert_eq!(rows.len(), 5, "{prefix}: config, tokenizer, tokenizer_config, calibration, weights");
                assert!(rows.iter().all(|r| r["bundle"] == true), "{prefix} ships");
                assert_eq!(rows.iter().map(|r| r["bytes"].as_u64().unwrap()).sum::<u64>(), pick.disk_bytes(), "{prefix}");
            }
        }
    }

    #[test]
    fn a_missing_folder_falls_back_to_the_nearest_present_one() {
        use Precision::*;
        let v6 = |p| Pick::new(DeciderModel::V6, p);
        let only = |set: &'static [Precision]| move |p: Pick| p.model == DeciderModel::V6 && set.contains(&p.precision);
        assert_eq!(resolve(v6(Int4), only(&[Bf16, Int8, Int4])).used, v6(Int4));
        assert_eq!(resolve(v6(Int4), only(&[Bf16, Int8])).used, v6(Int8));
        assert_eq!(resolve(v6(Int8), only(&[Bf16, Int4])).used, v6(Int4), "nearest by memory");
        assert_eq!(resolve(v6(Bf16), only(&[Int4])).used, v6(Int4));
        let r = resolve(v6(Int8), only(&[]));
        assert_eq!((r.used, r.precision_fell_back(), r.reason()), (v6(Int8), false, None), "nothing here: the chosen one fails with its own reason");
        assert!(resolve(v6(Int4), only(&[Bf16])).precision_fell_back());
    }

    /// V6 missing (an older bundle, a hand-deleted folder): V5 runs at the same precision, and the reason says so.
    #[test]
    fn a_missing_v6_falls_back_to_v5_with_a_reason() {
        use DeciderModel::*;
        use Precision::*;
        let v5_only = |p: Pick| p.model == V5;
        let r = resolve(Pick::DEFAULT, v5_only);
        assert_eq!(r.used, Pick::new(V5, Int4));
        assert!(r.model_fell_back() && !r.precision_fell_back());
        assert_eq!(r.reason().as_deref(), Some("d1-2B-heimdall-v6-4bit is not on this Mac; d1-2B-heimdall-v5-4bit loads in its place"));
        // Any V6 precision beats V5: the version is the choice that matters.
        assert_eq!(resolve(Pick::DEFAULT, |p| p == Pick::new(V6, Bf16) || p.model == V5).used, Pick::new(V6, Bf16));
        // V5 chosen and missing: V6 runs.
        assert_eq!(resolve(Pick::new(V5, Int8), |p| p.model == V6).used, Pick::new(V6, Int8));
        // V5 chosen and present: V5 runs, nothing said.
        let r = resolve(Pick::new(V5, Int4), |_| true);
        assert_eq!((r.used, r.reason()), (Pick::new(V5, Int4), None));
    }

    #[test]
    fn presence_needs_the_checkpoint_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!present(dir.path(), Pick::DEFAULT));
        let f = dir.path().join(Pick::DEFAULT.folder());
        std::fs::create_dir_all(&f).unwrap();
        for n in ["config.json", "tokenizer.json"] { std::fs::write(f.join(n), "{}").unwrap(); }
        assert!(!present(dir.path(), Pick::DEFAULT));
        std::fs::write(f.join("model.safetensors"), "x").unwrap();
        assert!(present(dir.path(), Pick::DEFAULT));
        assert!(!present(dir.path(), Pick::new(DeciderModel::V5, Precision::Int4)));
    }
}
