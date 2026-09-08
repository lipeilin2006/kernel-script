# kernel-script Agent Guide

## Project Scope

`kernel-script` is a Windows-only Rust workspace with four crates:

- `ks-core`: shared `no_std` protocol and ABI definitions.
- `ks-driver`: `no_std` WDM kernel driver. It performs target-process memory reads and writes.
- `ks-service`: SYSTEM user-mode service. It owns the TCP IPC server, driver handle, driver request dispatch, and user-mode process enumeration.
- `ks-gui`: user-mode egui/eframe OpenGL GUI and Lua runtime. It owns the Lua VM, coroutine scheduler, and asynchronous Named Pipe client.
- `ks-installer`: elevated egui GUI that manages the driver and service with `sc.exe` commands only.

`ks-installer` is an elevated egui GUI that manages the driver and backend
service with `sc.exe` only (`create`/`start`/`stop`/`delete`). It copies no
files; driver, service, GUI, and installer must sit in the same directory.

The deployment package uses this flat layout:

```text
driver-package/
├── ks-driver.sys
├── ks-service.exe
├── ks-gui.exe
└── ks-installer.exe
```

The intended data flow is:

```text
Lua coroutine / egui
    -> ks-gui Tokio TCP client
    -> ks-service Tokio TCP server
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
- Do not reintroduce removed synchronous Lua APIs. GUI Lua IPC APIs must remain asynchronous.
- Do not call blocking network operations, `block_on`, or synchronous driver operations from the GUI render thread.
- Lua VM objects must only be accessed by the GUI Lua thread. Never send `Lua`, `Thread`, `Function`, or registry keys to worker threads.
- Background workers may send only task IDs and owned plain data back to the GUI thread.

## GUI and Lua Lifecycle

The GUI frame lifecycle is:

```text
check_hot_reload
    -> poll/resume async Lua coroutines
    -> OnUpdate
    -> OnRender
    -> Lua GC
```

Rules:

- `OnRender` must only draw UI and read cached results.
- `OnUpdate` may start or poll asynchronous work but must not wait for network completion.
- Use `memory.async_*` plus `await_async` for process and memory operations.
- `start_async` creates a Lua coroutine and the Rust-side scheduler resumes it on later frames.
- Hot reload destroys the old Lua VM and therefore invalidates all old Lua coroutines.
- Multiple Lua scripts are loaded from `scripts/*.lua`; they run on the GUI Lua thread and share only scalar values explicitly stored through `shared.set/get/delete`.
- Do not let a coroutine hold an egui UI borrow across a yield.

Supported asynchronous Lua operations include:

```lua
memory.async_read_i32(pid, address)
memory.async_read_bytes(pid, address, size)
memory.async_read_rva(pid, relative_address, size)
memory.async_write_i32(pid, address, value)
memory.async_write_bytes(pid, address, data)
memory.async_write_rva(pid, relative_address, data)
memory.async_read_mdl(pid, address, size)
memory.async_write_mdl(pid, address, data)
memory.async_read_mdl_rva(pid, relative_address, size)
memory.async_write_mdl_rva(pid, relative_address, data)
memory.async_get_process_base(pid)
memory.async_list_processes()
memory.poll_async(task_id)
```

Typical usage:

```lua
start_async(function()
    local pid = await_async(memory.async_get_pid("notepad.exe"))
    local value = await_async(memory.async_read_i32(pid, "0x1407FFF0"))
    print(value)
end)
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

The current driver ABI limits one memory read or write to `256` bytes. Keep GUI and service validation aligned with the driver limit.

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

The generated driver is written by the current linker to:

```text
D:\kernel-script\ks-driver.sys
```

Inspect the result before deployment:

```powershell
dumpbin /headers D:\kernel-script\ks-driver.sys
```

Expected properties include x64 machine type, Native subsystem, and `DriverEntry` as the entry point.

## VirtualBox Test Workflow

The VM is test-only. Compilation happens on the host.

- Host source: `D:\kernel-script`.
- Host deployment directory: `D:\kernel-script\driver-package`.
- VM host-only/shared-folder mapping: `Z:\ = \\VBoxSvr\ksdriver`.
- VM SSH endpoint: `Admin@192.168.11.101`.

After a successful host build, copy only intended artifacts to the package directory:

```powershell
Copy-Item D:\kernel-script\ks-driver.sys D:\kernel-script\driver-package\ks-driver.sys -Force
Copy-Item D:\kernel-script\ks-driver.pdb D:\kernel-script\driver-package\ks-driver.pdb -Force
Copy-Item D:\kernel-script\target\release\ks-service.exe D:\kernel-script\driver-package\ks-service.exe -Force
Copy-Item D:\kernel-script\target\release\ks-gui.exe D:\kernel-script\driver-package\ks-gui.exe -Force
Copy-Item D:\kernel-script\target\release\ks-installer.exe D:\kernel-script\driver-package\ks-installer.exe -Force
Copy-Item D:\kernel-script\target\release\ks_service.pdb D:\kernel-script\driver-package\ks-service.pdb -Force
Copy-Item D:\kernel-script\target\release\ks_gui.pdb D:\kernel-script\driver-package\ks-gui.pdb -Force
Copy-Item D:\kernel-script\target\release\ks_installer.pdb D:\kernel-script\driver-package\ks-installer.pdb -Force
```

Keep every `.exe`/`.sys` paired with its matching `.pdb`; symbols only load
when the PDB matches the binary build.

Verify hashes on both host and VM before testing. The VM must have test signing enabled and the driver must be signed with a certificate for which the signing private key is available. A `.cer` file alone cannot sign a driver.

Typical VM driver service setup, from an elevated shell:

```cmd
copy /y Z:\ks-driver.sys C:\ks-test\ks-driver.sys
sc.exe create ks-driver type= kernel start= demand binPath= C:\ks-test\ks-driver.sys
sc.exe start ks-driver
sc.exe query ks-driver
```

If the service already exists, use `sc.exe config` instead of `create`. Error `577` means signature verification failed; it is not a Rust or IPC error.

Run the service in console mode for IPC testing:

```cmd
Z:\ks-service.exe --console
```

The GUI should be run from the VM desktop session, not from an SSH session, because OpenGL/Winit needs an interactive display.

## Verification Checklist

Before considering a change complete:

1. Run `cargo fmt --all`.
2. Run `cargo test --workspace`.
3. Run `cargo check --workspace`.
4. For driver changes, build `ks-driver --bin ks-driver --features wdk` using WDK 26100.
5. Check `cargo tree -e features` when changing dependencies.
6. Search for stale synchronous Lua calls after changing the Lua API.
7. If artifacts are deployed to the VM, verify SHA256 hashes.
8. Do not overwrite a known-good driver artifact with a failed or unsigned build.

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
- The driver exposes normal memory I/O (`IOCTL_READ_MEMORY`/`IOCTL_WRITE_MEMORY`
  and their RVA variants) alongside separate MDL-remap I/O
  (`IOCTL_READ_MEMORY_MDL`/`IOCTL_WRITE_MEMORY_MDL` and their RVA variants).
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
