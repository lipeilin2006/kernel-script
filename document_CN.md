# Kernel Script Luau API

本文档描述 `ks-gui` 当前实际注册到 Luau VM 的 API。每个 `scripts/*.lua`
脚本在独立 Luau VM 中运行，启用 JIT 编译。

## Lifecycle

可选的生命周期函数，由 GUI 每帧调用：

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

说明：

- `OnStart` 在脚本加载后调用一次。
- `OnUpdate` 在逻辑刷新阶段调用（60 Hz）。
- `OnRender` 在 GUI 渲染阶段调用。
- `OnDestroy` 在热重载或 GUI 退出时调用。
- 所有内存 API 调用都是同步的，阻塞 Lua 线程约 60-100μs。
- 如果延迟敏感，不要在 `ui.window` 回调中调用内存 API。

## Process API

### memory.get_pid

根据可执行文件名获取 PID，比较时不区分大小写。

```lua
local pid = memory.get_pid("notepad.exe")
```

进程名要求：非空、最多 255 字节、不包含 NUL 字节。

### memory.get_process_base

根据 PID 获取进程主模块基地址。

```lua
local base = memory.get_process_base(pid)
print(string.format("base = 0x%X", base))
```

基地址查询由 driver 完成，使用 `PsGetProcessSectionBaseAddress`。

## Absolute Memory API

地址参数支持：

- Lua 正整数。
- 十进制字符串。
- `0x` 或 `0X` 开头的十六进制字符串。

例如：

```lua
local a = "140702365450240"
local b = "0x7FF812345000"
```

地址 `0` 会提交给后台请求，最终由 service/driver 返回错误；它不会在 GUI
渲染回调入口同步抛错。负数地址和不支持的 Lua 类型会在参数转换阶段拒绝。

### memory.read_i32

读取一个 32 位有符号整数。

```lua
local value = memory.read_i32(pid, "0x1407FFF0")
```

### memory.read_bytes

读取字节数组。

```lua
local data = memory.read_bytes(pid, address, 16)
for index, byte in ipairs(data) do
    print(index, byte)
end
```

当前 driver 单次读取上限为 4096 字节。

### memory.write_i32

写入一个 32 位有符号整数。

```lua
memory.write_i32(pid, address, 123456789)
```

### memory.write_bytes

写入 Lua 字节数组。

```lua
memory.write_bytes(pid, address, {
    0x15, 0xCD, 0x5B, 0x07
})
```

每个元素应为可转换为字节的整数，数组最大为 4096 字节。

## RVA Memory API

RVA API 的最终地址由 driver 计算：

```text
absolute_address = process_image_base + relative_address
```

`relative_address` 是无符号相对偏移，地址相加发生溢出时请求失败。

### memory.read_rva

根据 PID 自动获取 image base，并读取 `base + relative_address` 处的数据。

```lua
local data = memory.read_rva(pid, 0x1234, 4)
```

### memory.write_rva

根据 PID 自动获取 image base，并写入 `base + relative_address` 处的数据。

```lua
memory.write_rva(pid, 0x1234, {
    0x15, 0xCD, 0x5B, 0x07
})
```

RVA 读写同样受 4096 字节单次 driver 传输限制。

## MDL Memory API

MDL 读写通过内核 MDL 重映射访问目标进程内存：附加到目标进程后以只读方式
锁定页面，再把同一物理页映射到内核地址空间，通过内核映射完成读写。该
路径绕过用户态页保护，因此可以读取和**修改只读、不可写内存**（例如代码
段）。

用法与普通读写完全一致，区分仅在于：

- `read_mdl*` / `write_mdl*` 是独立的 API，与普通
  `read*` / `write*` 互不影响。
- MDL 写入的是共享物理页：修改映像代码段会影响所有映射该模块的进程
  （写时复制页面除外）。
- 同样受 4096 字节单次传输限制。

### memory.read_mdl

```lua
local data = memory.read_mdl(pid, address, 16)
```

### memory.write_mdl

