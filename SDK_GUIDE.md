# Kernel Script SDK 接入指南

面向需要在自己的 Rust 程序里驱动 Kernel Script 的开发者。本指南覆盖依赖引入、驱动生命周期、内存/锁/挂起/hook 全部 API，以及每个调用都必须知道的线程模型和安全边界。

完整语言无关的 Lua API 见 [document.md](document.md) / [document_CN.md](document_CN.md)；本文件只讲 Rust SDK。

## 1. 架构与心智模型

```text
你的程序
  └─ ks-sdk（驱动生命周期 + ks-link 再导出 + shellcode 组装器）
       └─ ks-link（进程枚举 + 环形往返）
            └─ ks-core（协议 / 环形布局，no_std）
                 └─ 内核 ks-driver（worker 系统线程执行请求）
```

三条必须记住的规则：

- **每次调用都是一次同步的环形往返**，约 60–100 µs，会阻塞当前线程。不要在渲染线程或任何实时路径里调用。
- **没有用户态服务**：程序通过共享内存段直接与驱动对话，段/事件的 DACL 只授予 SYSTEM 和 Administrators，因此进程**必须提权运行**。
- **驱动只存在一个实例**。第二次 `start()` 会撞上单实例守卫（`STATUS_OBJECT_NAME_COLLISION`），应当被当成"复用现存实例"而不是失败。

## 2. 环境要求

