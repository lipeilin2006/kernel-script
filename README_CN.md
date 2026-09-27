# Kernel Script

[项目主页](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md)

## 使用方法

1. 将 `ks-launcher.exe`、`ks-driver.sys`、`ks-service.exe` 和 `ks-gui.exe`
   放在同一个目录。
2. 以管理员身份运行 `ks-launcher.exe`。
3. 点击 `Start Driver`。
4. Driver 启动后，点击 `Start Service`。
5. Service 启动后，点击 `Start GUI`。
6. 关闭时必须按 `Stop GUI`、`Stop Service`、`Stop Driver` 的顺序操作。

Launcher 启动前会根据自身同级目录中的文件重新创建 driver 和 service
注册。运行中的组件显示红色 `Stop ...` 按钮。所有命令输出和错误统一写入
launcher 同级的 `ks-launcher.log`。

service 手动诊断需要在提升权限的终端中运行 `ks-service.exe --console`。
GUI 使用 GLFW/OpenGL，必须从交互式桌面会话运行。

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
- 通过 `config` API 持久化脚本配置（`config.json`）。
- 键盘输入 API（`is_key_down` / `is_key_up` / `is_key_press`），游戏持有
  输入焦点时同样有效。
- 持续内存锁：service 每轮 sweep 用一次批量写 IOCTL 重放全部锁。
- EgUI/GLFW 透明覆盖层和缓存绘制命令。
- `ks-service` 在用户态使用 Toolhelp 枚举进程。
- SYSTEM-only driver 设备访问和首次打开进程绑定。
- Driver IOCTL 的敏感整数按显式小端序明文传输，包括 PID、地址、RVA、基址和指针
  字段；大小、数量和数据继续使用相同的显式线格式。
- `ks-core` 提供显式小端序的分帧 IPC 协议。
- launcher 每次启动随机化 SCM 服务名，全部停止后恢复组件原始文件名。

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
│   └── src/{app.rs,lua_runtime.rs,lua_runtime/,config_store.rs,overlay.rs,sync_ipc.rs,window_util.rs}
├── ks-launcher/               # 三步启动器
└── ks-test/                   # 独立 IPC benchmark 客户端
```

## Lua 生命周期

每个 GUI 加载的 `.lua` 文件运行在独立的 Luau VM 中：

```lua
function OnStart() end
function OnUpdate(dt) end
function OnDestroy() end
```

- `OnStart` 在加载后执行一次。
- `OnUpdate` 是唯一的每帧回调。整个覆盖层（渲染 + OnUpdate）帧率封顶 60Hz；
  回调变慢只会降低帧率。它在一次调用中完成计算、同步内存操作、UI 和绘制。
  预算超时仅记录告警，不会报错。
- 回调运行在 GUI Lua 线程；较长的同步 IPC 仍会延迟下一帧，因此脚本应限制单次工作量。
- `OnDestroy` 在热重载和退出时执行。

所有 memory API 都是同步调用，并在 GUI Lua 线程执行。完整 API 面包括
`memory.*`（读写、RVA、MDL、批量读取、指针链、内存锁）、`keyboard.*`
（`is_key_down`、`is_key_up`、`is_key_press`——无窗口焦点时同样有效）、
`config.*`（`config.json` 持久化配置）、`ui.*`（egui 控件）、`draw.*`
（覆盖层绘制）和 `engine.*`（计时与暂停控制）。完整 API 见
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

随时按 `Insert` 可以显示或隐藏 egui 脚本窗口。UI 隐藏期间 `draw.*` 覆盖层
和所有脚本计算仍然正常运行。

launcher 只提供三个顺序操作：`Start Driver`、`Start Service`、`Start GUI`。
前一项未运行时，后一项不可点击；运行中的项目显示红色 `Stop ...` 按钮。
关闭时必须按 GUI、Service、Driver 的顺序操作。错误和命令输出统一写入
`ks-launcher.log`。

## 安全边界

- `ks-core` 保持无依赖并兼容 `no_std`。
- driver 只负责内存操作，进程枚举由 service 负责。
- driver 设备使用 SYSTEM-only DACL，并绑定首次成功打开设备的进程。
- 协议、service 和 driver 边界都校验长度、数量、地址、PID 和帧大小。
- Lua VM 对象不会跨 worker thread 传递。

## License

本项目仅用于教育和研究目的。
