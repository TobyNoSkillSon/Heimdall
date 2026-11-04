//! Heimdall's own skills (docs/skills.md). The system prompt stays minimal; procedures live in skills under the repo's
//! `skills/` folder. Pi starts with `--no-skills --skill <data>/agent/skills`, so it loads exactly Heimdall's folder and
//! nothing a user installed for other agents. Each bundled skill is copied into the profile once, ever: the folder's
//! `.seeded` list names every skill copied before, so an agent's edit or removal (the `update-skills` skill) survives
//! every start, and a later build's change to a skill already seeded never arrives by itself. Nothing here syncs other
//! harnesses; agents copy those by hand.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The edition's own set under `skills/` (docs/editions.md): the user's, or the public one without his name or paths.
#[cfg(not(feature = "public"))]
pub const EDITION_SET: &str = "heimdall";
#[cfg(feature = "public")]
pub const EDITION_SET: &str = "public";
/// Skills every agent gets, Heimdall included (`update-skills`).
pub const SHARED_SET: &str = "shared";

/// The bundled `skills/` folder: the installed app's Resources copy (`scripts/bundle.sh`), or the checkout's for a
/// development or test binary.
pub fn bundled(exe: &Path) -> PathBuf {
    match exe.parent().and_then(Path::parent) {
        Some(contents) if crate::runtime::in_app_bundle(exe) => contents.join("Resources/skills"),
        _ => crate::runtime::checkout_root().join("skills"),
    }
}

/// What runs right after each seed, given the bundled sets and the profile's folder: the public edition's updater
/// brings unedited bundled skills up to this release there (`heimdall_update::skills::upgrade`). Set once at launch.
static AFTER_SEED: std::sync::OnceLock<fn(&[PathBuf], &Path)> = std::sync::OnceLock::new();

/// Sets what runs after each seed (once; later calls are ignored).
pub fn after_seed(hook: fn(&[PathBuf], &Path)) {
    let _ = AFTER_SEED.set(hook);
}

/// The edition's set and the shared set under the bundled `skills/` folder, in seeding order.
pub fn sets(bundled: &Path) -> [PathBuf; 2] {
    [bundled.join(EDITION_SET), bundled.join(SHARED_SET)]
}

/// Copies the bundled skills in (`seed`), then runs the hook set by `after_seed`, if any.
pub fn seed_and_upgrade(bundled: &Path, target: &Path) -> Result<Vec<String>> {
    let copied = seed(bundled, target);
    if let Some(hook) = AFTER_SEED.get() {
        hook(&sets(bundled), target);
    }
    copied
}

/// Pi's skill folder for a profile whose Pi config lives in `agent_dir` (`<data>/agent/config`): `<data>/agent/skills`.
pub fn folder(agent_dir: &Path) -> PathBuf {
    agent_dir.parent().unwrap_or(agent_dir).join("skills")
}

/// The list of skills ever copied into a profile's folder, one name per line.
pub const SEEDED: &str = ".seeded";