- Windows 10/11 x64（在 build 26300 上验证）
- 目标机器必须能加载映射驱动；HVCI 会拒绝可执行池分配（hook 的 shellcode/detour 路径会返回 `STATUS_INVALID_PAGE_PROTECTION`，内核文本补丁在 HVCI 下同样可能失败）
- Rust 1.85+，MSVC 工具链（驱动构建需要 WDK，SDK 使用者不需要）
- 私有兄弟仓库 [driver-loader](https://github.com/lipeilin2006/driver-loader) 必须检出为 `ks-driver` 的同级目录 `../driver-toolkit/dt-loader`（`ks-sdk` 通过路径依赖引用它）

## 3. 引入依赖

两个仓库按兄弟目录布局：

```text
<root>/
├── kernel-script/        # 本仓库
└── driver-toolkit/
    └── dt-loader/        # 私有 mapper 仓库
```

你的 `Cargo.toml`：

```toml
[dependencies]
ks-sdk = { path = "../kernel-script/ks-sdk" }
```

`ks-sdk` 自动再导出 `ks-link` 的全部 API 与 `ks-core` 的协议常量，所以你通常只需要 `ks_sdk::xxx` 一个路径。

`ks-sdk` 会把驱动镜像（`ks-sdk/assets/ks-driver.sys`）以 `include_bytes!` 嵌入你的二进制。镜像不在 git 里（被 `*.sys` 忽略），**构建前必须先用 WDK 编译驱动并拷贝到位**：

```powershell
# 在 VS Developer Command Prompt 里
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'
cargo build --release -p ks-driver --bin ks-driver --features wdk
Copy-Item ks-driver.sys ks-sdk/assets/ks-driver.sys -Force
```

## 4. 驱动生命周期

```rust
// 可选：把 mapper 的进度日志接到你自己的日志系统
ks_sdk::set_log_sink(|line| println!("kdu: {line}"));

ks_sdk::start()?;   // 映射内嵌镜像进内核（无需服务、无需签名）
```

`start()` **会先自愈上一次运行的残留**，再映射新实例：

1. 注册表发布（三个随机化对象名 + `Instance` 声明）存在时，先丢掉本进程缓存的会话并探测环形；
2. 有实例应答 → 用 `stop()` 正常关闭它，再映射新实例；
3. 环形版本不匹配（旧版 SDK 留下的实例）→ 用 `ks_link::shutdown_incompatible` 按稳定的 header 布局发一次 `Shutdown`，尽量关掉，避免要求重启；
4. 事件不存在或往返超时 → 判定为崩溃残留，用 `ks_link::clear_stale_publication` 清掉注册表，然后映射新实例。

因此重复调用 `start()` 的语义是"**替换**当前实例"（每次加载都会重新随机化对象名），而不是失败。仍然关不掉的实例（例如完全无响应的旧版驱动）会保留它的单实例标记，此时映射照旧以 `0xC0000035` 失败——记录被清掉也不会产生第二个实例。

`start()` 内部会等待注册表发布（`HKLM\SOFTWARE\KernelScript` 里的三个随机化对象名）完成，因此返回 `Ok(())` 即代表环形链路可用；再显式确认一次也无妨：

```rust
assert!(ks_sdk::published_object_names_strict().is_some());
ks_sdk::ping()?;
```

指定提供者/受害者（排查加载失败时）：

```rust
ks_sdk::start_with(Some(6), Some(1712))?;  // 只跑 provider 6
ks_sdk::start_with(None, None)?;           // 走完整表
```

关闭：

```rust
ks_sdk::stop()?;   // 环形 shutdown + 等待单实例标记释放 + 清理服务回退残留
```

### 环境变量

| 变量 | 作用 |
|---|---|
| `KS_SDK_MAP=1` | 只走手工映射，跳过签名服务回退（适合验证映射链路本身） |
| `KS_SDK_KDU_PRV=<id>` | `start()` 时优先尝试的 provider（旧名 `KS_TEST_KDU_PRV` 仍然生效） |
| `KS_SDK_VICTIM=<build>` | 固定受害者驱动版本（1627 / 1702 / 1712） |

## 5. 错误处理

两个错误类型，分工明确：

**`ks_sdk::Error`** —— 只来自驱动生命周期：

| 变体 | 含义 |
|---|---|
| `DriverEntry(u32)` | `DriverEntry` 返回的 NTSTATUS。`0xC0000035` = 单实例标记仍被一个**无法关闭**的实例持有（正常情况下 `start()` 会先关掉旧实例） |
| `NotReady { .. }` | 加载成功但后续信号超时（5 s） |
| `ScLoad(u32)` | 手工映射失败且服务回退也被拒绝（签名/策略） |
| `Setup(String)` | 映射前的内部步骤失败（临时目录、工作目录切换） |
| `Link(LinkError)` | `stop()` 背后的环形往返失败 |

**`ks_sdk::LinkError`** —— 只来自环形往返：

| 变体 | 含义 |
|---|---|
| `DriverNotReady { object, code }` | 段/事件不存在（驱动未加载），或 DACL 拒绝了你（未提权，`code == 5`） |
| `RingIncompatible` | 映射存在但魔数/版本不匹配（客户端与驱动镜像不同步） |
| `NtStatus(i32)` | 驱动返回的失败状态码 |
| `TimedOut` | 驱动未在取消窗口（5 s）内应答 |
| `TooManyEntries { limit }` | 单次批量条目超限 |
| `Encode` / `Decode` | 请求编码或响应解码失败 |
| `WinApi { operation, code }` | 底层 Win32 调用失败 |

`ProcessError` 只来自进程枚举（Toolhelp 快照），与驱动无关。

## 6. 进程与模块

```rust
let pid = ks_sdk::find_pid("game.exe")?;      // 进程名 -> PID
let base = ks_sdk::get_process_base(pid)?;    // 驱动视角的模块基址

for info in ks_sdk::list()? {                 // 全部进程
    println!("{} ({})", info.name, info.pid);  // ProcessInfo { pid, name }
}
```

> `pid` 不是永久身份：Windows 会复用 PID。读-到-用之间进程可能已退出，
> 后续调用会以 `NtStatus` 失败，不会崩溃。

## 7. 内存读写

最基础的一组，5 个语义开关由 `rva` / `mdl` 两个布尔控制：

```rust
// 普通读写（MmCopyVirtualMemory）
let data = ks_sdk::read_bytes(pid, addr, 16, false, false)?;
ks_sdk::write_bytes(pid, addr, &data, false, false)?;

// RVA：地址相对模块基址解析（等价于 addr = base + rva）
let bytes = ks_sdk::read_bytes(pid, 0x1234, 4, true, false)?;

// MDL：经 MDL 映射读写，可绕过用户态写保护（代码段、只读数据）
ks_sdk::write_bytes(pid, code_addr, &patch, false, true)?;
```

- `rva = true` 时 `address` 相对 `get_process_base(pid)`，与 `address` 是绝对地址互斥语义（两者不可混用同一含义）。
- `mdl = true` 写入的是**共享物理页**：对镜像节的编辑会被所有映射该镜像的进程看到。
- 单次读写上限 `MAX_DRIVER_TRANSFER_SIZE = 4096` 字节，超限在客户端就被拒绝。

## 8. 批量与指针链

```rust
// 一次往返读多个地址（上限 MAX_BATCH_ENTRIES = 256）
let raw = ks_sdk::batch_read(pid, 4, &[a1, a2, a3])?;

// 一次往返写多个地址（上限 MAX_BATCH_WRITE_ENTRIES = 64）
// 返回每个条目的 NTSTATUS（0 = 成功）
let statuses = ks_sdk::batch_write(pid, &[(a1, vec![1, 2, 3, 4])])?;

// 指针链遍历：base -> [o1..oN]，每级解引用 u64 指针（上限 32 级）
let final_addr =
    ks_sdk::traverse_pointer_chain(pid, base, &[0x100, 0x20, 0x8])?;
```

批量读的返回是**紧密排列的原始字节**，按地址顺序拼接，没有分隔；批量写的返回是一条 `Vec<i32>`，与输入顺序一一对应。

## 9. 内存分配

```rust
// 在目标进程分配（ZwAllocateVirtualMemory）
let buf = ks_sdk::alloc(pid, 0x1000, ks_sdk::ALLOC_PROTECT_READWRITE)?;

// PAGE_EXECUTE_READWRITE 是唯一允许的可执行保护
let rx = ks_sdk::alloc(pid, 0x1000, ks_sdk::ALLOC_PROTECT_EXECUTE_READWRITE)?;

ks_sdk::free(pid, buf)?;
```

- `protect` 只接受 `ALLOC_PROTECT_READWRITE`（0x04）与 `ALLOC_PROTECT_EXECUTE_READWRITE`（0x40），其它值在客户端就被拒绝。
- 上限 `MAX_ALLOC_SIZE = 16 MiB`，大小会被内核向上取整到页。
- 分配属于目标进程：目标退出时自动消失，也可用 `free` 提前释放。

## 10. 内存锁（持续写入）

锁让**驱动**在每次请求之间反复重写一段目标内存，不需要你的程序轮询。表在驱动侧，上限 `MAX_MEMORY_LOCKS = 64`。

```rust
ks_sdk::lock(1, pid, addr, &[0x90, 0x90, 0x90, 0x90])?;  // id=1，立即生效
ks_sdk::lock_rva(2, pid, 0x1234, &[0xC3])?;              // RVA 语义，插入时解析一次
ks_sdk::unlock(1)?;                                       // 单个解锁
ks_sdk::unlock_all(pid)?;                                 // 解锁该进程的全部锁

// 导出整个驱动锁表（含其它客户端创建的锁，例如 Lua 脚本）
let locks = ks_sdk::lock_list()?;
for lock in locks {
    println!("id={} pid={} addr=0x{:X} len={}", lock.id, lock.pid, lock.address, lock.len);
}
```

- `id` 必须非 0，驱动内唯一，未使用的 id 不会被回收。
- 锁在驱动卸载/`Shutdown` 时全部丢弃，卸载后驱动不再写任何目标内存。
- 锁重写运行在 `PASSIVE_LEVEL`、不 panic 的内核代码里；任何违反都会变成蓝屏而不是错误返回，因此不要传入无效 PID/地址组合。
- `lock_list()` 不带过滤：一次往返返回驱动侧全部锁（`id`/`pid`/`address`/`len`），不区分是谁创建的，因此 UI 可以把 Lua 脚本创建的锁与自己的锁一起显示。

## 11. 进程挂起 / 继续

```rust
ks_sdk::suspend(pid)?;   // 冻结该进程所有线程（PsSuspendProcess）
ks_sdk::resume(pid)?;    // 继续（PsResumeProcess）
```

- `pid = 0`（Idle）与 `pid = 4`（System）在协议层就被拒绝；冻结 System 会挂死整机。
- **客户端自己的 pid 也会被拒**：冻结正在等待本次往返的进程会死锁。
- 幂等：`suspend` 一个已经冻结的进程不会再次冻结，配套的 `resume` 仍然只运行一次。
- `resume` 只解冻本驱动冻结过的进程，不会减掉别人加的挂起计数。
- **挂起不会被驱动卸载清除**：`Shutdown` / `stop` / 卸载只忘记账本、刻意不解冻，因此冻结的进程会一直冻到客户端调用 `resume`。

## 12. 内核 Hook（MDL，MinHook 风格）

全部 hook API 以 `pid` 为首参数：`pid = 0` 表示内核半区（全局内核文本），非 0 表示该进程的用户半区。

### 12.1 基本用法

```rust
// 1. 探测（只读）：了解 prologue 情况，规划 detour
let probe = ks_sdk::hook_probe(pid, target)?;
//    probe.replaced_len / absolute_len / rip_relative / relative_branch

// 2. 记录一个 hook（默认 disabled）
let hook = ks_sdk::hook_create(pid, target, trampoline)?;
//    hook.id / replaced_len / original（原始字节，可用于重排 trampoline）

ks_sdk::hook_enable(pid, hook.id)?;    // 写入跳转
ks_sdk::hook_disable(pid, hook.id)?;   // 恢复原始字节（表项保留）
ks_sdk::hook_remove(pid, hook.id)?;    // 恢复并释放表项
```

`trampoline` 必须是**你自己已经拥有**的可执行地址，且与 `target` 在同一地址空间。驱动只负责跳转，不生成 trampoline。

### 12.2 Shellcode detour（驱动分配可执行内存）

```rust
let hook = ks_sdk::hook_shellcode(pid, target, &code)?;
//    hook.code_address —— 驱动分配并复制代码的缓冲区地址（内核或目标进程内）
```

驱动分配非分页可执行内存、复制字节、把跳转指向该缓冲区；缓冲区归表项所有，`remove` / `remove_all` / 卸载会在恢复原始字节之后才释放它。上限 `MAX_HOOK_SHELLCODE = 1024` 字节。

### 12.3 驱动生成 trampoline 的高层 detour

```rust
let detour = ks_sdk::hook_detour(pid, target, &code)?;
//    detour.trampoline_address —— 也可通过 call [rip-14] 到达
```

驱动把 `code` 复制到 `buffer + 8`，自己构建 trampoline：把被偷走的指令搬过去并重定位 RIP-relative 位移，尾部接一条跳回 `target + replaced_len` 的绝对跳转；trampoline 地址同时存放在 `buffer + 0`，因此代码里固定 6 字节序列 `FF 15 F2 FF FF FF`（`call qword ptr [rip-14]`）就能调用原函数。

- 被偷走的 prologue 含相对分支 → `STATUS_NOT_SUPPORTED`（重定位需要完整分支重写，驱动拒绝猜测）。
- RIP-relative 只能在 ±2 GiB 内重定位，驱动最多重试 8 次分配后放弃。

### 12.4 用户态写入的安全保证

对用户进程可执行内存的每次 MDL 写，驱动都会**自动**先 `suspend` 该进程、写完再 `resume` —— 保证没有线程正在执行被修改的字节。`suspend` 失败会直接返回错误、不会带着未冻结的进程强行打补丁。

- **自 hook**（`pid` == 你的进程自己）不会被挂起：冻结正在等待本次往返的进程会死锁，因此这条路退回"调用方保证 prologue 期间无人执行"的契约。
- **内核路径**（`pid = 0`）从不挂起：`PsSuspendProcess` 不会停住内核线程，没有可施加的冻结。
- 已由 `suspend()` 冻结的进程不会被重复冻结，驱动也不会替你 `resume` 它（`value = 0`）。
- 如果你**手动**用 `write_mdl` 往可执行页写数据（不走 hook API），`suspend`/`resume` 由你自己负责，见第 11 节。

### 12.5 客户端 shellcode 组装器

```rust
use ks_sdk::shellcode::{Reg, ShellcodeBuilder};

let code = ShellcodeBuilder::new()
    .push_regs(&[Reg::Rax, Reg::Rcx])
    .call_original()                    // FF 15 F2 FF FF FF
    .pop_regs(&[Reg::Rax, Reg::Rcx])
    .ret()
    .build();
```

组装在用户态完成、可单元测试（逐字节校验），驱动内不保留汇编器。

### 12.6 驱动自测

```rust
let patched = ks_sdk::hook_self_test()?;        // 1 = 已打补丁并还原
let shellcode = ks_sdk::hook_shellcode_self_test()?;
let detour = ks_sdk::hook_detour_self_test()?;  // 真实执行一次 detour
```

这三个都在驱动自己的叶子函数上跑完整的 create → enable → verify → disable → remove，不触碰任何外部内核代码，可在开发机上安全运行。

### 12.7 卸载时的强制恢复

```rust
ks_sdk::hook_remove_all()?;
ks_sdk::hook_enable_all()?;
ks_sdk::hook_disable_all()?;
```

`Request::Shutdown`、`comm::stop`、`DriverUnload` 都会先恢复每个补丁过的 prologue 再释放对象 —— 绝不让跳转指向即将消失的内存。

## 13. 并发与线程模型

- 环形是**单请求槽 + 客户端互斥锁**：同一时刻只有一个请求在途，多线程调用是安全的，但会串行化。
- 会话是**进程级**的，首次调用惰性建立；驱动重载后对象名会随机化，旧会话永久失联，必须用 `ks_sdk::close_session()` 丢弃它（或干脆结束进程）。
- **`close_session()` 只能在没有往返在途时调用**。如果驱动生命周期 job 可能关闭/替换会话，必须先用一个门控标志挡住所有往返源（`ks-gui` 就是这么做的：`sync_ipc` 在 job 期间立刻失败）。
- 驱动重载后，下一次往返会立刻失败而不是重连：设计上要求显式重建会话。

## 14. 安全边界与限制

- **段/事件 DACL 只授予 SYSTEM 与 Administrators**：客户端进程必须提权，否则段打开失败并返回 `ERROR_ACCESS_DENIED`。
- **单实例**：第二个 `start()` 必然失败；把它当复用信号而不是错误。
- **`Shutdown` 是破坏性的**：它会让 worker 退出并丢弃锁表与 hook 表；之后所有请求超时直到驱动重新加载。
- **挂起不解冻**（见第 11 节）。
- **内核 hook 不检测 PatchGuard / HVCI**：写受保护页会直接蓝屏，驱动不会替你规避。
- **PIDs 会被复用**，不要跨长时间缓存 PID；每次操作前重新 `find_pid`。
- **不要在实时路径上调用**：每次往返都会阻塞调用线程约 60–100 µs。

## 15. 测试

仓库自带两个提权的验证程序：

- `ks-test.exe full` —— 完整正确性套件（读写、RVA/MDL、批量、指针链、分配、锁、挂起/继续、并发往返），末尾带破坏性的 `Shutdown` 与重载验证
- `ks-hook-test.exe` —— hook 专用端到端：门禁检查 + 三个驱动自测 + 驱动构建 trampoline 的用户态 detour 测试

`ks-test.exe shutdown` 是独立的清理/验证子命令：没有任何存活实例时退出码 0，适合作为残留清理。

## 16. 许可与来源

驱动镜像通过 [dt-loader](https://github.com/lipeilin2006/driver-loader)（KDU 1.5.0 map 核心的 Rust 私有移植）手工映射加载。该仓库是私有的，**mapper 源码、第三方漏洞驱动 blob 与 KDU shellcode 绝不能进入公开仓库**。开发机器需要为工作区、dt-loader 检出目录以及 `%LOCALAPPDATA%\Temp\dt-loader-*` 临时目录添加杀软排除项（`ks-test` 常被误报为 `HackTool:Win64/KduDrv`）。
