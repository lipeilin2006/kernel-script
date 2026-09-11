# Kernel Script

基于内核的模块化 Windows 脚本框架，三层架构设计，兼顾安全性与稳定性。

## 架构

```
┌───────────────────────────────┐
│         ks-gui (图形界面)      │  <-- 运行在普通用户会话
└───────────────┬───────────────┘      egui overlay + Lua 脚本 + Draw API
                │  Named Pipe IPC
┌───────────────▼───────────────┐
│     ks-service (系统服务)      │  <-- 以 NT AUTHORITY\SYSTEM 运行
└───────────────┬───────────────┘      服务桥接 + 驱动分发
                │  IOCTL
┌───────────────▼───────────────┐
│      ks-driver (内核驱动)      │  <-- Ring 0
└───────────────────────────────┘      内存读写 + MDL 重映射
```

## 功能特性

- **Lua 脚本**: 支持热重载的 Lua 脚本，协程异步 IPC
- **内存读写**: 普通读写和 MDL 读写（绕过页保护），单次最大 4096 字节
- **批量读取**: 单次 IOCTL 读取多个内存区域 — N 个实体只需 1 次 IPC 往返
- **RVA API**: 驱动侧自动计算 image base + offset
- **Draw API**: 透明全屏窗口上的覆盖层渲染（直线、矩形、圆形、文字）
- **窗口查询**: 通过 DWM 查询目标进程窗口位置
- **多窗口支持**: 处理拥有多个窗口的进程
- **透明覆盖层**: GLFW + DWM 透明，鼠标穿透
- **中文字体**: 自动加载 `msyh.ttc` / `simhei.ttf` / `simsun.ttc`
- **高性能 IPC**: 基于 channel 的代理，持久连接，零拷贝帧处理

## 项目结构

```
kernel-script/
├── Cargo.toml                    # Workspace 根配置
├── README.md / README_CN.md
├── document.md / document_CN.md  # Lua API 参考
│
├── ks-core/                      # [R0/R3] 共享协议与 ABI
│   └── src/
│       ├── lib.rs                # no_std 兼容
│       ├── protocol.rs           # IOCTL 常量、线路消息
│       └── memory.rs             # 内存操作定义
│
├── ks-driver/                    # [Ring 0] WDM 内核驱动
│   ├── build.rs                  # WDK 链接标志
│   ├── seh_shim.c                # MmProbeAndLockPages SEH 边界
│   └── src/
│       ├── dispatch.rs           # IOCTL 派遣
│       ├── memory/               # 普通 + MDL 读写
│       └── wdm.rs                # FFI 声明
│
├── ks-service/                   # [Ring 3 - SYSTEM] 服务 + IPC
│   └── src/
│       ├── main.rs               # 服务入口 / 控制台模式
│       ├── driver_comm.rs        # DeviceIoControl 调用
│       ├── ipc.rs                # Named Pipe 服务端
│       └── process.rs            # Toolhelp 进程枚举
│
├── ks-gui/                       # [Ring 3 - User] egui overlay + Lua 运行时
│   └── src/
│       ├── main.rs               # GUI 入口
│       ├── app.rs                # 帧生命周期、DWM 透明
│       ├── ipc_client.rs         # Named Pipe 客户端
│       ├── lua_runtime.rs        # Lua VM、调度器、API 绑定
│       └── window_util.rs        # Win32 EnumWindows + DwmGetWindowAttribute
│
├── ks-installer/                 # 提权 GUI 安装器（仅 sc.exe）
│   └── src/main.rs
│
└── driver-package/               # 部署目录（扁平布局）
    ├── ks-driver.sys + .pdb
    ├── ks-service.exe + .pdb
    ├── ks-gui.exe + .pdb
    ├── ks-installer.exe + .pdb
    └── scripts/
        ├── monitor.lua           # 进程监控示例
        ├── search.lua            # 内存搜索示例
        ├── draw_test.lua         # 覆盖层绘制测试
        └── debug_rva.lua         # RVA 调试
```

## 构建

### 前置条件

- Rust 1.75+
- Visual Studio 2022+（含 C++ 工作负载）
- WDK 10.0.26100.0

### 工作区检查

