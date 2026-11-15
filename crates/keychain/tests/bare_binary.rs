//! An integration test is its own binary, built without `cfg(test)` in the library: it takes the same path as any
//! other crate's tests and any bare build (13 Nov: `target/release/heimdall --night-replay-smoke`). It must never
//! get the real Keychain.
#[test]
fn a_bare_test_binary_cannot_reach_the_real_keychain() {
    let exe = std::env::current_exe().unwrap();
    assert!(!keychain::installed_app(), "{}", exe.display());
    assert_ne!(keychain::kind(), keychain::Kind::Keychain, "{}", exe.display());
    assert_eq!(keychain::kind(), keychain::Kind::Memory, "cargo harness in deps/: {}", exe.display());
    // The same binary moved out of deps/ (a bare build) is refused outright.
    let bare = exe.parent().unwrap().parent().unwrap().join("heimdall");
    let (kind, reason) = keychain::choose(Some(&bare), std::env::var_os("HOME").map(std::path::PathBuf::from).as_deref(), None);
    assert_eq!(kind, keychain::Kind::Off, "{reason}");
}
