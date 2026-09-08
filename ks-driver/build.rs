use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=seh_shim.c");
    println!("cargo:rerun-if-env-changed=KS_DRIVER_WDK");
    println!("cargo:rerun-if-env-changed=WDK_ROOT");
    println!("cargo:rerun-if-env-changed=WDK_LIB");
    println!("cargo:rerun-if-env-changed=WDK_VERSION");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!("ks-driver requires a Windows target");
    }

    // Keep cargo check/build usable without a WDK. Set this only from a WDK
    // developer prompt when producing the final native-subsystem image.
    if env::var("CARGO_FEATURE_WDK").is_err() || env::var("KS_DRIVER_WDK").as_deref() != Ok("1") {
        println!("cargo:warning=framework mode: use --features wdk and KS_DRIVER_WDK=1 for a WDK .sys build");
        return;
    }

    let wdk_root = env::var_os("WDK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("KS_DRIVER_WDK=1 requires WDK_ROOT"));
    let wdk_lib = env::var_os("WDK_LIB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!("KS_DRIVER_WDK=1 requires WDK_LIB (...\\Lib\\<version>\\km\\x64)")
        });
    let version = env::var("WDK_VERSION").unwrap_or_else(|_| "10.0.26100.0".to_owned());
    let include_version = wdk_root.join("Include").join(&version);
    if !include_version.join("km").join("ntddk.h").exists() {
        panic!(
            "WDK {} kernel headers not found under {}. Set WDK_VERSION only when intentionally using another installed WDK",
            version,
            include_version.display()
        );
    }
    let include = include_version.join("km");
    let include_shared = include_version.join("shared");
    if !include.exists() || !include_shared.exists() || !wdk_lib.exists() {
        panic!(
            "invalid WDK_ROOT/WDK_LIB: {} / {}",
            include.display(),
            wdk_lib.display()
        );
    }

    let mut c = cc::Build::new();
    c.no_default_flags(true)
        .file("seh_shim.c")
        .include(&include)
        .include(&include_shared)
        .define("_KERNEL_MODE", None)
        .define("_AMD64_", None)
        .define("AMD64", None)
        .warnings(true)
        .flag("/nologo")
        .flag("/kernel")
        .flag("/GS-")
        .flag("/Zl")
        .compile("ks_seh_shim");

    println!("cargo:rustc-link-search=native={}", wdk_lib.display());
    println!("cargo:rustc-link-lib=ntoskrnl");
    println!("cargo:rustc-link-lib=hal");
    println!("cargo:rustc-link-lib=wdmsec");
    println!("cargo:rustc-link-lib=BufferOverflowK");
    println!("cargo:rustc-link-arg=/SUBSYSTEM:NATIVE");
    println!("cargo:rustc-link-arg=/DRIVER");
    println!("cargo:rustc-link-arg=/NODEFAULTLIB");
    println!("cargo:rustc-link-arg-bin=ks-driver=/ENTRY:DriverEntry");
    println!("cargo:rustc-link-arg-bin=ks-driver=/OUT:ks-driver.sys");
}
