use std::{path::PathBuf, process::Command};
#[allow(dead_code)]
#[path = "src/skill_index.rs"]
mod skill_index;
fn compile(args: &[&str], output: &std::path::Path) {
    let status = Command::new("/usr/bin/clang")
        .args(["-Os", "-mmacosx-version-min=14.4", "-Wall", "-Wextra"])
        .args(args).arg("runtime/tool-guardian.c").arg("-o").arg(output)
        .status().expect("compile process guardian");
    assert!(status.success(), "process guardian build failed");
}
fn main() {
    println!("cargo:rerun-if-changed=runtime/tool-guardian.c");
    println!("cargo:rustc-check-cfg=cfg(test_guardian_available)");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    // The relay's system prompt (`relay_prompt`): the relay section, Heimdall's prompt and the skills index, per edition.
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=src/skill_index.rs");
    for public in [false, true] {
        let (relay, prompt, sets) = skill_index::sources(public);
        for path in [relay, prompt, sets[0], sets[1]] {
            println!("cargo:rerun-if-changed={path}");
        }
        // A directory only reruns on its own entries' change; each skill folder and SKILL.md too.
        for set in sets {
            for entry in std::fs::read_dir(dir.join(set)).into_iter().flatten().flatten() {
                println!("cargo:rerun-if-changed={set}/{}", entry.file_name().to_string_lossy());
                println!("cargo:rerun-if-changed={set}/{}/SKILL.md", entry.file_name().to_string_lossy());
            }
        }
        let text = skill_index::compose_from(&dir, public).expect("relay prompt sources");
        std::fs::write(out.join(if public { "relay-public.md" } else { "relay.md" }), text).expect("write relay prompt");
    }
    let object = out.join("tool-guardian.o");
    let debug = std::env::var("PROFILE").unwrap() == "debug";
    compile(if debug { &["-c", "-DHEIMDALL_GUARDIAN_TEST_FAULTS"] } else { &["-c"] }, &object);
    assert!(Command::new("/usr/bin/ar").arg("crs").arg(out.join("libheimdall_guardian.a"))
        .arg(object).status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=heimdall_guardian");
    // Cargo unit-test executables do not enter the app's main. Only debug test
    // harnesses use the Rust fixture binary with the same cleanup entry point.
    if std::env::var("PROFILE").unwrap() == "debug" {
        println!("cargo:rustc-cfg=test_guardian_available");
    }
}
