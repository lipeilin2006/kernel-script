use std::env;
use std::path::PathBuf;

/// KDU 1.5.0 sources (paths relative to `KDU-1.5.0/Source/Hamakaze`),
/// the `KDU.vcxproj` build set minus `main.cpp` and its two test files:
/// `ks-sdk/kdu/ks_bridge.cpp` replaces the command-line entry with an
/// in-process FFI entry, so no `main` may exist in the archive (it would
/// clash with the Rust/CRT entry point at link time).
const KDU_SOURCES: &[&str] = &[
    "../Shared/ldr/ldr.cpp",
    "../Shared/minirtl/_filename.c",
    "../Shared/minirtl/_strcat.c",
    "../Shared/minirtl/_strcmp.c",
    "../Shared/minirtl/_strcmpi.c",
    "../Shared/minirtl/_strcpy.c",
    "../Shared/minirtl/_strend.c",
    "../Shared/minirtl/_strlen.c",
    "../Shared/minirtl/_strncmp.c",
    "../Shared/minirtl/_strncpy.c",
    "../Shared/minirtl/_strstri.c",
    "../Shared/minirtl/cmdline.c",
    "../Shared/minirtl/strtou64.c",
    "../Shared/minirtl/strtoul.c",
    "../Shared/minirtl/u64tohex.c",
    "../Shared/ntos/ntsup.c",
    "../Shared/thirdparty/tinyaes/aes.c",
    "../Shared/thirdparty/whirlpool/whirlpool.c",
    "compress.cpp",
    "diag.cpp",
    "drvmap.cpp",
    "dsefix.cpp",
    "envdetect.cpp",
    "hash.cpp",
    "hde/hde64.c",
    "hvdetect.cpp",
    "idrv/adlice.cpp",
    "idrv/alcpu.cpp",
    "idrv/amd.cpp",
    "idrv/asrdrv.cpp",
    "idrv/atszio.cpp",
    "idrv/binalyze.cpp",
    "idrv/dbk.cpp",
    "idrv/dell.cpp",
    "idrv/directio64.cpp",
    "idrv/echodrv.cpp",
    "idrv/evga.cpp",
    "idrv/gmer.cpp",
    "idrv/hilscher.cpp",
    "idrv/hp.cpp",
    "idrv/intel.cpp",
    "idrv/ipcdec.cpp",
    "idrv/kph.cpp",
    "idrv/leco.cpp",
    "idrv/lenovo.cpp",
    "idrv/lha.cpp",
    "idrv/mapmem.cpp",
    "idrv/marvinhw.cpp",
    "idrv/matrox.cpp",
    "idrv/mimidrv.cpp",
    "idrv/netease.cpp",
    "idrv/nvidia.cpp",
    "idrv/phymem.cpp",
    "idrv/procexp.cpp",
    "idrv/rtcore.cpp",
    "idrv/rzpnk.cpp",
    "idrv/tpup.cpp",
    "idrv/tpw.cpp",
    "idrv/winio.cpp",
    "idrv/winring0.cpp",
    "idrv/zemana.cpp",
    "idrv/zodiacon.cpp",
    "ipcsvc.cpp",
    "kduprov.cpp",
    "pagewalk.cpp",
    "provdb.cpp",
    "provlist.cpp",
    "ps.cpp",
    "shellcode.cpp",
    "sigcheck.cpp",
    "sup.cpp",
    "sym.cpp",
    "victim.cpp",
];

