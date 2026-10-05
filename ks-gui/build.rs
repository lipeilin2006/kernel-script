use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=ks-gui.manifest");
    // Match ks-test: release builds request elevation (the driver section
    // DACL only grants SYSTEM and Administrators, and the in-process KDU
    // mapper must run elevated); debug/test builds remain runnable without
    // UAC so cargo check and cargo test work normally.
    if env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("ks-gui.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}
