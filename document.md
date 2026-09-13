# Kernel Script Luau API

This document describes the Luau APIs registered by `ks-gui`. Each `scripts/*.lua`
script runs in its own Luau VM with JIT compilation enabled.

## Lifecycle

Optional lifecycle functions called by the GUI per frame:

```lua
function OnStart()
end

function OnUpdate(delta_time)
end

function OnRender()
end

function OnDestroy()
end
```

Notes:

- `OnStart` is called once after the script is loaded.
- `OnUpdate` is called during the logic tick phase (60 Hz).
- `OnRender` is called during the GUI render phase.
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
calls must be made inside `OnRender`. Coordinates are in egui logical points
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

UI APIs are called inside `OnRender`. The GUI uses `egui_overlay` with GLFW
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
end

function OnRender()
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
- Process list is enumerated in user mode by the service.
- Memory read/write and RVA computation are performed by the driver.
- Window rect enumeration runs in the GUI process (user session).
- Draw commands must be called inside `OnRender`.
- Coordinates are in egui logical points; divide physical pixels by
  `content_scale` for correct overlay alignment.
- Transport uses the `\\.\pipe\KernelScript` Named Pipe.
- Named Pipe and driver device access are controlled by Windows security
  descriptors.
