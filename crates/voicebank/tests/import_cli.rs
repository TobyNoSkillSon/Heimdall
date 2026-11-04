use std::{path::Path, process::Command};
use voicebank::{ClipMeta, Origin, Store};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_heimdall-voicebank"))
}
fn invoke(source: &Path, data: &Path, flags: &[&str]) -> serde_json::Value {
    let output = command()
        .args(["import", "--from"])
        .arg(source)
        .arg("--into")
        .arg(data)
        .args(flags)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn import_defaults_to_dry_run_requires_apply_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("bank");
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let mut settings = speak::drama3::settings();
    settings["asr"] = serde_json::json!({"expected":"Sure.","text":"Sure.","passed":true});
    Store::open(&source)
        .unwrap()
        .insert(
            &ClipMeta {
                origin: Origin::Bank,
                line_id: Some("sure".into()),
                text: "Sure.".into(),
                language: "en".into(),
                voice: speak::drama3::VOICE.into(),
                generator: speak::drama3::GENERATOR.into(),
                settings,
            },
            &speak::Audio {
                samples: vec![0.1; 441],
                sample_rate: 44100,
                first_audio_ms: 0.,
                synthesis_ms: 0.,
            },
        )
        .unwrap();
    heimdall_archive::store::close_packs();
    let preview = invoke(&source, &data, &[]);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["would_insert"], 1);
    assert!(!data.join("archive").exists());
    assert_eq!(invoke(&source, &data, &["--dry-run"]), preview);
    assert_eq!(invoke(&source, &data, &["--apply"])["inserted"], 1);
    assert_eq!(invoke(&source, &data, &["--apply"])["inserted"], 0);
    assert!(!command()
        .args(["import", "--from"])
        .arg(&source)
        .arg("--into")
        .arg(&data)
        .args(["--apply", "--dry-run"])
        .output()
        .unwrap()
        .status
        .success());
}
#[test]
fn fill_refuses_a_symlink_alias_to_the_installed_profile_before_opening_a_store() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let profile = home.join("Library/Application Support/Heimdall");
    std::fs::create_dir_all(&profile).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&profile, &alias).unwrap();
    let output = command()
        .env("HOME", &home)
        .args(["fill", "--bank"])
        .arg(&alias)
        .args(["--models", "missing"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing Heimdall's own data directory")
    );
    assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 0);
    let unresolved = temp.path().join("missing/../alias/archive");
    let output = command()
        .env("HOME", &home)
        .args(["fill", "--bank"])
        .arg(&unresolved)
        .args(["--models", "missing"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("parent-directory"));
    assert!(!temp.path().join("missing").exists());
    assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 0);
}

#[test]
fn import_refuses_parent_directory_and_profile_sources_before_writes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let profile = home.join("Library/Application Support/Heimdall");
    std::fs::create_dir_all(&profile).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&profile, &alias).unwrap();
    let data = temp.path().join("target");
    std::fs::create_dir(&data).unwrap();
    for source in [alias, temp.path().join("missing/../alias/archive")] {
        let output = command()
            .env("HOME", &home)
            .args(["import", "--from"])
            .arg(&source)
            .arg("--into")
            .arg(&data)
            .arg("--apply")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("parent-directory")
                || error.contains("refusing Heimdall's own data directory"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 0);
        assert!(!temp.path().join("missing").exists());
    }
}
#[test]
fn fill_set_must_be_reply_or_acks_before_any_model_or_store() {
    let temp = tempfile::tempdir().unwrap();
    let bank = temp.path().join("bank");
    let output = command()
        .args(["fill", "--backend", "drama3", "--set", "everything", "--bank"])
        .arg(&bank)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--set must be reply or acks"));
    assert!(!bank.exists());
}
