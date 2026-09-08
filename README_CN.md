# Kernel Script

基于内核的模块化 Windows 脚本框架，三层架构设计，兼顾安全性与稳定性。

## 架构

```
┌───────────────────────────────┐
│         ks-gui (图形界面)      │  <-- 运行在普通用户会话
└───────────────┬───────────────┘      egui 渲染 + Lua 脚本
                │  Named Pipe IPC
┌───────────────▼───────────────┐
│     ks-service (系统服务)      │  <-- 以 NT AUTHORITY\SYSTEM 运行
└───────────────┬───────────────┘      服务桥接 + 驱动分发
                │  IOCTL
┌───────────────▼───────────────┐
│      ks-driver (内核驱动)      │  <-- Ring 0
└───────────────────────────────┘      内存读写 + MDL 重映射
```

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
├── ks-gui/                       # [Ring 3 - User] egui + Lua 运行时
│   └── src/
│       ├── main.rs               # GUI 入口
│       ├── app.rs                # 帧生命周期
│       ├── ipc_client.rs         # Named Pipe 客户端
│       ├── lua_runtime.rs        # Lua VM、调度器、API 绑定
│       └── views/                # UI 面板
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
Copy-Item D:\kernel-script\target\release\ks-gui.exe "$pkg\ks-gui.exe" -Force
Copy-Item D:\kernel-script\target\release\ks_gui.pdb "$pkg\ks_gui.pdb" -Force
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

### 3. 编写 Lua 脚本

将 `.lua` 文件放入 `scripts/` 目录。每个脚本在独立 Lua VM 中运行；所有脚本通过 `shared.set/get` 共享标量值。

```lua
start_async(function()
    local pid = await_async(memory.async_get_pid("notepad.exe"))
    local hp = await_async(memory.async_read_i32(pid, 0x1407FFF0))
    print("HP: " .. hp)

    -- MDL 写入绕过只读页保护
    await_async(memory.async_write_mdl(pid, 0x1407FFF0, { 0xFF, 0x00, 0x00, 0x00 }))

    -- 列出所有进程
    for _, p in ipairs(await_async(memory.async_list_processes())) do
        print(p.pid, p.name)
    end
end)
```

## 安全特性

- **进程隔离**：GUI 运行在用户模式，服务以 SYSTEM 运行，驱动在 Ring 0
- **句柄保护**：驱动设备句柄仅由服务持有
- **DACL**：设备对象使用 `D:P(A;;GA;;;SY)` — 仅 SYSTEM 可访问
- **崩溃隔离**：GUI 崩溃不影响驱动或服务稳定性

## 许可证

本项目仅供学习用途。
