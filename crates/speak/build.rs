use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=src/pointwise.metal");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let air = out.join("pointwise.air");
    let lib = out.join("pointwise.metallib");
    for args in [
        vec![
            "-sdk",
            "macosx",
            "metal",
            "-std=metal4.0",
            "-fno-fast-math",
            "-c",
            "src/pointwise.metal",
            "-o",
            air.to_str().unwrap(),
        ],
        vec![
            "-sdk",
            "macosx",
            "metallib",
            air.to_str().unwrap(),
            "-o",
            lib.to_str().unwrap(),
        ],
    ] {
        assert!(
            Command::new("xcrun").args(args).status().unwrap().success(),
            "Metal shader compilation failed"
        );
    }
}
