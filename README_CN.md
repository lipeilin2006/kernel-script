# Kernel Script

[项目主页](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md)

## 使用方法

1. 将 `ks-gui.exe` 与其 `scripts` 目录放在同一目录（驱动镜像内嵌在可执行
   文件中：不安装任何东西，目标镜像不落盘）。
2. 以管理员身份运行 `ks-gui.exe`：驱动在启动时静默启动——内置镜像经进程内
   KDU 映射器直接映射进内核（不建服务、不需要签名），若上一次运行留下存活
   实例则直接复用。
3. 点击 `Stop` 关闭 GUI，退出时会静默关闭驱动。

现在没有用户态 service：GUI 通过共享内存 ring 直接与驱动通信。独立诊断请在
提升权限的终端中运行 `ks-test.exe`（full 模式）。GUI 使用 OpenGL，必须从
交互式桌面会话运行。

Kernel Script 是一个仅面向 Windows 的 Rust 工作区，用于通过共享内存 ring
传输和 WDM driver 执行 Luau 脚本。数据流如下：

```text
Lua（同步调用）
    -> ks-gui sync_ipc（阻塞）
    -> ks-sdk 转发 -> ks-link ring 往返（互斥体 + section + 事件）
    -> ks-driver worker 系统线程
        -> 请求执行（目标进程内存访问、锁表修改）
        -> 请求间隙：每轮循环重放一条锁
           （锁表为空：阻塞在请求事件上）
```

## 功能

- Luau JIT、脚本热重载和生命周期回调执行预算。
- 同步进程查找、模块基址、内存读写、RVA、MDL、批量读取、批量写入和指针链 API。
- 通过 `config` API 持久化脚本配置（`config.json`）。
- 键盘输入 API（`is_key_down` / `is_key_up` / `is_key_press`），游戏持有
  输入焦点时同样有效。
- 持续内存锁：每次 lock 调用以一次往返修改 driver 内部锁表；worker 线程在请求
  间隙逐条重放（持锁期间轮询请求事件、请求优先，锁表为空时阻塞等待不耗 CPU）。
- EgUI 透明覆盖层和缓存绘制命令。
- `ks-link` 在用户态使用 Toolhelp 枚举进程并按进程名查 PID，经 `ks-sdk`
  再导出。
- `ks-sdk` 门面：在 crate 根再导出全部 link API，并通过进程内 KDU 映射器
  负责驱动生命周期：`ks_sdk::start()` 映射内置的 `ks-driver.sys`（不建服务、
  不需要签名，目标镜像不落盘），`ks_sdk::stop()` 关闭驱动（构建它需要 MSVC）。
- 内核 section/事件使用手工构建的 DACL（仅 SYSTEM 和 Administrators）；
  ring 状态机加逐层校验防护每个请求。
- `ks-core` 提供 postcard 线格式（LEB128 变长整数 + 小端定长字段），由
  `RING_VERSION` 版本化。
- GUI 负责驱动生命周期：启动时的探测静默启动驱动，窗口关闭后由
  `driver::finish_on_exit` 调用 `ks_sdk::stop()` 关闭驱动，任务进行期间
  `sync_ipc` 拒绝一切内存调用。
- `ks-test.exe` 通过 `ks_sdk::start()`/`stop()` 加载内置 driver（旧的
  `ks-test sc` 走 `sc.exe`），轮询
  `HKLM\SOFTWARE\KernelScript` 获取发布对象名，对自身进程跑完整正确性套件和
  读写基准，发送 `shutdown`，最后验证单实例 claim 释放、发布对象名已删除
  且驱动可再次映射。请使用管理员权限运行。

## 工作区结构

