# ks-driver WDK build notes

[Project Home](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [中文 README](https://github.com/lipeilin2006/kernel-script/blob/main/README_CN.md) | [Agent Guide](https://github.com/lipeilin2006/kernel-script/blob/main/AGENTS.md)

`ks-driver` contains the Rust WDM framework and intentionally keeps the
structured-exception boundary in `seh_shim.c`. Build that file with MSVC and
the Windows Driver Kit, then link it into the native driver target. Rust cannot use
`catch_unwind` to catch kernel SEH exceptions.

There is no device object and no IOCTL dispatch: the driver creates one
named section and two named events, maps the ring into system space, and
services requests from a single worker system thread (`comm.rs`,
`request.rs`) that also replays the driver-side memory lock table between
requests (`lock.rs`). Security is the section/event DACL (SYSTEM +
Administrators), the ring state machine, and the single-instance marker
object (`comm.rs`).

The Rust crate is a `no_std` source framework. Its `wdm.rs` declarations must
be replaced by bindings generated from the exact WDK/Windows target used for
the driver before shipping; WDM structure layouts are ABI-sensitive and must
not be guessed across Windows versions. Every `Zw*`/`Rtl*`/`Ps*`/`Ob*`
extern used by Rust code must live in the `ntoskrnl.exe` `raw-dylib`
`verbatim` link block — see the driver import rules in `AGENTS.md`.

The implementation is fail-closed:

- Requests are postcard-decoded from the bounded request slot and validated
  in `ks-core` (`Request::validate`) and again before every operation.
- Process references are released by `ProcessRef`.
- Locked MDLs are unlocked and freed by `LockedMdl` on every return path.
- `MmProbeAndLockPages` is called only through the MSVC SEH shim.
- The old CR3/physical-memory access path is not part of this driver.
- Batch sizes, single-operation sizes (4096-byte cap) and chain lengths are
  bounded on every layer.
- Lock-table limits (64 entries, 4096 bytes each) are re-checked at insert;
  a full table answers `STATUS_QUOTA_EXCEEDED`.
- A second driver load (SCM service or manual mapper) is rejected by the
  single-instance marker object before the ring exists; the `Instance`
  registry value under `HKLM\SOFTWARE\KernelScript` is diagnostic only.
- The three ring object names get a fresh 16-hex random token at every
  startup and are published to `HKLM\SOFTWARE\KernelScript`; clients
  resolve them from there at session open, and a publish failure fails
  the load (no fallback reaches randomized names). Teardown deletes the
  three values, the `Instance` claim and the key again, so a cleanly
  exited driver leaves no registry state behind (only a crash keeps
  stale values, and the next load overwrites them).

## Cargo + WDK build

The crate supports a direct Cargo build when invoked with the MSVC target from
a WDK developer prompt. `build.rs` compiles `seh_shim.c` with `cc`, links the
WDK import libraries, and emits the Native/Driver/entry-point linker options.

```powershell
$env:KS_DRIVER_WDK = "1"
$env:WDK_ROOT = "C:\Program Files (x86)\Windows Kits\10"
$env:WDK_LIB = "C:\Program Files (x86)\Windows Kits\10\Lib\10.0.xxxxx.0\km\x64"
cargo build -p ks-driver --features wdk --target x86_64-pc-windows-msvc --release
```

Always pass `--release`: a dev-profile image overflows the worker system
thread's kernel stack on the first request round trip and bugchecks.

Unset `KS_DRIVER_WDK` and omit `--features wdk` for framework-only `cargo check`.
The WDK build has a dedicated `no_std` binary target and produces a native
driver image as a local build output. Inspect and sign that image according to
the target test environment before installation.
