# Kernel Script

Kernel Script is a Windows-only Rust workspace for running Luau scripts over a
protected user-mode service and WDM driver. The project is split into four
runtime layers:

```text
Luau script
    -> ks-gui synchronous Named Pipe client
    -> ks-service Tokio Named Pipe server
    -> DeviceIoControl
    -> ks-driver WDM memory operations
```

## Features

- Luau JIT scripts with hot reload and per-callback execution budgets.
- Synchronous process lookup, module-base lookup, memory read/write, RVA, MDL,
  batch-read, and pointer-chain APIs.
- EgUI/GLFW transparent overlay with cached draw commands.
- User-mode process enumeration in `ks-service`.
- SYSTEM-only driver device access with first-opener process binding.
- Explicit little-endian framed IPC protocol shared through `ks-core`.

## Workspace Structure

```text
kernel-script/
├── Cargo.toml
├── README.md / README_CN.md
├── document.md / document_CN.md
├── AGENTS.md
├── ks-core/
│   └── src/protocol.rs          # no_std wire protocol and ABI definitions
├── ks-driver/
│   ├── build.rs                 # WDK-only linker and SEH shim setup
│   ├── seh_shim.c               # kernel SEH and compatibility wrappers
│   └── src/
│       ├── dispatch.rs          # WDM dispatch and IOCTL validation
│       ├── memory/              # normal, MDL, batch, and pointer-chain access
│       └── wdm.rs               # WDK FFI declarations
├── ks-service/
│   └── src/
│       ├── main.rs              # service and console entry points
│       ├── driver_comm.rs       # DeviceIoControl client
│       ├── ipc.rs               # asynchronous Named Pipe server
│       └── process.rs           # Toolhelp process enumeration
├── ks-gui/
│   └── src/
│       ├── app.rs               # overlay application and frame rendering
│       ├── lua_runtime.rs       # Lua VM lifecycle and API registration
│       ├── lua_runtime/         # execution, scheduler, and runtime types
│       ├── sync_ipc.rs          # synchronous Named Pipe client
│       └── window_util.rs       # target-window geometry lookup
├── ks-installer/
│   └── src/main.rs              # elevated sc.exe service manager
└── ks-test/
    └── src/main.rs              # standalone IPC benchmark client
```

## Lua Lifecycle

Each `.lua` file loaded by the GUI runs in its own Luau VM:

```lua
function OnStart() end
function OnUpdate(dt) end
function OnRender() end
function OnDestroy() end
```

- `OnStart` runs once after loading.
- `OnUpdate` runs on the fixed logic scheduler and may perform synchronous memory
  operations.
- `OnRender` should draw UI and consume cached state. Do not put repeated
  blocking IPC work in UI callbacks.
- `OnDestroy` runs during hot reload and shutdown.

All memory functions are synchronous and execute on the GUI Lua thread. See
`document.md` or `document_CN.md` for the complete API reference.

## Build And Test

The normal workspace build checks user-mode crates and the driver framework:

```powershell
cargo fmt --all
cargo test --workspace
cargo check --workspace
cargo build --release --workspace
```

Build the GUI with unwind support when runtime panic recovery is required:

```powershell
cargo build --profile gui-release -p ks-gui
```

Build the native driver separately from a Visual Studio Developer Command
Prompt. The current supported WDK version is `10.0.26100.0`:

```powershell
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'

$vs = 'C:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat'
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && set `"KS_DRIVER_WDK=1`" && set `"WDK_ROOT=C:\Program Files (x86)\Windows Kits\10`" && set `"WDK_LIB=C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64`" && set `"WDK_VERSION=10.0.26100.0`" && cargo build --release -p ks-driver --bin ks-driver --features wdk"
```

Inspect the native image with `dumpbin` before testing. Confirm x64 machine
type, Native subsystem, `DriverEntry`, and no user-mode DLL imports.

## Running

The service must run as SYSTEM to open the driver device. For interactive
service diagnostics, run its console mode from an elevated environment:

```cmd
ks-service.exe --console
```

Run the GUI from an interactive desktop session because GLFW/OpenGL requires a
window station. Lua scripts are loaded from the `scripts` directory beside the
GUI executable. Files whose stem begins with `_` are kept available for manual
testing but are not loaded by default.

The installer only manages service registration with `sc.exe`; it does not
copy source files or manage build artifacts.

## Security Boundaries

- `ks-core` remains dependency-free and `no_std` compatible.
- The driver performs memory operations only; process enumeration stays in the
  service.
- The driver device has a SYSTEM-only DACL and binds requests to its first
  successful opener process.
- Every protocol, service, and driver boundary validates lengths, counts,
  addresses, PIDs, and frame sizes.
- GUI Lua objects never cross worker-thread boundaries.

## License

This project is for educational purposes only.