```lua
-- 修改只读内存/代码段
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

## Batch Read API（批量读取）

批量读取在单次 IPC 往返中执行多次内存读取。驱动只做一次进程查找 + N 次
`ks_copy_process_memory`，返回**无 size 前缀**的平铺字节缓冲区。消除逐次
IPC 开销，避免 Lua Table 分配。

**最大条目数**：每次调用最多 256 个。**总传输限制**：4096 字节。

无效地址（null 或不可读）会被跳过并零填充，不会导致整个 batch 失败。

### memory.batch_read

从目标进程读取多个内存区域，所有条目共享同一个 `size`。返回单个平铺字节
缓冲区，各条目数据紧密排列。

```lua
local raw = memory.batch_read(pid, 256, {
    0x1407FFF0,
    0x14080000,
})
```

### memory.batch_offset

根据尺寸数组计算平铺缓冲区中各条目的字节偏移。返回的表中索引 `i` 是
第 `i` 个条目的字节偏移，`total` 是总字节数。

```lua
local offsets = memory.batch_offset({ 0x100, 0x100, 0x100 })
-- offsets[1] = 0, offsets[2] = 256, offsets[3] = 512, offsets.total = 768
```

### memory.traverse_pointer_chain

使用单次 IPC 往返遍历目标进程中的指针链。从 `base` 开始，读取
`base + offsets[1]` 处的指针，再读取 `result + offsets[2]` 处的指针，
依此类推。返回最终地址（`u64`）。

```lua
local base = memory.get_process_base(pid)
local addr = memory.traverse_pointer_chain(pid, base, {
    0x1000,  -- base + 0x1000
    0x30,    -- (base + 0x1000) + 0x30
    0x80,    -- ... + 0x80
})
print(addr)
```

**最大偏移数**：每次调用最多 32 个。如果链中任何指针为 null 或不可读，
则返回 `0`。

### 示例：零分配实体扫描

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
    -- ... 绘制逻辑（每个字段无 Table 分配）
end
```

响应是单个 Luau byte string。`string.unpack` 直接在缓冲区上按偏移读取字段
——零中间 Table 分配，GC 压力极低。

## 窗口查询 API

### memory.get_window_rect

同步查询指定 PID 的所有可见窗口。返回 Lua 表（窗口矩形列表），找不到
窗口时返回 `nil`。

每个窗口矩形包含：

```lua
{ x = 100, y = 50, width = 800, height = 600 }
```

坐标单位为 egui 逻辑点（物理像素除以 `content_scale`）。用于 `draw.*`
覆盖层渲染。

```lua
local rects = memory.get_window_rect(pid)
if rects then
    for i, r in ipairs(rects) do
        print(r.x, r.y, r.width, r.height)
    end
end
```

实现方式：在 GUI 进程（用户会话）中运行 `EnumWindows`，使用
`DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` 获取精确窗口位置，
失败时回退到 `GetWindowRect`。

约束：`pid` 必须为正整数。

## Draw API

Draw 命令在透明全屏覆盖窗口上渲染。所有 draw 调用必须在 `OnRender` 中
执行。坐标单位为 egui 逻辑点（物理像素除以 `content_scale`）。

颜色为 RGBA 字节（`0..255`）。

### draw.line

绘制线段。

```lua
draw.line(x1, y1, x2, y2, r, g, b, a, thickness)
```

### draw.rect

绘制矩形边框。

```lua
draw.rect(x, y, width, height, r, g, b, a, thickness)
```

### draw.filled_rect

绘制填充矩形。

```lua
draw.filled_rect(x, y, width, height, r, g, b, a)
```

### draw.circle

绘制圆形边框。

```lua
draw.circle(x, y, radius, r, g, b, a, thickness)
```

### draw.filled_circle

绘制填充圆形。

```lua
draw.filled_circle(x, y, radius, r, g, b, a)
```

### draw.text

在指定位置绘制文字。`size` 为逻辑点字体大小。

```lua
draw.text(x, y, "你好", r, g, b, a, size)
```

## egui UI API

Lua UI API 在 `OnRender` 中使用。GUI 当前使用 `egui_overlay` + GLFW 窗口 +
`glow` OpenGL backend 实现透明全屏覆盖层渲染。

### ui.window

创建一个 egui 窗口。窗口内容通过 callback 绘制。

```lua
ui.window("Memory", function()
    ui.label("Memory Reader")
end)
```

callback 必须在当前调用中同步结束，不得在其中 yield。

### ui.label

普通文本标签。

```lua
ui.label("Kernel Script")
```

### ui.button

按钮被点击时返回 `true`。

```lua
if ui.button("Read") then
    -- 执行同步操作
end
```

### ui.separator

```lua
ui.separator()
```

### ui.checkbox

返回更新后的值和 changed 状态。

```lua
enabled, changed = ui.checkbox("Enabled", enabled)
```

### ui.drag_value

可拖动数值。`drag_value` 接受 `f64`；`drag_value_i8/u8/i16/u16/i32/u32/i64/u64/f32`
提供其余原生数值类型。参数为标签和当前值，返回更新后的值及 changed 状态。

```lua
value, changed = ui.drag_value("Scale", value)
pid, changed = ui.drag_value_u64("PID", pid)
```

### ui.text_edit

通用文本编辑器。参数依次为当前文本、是否多行、是否密码模式、是否代码
编辑模式，返回更新后的文本和 changed 状态。

