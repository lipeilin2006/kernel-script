use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=installer.manifest");
    // Cargo offers no cfg that distinguishes the bin unittest harness build
    // from the normal bin build (CARGO_CFG_TEST is unset for both), and a
    // requireAdministrator test binary cannot execute (`cargo test` fails
    // with os error 740). Embed the manifest only for release builds, which
    // is what gets deployed; debug/test builds run without auto-elevation.
    if env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("installer.manifest");
    // Embed a requireAdministrator manifest so Windows shows the UAC prompt
    // on launch. /MANIFESTUAC:NO keeps link.exe from merging its default
    // asInvoker trustInfo with ours.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}
