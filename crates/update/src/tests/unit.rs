use crate::release::{choose, Release};
use crate::source::{scheme_allowed, Source};
use crate::state::{check_due, Event, Machine, Phase, DAY};
use crate::{signing, Product, Version};

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn release(tag: &str, prerelease: bool) -> Release {
    let version = v(tag);
    Release {
        tag: tag.into(),
        assets: vec![Product::ALPHA.zip_name(&version), "SHA256SUMS".into(), "SHA256SUMS.sig".into()],
        version,
        name: String::new(),
        body: String::new(),
        draft: false,
        prerelease,
        published: true,
    }
}

#[test]
fn versions_order_by_semver_with_prereleases() {
    let ordered = ["0.1.0-alpha.1", "0.1.0-alpha.2", "0.1.0-alpha.10", "0.1.0-alpha.beta", "0.1.0-beta.1", "0.1.0-rc.1", "0.1.0", "0.1.1", "1.0.0"];
    for pair in ordered.windows(2) {
        assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
    }
    assert_eq!(v("v0.1.0-alpha.1+build.5"), v("0.1.0-alpha.1"));
    assert_eq!(v("v0.1.0-alpha.1").to_string(), "0.1.0-alpha.1");
    for bad in ["01.0.0", "1.0", "1.0.0-", "1.0.0-a..b", "1.0.0+", "x.y.z", "1.0.0-al pha"] {
        assert!(Version::parse(bad).is_none(), "{bad}");
    }
    assert_eq!(v(env!("CARGO_PKG_VERSION")).to_string(), "0.1.0-alpha.1");
}

#[test]
fn an_alpha_is_offered_newer_alphas_and_stable_releases_a_stable_build_never_a_prerelease() {
    let p = Product::ALPHA;
    let list = vec![release("v0.1.0-alpha.2", true), release("v0.1.0-alpha.3", true), release("v0.0.9", false)];
    assert_eq!(choose(&list, &v("0.1.0-alpha.1"), &p).unwrap().tag, "v0.1.0-alpha.3");
    assert_eq!(choose(&list, &v("0.1.0-alpha.3"), &p), None);
    // A stable build: prereleases are never offered, whatever marks them.
    let mut flagged_only = release("v0.2.0", true);
    flagged_only.prerelease = true;
    let unflagged_pre = release("v0.2.0-beta.1", false);
    assert_eq!(choose(&[flagged_only.clone(), unflagged_pre.clone()], &v("0.1.0"), &p), None);
    // An alpha takes a newer stable one, then stays on stable.
    let stable = release("v0.1.0", false);
    assert_eq!(choose(&[stable.clone(), release("v0.1.0-alpha.9", true)], &v("0.1.0-alpha.2"), &p).unwrap().tag, "v0.1.0");
    assert_eq!(choose(&[stable, flagged_only, unflagged_pre], &v("0.1.0"), &p), None);
    // Drafts, unpublished and incomplete releases are skipped.
    let mut draft = release("v0.1.0-alpha.5", true);
    draft.draft = true;
    let mut unpublished = release("v0.1.0-alpha.6", true);
    unpublished.published = false;
    let mut incomplete = release("v0.1.0-alpha.7", true);
    incomplete.assets.retain(|a| a != "SHA256SUMS.sig");
    assert_eq!(choose(&[draft, unpublished, incomplete], &v("0.1.0-alpha.1"), &p), None);
}

#[test]
fn the_release_list_parses_and_skips_entries_without_a_version_tag() {
    let json = br#"[{"tag_name":"v0.1.0-alpha.2","name":"A2","body":"Notes","draft":false,"prerelease":true,
        "published_at":"2026-10-27T10:00:00Z","assets":[{"name":"SHA256SUMS"}],"extra":1},
        {"tag_name":"nightly"},{"tag_name":"v0.1.0-alpha.3","draft":true,"published_at":null}]"#;
    let list = Release::parse_list(json).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].published && list[0].prerelease && list[0].assets == ["SHA256SUMS"]);
    assert!(list[1].draft && !list[1].published);
    assert!(Release::parse_list(&vec![b' '; 1_048_577]).is_err());
    assert!(Release::parse_list(b"not json").is_err());
}

#[test]
fn short_notes_are_plain_and_cut_at_a_word() {
    let mut r = release("v0.1.0-alpha.2", true);
    r.body = "# Title\n**Faster** start and `fixes`.\n- one\n```\ncode\n```\n## Details\nhidden".into();
    assert_eq!(r.short_notes(360), "Faster start and fixes.\n• one");
    r.body = "word ".repeat(100);
    let notes = r.short_notes(20);
    assert!(notes.ends_with('…') && notes.chars().count() <= 21, "{notes}");
}

