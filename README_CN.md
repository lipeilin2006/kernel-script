# Kernel Script

> **免责声明**：本项目仅供安全研究与学习交流使用，禁止用于任何违法或未
> 授权用途（例如：网络游戏作弊或破坏反作弊系统、入侵或篡改他人系统、恶意
> 破坏、商业滥用等）。因不当使用造成的一切后果由使用者自行承担。
>
> **闲鱼唯一官方商户：米烧酒可玛丽**；其他同名或近似店铺均与本项目无关。

[项目主页](https://github.com/lipeilin2006/kernel-script) | [English README](https://github.com/lipeilin2006/kernel-script/blob/main/README.md) | [Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document.md) | [中文 Lua API](https://github.com/lipeilin2006/kernel-script/blob/main/document_CN.md) | [Rust SDK 接入指南](https://github.com/lipeilin2006/kernel-script/blob/main/SDK_GUIDE.md)

## 使用方法

1. 将 `ks-gui.exe`、`ks-inspector.exe` 与 overlay 的 `scripts` 目录放在同一
   目录（驱动镜像内嵌在两个可执行文件中：不安装任何东西，目标镜像不落盘）。
2. 以管理员身份运行 `ks-gui.exe`：驱动在启动时静默启动——内置镜像经进程内
   KDU 映射器直接映射进内核（不建服务、不需要签名）。上一次运行留下的实例若
   仍能应答则直接复用；否则先清掉注册表残留，再映射一个新实例。
3. 需要 Cheat Engine 风格的内存工具时，同样以管理员身份运行
   `ks-inspector.exe`：它复用 `ks-gui` 已启动的驱动，两者可同时运行，最后
   关闭的那个负责静默关闭驱动。

现在没有用户态 service：两个程序都通过共享内存 ring 直接与驱动通信。独立诊断
请在提升权限的终端中运行 `ks-test.exe`（full 模式）。两个程序都通过 D3D11
交换链呈现，必须从交互式桌面会话运行。

Kernel Script 是一个仅面向 Windows 的 Rust 工作区，用于通过共享内存 ring
传输和 WDM driver 执行 Luau 脚本。数据流如下：

```text
ks-gui Lua（同步调用）              ks-inspector worker（同步调用）
    -> ks-gui sync_ipc（阻塞）          -> ks-ui-common sync_ipc（阻塞）
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
- EgUI 0.36 透明覆盖层，经 D3D11 交换链渲染；绘制命令每帧重新生成，显示循环
  100Hz，右上角显示 FPS。
- 独立的 `ks-inspector` 内存工具：进程列表、以模块基址为锚点分页的十六进制
  视图、Lua 脚本编辑器、数值修改器和驱动完整锁表，并带一个把选择持久化到
  `config.json` 的 `中文 / EN` 开关。它与 `ks-gui` 共用同一个驱动（单实例
  守卫让第二个程序直接接管已存在的实例）。
- `ks-link` 在用户态使用 Toolhelp 枚举进程并按进程名查 PID，经 `ks-sdk`
  再导出。
- `ks-sdk` 门面：在 crate 根再导出全部 link API，并通过进程内 KDU 映射器
  负责驱动生命周期：`ks_sdk::start()` 映射内置的 `ks-driver.sys`（不建服务、
  不需要签名，目标镜像不落盘），`ks_sdk::stop()` 关闭驱动。
- 内核 section/事件使用手工构建的 DACL（仅 SYSTEM 和 Administrators）；
  ring 状态机加逐层校验防护每个请求。
- `ks-core` 提供 postcard 线格式（LEB128 变长整数 + 小端定长字段），由
  `RING_VERSION` 版本化。
- 共享 `ks-ui-common` crate：驱动生命周期与 `sync_ipc` 门闸、阻塞式往返
  门面、配置存储、双语字符串表和中文字体安装，两个桌面程序都链接它，
  避免各自实现产生漂移。
- 两个程序共同负责驱动生命周期：启动时的探测静默启动驱动，最后一个程序
  关闭后由 `driver::finish_on_exit` 调用 `ks_sdk::stop()` 关闭驱动，任务进行
  期间 `sync_ipc` 拒绝一切内存调用。
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
│   ├── assets/                 # 内置驱动镜像
│   └── src/{lib.rs,kdu.rs}
├── ks-ui-common/               # 两个桌面程序共享的用户态基础设施
│   └── src/{driver.rs,sync_ipc.rs,config_store.rs,lang.rs,font.rs}
├── ks-gui/                     # overlay、Luau（含小型控制窗口）
│   └── src/{app.rs,lua_runtime.rs,lua_runtime/,overlay.rs,window_util.rs}
├── ks-inspector/               # 独立内存工具（进程列表、hex、脚本、锁表）
│   └── src/{main.rs,inspector.rs}
└── ks-test/                    # 内置 driver 的正确性 + benchmark 测试
```

## Lua 生命周期

每个由 `ks-gui` 加载的 `.lua` 文件运行在独立的 Luau VM 中：

```lua
function OnStart() end
function OnUpdate(dt) end
function OnDestroy() end
```

- `OnStart` 在加载后执行一次。
- `OnUpdate` 是唯一的每帧回调。整个覆盖层（渲染 + OnUpdate）按 100Hz 显示
  节奏运行；回调变慢只会降低帧率，右上角显示实际帧率。参数 `delta_time` 是距离
  上一帧的秒数，引擎暂停时为 `0`。它在一次调用中完成计算、同步内存操作、UI 和
  绘制。预算超时仅记录告警，不会报错。
- 回调运行在 GUI Lua 线程；每次内存 API 调用阻塞一次 ring 往返
  （约 10-15 us），因此脚本应限制单次工作量。
- `OnDestroy` 在热重载和退出时执行。

所有 memory API 都是同步调用，并在 GUI Lua 线程执行。完整 API 面包括
`memory.*`（读写、RVA、MDL、批量读取、指针链、内存锁）、`keyboard.*`
（`is_key_down`、`is_key_up`、`is_key_press`——无窗口焦点时同样有效）、
`config.*`（`config.json` 持久化配置）、`ui.*`（egui 控件）、`draw.*`
（覆盖层绘制）和 `engine.*`（计时与暂停控制）。完整 API 见
`document_CN.md` 或英文版 `document.md`。

`ks-inspector` 不是脚本宿主：它的进程列表、hex 视图、数值编辑器和锁表都通过
同一个单 worker 线程调用同步 SDK（绝不在渲染线程调用），因此不会阻塞任何 Lua
VM。

## 构建和测试

普通工作区检查（映射器是纯 Rust 的 `dt-loader` 同级 crate，通过 path 依赖
引入；工作区构建不需要 C++ 工具链）：

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

`ks-inspector` 没有 `catch_unwind`，使用普通 release profile 即可：

```powershell
cargo build --release -p ks-inspector
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
Administrators，因此 `ks-gui` 和 `ks-inspector`（都内嵌
`requireAdministrator` 清单）需要以提升权限运行。

两者都必须从交互式桌面运行，因为 D3D11 需要窗口站。Lua 脚本从 `ks-gui`
可执行文件旁的 `scripts` 目录加载。文件名 stem 以下划线开头的脚本保留用于
手动测试，但默认不会加载。

随时按 `Insert` 可以显示或隐藏 egui 脚本窗口。UI 隐藏期间 `draw.*` 覆盖层
和所有脚本计算仍然正常运行。

`KernelScript` 控制窗口是 overlay 唯一的界面：显示当前驱动状态
（`probing...` / `starting...` / `running`，启动失败时红字显示错误）、
`中文 / EN` 开关和 `退出程序` / `Quit` 按钮。按钮关闭 overlay 后，渲染循环
返回时 `ks-gui` 进入共享退出路径（`driver::finish_on_exit`）并释放进程级
会话。启动时的探测发现存活驱动则复用，否则在后台 worker 中进程内映射内置
镜像——生命周期叙事写入 `ks-gui.log`；启停进行期间 Lua 内存调用立即返回
“the driver is starting”。

`ks-inspector` 是独立内存工具：左侧进程列表、底部整宽驱动日志栏（驱动状态
加上启动日志，包括映射器的 `trying provider <id>` 尝试行；生命周期叙事同时
写入 `ks-inspector.log`），中间是十六进制视图、`scripts/*.lua` 脚本编辑器和
锁/数值表标签页。关闭它不影响 `ks-gui`（以及驱动）；两个程序中最后一个关闭时
才停止驱动，崩溃残留的实例会在下次启动或下次最终退出时被复用并清理。

## 安全边界

- `ks-core` 保持 `no_std`、无分配、不依赖 Windows（仅 postcard 和 heapless）。
- driver 负责内存操作和内存锁表，进程枚举由 `ks-link` 负责。
- 内核 section/事件使用手工 DACL 授予 SYSTEM 和 Administrators；没有设备
  对象，也没有 IOCTL 入口。
- 每层边界都校验长度、数量、地址、PID 和大小（ks-core `Request::validate`、
  driver 内、ks-link 内）。
- Lua VM 对象不会在 `ks-gui` 中跨 worker thread 传递；`ks-inspector` 只与
  其 worker 交换拥有所有权的 `Command`/`Event` 值，且绝不在渲染线程调用驱动。

## License

本项目仅用于教育和研究目的。