/// Locates `ml64.exe` for `shellmasm.asm`: the active VS toolchain dir
/// (`VCToolsInstallDir`, set by a developer prompt) first, then a walk of
/// the standard Visual Studio roots. `cc` already found `cl.exe` through
/// the same installation, but does not expose its path.
fn find_ml64() -> PathBuf {
    if let Ok(dir) = env::var("VCToolsInstallDir") {
        let candidate = PathBuf::from(dir)
            .join("bin")
            .join("HostX64")
            .join("x64")
            .join("ml64.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    for root in [
        PathBuf::from(r"C:\Program Files\Microsoft Visual Studio"),
        PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio"),
    ] {
        // Layouts: <root>\<edition>\... (2015) and <root>\<year>\<edition>\...
        // (2017+). Collect both candidate bases in one pass.
        let mut bases = Vec::new();
        let Ok(editions) = std::fs::read_dir(&root) else {
            continue;
        };
        for edition in editions.flatten() {
            let path = edition.path();
            bases.push(path.clone());
            if let Ok(years) = std::fs::read_dir(&path) {
                for year in years.flatten() {
                    bases.push(year.path());
                }
            }
        }
        for base in bases {
            let tools = base.join("VC").join("Tools").join("MSVC");
            let Ok(versions) = std::fs::read_dir(&tools) else {
                continue;
            };
            // Highest version first: MSVC version directories sort
            // lexicographically in the same order as numerically.
            let mut dirs: Vec<_> = versions.flatten().map(|v| v.path()).collect();
            dirs.sort();
            for dir in dirs.iter().rev() {
                let candidate = dir.join("bin").join("HostX64").join("x64").join("ml64.exe");
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    panic!("ml64.exe not found (Visual Studio with the x64 toolset is required to build ks-sdk)");
}

/// Assembles `shellmasm.asm` with MASM (the vcxproj masm.targets
/// equivalent) and returns the object path for `Build::object`.
fn assemble_shellmasm(hamakaze: &std::path::Path) -> PathBuf {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap_or_default());
    let object = out_dir.join("shellmasm.obj");
    let source = hamakaze.join("shellmasm.asm");

    println!("cargo:rerun-if-changed={}", source.display());

    let ml64 = find_ml64();
    let status = std::process::Command::new(&ml64)
        .arg("/nologo")
        .arg("/c")
        .arg(format!("/Fo{}", object.display()))
        .arg(&source)
        .status()
        .unwrap_or_else(|e| panic!("run {}: {e}", ml64.display()));
    if !status.success() {
        panic!("ml64 failed assembling {}", source.display());
    }
    object
}

/// Compiles the KDU mapper core plus `ks_bridge.cpp` into a static
/// library linked into ks-sdk. This runs for every profile — `cargo
/// test` needs the bridge too — and requires MSVC (cc discovers the
/// Visual Studio installation via the registry).
fn build_kdu() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let source = manifest.join("../KDU-1.5.0/Source");
    let hamakaze = source.join("Hamakaze");
    let log_header = manifest.join("kdu").join("ks_kdu_log.h");
    let bridge = manifest.join("kdu").join("ks_bridge.cpp");

    if !hamakaze.is_dir() {
        panic!(
            "KDU sources not found at {}; ks-sdk requires the in-process KDU \
             integration (see AGENTS.md)",
            hamakaze.display()
        );
    }

    println!("cargo:rerun-if-changed={}", manifest.join("kdu").display());
    println!("cargo:rerun-if-changed={}", hamakaze.display());
    println!("cargo:rerun-if-changed={}", source.join("Shared").display());

    let mut build = cc::Build::new();
    build
        // Release semantics: NDEBUG keeps the _DEBUG-only guard branches
        // (KDUProcessDrvMapSwitch refuses to run shellcode in debug).
        .define("NDEBUG", None)
        .define("_CONSOLE", None)
        .define("UNICODE", None)
        .define("_UNICODE", None)
        // Match KDU.vcxproj: ConformanceMode (/permissive- is required,
        // KDU sources use alternative tokens like `and`) and no buffer
        // security checks.
        .flag("/permissive-")
        .flag("/GS-")
        // Match the Rust side: the binary links the dynamic UCRT/VCRUNTIME.
        .flag("/MD")
        .include(&hamakaze)
        .include(&source)
        .include(source.join("Shared"))
        // Redirect printf_s/vprintf_s to the Rust log callback (see
        // kdu/ks_kdu_log.h); absolute so cl resolves it regardless of
        // the per-file include path.
        .flag(format!("/FI{}", log_header.display()))
        .file(&bridge);

    // MASM shellcode stubs (vcxproj masm.targets equivalent): referenced
    // by the DSE/zemana providers through KDUProviderCreate's dispatch.
    build.object(assemble_shellmasm(&hamakaze));

    for rel in KDU_SOURCES {
        build.file(hamakaze.join(rel));
    }

    build.compile("kskdu");
}

fn main() {
    build_kdu();
}