```text
kernel-script/
├── Cargo.toml
├── README.md / README_CN.md
├── document.md / document_CN.md
├── AGENTS.md
├── ks-core/                    # no_std 线协议和 ring 布局
│   └── src/{protocol.rs,ring.rs}
├── ks-driver/                  # WDM 内核 driver
│   ├── build.rs
│   ├── seh_shim.c
│   └── src/{comm.rs,lock.rs,request.rs,memory/,wdm.rs}
├── ks-link/                    # 用户态客户端
│   └── src/{lib.rs,lock.rs,process.rs}
├── ks-sdk/                     # SDK 门面：link API 再导出 + 进程内 KDU 加载
│   ├── build.rs                # KDU 映射核心编译（需要 MSVC）
│   ├── kdu/                    # ks_bridge.cpp + printf 钩子头
│   ├── assets/                 # provider 数据库 + 内置驱动镜像
│   └── src/{lib.rs,kdu.rs}
├── ks-gui/                     # overlay、Luau、同步 IPC 和驱动启停
│   └── src/{app.rs,lua_runtime.rs,lua_runtime/,config_store.rs,driver.rs,overlay.rs,sync_ipc.rs,window_util.rs}
└── ks-test/                    # 内置 driver 的正确性 + benchmark 测试
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
  回调变慢只会降低帧率。参数 `delta_time` 是距离上一帧的秒数，引擎暂停时为
  `0`。它在一次调用中完成计算、同步内存操作、UI 和绘制。
  预算超时仅记录告警，不会报错。
- 回调运行在 GUI Lua 线程；每次内存 API 调用阻塞一次 ring 往返
  （约 10-15 us），因此脚本应限制单次工作量。
- `OnDestroy` 在热重载和退出时执行。

所有 memory API 都是同步调用，并在 GUI Lua 线程执行。完整 API 面包括
`memory.*`（读写、RVA、MDL、批量读取、指针链、内存锁）、`keyboard.*`
（`is_key_down`、`is_key_up`、`is_key_press`——无窗口焦点时同样有效）、
`config.*`（`config.json` 持久化配置）、`ui.*`（egui 控件）、`draw.*`
（覆盖层绘制）和 `engine.*`（计时与暂停控制）。完整 API 见
`document_CN.md` 或英文版 `document.md`。

## 构建和测试

普通工作区检查（需要 MSVC：`ks-sdk` 用 `cc` 编译进程内 KDU 映射器，
`ks-gui`/`ks-test` 依赖它）：

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
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && cargo build --release -p ks-driver --bin ks-driver --features wdk"
```

必须使用 `--release`：dev profile 镜像会在首个请求时溢出 worker 系统线程的
内核栈并触发蓝屏。构建后用 `dumpbin` 检查 native driver image，确认 x64、
Native subsystem、`DriverEntry` 入口点，并确认导入表只有 `ntoskrnl.exe`。

## 运行

驱动创建的 section 和事件对象由内核侧保护，DACL 仅授予 SYSTEM 和
Administrators，因此 GUI（内嵌 `requireAdministrator` 清单）需要以提升权限
运行。

GUI 必须从交互式桌面运行，因为 OpenGL 需要窗口站。Lua 脚本从 GUI
可执行文件旁的 `scripts` 目录加载。文件名 stem 以下划线开头的脚本保留用于
手动测试，但默认不会加载。

随时按 `Insert` 可以显示或隐藏 egui 脚本窗口。UI 隐藏期间 `draw.*` 覆盖层
和所有脚本计算仍然正常运行。

`KernelScript` 窗口在 `Stop` 按钮上方显示当前驱动状态（`probing...` /
`starting...` / `running`，启动失败时红字显示错误）、可滚动的启动日志
（生命周期叙事加上映射器的 `trying provider <id>` 尝试行）以及 `Stop`
按钮：`Stop` 点击后关闭覆盖层窗口，渲染循环返回后 `ks-gui` 静默执行
`ks_sdk::stop()` 并释放进程级会话。启动时的探测发现存活驱动则复用，否则
在后台 worker 中进程内映射内置镜像——所有生命周期细节写入 `ks-gui.log`；
启停进行期间 Lua 内存调用立即返回 “the driver is starting”。

## 安全边界

- `ks-core` 保持 `no_std`、无分配、不依赖 Windows（仅 postcard 和 heapless）。
- driver 负责内存操作和内存锁表，进程枚举由 `ks-link` 负责。
- 内核 section/事件使用手工 DACL 授予 SYSTEM 和 Administrators；没有设备
  对象，也没有 IOCTL 入口。
- 每层边界都校验长度、数量、地址、PID 和大小（ks-core `Request::validate`、
  driver 内、ks-link 内）。
- Lua VM 对象不会跨 worker thread 传递。

## License

本项目仅用于教育和研究目的。
