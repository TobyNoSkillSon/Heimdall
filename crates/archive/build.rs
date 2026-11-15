fn main() {
    // One pinned workspace Opus source; no Homebrew/runtime codec dependency.
    println!("cargo:rerun-if-changed=../speakers/vendor/opus");
    let dst = cmake::Config::new("../speakers/vendor/opus")
        .profile("Release")
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        // cargo's job count (NUM_JOBS: two while the user uses the Mac), not a fixed four.
        .build_arg(format!("-j{}", std::env::var("NUM_JOBS").unwrap_or_else(|_| "2".into())))
        .build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=opus");
}
