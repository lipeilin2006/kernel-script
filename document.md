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
- `OnUpdate` is the only per-frame callback. The overlay frame rate is capped at 60 Hz.
- UI and draw APIs are available during `OnUpdate`.
- Keyboard state is snapshotted once per frame before `OnUpdate`; queries and
  press latches are consistent within a frame.
- `OnDestroy` is called on hot-reload or GUI exit.
- All memory API calls are synchronous and block the Lua thread for ~60-100μs.
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

Address `0` is forwarded to the service/driver which returns an error; it does
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

Memory lock continuously rewrites a byte pattern to a target address as fast
as the batch driver round trip allows. Each lock has an explicit stable `id`;
locking the same id again updates its PID, address, and data.

The lock table lives in `ks-service`: a dedicated rewrite thread replays
every entry through one batch write (`IOCTL_WRITE_MEMORY_BATCH`) in a
continuous spin. The driver keeps no
lock state. The table supports up to 64 entries, each 1–4096 bytes. All
writes use the same kernel primitive as ordinary `memory.write_*` calls.

### memory.lock

Locks a byte pattern to an absolute address. The service rewrites `data` to
`address` in the target process continuously until unlocked.

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

Locks a byte pattern at `base + relative_address`. The service resolves the
image base when the lock is created (same base as `get_process_base`).

```lua
memory.lock_rva(2, pid, 0x1234, {0x90, 0x90})
```

### memory.unlock_rva

Removes a lock by `id`; the original PID and RVA are not needed.

```lua
memory.unlock_rva(2)
```

Constraints:

- All lock APIs are synchronous; they only update the service lock table.
- The periodic write runs in `ks-service`, not in the driver.
- Data size per lock: 1–4096 bytes.
- Maximum locks: 64 (service-wide).
- Locks are cleared when the service stops.

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

Applies multiple writes in one service round trip and one kernel transition
(`IOCTL_WRITE_MEMORY_BATCH`): one process lookup for the whole batch, one
NTSTATUS per entry.

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
uses `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` for accurate
rects with fallback to `GetWindowRect`.

Constraints: requires `pid` as a positive integer.

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
    -- All memory calls are synchronous (~60-100μs each)
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

The GUI-to-service IPC uses synchronous blocking named pipe calls:

1. **GUI side**: Each memory API call opens a blocking pipe connection (or
   reuses a thread-local connection), writes the framed request, and reads
   the response synchronously.
2. **Service side**: `handle_client` reads frames from the pipe decoder,
   spawns each as a `spawn_blocking` task on Tokio's blocking pool.
3. **Zero-mutex IOCTL**: The driver handle is shared as `Arc<DriverHandle>`.
   Each blocking task calls `DeviceIoControl` directly without acquiring a
   mutex. Windows I/O manager serializes IRPs internally.

Round-trip latency: ~60-100μs per call. At 60fps (16.6ms frame budget), you
can comfortably fit 100+ synchronous memory reads per frame.

## Runtime Constraints

- Lua VM is only accessed by the GUI Lua thread.
- All memory API calls are synchronous and block the Lua thread for ~60-100μs.
- Single memory read/write limit: 4096 bytes.
- Batch read limit: 256 entries, 4096 bytes total.
- Memory lock limit: 64 entries, 4096 bytes per entry; rewritten continuously
  through one batch write IOCTL per sweep.
- Process list is enumerated in user mode by the service.
- Memory read/write and RVA computation are performed by the driver.
- Window rect enumeration runs in the GUI process (user session).
- Draw commands must be called inside `OnUpdate`.
- Coordinates are in egui logical points; divide physical pixels by
  `content_scale` for correct overlay alignment.
- Transport uses the `\\.\pipe\KernelScript` Named Pipe.
- Named Pipe and driver device access are controlled by Windows security
  descriptors.
- Driver requests use the ordinary memory IOCTL constants. Sensitive integer
  fields (PID, absolute address, RVA/base/pointer offsets, including batch and
  pointer-chain entries) are plain little-endian values. Sizes, counts, and
  memory data remain plaintext in the same explicitly encoded layouts.
