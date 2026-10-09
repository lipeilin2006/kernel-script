# kernel-script Agent Guide

[Project Home](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [中文 README](https://github.com/lipeilin2006/kernel-script/blob/main/README_CN.md) | [English Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md) | [Driver Notes](https://github.com/lipeilin2006/kernel-script/blob/main/ks-driver/README.md)

## Project Scope

`kernel-script` is a Windows-only Rust workspace with five crates:

- `ks-core`: shared `no_std`, allocation-free protocol and ring-layout
  definitions (postcard/serde/heapless only; no Windows API dependencies).
- `ks-driver`: `no_std` WDM kernel driver (Native subsystem). It creates the
  named section, the two named events, maps the ring into system space and
  services requests from one worker system thread; the same thread also
  replays the driver-side memory lock table between requests (polling the
  request event while locks are held, blocking on it while the table is
  empty). There is no device object, no IOCTL dispatch table and no IRP
  path.
- `ks-link`: user-mode client. Owns the section/event/mutex session, the
  synchronous round trip, the Toolhelp process enumeration, and the
  lock API surface (one round trip per mutation; the table lives in the
  driver). It is pure Rust and is consumed through `ks-sdk`.
- `ks-sdk`: the SDK facade. Re-exports the whole `ks-link` API at its
  crate root and owns the driver lifecycle: `ks_sdk::start()` maps the
  embedded target image (`ks-sdk/assets/ks-driver.sys`, `DRIVER_IMAGE`)
  in-process and `ks_sdk::stop()` shuts the driver down again — the
  pure-Rust mapper lives in the standalone sibling crate
  `dt-loader` — the private repo
  `https://github.com/lipeilin2006/driver-loader` (crate `dt-loader`,
  a port of the KDU 1.5.0 map core reached through the workspace
  `dt-loader` path dependency; the one C++-derived artifact is the
  extracted shellcode V3 machine code in
  `dt-loader/assets/shellcode_v3.bin`, produced once by
  `dt-loader/tools/shellcode_dump.cpp`). The loader-driver images live
  in `dt-loader/assets/drivers/` (7 provider blobs + 3 PROCEXP152
  victims); dt-loader takes the target image as a
  parameter and owns the mapper + loader assets, so this workspace
  builds only with that sibling checkout present. ks-sdk's `kdu`
  module is now the thin Kernel Script half (embedded `DRIVER_IMAGE`,
  publication wait, ring `stop`); `ks_sdk::start(provider)` takes an
  optional
  provider id: `Some(id)` runs exactly that provider, `None` walks the
  whole table in order until one maps the driver. Neither the target
  image nor anything else mapper-related
  is ever compiled: no `build.rs`, no `cc`, no MSVC requirement for the
  workspace build. `ks-gui` and `ks-test` depend on `ks-sdk`
  (not `ks-link` directly).
- `ks-gui`: user-mode egui/eframe OpenGL GUI and Lua runtime. It owns the
   Lua VM, the synchronous `sync_ipc` facade over `ks-sdk` (the re-exported
   link API), the draw command
   pipeline, and the Lua config store (`config.json`, module
   `ks-gui/src/config_store.rs`). Runs elevated (embedded
   `requireAdministrator` manifest) because the driver's section DACL only
    grants SYSTEM and Administrators. It also owns the driver lifecycle
    (`ks-gui/src/driver.rs`): a startup probe silently reuses a live
    driver or starts one through `ks_sdk::start()` on a background worker
    at launch, the GUI's single `KernelScript` window shows the current
    driver phase, a scrollable startup log (the lifecycle narrative plus
    the `trying provider <id>` lines from the ks_sdk log sink) and the
    `Stop` button (Stop only closes the overlay;
    the `ks_sdk::stop()` shutdown runs after the render loop returns, in
    `driver::finish_on_exit`), and `sync_ipc` is gated while a job can
    close or replace the process-wide session (`ks_link::close_session`).

The intended data flow is:

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

Process enumeration and process-name-to-PID lookup are `ks-link`
responsibilities (exposed through `ks-sdk`); the memory lock table lives in
the driver (see the lock
rules below). There is no user-mode service anymore; do not reintroduce
one.

## Workspace Rules

- Keep `ks-core` `#![no_std]`, allocation-free and Windows-free. The only
  allowed dependencies are `serde` (derive, `default-features = false`),
  `postcard` and `heapless`.
- Do not add user-mode Windows APIs to `ks-core` or `ks-driver`. Driver
  externs live in `ks-driver/src/wdm.rs` only.
- The driver performs memory reads/writes through the ring; it exposes no
  device interface. Its security boundary is the section/event DACL
  (hand-built, SYSTEM + Administrators, `GENERIC_ALL`) plus the ring state
  machine. The first-kernel-opener `EPROCESS` binding and the old
  device-object DACL are gone with the IOCTL path.
- The wire format is postcard (LEB128 varints + little-endian fixed
  fields) over the shared ring. Do not expose Rust struct layout, do not
  reintroduce the removed `zerocopy` ABI, and bump `RING_VERSION` whenever
  any layout or variant changes.
- Validate lengths, counts, addresses, PIDs and sizes at every trust
  boundary (`Request::validate` in ks-core, again in the driver operation
  layer, again in ks-link).
- Use `windows-sys` with narrow feature lists.
- Every kernel object handle the driver creates (ring section, ring
  events, worker thread) must be created with `OBJ_KERNEL_HANDLE`.
  `DriverEntry` can run in an arbitrary process context — the mapper
  runs in-process inside `ks-test` (`ks_sdk::start`), and the manual
  mapping path generally hands control to whatever process drove it — and
  handles in that process's
  table close when it exits, destroying the named ring objects while the
  driver stays loaded (the failure mode: registry names that point at
  objects that no longer exist). Registry-key and marker handles already
  follow this rule; keep every new `Zw*` create/open site consistent.
- Do not reintroduce removed synchronous Lua alternatives. GUI Lua memory
  APIs must remain synchronous and run on the GUI Lua thread only.
- Do not call blocking network operations, `block_on`, or driver round
  trips from the GUI render thread. `OnUpdate` may use `sync_ipc` only
  through Lua callbacks.
- Lua VM objects must only be accessed by the GUI Lua thread. Never send
  `Lua`, `Thread`, `Function`, or registry keys to worker threads.
- Lua scripts have no filesystem access. All persistent script state goes
  through the `config` API (`ks-gui/src/config_store.rs`): typed scalar
  entries only, bounded sizes, debounced atomic writes to `config.json`.
- Background workers may send only task IDs and owned plain data back to
  the GUI thread.

## GUI and Lua Lifecycle

The GUI frame lifecycle is:

```text
check_hot_reload
    -> OnUpdate (calculation, memory ops, UI, drawing)
    -> Lua GC
```

Rules:

- `OnUpdate` is the only per-frame Lua callback and runs once per GUI
  frame. It performs calculations, optional synchronous memory operations,
  UI calls, and drawing.
- All memory API calls are synchronous and block the Lua thread for one
  ring round trip (~60-100 us).
- Hot reload destroys the old Lua VM.
- Multiple Lua scripts are loaded from `scripts/*.lua`; they run on the
  GUI Lua thread.
- Driver start/stop lives in `ks-gui/src/driver.rs` and is silent: the
  startup probe spawns a background worker running `ks_sdk::start()` when
  no live instance answers, and `driver::finish_on_exit` runs
  `ks_sdk::stop()` after the render loop returns. The GUI shows the
  current phase in its `KernelScript` window (`probing...` /
  `starting...` / `running`, or the start error in red) above a
  scrollable startup log — the lifecycle narrative plus the
  `trying provider <id>` lines the ks_sdk log sink receives — and the
  `Stop` button, which only closes the overlay. While such a job is in flight
  every `sync_ipc` call fails immediately with "the driver is
  starting". The gate is what makes `ks_link::close_session`
  safe: the job may free the process-wide session on its way out, and the
  single-threaded Lua rule plus the flag guarantees no round trip is in
  flight when it does.

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

## Ring and Protocol

The transport is one named section plus two named events, all created by
the driver and re-opened by clients:

- Section: `\BaseNamedObjects\KernelScriptSection` (client:
  `Global\KernelScriptSection`).
- Request event (client sets, auto-reset):
  `\BaseNamedObjects\KernelScriptRequest`.
- Response event (driver sets, client only waits):
  `\BaseNamedObjects\KernelScriptResponse`.
- Client mutex (clients only, created by the first client):
  `Global\KernelScriptRingMutex`.

Layout constants live in `ks-core/src/ring.rs` (`RING_MAGIC`, `RING_VERSION`,
`HEADER_SPACE`, `REQUEST_OFFSET/SIZE`, `RESPONSE_OFFSET/META_SIZE/BULK_SIZE`,
`RING_TOTAL_SIZE`, `STATE_IDLE/REQUEST/PROCESSING/RESPONSE`). All header
fields are sequentially consistent atomics; the ring is a single request
slot guarded by the four-state machine:

```text
IDLE --client--> REQUEST --driver--> PROCESSING --driver--> RESPONSE
 ^                  |
 +-- client cancel (5 s, only while still REQUEST)
```

Round-trip rules (implemented in `ks-link/src/lib.rs`):

- Every round trip holds the client mutex, drains stale response signals,
  publishes a postcard-encoded `Request`, sets `STATE_REQUEST`, signals the
  request event, then waits for the response event.
- The driver echoes the request `sequence` as `response_sequence`; a
  response is only accepted when both the state and the sequence match.
- A request the driver never picked up is cancelled back to `STATE_IDLE`
  after 5 s; one it is already processing is waited out.
- `RingHeader::status` is the single source of truth for transport-level
  failure; `ResponseMeta` only describes a successful payload.
- Object names are randomized per load, so a driver reload creates fresh
  objects: a client session holds the previous load's handles, its round
  trips time out, and the client must drop it (`ks_link::close_session`,
  under ks-gui's lifecycle gate) or start a new process to open one
  against the current load — a session never reconnects on its own.

Payloads are postcard-encoded (`ks-core/src/protocol.rs`): `Ping`,
`GetProcessBase`, `Read`/`Write` (with `rva` and `mdl` flags), `BatchRead`,
`BatchWrite`, `TraverseChain`, `Shutdown`, `Lock`, `Unlock`, `UnlockAll`.
The response is a fixed 64-byte
`ResponseMeta` slot followed by a raw bulk region the driver fills in
place.

`Request::Shutdown` winds the worker down: the response header and the
response event are published before the worker exits, so the call is an
ordinary round trip (`ks_sdk::shutdown`, defined in ks-link). It also drops the whole lock
table before the worker exits, so a shut-down driver performs no further
target writes. Every later request times out
until the driver is reloaded, and stopping the service afterwards runs the
normal unload path against an already-exited thread. It is destructive:
ks-test sends it after the correctness suite and benchmarks in both load
modes. In the default KDU mode it is the *only* unload path — the worker
runs a `self_teardown` that closes the ring handles, erases the registry
publication (the three name values, the `Instance` claim and the key) and
releases the marker last — because KDU never runs
`DriverUnload`; `ks-test full` then verifies claim released, the
published names removed, that a second `kdu -map` succeeds after
shutdown, and that no live instance
remains. The legacy `ks-test sc` path follows the shutdown with the `sc
stop`/`sc query`/`sc delete` teardown that checks the service reports
STOPPED and is removed.

After the section, both events and the worker thread exist, the driver
publishes the three randomized kernel object names as `REG_SZ` values
under `HKLM\SOFTWARE\KernelScript` (`SectionName`, `RequestEventName`,
`ResponseEventName`). ks-link resolves the names from there when it opens
a session (kernel `\BaseNamedObjects\...` names are mapped to the client
`Global\...` namespace) and falls back to the compiled-in defaults when
the key is unreadable — a fallback that can never reach a randomized
load, so an unreadable key just fails the session open. Because the
registry is the only discovery path, publication failure fails driver
start. ks-test is stricter: its readiness signal is polling the key
(`ks_sdk::published_object_names_strict`, no fallback) until all three
values exist, then pinging the ring. Teardown (`release_instance`, from
the worker's `self_teardown` as well as from `comm::stop`) erases the
whole record again — the three values, the `Instance` claim and the key
itself — so a cleanly exited driver leaves the key absent; only a crash
keeps stale values, and those are overwritten by the next load's publish.

The names are randomized at every startup: `RingNames::generate`
(`ks-driver/src/comm.rs`) draws a 64-bit token with `RtlRandomEx`, seeded
from interrupt time and a stack address, and appends it as 16 lowercase
hex chars to the fixed prefixes — the published values change per load,
so nobody can predict the next load's names to squat them in advance.
What makes randomization safe is the single-instance guard: `comm::start`
claims a kernel-only marker event
`\BaseNamedObjects\KernelScriptInstance` before any ring object exists
(user-mode clients never open it), and a marker that is already openable
— a second SCM service, or a manual mapper racing this load — makes the
new instance fail with `STATUS_OBJECT_NAME_COLLISION`, which SCM reports
as `ERROR_ALREADY_EXISTS`. Two driver images must never coexist: they
would split the ring and the lock table between them. The claim also
writes `Instance` as `REG_DWORD` 1 under `HKLM\SOFTWARE\KernelScript` for
user-mode diagnostics; because registry values outlive a crash, the
marker object — not the registry — decides liveness, and a stale value is
deleted at the next start. Teardown releases the whole registry record
first — the three name values, the `Instance` value and the key itself —
and the marker object last, so a racing reload stays rejected until
teardown completes. The marker name itself is fixed — it is the probe
both loads must agree on; only the three ring object names randomize.

The driver resolves each distinct PID once per batch
(`memory::batch_write_process_memory`) instead of running
`PsLookupProcessByProcessId` per entry. One driver read or write is capped
at `4096` bytes; batch limits (`MAX_BATCH_ENTRIES = 256`,
`MAX_BATCH_WRITE_ENTRIES = 64`) are enforced by ks-core validation and
re-checked in the driver.

Memory locks live entirely in `ks-driver` (`lock.rs`), and are replayed by
the ring worker's own loop — there is no dedicated rewrite thread. The
worker mutates a static driver-side table (one `Request::Lock`/`Unlock`/
`UnlockAll` round trip per Lua call). Each loop pass:

- with the table non-empty, *polls* the request event (`timeout = 0`) so a
  pending request is always handled first, then rewrites exactly one lock
  entry (`lock::sweep_step`), and loops again — continuous rewriting with
  no inter-pass sleep while locks are held;
- with the table empty, blocks on the request event, so an idle driver
  burns no CPU. A `Lock` request is itself what wakes the worker; there is
  no private wake event.

The merged worker runs at kernel priority 6 (below the normal-class base,
the mirror of `THREAD_PRIORITY_BELOW_NORMAL`) so the spinning loop can
never starve the game or the GUI; requests still run at once whenever the
CPU is free. Rules that keep this from repeating the bugchecks that killed
the first kernel lock worker:

- The table is guarded by a fast mutex whose critical sections only copy
  bytes; target writes happen after the mutex is released, and nothing
  that can wait runs inside a section.
- Everything runs at `PASSIVE_LEVEL`, and no table operation can panic
  (the workspace builds with `panic = "abort"`: any panic is a bugcheck).
- Only the worker ever mutates or replays the table, so the sweep cursor
  and scratch buffer are single-consumer by construction. `comm::stop`
  flags the worker, signals the request event (the only wake source) and
  joins it before releasing the ring objects and clearing the table;
  `Request::Shutdown` clears the table before the worker exits.
- Lock limits (64 entries, max 4096 bytes each, `id != 0`, `pid != 0`,
  `address != 0` unless `rva`) are enforced by ks-core validation, by
  ks-link before the round trip, and again in the driver's table insert.
  A full table answers `STATUS_QUOTA_EXCEEDED`, which ks-link maps back to
  `TooManyEntries`.
- Do not replace the one-entry-per-pass rewrite with sleeps while locks
  are held, do not move the polling wait to a blocking wait while the
  table is non-empty (requests would stall behind a whole sweep), and do
  not move the table back to user mode.

## Driver Import Rules (ntdll.dll trap)

windows-sys declares `Zw*`/`Rtl*` routines against `ntdll.dll` as
`raw-dylib` imports. rustc packs the generated import objects into the
*declaring crate's* rlib, and rlibs are searched before the WDK import
libraries, so plain extern declarations in the driver resolve from a
phantom `ntdll.dll` dependency and the kernel loader refuses the image
(StartService fails; `dumpbin /imports` shows `ntdll.dll`).

Therefore in `ks-driver/src/wdm.rs`:

- Every `Zw*`/`Rtl*`/`Ps*`/`Ob*` extern used by Rust code must live in the
  `#[link(name = "ntoskrnl.exe", kind = "raw-dylib", modifiers =
  "+verbatim")]` block (x64 only: `import_name_type` is rejected off-x86).
- The `ks_*` SEH-shim functions must stay in a separate plain
  `extern "system"` block so they resolve against the bundled shim object.
- Some WDK routines are header-only and are not exported by ntoskrnl at
  all — `ExInitializeFastMutex` is one (importing it made StartService fail
  with error 127 / `STATUS_ENTRYPOINT_NOT_FOUND`). Use `wdm::init_fast_mutex`
  instead, and before adding any new kernel import check this machine's
  export table (`dumpbin /exports C:\Windows\System32\ntoskrnl.exe`).
- After every driver build, verify `dumpbin /imports ks-driver.sys` lists
  `ntoskrnl.exe` only.

## Driver Build

Normal workspace checks do not generate the native driver image:

```powershell
cargo check --workspace
cargo test --workspace
cargo build --release --workspace
```

Build the GUI with the unwind-enabled profile when runtime panic recovery
is required:

```powershell
cargo build --profile gui-release -p ks-gui
```

The normal release profile intentionally uses `panic = "abort"` for
fail-fast components such as the driver and must not be used when GUI
`catch_unwind` recovery is required.

For a WDK driver build, use a Visual Studio Developer Command Prompt and
set the WDK variables explicitly. The known working WDK configuration is
`10.0.26100.0`:

```powershell
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'

$vs = 'C:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat'
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && cargo build --release -p ks-driver --bin ks-driver --features wdk"
```

Always pass `--release`. A dev-profile (unoptimized) image overflows the
worker system thread's kernel stack on the first request round trip —
unoptimized postcard/heapless chains blow the ~16 KiB kernel stack — and
bugchecks with `0x50` at a 32/64 KiB-aligned address. Verified: every dev
build crashed at the first ping while the identical source in `--release`
passed the full suite; do not test or ship a dev-profile driver image.

The build script compiles `seh_shim.c` with MSVC and links the
Native-subsystem driver image. The C shim contains the SEH boundary around
`MmProbeAndLockPages`/`MmCopyVirtualMemory` and the kernel-link
compatibility symbols required by the Rust MSVC output:

- `_fltused`
- `__CxxFrameHandler3`

Do not link the user-mode CRT into the driver. Do not replace the handler
with an incompatible zero-argument function. Route kernel calls that need
an SEH boundary through `seh_shim.c` `ks_*` wrappers.

The driver image is written to the workspace root as `ks-driver.sys`.
Inspect it with platform linker tools before loading it. Driver signing
and VM deployment are environment-specific and are outside the workspace
source tree.

## Mapper (dt-loader Rust port)

`ks-test` (full, minimal) defaults to manual mapping; `ks-test sc
[full]` is the legacy SCM path; `ks-test shutdown` is a standalone
cleanup/verify subcommand.

The mapper is a **pure-Rust port of the KDU 1.5.0 map core** in the
standalone `dt-loader` crate — the private sibling repo
`https://github.com/lipeilin2006/driver-loader` (clone it next to this
workspace as `../driver-toolkit/dt-loader`; this workspace reaches it
through the `dt-loader` path dependency; `ks-sdk/src/kdu` is only the
Kernel Script lifecycle shim — embedded `DRIVER_IMAGE`,
registry-publication wait, ring `stop`). **This repository is
public**: the mapper sources, the third-party loader-driver blobs and
the KDU-derived shellcode must NEVER be committed or pushed here (the
`.gitignore` privacy block guards the known paths; check `git status`
for anything staged out of the private checkout before committing).
There is no C++ in the build and no `kdu.exe` child process: the
target image only ever reaches disk on the legacy `sc` path (SCM
needs a `binPath`):

- Module map: `nt.rs` (raw FFI against ntdll/kernel32/advapi32/rpcrt4,
  each with an explicit `#[link]`), `pe.rs` (image layout + kernel
  import resolution + ntoskrnl loading via `LoadLibraryExW(...,
  DONT_RESOLVE_DLL_REFERENCES)` + `GetProcAddress`), `env.rs` (build
  number, elevation, HVCI, pool-tag selection), `loader.rs` (service
  registry entries + `NtLoadDriver`/`NtUnloadDriver` + device open),
  `victim/` (PROCEXP152 drop/load/open, dispatch-signature query,
  `IRP_MJ_CREATE` execution; one module per victim build),
  `assets.rs` (`include_bytes!` of the `assets/drivers/` blobs),
  `shellcode.rs` (the 2048-byte `SHELLCODE`
  blob: init stub + embedded V3 machine code + resolved import table),
  `payload.rs` (UUID-named shared section holding the V3 payload
  header + import-resolved image copy), `superfetch.rs` (V2P
  translation via the Superfetch PFN query — the retained providers
  all translate through it, built lazily at first use and dropped
  after every attempt), `provider/` (one module per provider plus
  `ioctl.rs` primitives and the static table in `mod.rs`) and
  `dispatch.rs` (route selection + map orchestration).
- The provider database is gone: the 7 retained provider driver blobs
  plus 3 PROCEXP152 victims live as individual `.sys` files under
  `dt-loader/assets/drivers/` (`include_bytes!` through dt-loader's
  `assets.rs`),
  extracted from KDU's packed database by `build_loader_drivers.ps1`.
  The retained ids are 44 / 56 / 57 / 60 / 67 plus 34 (WinIo64.sys,
  "MSI Foundation Service": map/unmap protocol, device `\Device\WinIo`,
  the 40-byte `WINIO_PHYSICAL_MEMORY_INFO`, page-aligned `SectionOffset`
  with the page offset walked in user mode — probe-verified map/IOCTL
  round trip, staged separately by `dt-loader/tools/winio_probe.ps1`) and 68
  (Kinkajou.sys, hand-extracted, not in KDU's database: WHQL-signed
  Microsoft lab driver, device `\Device\Kinkajou`, METHOD_BUFFERED
  IOCTLs with no length validation — `register_driver` sends init
  0x221C08 (live-ntoskrnl byte-pattern scan + raw EPROCESS offsets
  from the input: ActiveProcessLinks at +0x28, UniqueProcessId at
  +0x30, Germanium 0x1D8/0x1D0 for build >= 26100 else 0x448/0x440
  for >= 19041) before read 0x221A58 / write 0x221A5C, which take
  pid@+0x00, addr@+0x08 and a size/pointer pair at +0x10/+0x18
  (read: size then destination; write: source then size) and move the
  data through per-process user buffers — the read/walk path runs
  against THIS process's DirectoryTableBase. Field-verified on build
  26300: `ks-test loadone 68 1712` and a pinned manual-map-only `full`
  suite pass end to end, which also confirms the expired-but-timestamped
  WHQL certificate loads (not blocklisted), the needle matches the live
  ntoskrnl, and the Germanium 0x1D0/0x1D8 offsets are correct).
  The rest of KDU's providers were removed after field verification on
  the development machine: Intel NAL / EneIo64 / DirectIo64 /
  EtdSupport / AsrDrv107 are signed with certificates Microsoft has
  revoked (kernel loads fail with `0xC0000603` on current builds),
  EleetX1's brute-force physical scan bugchecked the machine, CORMEM
  and PGRHostControl failed their kernel-write primitives, and Lenovo
  Diagnostics needs dbghelp symbol resolution (never ported). Re-add a
  provider by restoring its blob in `assets/drivers/`, its table entry
  in `provider/mod.rs` and its module in `provider/`;
  `start(Some(id))` runs it alone, `start(None)` walks the table in
  order.
- The target image never touches disk. `pe::MappedImage::load` lays
  headers and sections out at their virtual addresses **without**
  applying relocations and **without** rewriting `ImageBase`: the
  kernel shellcode relocates the payload copy from
  `delta = exbuffer - popth->ImageBase`, so the only invariant is that
  the field names the base the absolute pointers currently sit at —
  keeping the preferred base in both places satisfies it. Kernel
  imports are resolved by name from the copy
  (`pe::resolve_kernel_import`, ntoskrnl-only single-descriptor walk).
- Only the helper drivers are written to disk, and each is deleted
  again: the provider's vulnerable driver is extracted into the process
  working directory — a fresh temp root that is the CWD for the
  duration of the map call and is removed afterwards — and the victim
  (`PROCEXP152.sys`) into `%SystemRoot%\system32\drivers`, where it is
  removed once the payload has run. `ks_sdk::start` creates the root,
  switches the CWD, runs the chain, restores the CWD and deletes the
  root.
- Progress/log lines go through the internal `emit` (default: stdout
  with the historic `kdu: ` prefix; ks-test installs a sink into its
  step log). The line format `trying provider <id>` is load-bearing —
  ks-gui's startup log and the tests grep it.
- The shellcode V3 machine code (`shellcode.rs`, `SHELLCODE_V3`) is
  position-independent machine code extracted once from the verified
  MSVC build of KDU's `shellcode.cpp` by
  `dt-loader/tools/shellcode_dump.cpp` and committed as
  `dt-loader/assets/shellcode_v3.bin` (1581 bytes, sha256 2ACB3BE4…).
  Never "re-implement" it in Rust or change the extraction procedure —
  the blob is copied byte-for-byte into the kernel shellcode structure
  and any byte difference is a kernel bugcheck. Regeneration requires
  the pre-port C++ build (see the tool's header comment).

- Shellcode **V3 is mandatory**. V1 (KDU's default) starts `DriverEntry`
  as a bare system-thread routine with a `NULL` driver object — this
  driver rejects that — and the reported status then reflects thread
  creation, not the entry's own result. V3 builds a real
  `DRIVER_OBJECT`, calls `DriverEntry(driverObject, &regPath)`
  synchronously; its NTSTATUS is `dispatch::map_driver`'s `Ok` value, and
  `0xC0000035` (returned directly) is how the dup-instance check
  recognizes the single-instance guard rejecting a second load.
- The driver object name (the mapper's per-attempt `driver_name`,
  dt-loader `src/lib.rs`) must
  be unique per attempt:
  V3's driver object is permanent
  and nothing deletes it, so a reused name would collide in
  `ObCreateObject` with the same status code before the marker probe
  ever runs.
- Success is judged from the returned NTSTATUS (`0` = success).
  Readiness is the registry publication (`ks_sdk::start` polls it via
  `ks_sdk::published_object_names_strict`; ks-test additionally pings the
  ring once names appear).
- Loading order: the **manual-map provider chain first**:
  `ks_sdk::start(provider)` — `Some(id)` runs that provider alone;
  `None` (what ks-gui and ks-test use) walks the whole `PROVIDERS`
  table in order, one attempt per id, until a payload reports a status.
  That status ends the chain — `DriverEntry`'s own NTSTATUS cannot
  change with the provider, `0xC0000035` (single-instance rejection)
  included. Only when manual mapping did not run the driver (no
  provider executed the payload, or the payload reported failure) does
  the normal service load run as the fallback (`kdu/sc.rs`): the signed
  image goes through a service registry entry plus `NtLoadDriver`.
  `KS_SDK_KDU_PRV=<id>` (the retired `KS_TEST_KDU_PRV` is still
  honored) is tried first in the `None` mode; `KS_SDK_MAP=1` skips the
  service fallback entirely (manual mapping only — ks-test's `load`
  matrix sets it so a broken map cannot pass through the signed service
  path). `ks_sdk::stop` also unloads a service-loaded instance
  (`NtUnloadDriver` + service key + image file), and
  `ks_sdk::cleanup_service_load` recovers a leftover service load from
  a killed run (the image file stays locked until then).
  `KS_SDK_VICTIM=<build>` pins the victim build (1627/1702/1712).
- The payload encryption key is the pool tag: `KDU_CONTEXT` stores
  `EncryptKey` and `MemoryTag` in one `union`, so the shellcode's
  `Tag` decode always matches the encode. The Rust port keeps that
  identity — `dispatch::run_map` encodes with the same `memory_tag`
  that `shellcode::build` writes into the blob. Never split them: a
  mismatch decodes the payload to garbage and the victim executes it
  (two field bugchecks came from exactly this class of divergence,
  before the union identity was restored — they also briefly blamed
  the PROCEXP152 16.27 victim, which was exonerated once the key
  identity was fixed and now serves as the primary victim again). An attempt that dies before its payload runs moves the chain to
  the next id; a payload that actually ran ends the chain at once:
  `DriverEntry`'s own NTSTATUS — including the `0xC0000035`
  single-instance rejection — cannot change with the provider. The last
  provider that succeeded in the process is tried first on the next
  `start` (recorded in `LAST_GOOD_PROVIDER`, dt-loader `src/lib.rs`), so
  a repeated start — ks-test full's duplicate-load and post-shutdown
  re-map checks — goes straight to the working id. Every attempt gets a
  fresh V3 driver-object name, and an exhausted chain reports
  `Error::NoProvider { tried }` with the ids attempted.
- Route selection mirrors KDU exactly: `FLAG_PHYSICAL_BRUTE_FORCE` →
  page-by-page physical scan patching the victim dispatch
  (registry `HARDWARE\RESOURCEMAP\System Resources\Physical Memory`
  `.Translated` list); `FLAG_ROOT_FROM_LOWSTUB | FLAG_PREFER_PHYSICAL`
  → physical-translate through the provider's kernel-VM write; else →
  direct kernel-VM write. The shellcode blob is page-locked
  (`shellcode::LockedMemory`) before the scan, like KDU's
  `supAllocateLockedMemory`.
- Teardown inverts (see `Request::Shutdown` above): `ks-test full`
  verifies claim released, that a re-map after shutdown succeeds, and
  that no live driver instance remains. `ks-test shutdown` exits 0 when
  nothing live remains and is what leftover cleanup at start and `Drop`
  spawn (a ks-link session never reconnects on its own; ks-test keeps one
  session per process and uses a fresh `ks-test shutdown` process instead
  of `ks_link::close_session`).
- KDU never runs `DriverUnload`: every successful map leaks one
  `THREAD_OBJECT` with its kernel stack and one permanent V3 driver
  object until reboot — accepted for a test harness.
- Signing is not required in the default mode; `ks-test sc` is the only
  path that loads a signed image through SCM.
- The `KDU-1.5.0/` C++ tree has been deleted from the workspace
  (untracked from git first, then removed): nothing builds it, the
  loader-driver blobs and `shellcode_v3.bin` are committed assets in
  dt-loader, and the mapper is fully self-contained. The extraction
  scripts (`dt-loader/tools/extract_kdu_drivers.ps1` /
  `dt-loader/tools/build_loader_drivers.ps1`) and
  `dt-loader/tools/shellcode_dump.cpp` document how to regenerate
  those artifacts if the KDU package is ever re-obtained, but are
  dormant without it.

## Verification Checklist

Before considering a change complete:

1. Run `cargo fmt --all`.
2. Run `cargo test --workspace`.
3. Run `cargo check --workspace` (and `cargo clippy --workspace`, which
   must stay warning-free apart from the build script's framework note).
4. For driver changes, build `ks-driver --release --bin ks-driver
   --features wdk` using WDK 26100, verify the import table lists
   `ntoskrnl.exe` only, copy the image to `ks-sdk/assets/ks-driver.sys`
   and rebuild `ks-test` (the image is embedded with `include_bytes`).
   For mapper changes, also run `cargo fmt`/`clippy` inside the
   dt-loader checkout and run `drvtest7.ps1` elevated (the default KDU
   mode exercises the Rust mapper end to end).
5. Check `cargo tree -e features` when changing dependencies.
6. Search for stale synchronous Lua calls after changing the Lua API.
7. If artifacts are deployed to the VM, verify SHA256 hashes.
8. Do not load an unverified native driver image in a test environment.

## Known Warnings and Limitations

- The driver is a Native-subsystem kernel image and cannot be validated by
  running it as a normal user-mode executable.
- Process names and process metadata are collected in user mode by
  Toolhelp in `ks-link`; a PID is not a permanent process identity because
  Windows can reuse PIDs.
- The GUI uses egui/eframe with the `glow` OpenGL backend. The native
  eframe window is required as the OpenGL host and currently uses the
  default opaque window configuration.
- The GUI loads the first available `msyh.ttc`, `simsun.ttc`, or
  `simhei.ttf` from `C:\Windows\Fonts` so Chinese Lua/UI text renders on
  Windows. The GUI and release `ks-test` binaries embed a
  `requireAdministrator` manifest (debug/test builds skip it so
  `cargo test` can run unelevated).
- The driver internally exposes normal memory I/O (`Read`/`Write` without
  `mdl`) alongside separate MDL-remap I/O (`mdl = true`); the `rva` flag
  is orthogonal and resolves the address against the target module base.
  MDL access attaches to the target, probes the MDL with read access only,
  locks pages, and maps them into kernel space so writes bypass user-mode
  page protection (code sections, read-only data). MDL writes hit the
  shared physical page: image-section edits are visible to every process
  mapping that image. Keep both paths independent; do not silently fall
  back between them.
- An `IOCTL_ALLOC_MEM` target-process allocation feature was attempted
  twice and removed entirely; all alloc wire protocol, GUI, and Lua API
  code is gone. If target-process allocation is ever re-attempted, verify
  the built image with `dumpbin /imports` and route kernel calls through
  `seh_shim.c` `ks_*` wrappers.
- A first kernel-side lock worker was removed after repeated bugchecks in
  the field; locks moved client side, then returned to the driver with the
  invariants listed in the lock rules above (fast mutex around copies only,
  single-consumer sweep on the joined worker, `PASSIVE_LEVEL`, panic-free
  table code). Keep those invariants if the lock replay is ever reworked:
  the failure mode is a bugcheck, not an error message.
- Do not claim that an unsigned driver is VM-loadable merely because it
  compiled successfully.
- Windows Defender flags the KDU-mode test binaries (for example
  `HackTool:Win64/KduDrv` and `HackTool:Win64/KernelDrUtil`) and may
  quarantine `ks-test.exe` — which contains the Rust mapper and the
  embedded loader-driver blobs — at launch; the
   harness then exits silently with no output. Test machines need a
   Defender exclusion for the workspace, the dt-loader checkout and the
   `%LOCALAPPDATA%\Temp\dt-loader-*` staging directories (or
   equivalent AV handling).
- The KDU-mapped image registers no unwind information (`ntoskrnl`
  exports no `RtlAddFunctionTable`), so an exception raised outside the
  `seh_shim.c` probe guards bugchecks instead of being caught; the
  harness only probes its own committed pages for that reason.