/// Copies each bundled skill of the edition's set and the shared set into `target` unless `.seeded` names it (copied
/// before, perhaps since removed) or a folder of that name is there. Each copy goes to a temporary name first, so an
/// interrupted copy never leaves a half skill. Returns the names copied. A missing bundle copies nothing.
pub fn seed(bundled: &Path, target: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;
    let list = target.join(SEEDED);
    let mut seeded: Vec<String> = std::fs::read_to_string(&list).unwrap_or_default().lines().map(str::to_owned).collect();
    let mut copied = vec![];
    for set in [EDITION_SET, SHARED_SET] {
        let Ok(entries) = std::fs::read_dir(bundled.join(set)) else { continue };
        let mut skills: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.join("SKILL.md").is_file()).collect();
        skills.sort();
        for skill in skills {
            let name = skill.file_name().unwrap().to_string_lossy().into_owned();
            let dest = target.join(&name);
            if seeded.contains(&name) || dest.exists() {
                continue;
            }
            let part = target.join(format!(".{name}.part"));
            let _ = std::fs::remove_dir_all(&part);
            copy_dir(&skill, &part)?;
            std::fs::rename(&part, &dest)?;
            seeded.push(name.clone());
            crate::runtime::atomic_write(&list, (seeded.join("\n") + "\n").as_bytes())?;
            copied.push(name);
        }
    }
    Ok(copied)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if std::fs::metadata(entry.path())?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest).with_context(|| format!("copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static SEEN: Mutex<Vec<(Vec<PathBuf>, PathBuf, bool)>> = Mutex::new(vec![]);

    fn record(sets: &[PathBuf], profile: &Path) {
        // Seeding has finished by the time the hook runs.
        let seeded = profile.join("night-pass/SKILL.md").is_file();
        SEEN.lock().unwrap().push((sets.to_vec(), profile.to_path_buf(), seeded));
    }

    #[test]
    fn the_upgrade_hook_runs_after_seeding_with_the_edition_sets() {
        let d = tempfile::tempdir().unwrap();
        let bundle = d.path().join("bundle");
        std::fs::create_dir_all(bundle.join(super::EDITION_SET).join("night-pass")).unwrap();
        std::fs::write(bundle.join(super::EDITION_SET).join("night-pass/SKILL.md"), "bundled").unwrap();
        let target = d.path().join("skills");
        super::after_seed(record);
        assert_eq!(super::seed_and_upgrade(&bundle, &target).unwrap(), ["night-pass"]);
        let seen = SEEN.lock().unwrap();
        let ours: Vec<_> = seen.iter().filter(|(_, p, _)| *p == target).collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].0, [bundle.join(super::EDITION_SET), bundle.join(super::SHARED_SET)]);
        assert!(ours[0].2);
    }

    #[test]
    fn a_fresh_profile_gets_the_bundled_skills_once_and_edits_survive() {
        let d = tempfile::tempdir().unwrap();
        let bundle = d.path().join("bundle");
        for (set, name) in [(super::EDITION_SET, "night-pass"), (super::SHARED_SET, "update-skills"), ("workers", "git-workflow")] {
            std::fs::create_dir_all(bundle.join(set).join(name).join("references")).unwrap();
            std::fs::write(bundle.join(set).join(name).join("SKILL.md"), format!("bundled {name}")).unwrap();
            std::fs::write(bundle.join(set).join(name).join("references/more.md"), "more").unwrap();
        }
        let target = super::folder(&d.path().join("agent/config"));
        assert_eq!(target, d.path().join("agent/skills"));
        assert_eq!(super::seed(&bundle, &target).unwrap(), ["night-pass", "update-skills"]);
        assert_eq!(std::fs::read_to_string(target.join("night-pass/references/more.md")).unwrap(), "more");
        assert!(!target.join("git-workflow").exists(), "worker skills are for the T3 harnesses, not Pi");
        // An agent edits one skill and removes another; the next start keeps both and copies only a newly bundled one.
        std::fs::write(target.join("night-pass/SKILL.md"), "edited").unwrap();
        std::fs::remove_dir_all(target.join("update-skills")).unwrap();
        std::fs::create_dir_all(bundle.join(super::EDITION_SET).join("islands")).unwrap();
        std::fs::write(bundle.join(super::EDITION_SET).join("islands/SKILL.md"), "new").unwrap();
        assert_eq!(super::seed(&bundle, &target).unwrap(), ["islands"]);
        assert_eq!(std::fs::read_to_string(target.join("night-pass/SKILL.md")).unwrap(), "edited");
        assert!(!target.join("update-skills").exists(), "a removed skill stays removed");
        assert_eq!(std::fs::read_to_string(target.join(super::SEEDED)).unwrap(), "night-pass\nupdate-skills\nislands\n");
        // A half copy from an interrupted start is replaced, never loaded.
        std::fs::create_dir_all(target.join(".lost.part")).unwrap();
        std::fs::create_dir_all(bundle.join(super::SHARED_SET).join("lost")).unwrap();
        std::fs::write(bundle.join(super::SHARED_SET).join("lost/SKILL.md"), "whole").unwrap();
        assert_eq!(super::seed(&bundle, &target).unwrap(), ["lost"]);
        assert!(!target.join(".lost.part").exists() && target.join("lost/SKILL.md").is_file());
        assert!(super::seed(&d.path().join("absent"), &target).unwrap().is_empty());
    }
    #[test]
    fn the_installed_app_reads_its_bundled_copy() {
        let app = std::path::Path::new("/Users/x/Applications/Heimdall.app/Contents/MacOS/Heimdall");
        assert_eq!(super::bundled(app), std::path::Path::new("/Users/x/Applications/Heimdall.app/Contents/Resources/skills"));
        let dev = super::bundled(std::path::Path::new("/tmp/target/debug/heimdall"));
        // The personal set (`skills/heimdall`) is not in the public source export; the public build checks its own.
        assert!(dev.join(super::SHARED_SET).join("update-skills/SKILL.md").is_file(), "{}", dev.display());
        assert!(!cfg!(feature = "public") || dev.join(super::EDITION_SET).is_dir(), "{}", dev.display());
    }
}
