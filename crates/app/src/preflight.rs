//! Agent pre-flight runs on a helper thread: a read-only integrity check of the runtime inside the app
//! (files, pin, exact Node and Pi versions). Nothing is downloaded or repaired; no inference or cloud login.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Missing,
    Broken(String),
}
impl Failure {
    pub fn island(&self) -> (String, String, ui::HealthControl) {
        // The runtime is sealed inside the app: only a fresh copy of Heimdall restores it.
        match self {
            Self::Missing => (
                "Agent runtime is missing from Heimdall".into(),
                "Reinstall Heimdall".into(),
                ui::HealthControl::None,
            ),
            Self::Broken(_) => (
                "Agent runtime in Heimdall is damaged".into(),
                "Reinstall Heimdall".into(),
                ui::HealthControl::None,
            ),
        }
    }
}
pub fn check(_data: &Path, fixture: Option<PathBuf>) -> Result<String, Failure> {
    if let Some(exe) = fixture {
        if !exe.is_file() {
            return Err(Failure::Missing);
        }
        let out = agent::runtime::checked(
            std::process::Command::new(exe).arg("--version"),
            Duration::from_secs(10),
        )
        .map_err(|e| Failure::Broken(e.to_string()))?;
        if out.trim() != agent::runtime::pi_version() {
            return Err(Failure::Broken("wrong Pi version".into()));
        }
        return Ok(out.trim().into());
    }
    let runtime = agent::runtime::Runtime::installed().map_err(|e| Failure::Broken(format!("{e:#}")))?;
    if !runtime.root.join("receipt.json").is_file() {
        return Err(Failure::Missing);
    }
    runtime.verify().map_err(|e| Failure::Broken(format!("{e:#}")))?;
    Ok(agent::runtime::pi_version())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fake(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("pi");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        p
    }
    #[test]
    fn exact_version_only() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            check(d.path(), Some(fake(d.path(), &format!("echo {}", agent::runtime::pi_version())))),
            Ok(agent::runtime::pi_version())
        );
        assert!(matches!(
            check(d.path(), Some(fake(d.path(), "echo 1.0.2"))),
            Err(Failure::Broken(_))
        ));
        assert!(check(d.path(), Some(d.path().join("absent"))).is_err());
    }
    #[test]
    fn island_gives_an_action_without_commands_or_raw_errors() {
        let (label, action, _) = Failure::Broken("private detail".into()).island();
        assert!(!label.contains("private detail"));
        assert_eq!(action, "Reinstall Heimdall");
    }
}
