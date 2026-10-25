use std::env;
use std::path::PathBuf;

/// Embeds the elevation manifest into release binaries so the hook test can
/// open the driver's section (its DACL only grants SYSTEM/Administrators).
fn main() {
    println!("cargo:rerun-if-changed=ks-hook-test.manifest");
    if env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("ks-hook-test.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}
