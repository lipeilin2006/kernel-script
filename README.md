# Kernel Script

A modular kernel-based scripting framework for Windows, featuring a three-tier architecture for enhanced security and stability.

## Architecture

```
┌───────────────────────────────┐
│       ks-gui (用户界面)        │  <-- 运行在普通用户权限 (User Session)
└───────────────┬───────────────┘      egui 渲染 + Lua 脚本管理
                │  进程间通信 (Named Pipe)
┌───────────────▼───────────────┐
│     ks-service (系统服务)     │  <-- 运行在 NT AUTHORITY\SYSTEM 权限
└───────────────┬───────────────┘      Lua 引擎 + 通信桥梁
                │  内核通信 (IOCTL)
┌───────────────▼───────────────┐
│      ks-driver (内核驱动)      │  <-- Ring 0 最高权限
└───────────────────────────────┘      内存读写 + 进程管理
```

## Project Structure

```
kernel-script/
├── Cargo.toml                    # Workspace 配置文件
├── README.md
│
├── ks-core/                      # [R0/R3 通用] 共享数据结构与通信协议
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                # 保证 no_std 兼容
│       ├── protocol.rs           # R3 与 R0 通信的指令结构体
│       └── memory.rs             # 基础内存操作封装定义
│
├── ks-driver/                    # [Ring 0] Windows 内核驱动 (no_std)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── dispatch.rs           # I/O 控制派遣函数
│       ├── memory/               # 内核内存读写实现
│       └── utils.rs
│
├── ks-service/                   # [Ring 3 - SYSTEM] 系统服务 + IPC 桥梁
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs               # 服务入口
│       ├── driver_comm.rs        # 与驱动通信
│       ├── ipc.rs                # 与 GUI 的 IPC 通信
│       └── process.rs             # Toolhelp 进程枚举
│
├── ks-gui/                       # [Ring 3 - User] egui 用户界面 + Lua runtime
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs               # GUI 入口
│       ├── app.rs                # 主应用逻辑
│       ├── ipc_client.rs         # 与服务的 IPC 通信
│       └── views/                # ImGui 面板
│           ├── console.rs        # Lua 交互式控制台
│           ├── mem_viewer.rs     # 内存查看器
│           └── script_mgr.rs     # 脚本管理器
│
├── scripts/                      # Lua 功能脚本
│   ├── main.lua                  # 默认初始化脚本
│   ├── aob_scan.lua              # 特征码扫描示例
│   └── struct_parser.lua         # 结构体解析示例
│
└── build/                        # 编译产物输出
```

## Building

### Prerequisites

- Rust 1.75+
- Windows SDK
- WDK (Windows Driver Kit) for ks-driver

### Build Commands

```bash
# Build all crates
cargo build

# Build in release mode
cargo build --release

# Build individual crates
cargo build -p ks-core
cargo build -p ks-driver
cargo build -p ks-service
cargo build -p ks-gui
cargo build -p ks-installer
```

### Installer

Run `ks-installer` from an elevated terminal. By default it uses the directory
containing the installer executable as the package directory:

```cmd
ks-installer.exe install
ks-installer.exe status
ks-installer.exe uninstall
```

The installer copies itself and the GUI/service binaries to
`C:\Program Files\KernelScript`, copies the driver through SetupAPI, installs
both services as `LocalSystem`, enables the `KsService` Service SID, and starts
the driver before the service. During uninstall, it schedules removal of its
own installation directory after the installer process exits. It never expects
or copies a signing private key.

## Usage

### 1. Install and Start Service

```bash
# Install the service (requires Administrator)
ks-service --install

# Start the service
net start KsService

# Or run in console mode for debugging
ks-service --console
```

### 2. Run GUI

```bash
ks-gui
```

### 3. Write Lua Scripts

Create `.lua` files in the `scripts/` directory. `main.lua` is the process
manager and `memory.lua` is the memory reader/writer.

```lua
-- Example: Read game memory
local pid = await_async(memory.async_get_pid("game.exe"))
local hp = await_async(memory.async_read_i32(pid, 0x1407FFF0))
print("Current HP: " .. hp)

local image_base = await_async(memory.async_get_process_base(pid))
print(string.format("Image base: 0x%X", image_base))

local bytes = await_async(memory.async_read_rva(pid, 0x1234, 4))
await_async(memory.async_write_rva(pid, 0x1234, { 0x15, 0xCD, 0x5B, 0x07 }))

-- Process names are matched case-insensitively by the driver.
-- The Windows image-name field is limited to 15 bytes.
local pid = await_async(memory.async_get_pid("notepad.exe"))

-- List all processes. Each item contains `name` and `pid`.
for _, process in ipairs(await_async(memory.async_list_processes())) do
    print(process.pid .. " " .. process.name)
end

-- Recommended for memory/process operations from the GUI frame callbacks:
-- the request is sent by a background IPC worker and await_async yields the
-- Lua coroutine without blocking OnUpdate or OnRender.
start_async(function()
    local processes = await_async(memory.async_list_processes())
    for _, process in ipairs(processes) do
        print(process.pid .. " " .. process.name)
    end
end)

-- await_async yields only the Lua coroutine. It never blocks the ImGui
-- rendering thread. OnRender should only draw cached values.

start_async(function()
    local value = await_async(memory.async_read_i32(pid, "0x1407FFF0"))
    print("Current HP: " .. value)
end)

-- Example: Write memory
await_async(memory.async_write_i32(pid, 0x1407FFF0, 9999))
```

## Security Features

- **Process Isolation**: GUI runs in user mode, service runs as SYSTEM
- **Handle Protection**: Driver handle is held by service, not GUI
- **Code Separation**: Sensitive logic is isolated in service layer
- **Crash Isolation**: GUI crashes don't affect driver stability

## Development Roadmap

### Phase 1: Basic Communication
- [x] ks-core protocol definitions
- [ ] Basic ks-driver with simple read/write
- [ ] ks-service driver communication
- [ ] GUI connection to service

### Phase 2: UI Integration
- [ ] ImGui overlay rendering
- [ ] Lua engine integration
- [ ] Console and memory viewer

### Phase 3: Advanced Features
- [ ] CR3 page table walking
- [ ] Process hiding/anti-detection
- [ ] Hot-reload script support
- [ ] Multi-client support

## License

This project is for educational purposes only.