```lua
text, changed = ui.text_edit(text, true, false, true)
password, changed = ui.text_edit(password, false, true, false)
```

### ui.color_edit_button_srgba

颜色选择器。参数为 RGBA 四个 `0..255` 字节，返回更新后的 RGBA 和 changed
状态。

```lua
r, g, b, a, changed = ui.color_edit_button_srgba(40, 120, 220, 255)
```

### ui.spinner

显示 egui loading spinner。

```lua
ui.spinner()
```

### ui.slider_*

滑动条支持 `i8`、`u8`、`i16`、`u16`、`i32`、`u32`、`i64`、`u64`、`f32` 和
`f64`。参数为标签、当前值、最小值、最大值，返回新值和 changed 状态。

```lua
value, changed = ui.slider_i32("Volume", value, 0, 100)
ratio, changed = ui.slider_f32("Ratio", ratio, 0.0, 1.0)
```

### ui.radio

单选按钮。参数为按钮文本和当前选中的文本，返回该按钮是否处于选中状态
以及本次是否被点击。

```lua
selected, clicked = ui.radio("Easy", selected)
if clicked then selected = "Easy" end
```

### ui.combo_box

下拉框。参数为标签、当前选中项和字符串数组，返回新选中项和 changed 状态。

```lua
mode, changed = ui.combo_box("Mode", mode, {"Read", "Write"})
```

### ui.progress_bar

显示进度条。`fraction` 会被限制在 `0.0..=1.0`。

```lua
ui.progress_bar(0.75, "Loading")
```

### ui.collapsing_header

可展开/折叠的内容区域。

```lua
ui.collapsing_header("Details", function()
    ui.label("Additional information")
end)
```

### ui.scroll_area

垂直滚动区域。第一个参数是最大高度。

```lua
ui.scroll_area(240, function()
    for i = 1, 100 do ui.label("Item " .. i) end
end)
```

### ui.selectable_label

可选列表项。返回本次是否点击和传入的选中状态。

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

分别对应 egui 的小号文本、弱化文本和代码文本。

```lua
ui.small("Secondary text")
ui.weak("Optional detail")
ui.code("0x140000000")
```

### ui.add_space

插入垂直空白。

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
    status = "就绪",
}

function OnUpdate(dt)
    -- 所有内存调用都是同步的（每次约 60-100μs）
end

function OnRender()
    ui.window("Kernel Script", function()
        if ui.button("附加") then
            state.pid = memory.get_pid(state.process_name)
            if state.pid > 0 then
                state.base = memory.get_process_base(state.pid)
                state.status = string.format("已附加 0x%X", state.base)
                state.rects = memory.get_window_rect(state.pid)
            else
                state.status = "未找到进程"
            end
        end
        state.process_name = select(
            1, ui.text_edit(state.process_name, false, false, false)
        )
        ui.label("状态: " .. state.status)
        ui.monospace(string.format("pid=%s base=0x%X", tostring(state.pid), state.base))
    end)

    if state.rects then
        for _, r in ipairs(state.rects) do
            draw.rect(r.x - 2, r.y - 2, r.width + 4, r.height + 4, 255, 0, 0, 200, 3.0)
        end
    end
end
```

## IPC 架构

GUI 到 service 的 IPC 使用同步阻塞 Named Pipe 调用：

1. **GUI 侧**：每次内存 API 调用打开一个阻塞管道连接（或复用线程本地连接），
   写入帧请求，同步读取响应。
2. **Service 侧**：`handle_client` 从管道 decoder 读取帧，
   每帧 `spawn_blocking` 在 Tokio blocking pool 上并发执行。
3. **零锁 IOCTL**：驱动 handle 以 `Arc<DriverHandle>` 共享。每个 blocking task
   直接调用 `DeviceIoControl`，无需获取 mutex。Windows I/O manager 内部序列化 IRP。

往返延迟：每次调用约 60-100μs。在 60fps（16.6ms 帧预算）下，每帧可以轻松
执行 100+ 次同步内存读取。

## Runtime Constraints

- Lua VM 只在 GUI Lua 线程访问。
- 所有内存 API 调用都是同步的，阻塞 Lua 线程约 60-100μs。
- 单次内存读写最多 4096 字节。
- 批量读取限制：最多 256 个条目，总计 4096 字节。
- 进程列表由 service 在用户态枚举。
- 内存读写和 RVA 计算由 driver 执行。
- 窗口枚举在 GUI 进程（用户会话）中执行。
- Draw 命令必须在 `OnRender` 中调用。
- 坐标单位为 egui 逻辑点；物理像素需除以 `content_scale` 才能正确对齐。
- 服务传输使用 `\\.\pipe\KernelScript` Named Pipe。
- Named Pipe 和 driver device 的权限由 Windows 安全描述符控制。
