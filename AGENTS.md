# kernel-script Agent Guide

[Project Home](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [中文 README](https://github.com/lipeilin2006/kernel-script/blob/main/README_CN.md) | [English Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md) | [Driver Notes](https://github.com/lipeilin2006/kernel-script/blob/main/ks-driver/README.md)

## Project Scope

`kernel-script` is a Windows-only Rust workspace with four crates:

- `ks-core`: shared `no_std` protocol and ABI definitions.
- `ks-driver`: `no_std` WDM kernel driver. It performs target-process memory reads and writes.
- `ks-service`: SYSTEM user-mode service. It owns the Named Pipe IPC server, driver handle, driver request dispatch, the user-mode process enumeration, and the memory lock table with its periodic rewrite task.
- `ks-gui`: user-mode egui/eframe OpenGL GUI and Lua runtime. It owns the Lua VM, synchronous Named Pipe IPC client, draw command pipeline, and the Lua config store (`config.json`, module `ks-gui/src/config_store.rs`).
- `ks-launcher`: elevated egui GUI with ordered `Start Driver`, `Start Service`, and `Start GUI` actions.

`ks-launcher` uses `sc.exe` to start the driver and service, then launches the
GUI. It performs no file installation or copying; all errors and command output
are written to its log file.

Clicking a Start button renames that component's file (`ks-driver.sys`,
`ks-service.exe`, or `ks-gui.exe`) to a fresh random name before the service
registration or process spawn references it, and every service creation
registers a fresh random SCM name (passed to `ks-service` through
`--service-name` in binPath, because the windows-service dispatcher requires
the registered name). Once a component stops, its file is renamed back to
the canonical name (retried once per second until it succeeds; the driver
image can stay locked for a short moment after the service reports
STOPPED). All names
live in `ks-launcher.state` next to the launcher so stop/cleanup works
across restarts; do not delete that file while components are running.

The intended data flow is:

```text
Lua (synchronous call)
    -> ks-gui blocking Named Pipe IPC
    -> ks-service Tokio Named Pipe server
    -> driver worker / DeviceIoControl
    -> ks-driver
```

Process enumeration and process-name-to-PID lookup are service responsibilities. Do not add process enumeration back to the kernel driver unless there is a documented kernel-only requirement.

## Workspace Rules

- Keep `ks-core` dependency-free and `#![no_std]` compatible.
- Do not add Tokio, Lua, GUI, or user-mode Windows APIs to `ks-core` or `ks-driver`.
- Keep the driver limited to memory operations and the minimum required IOCTL surface.
- The driver device uses an explicit SYSTEM-only DACL (`D:P(A;;GA;;;SY)`). The service runs as SYSTEM; administrators and standard users must not open the device directly.
- The driver also binds the first successful device opener to its `EPROCESS`; subsequent create/control requests from another process are rejected. This is defense in depth, not a replacement for a service-specific DACL.
- Use explicit little-endian wire encoding. Do not expose Rust struct layout on the TCP protocol.
- Validate lengths, counts, addresses, PIDs, and frame sizes at every trust boundary.
- Use `windows-sys` with narrow feature lists when possible.
- Use `zerocopy` only for validated fixed-layout driver-local ABI views. Keep
  `ks-core` dependency-free and keep TCP/Named Pipe payloads explicitly
  little-endian and length-checked.
- Do not reintroduce removed synchronous Lua APIs. GUI Lua IPC APIs must remain synchronous.
- Do not call blocking network operations, `block_on`, or synchronous driver operations from the GUI render thread.
- Lua VM objects must only be accessed by the GUI Lua thread. Never send `Lua`, `Thread`, `Function`, or registry keys to worker threads.
- Lua scripts have no filesystem access. All persistent script state goes
  through the `config` API (`ks-gui/src/config_store.rs`): typed scalar
  entries only, bounded sizes, debounced atomic writes to `config.json`.
- Background workers may send only task IDs and owned plain data back to the GUI thread.

## GUI and Lua Lifecycle

The GUI frame lifecycle is:

```text
check_hot_reload
    -> OnUpdate (calculation, IPC, UI, drawing)
    -> Lua GC
```

Rules:

- `OnUpdate` is the only per-frame Lua callback and runs once per GUI frame. It performs calculations,
  optional synchronous memory operations, UI calls, and drawing.
- All memory API calls are synchronous and block the Lua thread for ~60-100μs.
- Hot reload destroys the old Lua VM.
- Multiple Lua scripts are loaded from `scripts/*.lua`; they run on the GUI Lua thread.

Supported synchronous Luau operations include:

```lua
memory.get_pid(name)
memory.get_process_base(pid)
memory.read_i32(pid, address)
memory.read_bytes(pid, address, size)
memory.write_i32(pid, address, value)
memory.write_bytes(pid, address, data)
memory.read_rva(pid, relative_address, size)
memory.write_rva(pid, relative_address, data)
memory.read_mdl(pid, address, size)
memory.write_mdl(pid, address, data)
memory.read_mdl_rva(pid, relative_address, size)
memory.write_mdl_rva(pid, relative_address, data)
memory.batch_read(pid, size, addresses)
memory.batch_offset(sizes)
memory.batch_write(pid, writes)
memory.traverse_pointer_chain(pid, base, offsets)
memory.lock(id, pid, address, data)
memory.unlock(id)
memory.unlock_all(pid)
memory.lock_rva(id, pid, relative_address, data)
memory.unlock_rva(id)
keyboard.is_key_down(key)
keyboard.is_key_up(key)
keyboard.is_key_press(key)
config.set(key, value)
config.get(key, default)
config.remove(key)
config.save()
```

Typical usage:

```lua
local pid = memory.get_pid("notepad.exe")
local base = memory.get_process_base(pid)
local value = memory.read_i32(pid, "0x1407FFF0")
print(value)
```

## IPC and Protocol

The GUI-to-service transport is the local Windows Named Pipe `\\.\pipe\KernelScript`.

- `ks-service` uses Tokio Windows Named Pipes and `BytesMut` for asynchronous framed reads.
- The pipe rejects remote clients and uses a bounded four-instance server. The
  transport type alone is not authentication; keep its Windows security
  descriptor restrictive if the service launch model changes.
- Complete frames should be transferred with `BytesMut::split_to(...).freeze()` where ownership is needed.
- Do not use `payload.to_vec()` merely to extend a frame lifetime.
- The service uses a blocking boundary for synchronous `DeviceIoControl` calls. This is expected; the GUI must never observe that blocking operation.
- Process enumeration uses Windows Toolhelp APIs in `ks-service/src/process.rs`.
- The current process list wire response exposes `pid` and `name`. The service's internal Toolhelp record also collects `parent_pid` and `thread_count`; extend the wire format before exposing those fields to clients.

The current driver ABI limits one memory read or write to `4096` bytes. Keep GUI and service validation aligned with the driver limit.

## Driver Build

Normal workspace checks do not generate the native driver image:

```powershell
cargo check --workspace
cargo test --workspace
cargo build --release --workspace
```

Build the GUI with the unwind-enabled profile when runtime panic recovery is
required:

```powershell
cargo build --profile gui-release -p ks-gui
```

The normal release profile intentionally uses `panic = "abort"` for fail-fast
components such as the driver and must not be used when GUI `catch_unwind`
recovery is required.

For a WDK driver build, use a Visual Studio Developer Command Prompt and set the WDK variables explicitly. The known working WDK configuration is `10.0.26100.0`:

```powershell
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'

$vs = 'C:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat'
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && cargo build -p ks-driver --bin ks-driver --features wdk"
```

The build script compiles `seh_shim.c` with MSVC and links the Native-subsystem driver image. The C shim contains the SEH boundary around `MmProbeAndLockPages` and the kernel-link compatibility symbols required by the Rust MSVC output:

- `_fltused`
- `__CxxFrameHandler3`

Do not link the user-mode CRT into the driver. Do not replace the handler with an incompatible zero-argument function.

The secure device wrapper uses `WdmlibIoCreateDeviceSecure` and links the WDK
`wdmsec` and `BufferOverflowK` libraries. Keep this dependency in the WDK-only
driver build path.

All driver memory traffic uses the ordinary memory IOCTLs with plain,
explicit little-endian fields. Sensitive integer fields, sizes, counts, and
data are transmitted without obfuscation.

Memory locks live entirely in `ks-service` (`driver_comm.rs`): a dedicated
OS thread (`lock_rewrite_loop`, spawned by `run_lock_worker`) applies every
entry with a single `IOCTL_WRITE_MEMORY_BATCH` per sweep in a continuous
spin, with no inter-sweep sleep; it also runs at `THREAD_PRIORITY_BELOW_NORMAL`
so the spinning thread can never starve the game or the service. Do not
replace the batching with per-lock IOCTLs and do not use `yield_now`
instead of pure spinning: full-speed sweeps with the batch write keep the
per-lock rate at the IOCTL round-trip bound, and both per-sweep sleeps and
yields were observed to either cap the rate or add cache-polluting context
switches. The batch write (`IOCTL_WRITE_MEMORY_BATCH`, 0x0022_203C) carries up to
`MAX_BATCH_WRITE_ENTRIES` (64) entries of
`u64 pid, u64 address, u32 size, u32 pad, data` in one kernel transition
and returns one NTSTATUS per entry; per-entry write failures are ignored by
the sweep. Performance notes: the rewriter caches the encoded batch request
keyed on a lock-table version counter (`LOCKS_VERSION`, bumped on every
mutation), so sweeps between mutations submit the cached buffer with no
table clone, no re-encode, and no table lock; the driver resolves each
distinct PID once per batch (`memory::batch_write_process_memory`) instead
of running `PsLookupProcessByProcessId` per entry. The driver keeps
no lock state and exposes no lock IOCTLs. Lock limits (64 entries, max 4096
bytes each) are enforced in the service.
Inspect the native driver image with platform linker tools before loading it.
Driver signing and VM deployment are environment-specific and are outside the
workspace source tree.

## Verification Checklist

Before considering a change complete:

1. Run `cargo fmt --all`.
2. Run `cargo test --workspace`.
3. Run `cargo check --workspace`.
4. For driver changes, build `ks-driver --bin ks-driver --features wdk` using WDK 26100.
5. Check `cargo tree -e features` when changing dependencies.
6. Search for stale synchronous Lua calls after changing the Lua API.
7. If artifacts are deployed to the VM, verify SHA256 hashes.
8. Do not load an unverified native driver image in a test environment.

## Known Warnings and Limitations

- An `IOCTL_ALLOC_MEM` (`0x80001040`) target-process allocation feature was
  attempted twice and removed entirely. Both implementations produced a
  driver that imported `ZwAllocateVirtualMemory`/`ZwClose` from
  `ntdll.dll` (the WDK km `ntoskrnl.lib` has no `__imp_` stubs for them, so
  dllimport references fall through to the SDK user-mode `ntdll.lib`), and
  the kernel loader cannot resolve `ntdll.dll` as a driver dependency
  (StartService failed while the pre-change build loaded fine). If target
  process allocation is ever re-attempted, verify the built image with
  `dumpbin /imports` contains no `ntdll.dll` before deploying, and route
  kernel calls through `seh_shim.c` `ks_*` wrappers. All alloc wire
  protocol, service, GUI, and Lua API code has been removed.
- A kernel-side lock worker (system thread + kernel lock table +
  `IOCTL_LOCK_MEMORY*`) was also removed after repeated bugchecks in the
  field. A system thread performing periodic writes through
  `MmCopyVirtualMemory` bugchecked on real game targets even though the
  identical `IOCTL_WRITE_MEMORY` path from the service process was stable.
  Locks must stay service-side (`run_lock_worker` in `driver_comm.rs`);
  do not reintroduce kernel background writers.
- The driver internally exposes normal memory I/O
  (`IOCTL_READ_MEMORY`/`IOCTL_WRITE_MEMORY` and their RVA variants) alongside
  separate MDL-remap I/O (`IOCTL_READ_MEMORY_MDL`/`IOCTL_WRITE_MEMORY_MDL`
  and their RVA variants); callers must use the plain little-endian field
  encoding because that encoding is now the normal IOCTL ABI.
  MDL access attaches to the target, probes the MDL with read access only,
  locks pages, and maps them into kernel space so writes bypass user-mode
  page protection (code sections, read-only data). MDL writes hit the shared
  physical page: image-section edits are visible to every process mapping
  that image. Keep both paths independent; do not silently fall back between
  them.
- `ks-driver` is a Native-subsystem kernel image and cannot be validated by running it as a normal user-mode executable.
- Process names and process metadata are collected in user mode by Toolhelp; a PID is not a permanent process identity because Windows can reuse PIDs.
- The GUI uses egui/eframe with the `glow` OpenGL backend. The native eframe window is required as the OpenGL host and currently uses the default opaque window configuration.
- The GUI loads the first available `msyh.ttc`, `simsun.ttc`, or `simhei.ttf` from `C:\Windows\Fonts` so Chinese Lua/UI text renders on Windows.
- Do not claim that an unsigned driver is VM-loadable merely because it compiled successfully.
