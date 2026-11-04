/// The Apple clang signature in `native/build-lock.json`: the lock is the one place the
/// compiler is pinned, so a compiler update is a lock edit after parity, not a code change.
fn locked_compiler() -> String {
    let lock = std::fs::read_to_string("native/build-lock.json").expect("native/build-lock.json");
    let rest = lock
        .split("\"compiler\":")
        .nth(1)
        .expect("build-lock.json has a \"compiler\" entry");
    let rest = &rest[rest.find('"').expect("compiler is a string") + 1..];
    rest[..rest.find('"').expect("compiler string ends")].to_string()
}

fn main() {
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-changed=vendor");
    let mac = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos";
    let mut cfg = cmake::Config::new("native");
    cfg.profile("Release")
        .define("GGML_METAL", if mac { "ON" } else { "OFF" })
        .define("GGML_METAL_EMBED_LIBRARY", "ON")
        .define("GGML_NATIVE", "OFF")
        .define("GGML_CCACHE", "OFF")
        .define("CMAKE_C_FLAGS_RELEASE", "-O3 -DNDEBUG -fno-fast-math")
        .define("CMAKE_CXX_FLAGS_RELEASE", "-O3 -DNDEBUG -fno-fast-math")
        .define("GGML_BLAS", "OFF")
        .define("GGML_OPENMP", "OFF")
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .build_arg("-j4");
    if mac {
        println!("cargo:rerun-if-changed=src/kernels.metal");
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        for args in [
            vec![
                "metal",
                "-std=metal4.0",
                "-fno-fast-math",
                "-c",
                "src/kernels.metal",
                "-o",
            ],
            vec!["metallib"],
        ] {
            let mut cmd = std::process::Command::new("xcrun");
            if args[0] == "metal" {
                cmd.args(args).arg(out.join("speakers.air"));
            } else {
                cmd.args(args)
                    .arg(out.join("speakers.air"))
                    .arg("-o")
                    .arg(out.join("speakers.metallib"));
            }
            assert!(
                cmd.status().expect("Metal toolchain").success(),
                "speaker shader compilation failed"
            );
        }
        cfg.define("CMAKE_C_COMPILER", "/usr/bin/clang")
            .define("CMAKE_CXX_COMPILER", "/usr/bin/clang++");
        let version = std::process::Command::new("/usr/bin/clang")
            .arg("--version")
            .output()
            .expect("Apple clang required");
        let version = String::from_utf8(version.stdout).expect("clang version UTF8");
        let pinned = locked_compiler();
        assert!(
            version.starts_with(&pinned),
            "native compiler changed (locked {pinned:?}, found {:?}): run the reference parity \
             test (tests/reference_parity.rs) with the new compiler, then update \"compiler\" in \
             native/build-lock.json",
            version.lines().next().unwrap_or_default()
        );
    }
    let dst = cfg.build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    for name in ["heimdall_nemo", "ggml", "ggml-cpu", "ggml-base", "opus"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
    if mac {
        println!("cargo:rustc-link-lib=static=ggml-metal");
        println!("cargo:rustc-link-lib=framework=Accelerate");
        println!("cargo:rustc-link-lib=framework=Metal");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=c++");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
