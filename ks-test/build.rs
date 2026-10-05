use std::env;
use std::path::PathBuf;

/// Embeds the elevation manifest into release binaries (debug/test
/// builds skip it so `cargo test` can run unelevated). The in-process
/// KDU mapper this build script used to own moved to `ks-sdk`.
fn main() {
    println!("cargo:rerun-if-changed=ks-test.manifest");
    // Match ks-launcher: release builds request elevation; debug/test builds
    // remain runnable without UAC so cargo test works normally.
    if env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("ks-test.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}
