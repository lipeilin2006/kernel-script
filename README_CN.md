# Kernel Script

Kernel Script 是一个仅面向 Windows 的 Rust 工作区，用于通过受保护的用户态
service 和 WDM driver 执行 Luau 脚本。运行时分层如下：

```text
Luau 脚本
    -> ks-gui 同步 Named Pipe 客户端
    -> ks-service Tokio Named Pipe 服务端
    -> DeviceIoControl
    -> ks-driver WDM 内存操作
```

## 功能

- Luau JIT、脚本热重载和生命周期回调执行预算。
- 同步进程查找、模块基址、内存读写、RVA、MDL、批量读取和指针链 API。
- EgUI/GLFW 透明覆盖层和缓存绘制命令。
- `ks-service` 在用户态使用 Toolhelp 枚举进程。
- SYSTEM-only driver 设备访问和首次打开进程绑定。
- `ks-core` 提供显式小端序的分帧 IPC 协议。

## 工作区结构

```text
kernel-script/
├── Cargo.toml
├── README.md / README_CN.md
├── document.md / document_CN.md
├── AGENTS.md
├── ks-core/                    # no_std 协议和 ABI
├── ks-driver/                  # WDM 内核 driver
│   ├── build.rs
│   ├── seh_shim.c
│   └── src/{dispatch.rs,memory/,wdm.rs}
├── ks-service/                 # SYSTEM service 和 IPC
│   └── src/{main.rs,driver_comm.rs,ipc.rs,process.rs}
├── ks-gui/                    # overlay、Luau 和同步 IPC
│   └── src/{app.rs,lua_runtime.rs,lua_runtime/,sync_ipc.rs,window_util.rs}
├── ks-installer/              # 使用 sc.exe 的 service 管理器
└── ks-test/                   # 独立 IPC benchmark 客户端
```

## Lua 生命周期

每个 GUI 加载的 `.lua` 文件运行在独立的 Luau VM 中：

```lua
function OnStart() end
function OnUpdate(dt) end
function OnRender() end
function OnDestroy() end
```

- `OnStart` 在加载后执行一次。
- `OnUpdate` 由固定逻辑调度器调用，可以执行同步内存操作。
- `OnRender` 应主要绘制 UI 和读取缓存结果，不应在 UI 回调中重复执行阻塞 IPC。
- `OnDestroy` 在热重载和退出时执行。

所有 memory API 都是同步调用，并在 GUI Lua 线程执行。完整 API 见
`document_CN.md` 或英文版 `document.md`。

## 构建和测试

普通工作区检查：

```powershell
cargo fmt --all
cargo test --workspace
cargo check --workspace
cargo build --release --workspace
```

GUI 需要运行时 panic 恢复时使用 unwind profile：

```powershell
cargo build --profile gui-release -p ks-gui
```

driver 必须在 Visual Studio Developer Command Prompt 中使用 WDK 单独构建。
当前支持的 WDK 版本为 `10.0.26100.0`：

```powershell
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'

$vs = 'C:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat'
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && set `"KS_DRIVER_WDK=1`" && set `"WDK_ROOT=C:\Program Files (x86)\Windows Kits\10`" && set `"WDK_LIB=C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64`" && set `"WDK_VERSION=10.0.26100.0`" && cargo build --release -p ks-driver --bin ks-driver --features wdk"
```

使用 `dumpbin` 检查 native driver image，确认 x64、Native subsystem、
`DriverEntry` 入口点，并确认没有用户态 DLL 导入。

## 运行

service 必须以 SYSTEM 身份运行才能打开 driver 设备。交互式诊断可以在提升
权限的环境中运行：

```cmd
ks-service.exe --console
```

GUI 必须从交互式桌面运行，因为 GLFW/OpenGL 需要窗口站。Lua 脚本从 GUI
可执行文件旁的 `scripts` 目录加载。文件名 stem 以下划线开头的脚本保留用于
手动测试，但默认不会加载。

installer 只使用 `sc.exe` 管理 service 注册，不复制源码，也不管理构建产物。

## 安全边界

- `ks-core` 保持无依赖并兼容 `no_std`。
- driver 只负责内存操作，进程枚举由 service 负责。
- driver 设备使用 SYSTEM-only DACL，并绑定首次成功打开设备的进程。
- 协议、service 和 driver 边界都校验长度、数量、地址、PID 和帧大小。
- Lua VM 对象不会跨 worker thread 传递。

## License

本项目仅用于教育和研究目的。
