# Kernel Script

[Home](https://github.com/lipeilin2006/kernel-script) | [中文 README](https://github.com/lipeilin2006/kernel-script/blob/main/README_CN.md) | [Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md)

## Usage

1. Place `ks-gui.exe` in a directory with its `scripts` folder (the driver
   image is embedded in the executable; nothing is installed and no target
   image ever reaches the disk).
2. Run `ks-gui.exe` as administrator. The driver starts silently at
   launch: the embedded image is mapped in-process through the in-process
   KDU mapper — no service, no signing — and a live instance left by a
   previous run is reused instead.
3. Click `Stop` to close the GUI; it shuts the driver down silently on
   the way out.

There is no user-mode service: the GUI talks to the driver directly through
a shared-memory ring. For standalone driver diagnostics run `ks-test.exe`
(full-suite mode) from an elevated terminal. The GUI requires an interactive
desktop session because it uses OpenGL.

Kernel Script is a Windows-only Rust workspace for running Luau scripts over
a shared-memory ring transport and a WDM driver. The intended data flow is:

```text
Lua (synchronous call)
    -> ks-gui sync_ipc (blocking)
    -> ks-sdk re-export -> ks-link ring round trip (mutex + section + events)
    -> ks-driver worker system thread
        -> request execution (target-process memory access, lock table
           mutation)
        -> between requests: rewrite one lock entry per loop pass
           (table empty: sleep on the request event)
```

## Features

- Luau JIT scripts with hot reload and per-callback execution budgets.
- Synchronous process lookup, module-base lookup, memory read/write, RVA, MDL,
  batch-read, batch-write, and pointer-chain APIs.
- Script config persistence through a `config` API (`config.json`).
- Keyboard input API (`is_key_down` / `is_key_up` / `is_key_press`) that works
  while the game owns input focus.
- Continuous memory locks: each lock call mutates a driver-side table in
  one round trip; the worker thread replays entries between requests
  (polls the request event while locks are held so requests win, blocks on
  it when the table is empty so an idle driver burns no CPU).
- EgUI transparent overlay with cached draw commands.
- User-mode process enumeration and process-name-to-PID lookup in `ks-link`
  (Toolhelp), re-exported by `ks-sdk`.
- `ks-sdk` facade: re-exports the whole link API at its crate root and owns
  the driver lifecycle: `ks_sdk::start()` maps the embedded `ks-driver.sys`
  through the in-process KDU mapper (no service, no signature, no file on
  disk for the target image) and `ks_sdk::stop()` shuts it down again
  (building it requires MSVC).
- Kernel section/events protected by a hand-built DACL that grants SYSTEM and
  Administrators; the ring state machine plus per-layer validation guards
  every request.
- Postcard wire format (LEB128 varints + little-endian fixed fields) shared
  through `ks-core`, versioned by `RING_VERSION`.
- The GUI owns the driver lifecycle: a startup probe starts the driver
  silently at launch and `driver::finish_on_exit` stops it again when the
  window closes, through `ks_sdk::start()`/`ks_sdk::stop()` on a
  background worker, and `sync_ipc` refuses memory calls while such a job
  is in flight.
- `ks-test.exe` runs through `ks_sdk::start()`/`stop()` with the embedded
  driver (legacy `ks-test sc` uses `sc.exe` instead), polls
  `HKLM\SOFTWARE\KernelScript` for the published object names, runs the full
  correctness suite and read/write benchmarks against its own process, sends
  `shutdown`, then verifies the single-instance claim is released, the
  published object names are gone, and the driver maps again. Run it elevated.

## Workspace Structure

```text
kernel-script/
├── Cargo.toml
├── README.md / README_CN.md
├── document.md / document_CN.md
├── AGENTS.md
├── ks-core/
│   └── src/
│       ├── protocol.rs          # no_std wire protocol (Request/Response)
│       └── ring.rs              # ring layout, state machine, name constants
├── ks-driver/
│   ├── build.rs                 # WDK-only linker and SEH shim setup
│   ├── seh_shim.c               # kernel SEH and compatibility wrappers
│   └── src/
│       ├── comm.rs              # named section/events, worker, registry names
│       ├── lock.rs              # driver-side lock table (worker replays it)
│       ├── request.rs           # request decode, validation, dispatch
│       ├── memory/              # normal, MDL, batch, and pointer-chain access
│       └── wdm.rs               # WDK FFI declarations
├── ks-link/
│   └── src/
│       ├── lib.rs               # session, synchronous ring round trip
│       ├── lock.rs              # lock API (one round trip per mutation)
│       └── process.rs           # Toolhelp process enumeration
├── ks-sdk/
│   ├── build.rs                 # in-process KDU mapper build (MSVC)
│   ├── kdu/                     # ks_bridge.cpp + printf hook header
│   ├── assets/                  # provider database + embedded driver image
│   └── src/                     # start/stop loader + ks-link API re-export
├── ks-gui/
│   └── src/
│       ├── app.rs               # overlay application and frame rendering
│       ├── lua_runtime.rs       # Lua VM lifecycle and API registration
│       ├── lua_runtime/         # engine API, execution, keyboard, and runtime types
│       ├── config_store.rs      # Lua config persistence (config.json)
│       ├── driver.rs            # driver status window + sync_ipc gate
│       ├── overlay.rs            # generic draw-command painter bridge
│       ├── sync_ipc.rs          # synchronous facade over ks-sdk
│       └── window_util.rs       # target-window geometry lookup
└── ks-test/
    └── src/main.rs              # embedded-driver correctness + benchmark harness
```

## Lua Lifecycle

Each `.lua` file loaded by the GUI runs in its own Luau VM:

```lua
function OnStart() end
function OnUpdate(dt) end
function OnDestroy() end
```

- `OnStart` runs once after loading.
- `OnUpdate` is the only per-frame callback. The whole overlay (rendering plus
  OnUpdate) is capped at 60 Hz; a slow callback simply lowers the frame rate.
  Its `delta_time` argument is the elapsed time since the previous frame in
  seconds, or `0` while the engine is paused.
  It performs calculations, optional synchronous memory operations, UI calls,
  and drawing in one Lua invocation. Budgets are advisory warnings, never
  errors.
- The callback runs on the GUI Lua thread; each memory API call blocks for one
  ring round trip (~10-15 us), so scripts should keep work bounded.
- `OnDestroy` runs during hot reload and shutdown.

All memory functions are synchronous and execute on the GUI Lua thread. The
full API surface includes `memory.*` (read/write, RVA, MDL, batch read,
pointer chain, locks), `keyboard.*` (`is_key_down`, `is_key_up`,
`is_key_press` — works without window focus), `config.*` (persisted
`config.json` entries), `ui.*` (egui widgets), `draw.*` (overlay drawing),
and `engine.*` (timing and pause control). See `document.md` or
`document_CN.md` for the complete API reference.

## Build And Test

The normal workspace build checks user-mode crates and the driver framework
(it requires MSVC: `ks-sdk` compiles the in-process KDU mapper with `cc`, and
`ks-gui`/`ks-test` depend on it):

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
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && cargo build --release -p ks-driver --bin ks-driver --features wdk"
```

Always pass `--release`: a dev-profile image overflows the worker system
thread's kernel stack on the first request and bugchecks. Inspect the native
image with `dumpbin` before testing. Confirm x64 machine type, Native
subsystem, `DriverEntry`, and that the import table lists `ntoskrnl.exe`
only.

## Running

The driver's section and event objects are created by the kernel driver
itself and protected by a DACL that grants SYSTEM and Administrators, so the
GUI (which embeds a `requireAdministrator` manifest) runs elevated.

Run the GUI from an interactive desktop session because OpenGL requires a
window station. Lua scripts are loaded from the `scripts` directory beside
the GUI executable. Files whose stem begins with `_` are kept available for
manual testing but are not loaded by default.

Press `Insert` at any time to show or hide the egui script windows. The
`draw.*` overlay layer and all script calculations keep running while the UI
is hidden.

The `KernelScript` window shows the current driver state (`probing...`,
`starting...`, `running`, or the start error in red) above a scrollable
startup log (the lifecycle narrative plus the provider attempts while the
in-process mapper walks its fallback chain) and the GUI's only
control, the `Stop` button: it closes the overlay, and once the render
loop returns `ks-gui` shuts the driver down silently (`ks_sdk::stop()`)
and drops the process-wide session. At launch a startup probe reuses a
live driver or starts the embedded image in-process on a background
worker — all lifecycle detail goes to `ks-gui.log`, and Lua memory calls
fail immediately with "the driver is starting" while such an operation
is in flight.

## Security Boundaries

- `ks-core` stays `no_std`, allocation-free and Windows-free (postcard and
  heapless only).
- The driver performs memory operations and holds the memory lock table;
  process enumeration stays in `ks-link`.
- Kernel section/events have a hand-built DACL granting SYSTEM and
  Administrators; there is no device object and no IOCTL entry point.
- Every boundary validates lengths, counts, addresses, PIDs, and sizes
  (`Request::validate` in ks-core, again in the driver, again in ks-link).
- GUI Lua objects never cross worker-thread boundaries.

## License

This project is for educational purposes only.
