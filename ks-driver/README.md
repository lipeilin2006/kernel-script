# ks-driver WDK build notes

`ks-driver` contains the Rust WDM framework and intentionally keeps the
structured-exception boundary in `seh_shim.c`. Build that file with MSVC and
the Windows Driver Kit, then link it into the `.sys` target. Rust cannot use
`catch_unwind` to catch kernel SEH exceptions.

The Rust crate is a `no_std` source framework. Its `wdm.rs` declarations must
be replaced by bindings generated from the exact WDK/Windows target used for
the driver before shipping; WDM structure layouts are ABI-sensitive and must
not be guessed across Windows versions.

The implementation is fail-closed:

- `IRP_MJ_DEVICE_CONTROL` checks every METHOD_BUFFERED length before reading
  `SystemBuffer`.
- Process references are released by `ProcessRef`.
- Locked MDLs are unlocked and freed by `LockedMdl` on every return path.
- `MmProbeAndLockPages` is called only through the MSVC SEH shim.
- The old CR3/physical-memory access path is not part of this driver.

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

Unset `KS_DRIVER_WDK` and omit `--features wdk` for framework-only `cargo check`.
The WDK build has a dedicated `no_std` binary target and writes `ks-driver.sys`.
The final packaging step must provide an INF and apply a valid test signature
before installation.
