# Kernel Script Lua API

This document describes the Lua APIs registered by `ks-gui`. Each `scripts/*.lua`
script runs in its own Lua VM. All scripts share scalar values through
`shared.set/get/delete` but do not share Lua tables, functions, threads, or
userdata.

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
- Do not wait for IPC inside `OnRender`.
- Do not call `await_async` or `coroutine.yield` inside a `ui.window` callback.
- Network and driver requests must run inside a `start_async` coroutine.

## Async Tasks

### start_async

Starts a Lua coroutine that is automatically resumed by the GUI each frame.

```lua
local co = start_async(function(arg)
    print(arg)
end, "hello")
```

Returns a coroutine object; direct manipulation is usually unnecessary.

### await_async

Waits for an async task to complete. Only suspends the current Lua coroutine
during the wait; never blocks the GUI thread.

```lua
local value = await_async(memory.async_read_i32(pid, address))
```

On failure a Lua error is raised inside the coroutine. Use `pcall` to catch:

```lua
start_async(function()
    local ok, result = pcall(function()
        return await_async(memory.async_read_i32(pid, address))
    end)
    if ok then
        print("value:", result)
    else
        print("failed:", result)
    end
end)
```

### memory.poll_async

Non-blocking task result poll. Returns `nil` if the task has not completed.

```lua
local task_id = memory.async_list_processes()
local result = memory.poll_async(task_id)
if result then
    if result.error then
        print("failed:", result.error)
    else
        local value = result.value
    end
end
```

Success shape:

```lua
{ done = true, value = ... }
```

Failure shape:

```lua
{ done = true, error = "error message" }
```

## Process API

### memory.async_list_processes

Asynchronously lists all running processes.

```lua
local processes = await_async(memory.async_list_processes())
for _, process in ipairs(processes) do
    print(process.pid, process.name)
end
```

Each record contains:

```lua
{ pid = 1234, parent_pid = 1000, thread_count = 12, name = "notepad.exe" }
```

Process enumeration is performed by `ks-service` using Windows Toolhelp APIs.

### memory.async_get_pid

Asynchronously resolves a process name to a PID (case-insensitive comparison).

```lua
local pid = await_async(memory.async_get_pid("notepad.exe"))
```

Constraints: non-empty, at most 255 bytes, no NUL bytes.

### memory.async_get_process_base

Asynchronously retrieves the main module base address for a given PID.

```lua
local base = await_async(memory.async_get_process_base(pid))
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

### memory.async_read_i32

Reads a 32-bit signed integer.

```lua
local value = await_async(memory.async_read_i32(pid, "0x1407FFF0"))
```

### memory.async_read_bytes

Reads a byte array.

```lua
local data = await_async(memory.async_read_bytes(pid, address, 16))
for i, byte in ipairs(data) do
    print(i, byte)
end
```

Single transfer limit: 256 bytes.

### memory.async_write_i32

Writes a 32-bit signed integer.

```lua
await_async(memory.async_write_i32(pid, address, 123456789))
```

### memory.async_write_bytes

Writes a Lua byte array.

```lua
await_async(memory.async_write_bytes(pid, address, {
    0x15, 0xCD, 0x5B, 0x07
}))
```

Each element must be coercible to a byte. Maximum 256 bytes per call.

## RVA Memory API

RVA APIs let the driver compute the absolute address:

```text
absolute_address = process_image_base + relative_address
```

`relative_address` is an unsigned offset. Addition overflow causes the request
to fail.

### memory.async_read_rva

Reads `base + relative_address` after resolving the image base internally.

```lua
local data = await_async(memory.async_read_rva(pid, 0x1234, 4))
```

### memory.async_write_rva

Writes `base + relative_address` after resolving the image base internally.

```lua
await_async(memory.async_write_rva(pid, 0x1234, {
    0x15, 0xCD, 0x5B, 0x07
}))
```

RVA read/write is also limited to 256 bytes per transfer.

## MDL Memory API

MDL read/write access target process memory through kernel MDL remapping: the
driver attaches to the target, locks pages with read-only access, and maps the
same physical pages into kernel address space. Reads and writes go through the
kernel mapping, which **bypasses user-mode page protection** (read-only sections,
code pages).

Usage is identical to normal read/write. The only differences:

- `async_read_mdl*` / `async_write_mdl*` are separate APIs; they do not fall
  back to normal read/write.
- MDL writes hit shared physical pages: modifying an image code section affects
  every process mapping that module (copy-on-write pages excepted).
- Same 256-byte single transfer limit applies.

### memory.async_read_mdl

```lua
local data = await_async(memory.async_read_mdl(pid, address, 16))
```

### memory.async_write_mdl

```lua
-- Modify read-only memory / code section
await_async(memory.async_write_mdl(pid, address, {
    0x90, 0x90, 0x90, 0xC3
}))
```

### memory.async_read_mdl_rva

```lua
local data = await_async(memory.async_read_mdl_rva(pid, 0x1234, 4))
```

### memory.async_write_mdl_rva

```lua
await_async(memory.async_write_mdl_rva(pid, 0x1234, {
    0x15, 0xCD, 0x5B, 0x07
}))
```

## Shared Globals

Multiple Lua VMs exchange simple values through Rust-side shared storage.

### shared.set

```lua
shared.set("selected_pid", 1234)
shared.set("target_address", "0x7FF812345000")
shared.set("enabled", true)
```

Supported value types: `nil`, boolean, integer, finite number, string.
Lua tables, functions, threads, and userdata cannot be shared.

### shared.get

```lua
local pid = shared.get("selected_pid")
```

Returns `nil` for missing keys.

### shared.delete

```lua
shared.delete("selected_pid")
```

Key limit: 128 bytes.

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

### Poll Pattern (Recommended)

```lua
local state = {
    process_name = "notepad.exe",
    pid = 0,
    pid_task = nil,
    base = 0,
    base_task = nil,
    rects = nil,
    status = "Ready",
}

function OnUpdate(dt)
    if state.pid_task then
        local result = memory.poll_async(state.pid_task)
        if result then
            state.pid_task = nil
            if result.error then
                state.status = "Failed: " .. result.error
            else
                state.pid = result.value
                shared.set("selected_pid", state.pid)
                state.base_task = memory.async_get_process_base(state.pid)
            end
        end
    end

    if state.base_task then
        local result = memory.poll_async(state.base_task)
        if result then
            state.base_task = nil
            if result.error then
                state.status = "Base failed: " .. result.error
            else
                state.base = result.value
                state.status = string.format("Attached 0x%X", state.base)
                state.rects = memory.get_window_rect(state.pid)
            end
        end
    end
end

function OnRender()
    ui.window("Kernel Script", function()
        if ui.button("Attach") and not state.pid_task then
            state.pid_task = memory.async_get_pid(state.process_name)
            state.status = "Looking up " .. state.process_name .. "..."
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

## Runtime Constraints

- Lua VM is only accessed by the GUI Lua thread.
- Background Tokio tasks transfer only task IDs and owned plain data.
- `OnRender` must not perform synchronous network or driver operations.
- `OnUpdate` must not await futures; use coroutines or `poll_async`.
- Single memory read/write limit: 256 bytes.
- Process list is enumerated in user mode by the service.
- Memory read/write and RVA computation are performed by the driver.
- Window rect enumeration runs in the GUI process (user session).
- Draw commands must be called inside `OnRender`.
- Coordinates are in egui logical points; divide physical pixels by
  `content_scale` for correct overlay alignment.
- Transport uses the `\\.\pipe\KernelScript` Named Pipe.
- Named Pipe and driver device access are controlled by Windows security
  descriptors.
