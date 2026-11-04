// The shipped dylib's own install name: without this it is the absolute target/ path of the build machine
// (scripts/check_public_bundle.py refuses a bundle that names the builder's home). Heimdall dlopens it by path.
fn main() {
    println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libd1v5_engine.dylib");
}
