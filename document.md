# Kernel Script Luau API

[Home](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [中文 README](https://github.com/lipeilin2006/kernel-script/blob/main/README_CN.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md)

This document describes the Luau APIs registered by `ks-gui`. Each `scripts/*.lua`
script runs in its own Luau VM with JIT compilation enabled.

## Lifecycle

Optional lifecycle functions called by the GUI per frame:

```lua
function OnStart()
end

function OnUpdate(delta_time)
end

function OnDestroy()
end
```

Notes:

- `OnStart` is called once after the script is loaded.
- `OnUpdate(delta_time)` is the only per-frame callback. `delta_time` is the
  elapsed time since the previous frame in seconds; it is `0` while the engine
  is paused. The overlay frame rate is capped at 60 Hz.
- UI and draw APIs are available during `OnUpdate`.
- Keyboard state is snapshotted once per frame before `OnUpdate`; queries and
  press latches are consistent within a frame.
- `OnDestroy` is called on hot-reload or GUI exit.
- The `engine` table (see Engine API) exposes pause/resume, VM memory and
  timing helpers.
- All memory API calls are synchronous and block the Lua thread for ~10-15μs.
- Do not call memory APIs inside `ui.window` callbacks if latency is critical.

## Process API

### memory.get_pid

Resolves a process name to a PID (case-insensitive comparison).

```lua
local pid = memory.get_pid("notepad.exe")
```

Constraints: non-empty, at most 255 bytes, no NUL bytes.

### memory.get_process_base

Retrieves the main module base address for a given PID.

```lua
local base = memory.get_process_base(pid)
print(string.format("base = 0x%X", base))
```

Uses `PsGetProcessSectionBaseAddress` inside the driver.

## Absolute Memory API

Address parameters accept:

- Lua positive integers.
- Decimal strings.
- Hex strings prefixed with `0x` or `0X`.

Examples:

```lua
local a = "140702365450240"
local b = "0x7FF812345000"
```

Address `0` is forwarded to the driver which returns an error; it does
not throw synchronously in the GUI render callback. Negative addresses and
unsupported Lua types are rejected at parameter conversion time.

### memory.read_i32

Reads a 32-bit signed integer.

```lua
local value = memory.read_i32(pid, "0x1407FFF0")
```

### memory.read_bytes

Reads a byte array.

```lua
local data = memory.read_bytes(pid, address, 16)
for i, byte in ipairs(data) do
    print(i, byte)
end
```

Single transfer limit: 4096 bytes.

### memory.write_i32

Writes a 32-bit signed integer.

```lua
memory.write_i32(pid, address, 123456789)
```

### memory.write_bytes

Writes a Lua byte array.

```lua
memory.write_bytes(pid, address, {
    0x15, 0xCD, 0x5B, 0x07
})
```

Each element must be coercible to a byte. Maximum 4096 bytes per call.

## RVA Memory API

RVA APIs let the driver compute the absolute address:

```text
absolute_address = process_image_base + relative_address
```

`relative_address` is an unsigned offset. Addition overflow causes the request
to fail.

### memory.read_rva

Reads `base + relative_address` after resolving the image base internally.

```lua
local data = memory.read_rva(pid, 0x1234, 4)
```

### memory.write_rva

Writes `base + relative_address` after resolving the image base internally.

```lua
memory.write_rva(pid, 0x1234, {
    0x15, 0xCD, 0x5B, 0x07
})
```

RVA read/write is also limited to 4096 bytes per transfer.

## Config API

Scripts cannot access the filesystem. Persistent settings go through the
`config` API, which stores typed key/value entries in `config.json` beside
`ks-gui.exe`. The store lives in the GUI process: it survives script hot
reload, and pending changes are written to disk within one second (also on
`config.save()` and when the GUI exits).

### config.set

Stores a value under a key. Accepts `boolean`, `integer`, `number` or
`string` (tables/functions are rejected). Returns `true` on success.

```lua
config.set("aimbot.fov", 45.0)
config.set("aimbot.enabled", true)
config.set("target.name", "boss")
```

### config.get

Returns the stored value for a key, or `default` (nil when omitted) if the
key does not exist.

```lua
local fov = config.get("aimbot.fov", 45.0)
local enabled = config.get("aimbot.enabled", false)
```

### config.remove

Deletes a key. Returns `true` when the key existed.

```lua
config.remove("target.name")
```

### config.save

Forces an immediate write of pending changes. Returns `true` when everything
is persisted.

```lua
config.save()
```

Constraints:

- Key length: 1–128 bytes.
- String value length: up to 4096 bytes.
- Maximum entries: 256 (shared by all loaded scripts; prefix keys with the
  script name to avoid collisions).
- Writes are debounced (at most one write per second) and atomic
  (temp file + rename); a crash never leaves a half-written file.

## Keyboard Input API

Keyboard queries read the physical key state of the whole desktop
(`GetAsyncKeyState`), so they work while another window owns input focus.
The engine snapshots all keys once per frame; `is_key_press` reports a
down-edge since the previous frame (60 Hz detection granularity).

`key` accepts a name or a raw virtual-key code (integer 0–255). Recognized
names: `"a"`–`"z"`, `"0"`–`"9"`, `"f1"`–`"f24"`, `"space"`, `"tab"`,
`"enter"`/`"return"`, `"backspace"`/`"back"`, `"escape"`/`"esc"`,
`"shift"`/`"lshift"`/`"rshift"`, `"ctrl"`/`"lctrl"`/`"rctrl"`,
`"alt"`/`"lalt"`/`"ralt"`, `"insert"`/`"ins"`, `"delete"`/`"del"`,
`"home"`, `"end"`, `"pageup"`, `"pagedown"`, `"up"`, `"down"`, `"left"`,
`"right"`, `"capslock"`, `"pause"`, `"lwin"`, `"rwin"`, `"numpad0"`–
`"numpad9"`, `"multiply"`, `"add"`, `"subtract"`, `"divide"`,
`"decimal"`, and mouse buttons `"mouse1"`–`"mouse5"` (also
`"lbutton"`, `"rbutton"`, `"mbutton"`, `"xbutton1"`, `"xbutton2"`).

### keyboard.is_key_down

```lua
if keyboard.is_key_down("f") then
    -- held right now
end
```

### keyboard.is_key_up

```lua
if keyboard.is_key_up("shift") then
    -- not held
end
```

### keyboard.is_key_press

True when a full press-then-release cycle was observed since the last call.
The flag latches across frames and is **consumed when read**, so a single
press fires exactly once — ideal for toggles.

```lua
if keyboard.is_key_press("mouse2") then
    aimbot_enabled = not aimbot_enabled
end
```

## Memory Lock API

Memory lock continuously rewrites a byte pattern to a target address at
kernel speed: the call mutates a driver-side lock table in a single round
trip, and the driver's worker thread replays entries between requests.
Each lock has an explicit stable `id`; locking the same id again updates
its PID, address, and data.

The lock table lives in the driver (`ks-driver/src/lock.rs`): while any
lock is held the worker only polls the request event (so pending requests
are always handled first) and rewrites one entry per loop pass; when the
table is empty it blocks on the request event, so an idle driver burns no
CPU. The table supports up to 64 entries, each 1–4096 bytes. All writes
use the same kernel primitive as ordinary `memory.write_*` calls.

### memory.lock

Locks a byte pattern to an absolute address. The driver keeps rewriting
`data` to `address` in the target process until unlocked.

```lua
memory.lock(1, pid, address, {0x90, 0x90, 0x90, 0xC3})
memory.lock(1, pid, new_address, {0x90, 0x90, 0x90, 0xC3}) -- moves lock 1
```

### memory.unlock

Removes a lock by `id`. Does **not** restore the original value; it only stops
subsequent periodic writes.

```lua
memory.unlock(1)
```

### memory.unlock_all

Removes all locks for a given PID.

```lua
memory.unlock_all(pid)
```

### memory.lock_rva

Locks a byte pattern at `base + relative_address`. `ks-link` resolves the
image base when the lock is created (same base as `get_process_base`).

```lua
memory.lock_rva(2, pid, 0x1234, {0x90, 0x90})
```

### memory.unlock_rva

Removes a lock by `id`; the original PID and RVA are not needed.

```lua
memory.unlock_rva(2)
```

### memory.lock_list

Dumps every lock the driver is replaying, including locks created by
another client (a Rust tool, another script). Returns an array of
`{ id, pid, address, len }`.

```lua
for _, lock in ipairs(memory.lock_list()) do
    print(lock.id, lock.pid, string.format("0x%X", lock.address), lock.len)
end
```

Constraints:

- All lock APIs are synchronous; each one is a single round trip that
  mutates the driver-side lock table.
- The periodic write runs in the driver, not in `ks-link`.
- Data size per lock: 1–4096 bytes.
- Maximum locks: 64 (driver-wide). When the table is full, a further
  distinct lock id fails with a "too many entries" error.
- Locks are cleared by `unlock`/`unlock_all`, by stopping the driver, and
  by driver shutdown. They are NOT cleared when the client process exits:
  a crashed script host leaves its locks rewriting until the driver is
  stopped.

## Process Suspend / Resume

### memory.suspend

Freezes every thread of the target process (kernel `PsSuspendProcess`).

```lua
memory.suspend(pid)
```

### memory.resume

Continues a process this driver previously froze (kernel `PsResumeProcess`).

```lua
memory.resume(pid)
```

Constraints:

- `pid = 0` (Idle) and `pid = 4` (System) are refused; freezing the System
  process hangs the machine.
- Idempotent: repeating `suspend` does not freeze twice; `resume` only
  thaws processes this driver froze.
- **Suspends are NOT cleared by driver shutdown**: a script must
  guarantee a matching `resume`, otherwise the target stays frozen.
- MDL writes (code patching) inside the hook API automatically suspend the
  target first; see the SDK guide's hook section.

## Hook API (MinHook-style)

Kernel inline hooks reachable from Lua, mirroring the Rust SDK surface.
Every call is one driver round trip. `pid` selects the target space: `0`
hooks the kernel's global text, a non-zero `pid` hooks that process's
user half. Addresses accept a number or a `0x`-prefixed string; address
fields in the returned tables are hex strings so kernel addresses survive
Lua's f64 numbers exactly.

Hard limits of kernel inline hooking (the API does **not** detect or work
around them): PatchGuard-protected bytes, HVCI-enforced code pages
(`STATUS_INVALID_PAGE_PROTECTION`), a thread already executing inside the
replaced prologue (kernel code has no safe thread-freeze), and a
trampoline that does not reproduce the stolen instructions plus a jump
back to `target + replaced_len`. Misuse bugchecks the machine; remove
hooks before freeing any trampoline memory you own.

### memory.hook_probe

Read-only prologue report — the planning step before a detour. Returns
`replaced_len`, `absolute_len`, `original` (byte table), `rip_relative`
and `relative_branch`. `hook_detour` refuses a prologue containing a
relative branch.

```lua
local probe = memory.hook_probe(pid, target)
if probe.relative_branch then error("cannot relocate a relative branch") end
```

### memory.hook_create

Records a hook whose trampoline **you** own and leaves it disabled.
`trampoline` must be executable memory in the same space that stays valid
until `hook_remove` — for a user-space target, e.g. an
`memory.alloc(pid, n, 0x40)` buffer holding the stolen instructions plus
a jump back to `target + replaced_len`. Returns `{ id, replaced_len,
original }`.

```lua
local h = memory.hook_create(pid, target, trampoline)
memory.hook_enable(pid, h.id)
```

### memory.hook_enable / memory.hook_disable / memory.hook_remove

Applies the jump, restores the original prologue (keeping the entry for a
later enable), or drops the entry. `hook_remove` also frees driver-owned
detour/shellcode buffers — call it before any user-owned trampoline goes
away.

### memory.hook_enable_all / memory.hook_disable_all / memory.hook_remove_all

Global (no `pid`): operate on every hook the driver recorded and return
how many were touched. Driver shutdown removes all hooks implicitly.

### memory.hook_shellcode

The driver allocates executable non-paged memory in the target space,
copies `code` (position-independent x64, at most 1024 bytes) there and
points the target's jump at it. The buffer belongs to the hook entry and
is freed by `hook_remove`. Returns `{ id, replaced_len, original,
code_address }`. HVCI makes the allocation fail.

### memory.hook_detour

Same as `hook_shellcode`, but the driver also builds the trampoline
itself: the stolen instructions with RIP-relative operands rebased, plus
a jump back to `target + replaced_len`. Call the original from `code`
with the fixed 6-byte sequence `FF 15 F2 FF FF FF` (`call qword ptr
[rip-14]`). Returns `{ id, replaced_len, original, code_address,
trampoline_address }`.

```lua
-- detour that calls the original, adds one to its result and returns
local code = {
    0xFF, 0x15, 0xF2, 0xFF, 0xFF, 0xFF, -- call [rip-14] (original)
    0x83, 0xC0, 0x01,                   -- add eax, 1
    0xC3,                               -- ret
}
local h = memory.hook_detour(pid, target, code)
memory.hook_enable(pid, h.id)
```

Constraints:

- 16 hooks maximum; ids start at 1 and are never reused. Duplicate
  targets fail with an "object name collision" error.
- A user-space hook write suspends the target process first; a refusal to
  suspend fails the write. A `pid` equal to this GUI's own pid skips the
  suspension (it would deadlock the round trip).
- The kernel path (`pid = 0`) is never suspended: `PsSuspendProcess` does
  not stop kernel threads.
- The driver's own self tests (`ks-hook-test`) stay Rust-only.

## MDL Memory API

MDL read/write access target process memory through kernel MDL remapping: the
driver attaches to the target, locks pages with read-only access, and maps the
same physical pages into kernel address space. Reads and writes go through the
kernel mapping, which **bypasses user-mode page protection** (read-only sections,
code pages).

Usage is identical to normal read/write. The only differences:

- `read_mdl*` / `write_mdl*` are separate APIs; they do not fall
  back to normal read/write.
- MDL writes hit shared physical pages: modifying an image code section affects
  every process mapping that module (copy-on-write pages excepted).
- Same 4096-byte single transfer limit applies.

### memory.read_mdl

```lua
local data = memory.read_mdl(pid, address, 16)
```

### memory.write_mdl

```lua
-- Modify read-only memory / code section
memory.write_mdl(pid, address, {
    0x90, 0x90, 0x90, 0xC3
})
```

### memory.read_mdl_rva

```lua
local data = memory.read_mdl_rva(pid, 0x1234, 4)
```

### memory.write_mdl_rva

```lua
memory.write_mdl_rva(pid, 0x1234, {
    0x15, 0xCD, 0x5B, 0x07
})
```

## Batch Read API

Batch read performs multiple memory reads in a single IPC round-trip. The driver
does one process lookup and N copies, returning a flat byte buffer with no size
prefixes. This eliminates per-read IPC overhead and avoids Lua Table allocation.

**Maximum entries**: 256 per call. **Total transfer limit**: 4096 bytes.

Invalid addresses (null or unreadable) are skipped and zero-filled in the output
buffer, so valid entries are still returned even if some pointers are stale.

### memory.batch_read

Reads multiple memory regions from a target process. All entries share the same
`size`. Returns a single flat byte buffer where each entry's data is packed
contiguously.

```lua
local raw = memory.batch_read(pid, 256, {
    0x1407FFF0,
    0x14080000,
})
```

### memory.batch_offset

Calculates byte offsets into a flat buffer from an array of sizes. Returns a
table where index `i` is the byte offset of the `i`-th entry, and `total` is
the total byte count.

```lua
local offsets = memory.batch_offset({ 0x100, 0x100, 0x100 })
-- offsets[1] = 0, offsets[2] = 256, offsets[3] = 512, offsets.total = 768
```

### memory.traverse_pointer_chain

Walks a pointer chain in a target process using a single IPC round-trip.
Starting from `base`, reads a pointer at `base + offsets[1]`, then at
`result + offsets[2]`, and so on. Returns the final address as a `u64`.

```lua
local base = memory.get_process_base(pid)
local addr = memory.traverse_pointer_chain(pid, base, {
    0x1000,  -- base + 0x1000
    0x30,    -- (base + 0x1000) + 0x30
    0x80,    -- ... + 0x80
})
print(addr)
```

**Maximum offsets**: 32 per call. Returns `0` if any pointer in the chain is
null or unreadable.

### Example: Zero-Allocation Entity Scan

## Batch Write API

### memory.batch_write

Applies multiple writes in one ring round trip and one kernel transition:
one process lookup for the whole batch, one NTSTATUS per entry.

```lua
local results = memory.batch_write(pid, {
    { address = "0x7FF812345000", data = {0x90, 0x90} },
    { address = "0x7FF812345100", data = {1, 2, 3, 4} },
    { address = base + offset,    data = {0xE9} },
})
-- results[i] is true when entry i was written successfully
```

Constraints:

- 1–64 entries per call (`MAX_BATCH_WRITE_ENTRIES`).
- Each `data` is 1–4096 bytes.
- `address` accepts the same forms as other memory APIs (decimal number or
  hex string).
- Entries execute in input order; a failed entry does not stop the others.

### Example: Zero-Allocation Entity Scan

```lua
local ENTITY_SIZE = 0x100
local FIELD_HP_OFFSET = 0x40
local FIELD_POS_OFFSET = 0x4C

local raw = memory.batch_read(pid, ENTITY_SIZE, entities)
local offsets = memory.batch_offset({ ENTITY_SIZE })
for i = 1, #entities do
    local base = (i - 1) * ENTITY_SIZE
    local hp = string.unpack("<i4", raw, base + FIELD_HP_OFFSET)
    local x, y, z = string.unpack("<fff", raw, base + FIELD_POS_OFFSET)
    -- ... draw logic (zero Table allocation per field)
end
```

The response is a single Luau byte string. `string.unpack` reads fields at
calculated offsets directly — zero intermediate Table allocation, minimal GC
pressure.

## Allocation API

### memory.alloc

Reserves, commits and zeroes a region of virtual memory inside a target
process, through one driver round trip. Returns the base address.

```lua
local buffer = memory.alloc(pid, 0x1000)            -- PAGE_READWRITE
local code = memory.alloc(pid, 0x200, 0x40)         -- PAGE_EXECUTE_READWRITE
memory.write_bytes(pid, buffer, {1, 2, 3, 4})
```

- `size` is 1 byte to 16 MiB (`MAX_ALLOC_SIZE`); the driver rounds it up to
  whole pages.
- `protect` is optional and must be one of `0x04` (`PAGE_READWRITE`, the
  default), `0x20` (`PAGE_EXECUTE_READ`) or `0x40`
  (`PAGE_EXECUTE_READWRITE`). `0x02` (`PAGE_READONLY`) is refused here: a
  read-only allocation could not be written through `memory.write_bytes` /
  `memory.write_i32` (only the MDL path could), so allocate `0x04` and
  `memory.protect` it down if a read-only span is really wanted.
  `PAGE_NOACCESS` (`0x01`) and `PAGE_GUARD` (`0x100`) are rejected too —
  they make the target fault on its own memory.
- The allocation belongs to the target process: it is freed when that
  process exits, and `memory.free` releases it earlier.

### memory.free

Releases the whole VAD that contains `address` (`MEM_RELEASE`, no size
needed). **This is dangerous, not guarded**: the kernel only requires an
allocation base, and the driver keeps no ownership table, so an address
belonging to the target itself — its heap, a stack, the base of a mapped
view — is freed just as happily and that mapping comes down. Only pass
addresses you allocated (or knowingly intend to destroy).

```lua
memory.free(pid, buffer)
```

## Page Protection API

Change the page protection of **already mapped** memory — image pages,
the target's own allocations and `memory.alloc` pages alike — and query
a region before touching it. Every call is one driver round trip.

### memory.query_region

Describes the region containing `address` (`ZwQueryVirtualMemory` with
`MemoryBasicInformation`). This is the probe step before `protect`: the
span must sit inside one region, and `state`/`typ` say whether the pages
may change at all.

Returns a table:

```lua
{
    base = 0x140000000,          -- region start (page aligned)
    allocation_base = 0x140000000, -- VAD / image base it belongs to
    region_size = 0x10000,
    state = 0x1000,              -- 0x1000 MEM_COMMIT / 0x2000 MEM_RESERVE / 0x10000 MEM_FREE
    protect = 0x20,              -- current protection; 0 when free
    allocation_protect = 0x20,   -- protection the VAD was created with
    typ = 0x1000000,             -- 0x1000000 MEM_IMAGE / 0x40000 MEM_MAPPED / 0x20000 MEM_PRIVATE
}
```

```lua
local region = memory.query_region(pid, address)
if region.state ~= 0x1000 then error("address is not committed") end
-- walk to the next region:
local next_addr = region.base + region.region_size
```

### memory.protect

Changes the protection of `[address, address + size)` to `protect` and
returns the **first page's** previous protection. `address` need not be
page aligned — the kernel rounds the range itself.

```lua
local old = memory.protect(pid, address, 0x1000, 0x20)   -- -> PAGE_EXECUTE_READ
memory.protect(pid, address, 0x1000, 0x04, true)         -- with CFG no-update
```

- `protect` is one of `0x02` (`PROT_READ`), `0x04` (`PROT_RW`), `0x20`
  (`PROT_RX`), `0x40` (`PROT_RWX`); everything else is rejected before
  the round trip. Single span: 1 byte to 256 MiB (`MAX_PROTECT_SIZE`).
- Optional 5th argument `cfg_no_update`: attaches
  `PAGE_TARGETS_NO_UPDATE` so the region's existing CFG bitmap entries
  are left untouched instead of being re-marked valid indirect-call
  targets (the kernel's default on a protection change). The flag only
  pairs with an executable protection (`0x20`/`0x40`); any other
  combination is refused before the round trip.
- **The span must lie entirely inside existing mappings.** A range
  crossing a free hole fails with the kernel's own status, which comes
  back verbatim — probe with `query_region` first and use its
  `region_size`.
- The returned old value covers the first page only; across several
  regions the pages may differ, so exact restoration means one call per
  region. The kernel rounds the span to whole pages — use
  `memory.protect_span` when the covered length matters.
- Dynamic Code Policy / ACG refuses to make private pages executable
  (`STATUS_INVALID_PAGE_PROTECTION`); image pages are usually exempt.
  HVCI/VBS accepts the change but still refuses the write or execution
  that follows.
- Changing `MEM_IMAGE` pages to read-write triggers copy-on-write and
  breaks file sharing for every process mapping that image;
  `0x20`/`0x40` do not.

### memory.protect_span

Same as `memory.protect`, but also returns the length the kernel actually
covered: the span is rounded to whole pages, so a one-byte request still
changes a whole page. Returns `old_protect, bytes_protected`.

```lua
local old, len = memory.protect_span(pid, address, 1, 0x40)
-- len == 0x1000: one byte of request covered a whole page
```

## Window Rect API

### memory.get_window_rect

Synchronously queries all visible windows belonging to a given PID. Returns a
Lua table (list of window rects) or `nil` if no windows are found.

Each window rect contains:

```lua
{ x = 100, y = 50, width = 800, height = 600 }
```

Coordinates are in egui logical points (physical pixels divided by
`content_scale`). Use this for overlay rendering with `draw.*`.

```lua
local rects = memory.get_window_rect(pid)
if rects then
    for i, r in ipairs(rects) do
        print(r.x, r.y, r.width, r.height)
    end
end
```

Implementation: runs `EnumWindows` in the GUI process (user session), then
returns the client area (`GetClientRect` + `ClientToScreen`), so the title
bar and borders are excluded; a window whose client rect is unavailable
falls back to `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` and then
`GetWindowRect`.

Constraints: requires `pid` as a positive integer.

## Engine API

Script-level engine control and diagnostics. All functions are synchronous
and cheap (no driver round trip).

### engine.is_paused

True while the engine pause is active. `OnUpdate` keeps running while
paused, but `delta_time` is delivered as `0`.

```lua
if engine.is_paused() then return end
```

### engine.pause / engine.resume

Toggles the engine pause: a "freeze logic, keep drawing" switch. UI and
keyboard keep working, `delta_time` becomes `0`, so timers and movement
code can gate on it.

```lua
if keyboard.is_key_press("f8") then
    if engine.is_paused() then engine.resume() else engine.pause() end
end
```

### engine.memory_used

Bytes currently allocated by the Lua VM (against the per-script memory
limit).

```lua
ui.label(string.format("lua: %.1f KiB", engine.memory_used() / 1024))
```

### engine.time_us

Monotonic microsecond timestamp, for measuring sections inside `OnUpdate`.

```lua
local t0 = engine.time_us()
-- ... work ...
print("took", engine.time_us() - t0, "us")
```

## Draw API

Draw commands render on the transparent fullscreen overlay window. All draw
calls must be made inside `OnUpdate`. Coordinates are in egui logical points
(physical pixels divided by `content_scale`).

Colors are RGBA bytes (`0..255`).

### draw.line

Draws a line segment.

```lua
draw.line(x1, y1, x2, y2, r, g, b, a, thickness)
```

### draw.rect

Draws a rectangle outline.

```lua
draw.rect(x, y, width, height, r, g, b, a, thickness)
```

### draw.filled_rect

Draws a filled rectangle.

```lua
draw.filled_rect(x, y, width, height, r, g, b, a)
```

### draw.circle

Draws a circle outline.

```lua
draw.circle(x, y, radius, r, g, b, a, thickness)
```

### draw.filled_circle

Draws a filled circle.

```lua
draw.filled_circle(x, y, radius, r, g, b, a)
```

### draw.text

Draws text at a position. `size` is font size in logical points.

```lua
draw.text(x, y, "Hello", r, g, b, a, size)
```

## egui UI API

UI APIs are called inside `OnUpdate`. The GUI uses `egui_overlay` with GLFW
window and `glow` OpenGL backend for transparent fullscreen overlay rendering.

### ui.window

Creates an egui window. Content is drawn through a callback.

```lua
ui.window("Memory", function()
    ui.label("Memory Reader")
end)
```

The callback must return synchronously; do not yield inside it.

### ui.label

```lua
ui.label("Kernel Script")
```

### ui.button

Returns `true` when clicked.

```lua
if ui.button("Read") then
    -- submit request
end
```

### ui.separator

```lua
ui.separator()
```

### ui.checkbox

Returns the updated value and a changed flag.

```lua
enabled, changed = ui.checkbox("Enabled", enabled)
```

### ui.drag_value

Draggable numeric value. `drag_value` accepts `f64`; typed variants
`drag_value_i8/u8/i16/u16/i32/u32/i64/u64/f32` cover the remaining primitive
types. Returns the updated value and changed flag.

```lua
value, changed = ui.drag_value("Scale", value)
pid, changed = ui.drag_value_u64("PID", pid)
```

### ui.text_edit

Generic text editor. Parameters: current text, multiline flag, password flag,
code mode flag. Returns updated text and changed flag.

```lua
text, changed = ui.text_edit(text, true, false, true)
password, changed = ui.text_edit(password, false, true, false)
```

### ui.color_edit_button_srgba

Color picker. Parameters: R, G, B, A as `0..255` bytes. Returns updated RGBA
and changed flag.

```lua
r, g, b, a, changed = ui.color_edit_button_srgba(40, 120, 220, 255)
```

### ui.spinner

```lua
ui.spinner()
```

### ui.slider_*

Sliders for `i8`, `u8`, `i16`, `u16`, `i32`, `u32`, `i64`, `u64`, `f32`, `f64`.
Parameters: label, current value, min, max. Returns new value and changed flag.

```lua
value, changed = ui.slider_i32("Volume", value, 0, 100)
ratio, changed = ui.slider_f32("Ratio", ratio, 0.0, 1.0)
```

### ui.radio

Radio button. Returns whether the button is selected and whether it was clicked
this frame.

```lua
selected, clicked = ui.radio("Easy", selected)
if clicked then selected = "Easy" end
```

### ui.combo_box

Dropdown. Parameters: label, current selection, string array. Returns new
selection and changed flag.

```lua
mode, changed = ui.combo_box("Mode", mode, {"Read", "Write"})
```

### ui.progress_bar

`fraction` is clamped to `0.0..=1.0`.

```lua
ui.progress_bar(0.75, "Loading")
```

### ui.collapsing_header

Collapsible content region.

```lua
ui.collapsing_header("Details", function()
    ui.label("Additional information")
end)
```

### ui.scroll_area

Vertical scroll region. First parameter is max height.

```lua
ui.scroll_area(240, function()
    for i = 1, 100 do ui.label("Item " .. i) end
end)
```

### ui.horizontal

Row container: widgets drawn inside the body share one horizontal row
instead of stacking vertically.

```lua
ui.horizontal(function()
    ui.label("PID:")
    ui.monospace(tostring(pid))
    if ui.button("Refresh") then state.pid = memory.get_pid(name) end
end)
```

### ui.selectable_label

Selectable list item. Returns whether it was clicked and the current selection
state.

```lua
clicked, selected = ui.selectable_label("Process", selected)
```

### ui.heading / ui.monospace

```lua
ui.heading("Memory Viewer")
ui.monospace("0x140000000: 4D 5A")
```

### ui.text / ui.text_wrapped

Body text. `text` renders as a single unbroken run; `text_wrapped` breaks
into the available width.

```lua
ui.text("single line run")
ui.text_wrapped("this long sentence wraps inside the window width")
```

### ui.hyperlink_to

```lua
ui.hyperlink_to("Project page", "https://example.com")
```

### ui.small / ui.weak / ui.code

```lua
ui.small("Secondary text")
ui.weak("Optional detail")
ui.code("0x140000000")
```

### ui.add_space

Vertical spacing.

```lua
ui.add_space(8)
```

## Complete Example

```lua
local state = {
    process_name = "notepad.exe",
    pid = 0,
    base = 0,
    rects = nil,
    status = "Ready",
}

function OnUpdate(dt)
    -- All memory calls are synchronous (~10-15μs each)
    ui.window("Kernel Script", function()
        if ui.button("Attach") then
            state.pid = memory.get_pid(state.process_name)
            if state.pid > 0 then
                state.base = memory.get_process_base(state.pid)
                state.status = string.format("Attached 0x%X", state.base)
                state.rects = memory.get_window_rect(state.pid)
            else
                state.status = "Process not found"
            end
        end
        state.process_name = select(
            1, ui.text_edit(state.process_name, false, false, false)
        )
        ui.label("Status: " .. state.status)
        ui.monospace(string.format("pid=%s base=0x%X", tostring(state.pid), state.base))
    end)

    if state.rects then
        for _, r in ipairs(state.rects) do
            draw.rect(r.x - 2, r.y - 2, r.width + 4, r.height + 4, 255, 0, 0, 200, 3.0)
        end
    end
end
```

## IPC Architecture

The GUI-to-driver transport is a synchronous shared-memory ring:

1. **Lua side**: each memory API call blocks the GUI Lua thread until the
   round trip completes (`sync_ipc` over `ks-link`).
2. **Client side**: `ks-link` serializes a postcard-encoded request into
   the single request slot while holding a named mutex, publishes it by
   setting the request event, and waits for the response event (a request
   the driver never picks up is cancelled after 5 seconds).
3. **Driver side**: one worker system thread services the slot: it decodes
   and validates the request, performs the memory operation, and publishes
   the response with a matching sequence number.

Round-trip latency: ~10-15μs per call. At 60fps (16.6ms frame budget), you
can comfortably fit 500+ synchronous memory reads per frame.

## Runtime Constraints

- Lua VM is only accessed by the GUI Lua thread.
- All memory API calls are synchronous and block the Lua thread for ~10-15μs.
- Single memory read/write limit: 4096 bytes.
- Batch read limit: 256 entries, 4096 bytes total.
- Memory lock limit: 64 entries, 4096 bytes per entry; replayed by the
  driver worker between requests (poll while locks are held, blocking
  wait when the table is empty).
- Process list is enumerated in user mode by `ks-link` (Toolhelp).
- Memory read/write and RVA computation are performed by the driver.
- Window rect enumeration runs in the GUI process (user session).
- Draw commands must be called inside `OnUpdate`.
- Coordinates are in egui logical points; divide physical pixels by
  `content_scale` for correct overlay alignment.
- Transport is one named section plus two named events (plus a
  client-side mutex), all created by the driver.
- Kernel objects are protected by a hand-built DACL granting SYSTEM and
  Administrators; the driver exposes no device object.
- The wire format is postcard (LEB128 varints + little-endian fixed
  fields); `RING_VERSION` guards every layout change.