#[test]
fn the_source_is_one_constant_and_the_override_must_be_https() {
    let default = Source::default();
    assert_eq!(default.list_url, "https://api.github.com/repos/TobyNoSkillSon/Heimdall/releases?per_page=20");
    assert_eq!(
        default.asset_url("v0.1.0-alpha.2", "SHA256SUMS"),
        "https://github.com/TobyNoSkillSon/Heimdall/releases/download/v0.1.0-alpha.2/SHA256SUMS"
    );
    let custom = Source::from_base(Some("https://example.com/r/")).unwrap();
    assert_eq!(custom.list_url, "https://example.com/r/releases.json");
    assert_eq!(custom.asset_url("v1.0.0", "a.zip"), "https://example.com/r/v1.0.0/a.zip");
    assert!(Source::from_base(Some("http://example.com")).is_err());
    assert!(Source::from_base(Some("ftp://example.com")).is_err());
    assert_eq!(Source::from_base(Some("  ")).unwrap(), Source::default());
    // file:// only in tests and local-releases builds; plain HTTP only to the loopback fixture in tests.
    assert!(scheme_allowed("file:///tmp/x") && scheme_allowed("http://127.0.0.1:9/x"));
    assert!(!scheme_allowed("http://localhost:9/x") && !scheme_allowed("http://127.0.0.2/x"));
}

#[test]
fn signatures_verify_only_with_a_trusted_key_and_the_exact_bytes() {
    let seed = signing::generate_seed().unwrap();
    let key = signing::public_key(&seed);
    let sums = b"abc  Heimdall-Alpha-0.1.0-arm64.zip\n";
    let sig = signing::sign(&seed, sums);
    assert!(signing::verify_sums(sums, &sig, &[key]).is_ok());
    assert!(signing::verify_sums(b"abd  Heimdall-Alpha-0.1.0-arm64.zip\n", &sig, &[key]).is_err());
    let other = signing::public_key(&signing::generate_seed().unwrap());
    assert!(signing::verify_sums(sums, &sig, &[other]).unwrap_err().0.contains("does not trust"));
    assert!(signing::verify_sums(sums, "ed25519 nope", &[key]).is_err());
    assert!(signing::verify_sums(sums, &sig.replace("ed25519", "rsa"), &[key]).is_err());
    // The compiled-in key is a real key, not the zero placeholder.
    assert_ne!(signing::ALPHA_KEY, [0u8; 32]);
    assert!(ed25519_dalek::VerifyingKey::from_bytes(&signing::ALPHA_KEY).is_ok());
}

#[test]
fn key_files_round_trip_and_a_readable_secret_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let seed = signing::generate_seed().unwrap();
    let path = dir.path().join("k");
    std::fs::write(&path, signing::encode_secret(&seed)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(signing::read_secret(&path).unwrap_err().0.contains("private"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(signing::read_secret(&path).unwrap(), seed);
    let public = signing::public_key(&seed);
    assert_eq!(signing::decode_public(&signing::encode_public(&public)).unwrap(), public);
}

#[test]
fn the_update_machine_follows_vellas_flow() {
    let r = release("v0.1.0-alpha.2", true);
    let mut m = Machine::default();
    assert!(m.handle(Event::Checked(Some(r.clone()))));
    assert_eq!(m.phase, Phase::Available(r.clone()));
    assert!(m.handle(Event::Failed("Heimdall is recording".into())));
    assert_eq!(m.phase, Phase::Available(r.clone()));
    assert!(m.handle(Event::Confirmed));
    assert!(m.last_error.is_none() && m.phase.busy());
    assert!(!m.handle(Event::Confirmed));
    assert!(m.handle(Event::Checked(None)) == false && m.phase.busy(), "a check never interrupts a download");
    assert!(m.handle(Event::Verified { busy: true }));
    assert_eq!(m.phase, Phase::WaitingForIdle(r.clone()));
    assert!(m.handle(Event::BecameIdle));
    assert_eq!(m.phase, Phase::Installing(r.clone()));
    assert!(m.handle(Event::Failed("x".into())));
    assert_eq!(m.phase, Phase::Available(r));
    assert!(m.handle(Event::CheckFailed("offline".into())) && m.phase.release().is_some());
}

#[test]
fn checks_are_due_daily() {
    assert!(check_due(None, 100, DAY));
    assert!(!check_due(Some(100), 100 + DAY - 1, DAY));
    assert!(check_due(Some(100), 100 + DAY, DAY));
    assert!(check_due(Some(100), 50, DAY), "a clock that went backwards");
}

/// VM finding P-09 (28 Oct): the installed app's own process reporting another version (Info.plist and binary
/// disagree) is a failure with the reason named, not an endless "earlier launch" wait until the 15-min load timeout.
#[test]
fn a_status_from_the_installed_process_at_another_version_is_failing_with_the_reason() {
    use crate::readiness::{evaluate, State};
    let status = br#"{"app_pid":42,"version":"0.1.0-alpha.1","phase":"ready","detail":""}"#;
    match evaluate(Some(status), "0.1.0-alpha.2", &|pid| pid == 42) {
        State::Failing(reason) => assert!(reason.contains("0.1.0-alpha.1") && reason.contains("0.1.0-alpha.2"), "{reason}"),
        other => panic!("expected Failing, got {other:?}"),
    }
    // Another process (the previous launch) is still a wait.
    assert!(matches!(evaluate(Some(status), "0.1.0-alpha.2", &|_| false), State::Waiting(_)));
    assert!(matches!(evaluate(Some(status), "0.1.0-alpha.1", &|pid| pid == 42), State::Ready(_)));
}