```powershell
cargo fmt --all
cargo test --workspace
cargo check --workspace
```

### WDK 驱动构建

需要 Visual Studio Developer Command Prompt，并设置 WDK 环境变量：

```powershell
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'

$vs = 'C:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat'
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && cargo build -p ks-driver --bin ks-driver --features wdk"
```

### Release 构建

```powershell
cargo build --release --workspace
cargo build --profile gui-release -p ks-gui    # 启用 unwind 用于 catch_unwind
```

### 部署到包目录

```powershell
$pkg = 'D:\kernel-script\driver-package'
Copy-Item D:\kernel-script\ks-driver.sys "$pkg\ks-driver.sys" -Force
Copy-Item D:\kernel-script\ks-driver.pdb "$pkg\ks-driver.pdb" -Force
Copy-Item D:\kernel-script\target\release\ks-service.exe "$pkg\ks-service.exe" -Force
Copy-Item D:\kernel-script\target\release\ks_service.pdb "$pkg\ks-service.pdb" -Force
Copy-Item D:\kernel-script\target\gui-release\ks-gui.exe "$pkg\ks-gui.exe" -Force
Copy-Item D:\kernel-script\target\gui-release\ks_gui.pdb "$pkg\ks_gui.pdb" -Force
Copy-Item D:\kernel-script\target\release\ks-installer.exe "$pkg\ks-installer.exe" -Force
Copy-Item D:\kernel-script\target\release\ks_installer.pdb "$pkg\ks_installer.pdb" -Force
```

## 使用

### 1. 安装驱动和服务

以管理员身份运行 `ks-installer.exe`，或手动：

```cmd
sc.exe create ks-driver type= kernel start= demand binPath= C:\path\to\ks-driver.sys
sc.exe start ks-driver
ks-service.exe --console
```

### 2. 启动 GUI

```cmd
ks-gui.exe
```

GUI 创建一个全屏透明覆盖窗口，使用 DWM 透明。鼠标点击会穿透到下方应用，除非光标在 UI 元素上。

### 3. 编写 Lua 脚本

将 `.lua` 文件放入 `scripts/` 目录。每个脚本在独立 Lua VM 中运行；所有脚本通过 `shared.set/get` 共享标量值。

#### 轮询模式（推荐）

```lua
local state = { pid_task = nil, pid = 0 }

function OnUpdate(dt)
    if state.pid_task then
        local result = memory.poll_async(state.pid_task)
        if result then
            state.pid_task = nil
            if result.error then
                print("失败:", result.error)
            else
                state.pid = result.value
            end
        end
    end
end

function OnRender()
    ui.window("示例", function()
        if ui.button("开始") and not state.pid_task then
            state.pid_task = memory.async_get_pid("notepad.exe")
        end
        ui.label("PID: " .. tostring(state.pid))
    end)
end
```

#### 协程模式

```lua
start_async(function()
    local pid = await_async(memory.async_get_pid("notepad.exe"))
    local hp = await_async(memory.async_read_i32(pid, 0x1407FFF0))
    print("HP: " .. hp)
end)
```

#### Draw 覆盖层

```lua
function OnRender()
    -- 在透明全屏覆盖层上绘制
    draw.rect(100, 100, 200, 150, 255, 0, 0, 200, 3.0)
    draw.filled_rect(100, 100, 200, 150, 0, 255, 0, 50)
    draw.circle(200, 175, 30.0, 255, 255, 0, 220, 2.0)
    draw.filled_circle(200, 175, 30.0, 255, 200, 0, 160)
    draw.line(100, 100, 300, 250, 255, 255, 255, 200, 1.0)
    draw.text(100, 260, "你好！", 0, 255, 0, 255, 14.0)
end
```

## 安全特性

- **进程隔离**：GUI 运行在用户模式，服务以 SYSTEM 运行，驱动在 Ring 0
- **句柄保护**：驱动设备句柄仅由服务持有
- **DACL**：设备对象使用 `D:P(A;;GA;;;SY)` — 仅 SYSTEM 可访问
- **崩溃隔离**：GUI 崩溃不影响驱动或服务稳定性
- **会话隔离**：服务运行在 session 0；窗口枚举在 GUI 进程中执行

## 许可证

本项目仅供学习用途。
