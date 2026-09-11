# Kernel Script

A modular kernel-based scripting framework for Windows, featuring a three-tier architecture for enhanced security and stability.

## Architecture

```
┌───────────────────────────────┐
│         ks-gui (GUI)          │  <-- Runs in User Session
└───────────────┬───────────────┘      egui overlay + Lua scripting + Draw API
                │  Named Pipe IPC
┌───────────────▼───────────────┐
│     ks-service (SYSTEM)       │  <-- Runs as NT AUTHORITY\SYSTEM
└───────────────┬───────────────┘      Service bridge + driver dispatch
                │  IOCTL
┌───────────────▼───────────────┐
│      ks-driver (Kernel)       │  <-- Ring 0
└───────────────────────────────┘      Memory read/write + MDL remap
```

## Features

- **Lua Scripting**: Hot-reloadable Lua scripts with coroutine-based async IPC
- **Memory Read/Write**: Normal and MDL-based (bypasses page protection), up to 4096 bytes per transfer
- **Batch Read**: Single IOCTL reads multiple memory regions — one IPC round-trip for N entities
- **RVA API**: Driver-side address calculation from image base + offset
- **Draw API**: Overlay rendering (lines, rectangles, circles, text) on transparent fullscreen window
- **Window Rect**: Query target process window positions via DWM
- **Multi-Window**: Handles processes with multiple windows
- **Transparent Overlay**: GLFW + DWM transparency with mouse passthrough
- **Chinese Font Support**: Auto-loads `msyh.ttc` / `simhei.ttf` / `simsun.ttc`
- **High-Performance IPC**: Channel-based proxy with persistent connections, zero-copy frame handling

## Project Structure

```
kernel-script/
├── Cargo.toml                    # Workspace root
├── README.md / README_CN.md
├── document.md / document_CN.md  # Lua API reference
│
├── ks-core/                      # [R0/R3] Shared protocol & ABI
│   └── src/
│       ├── lib.rs                # no_std compatible
│       ├── protocol.rs           # IOCTL constants, wire messages
│       └── memory.rs             # Memory operation definitions
│
├── ks-driver/                    # [Ring 0] WDM kernel driver
│   ├── build.rs                  # WDK link flags
│   ├── seh_shim.c                # SEH boundary for MmProbeAndLockPages
│   └── src/
│       ├── dispatch.rs           # IOCTL dispatch
│       ├── memory/               # Normal + MDL read/write
│       └── wdm.rs                # FFI declarations
│
├── ks-service/                   # [Ring 3 - SYSTEM] Service + IPC
│   └── src/
│       ├── main.rs               # Service entry / console mode
│       ├── driver_comm.rs        # DeviceIoControl calls
│       ├── ipc.rs                # Named Pipe server
│       └── process.rs            # Toolhelp process enumeration
│
├── ks-gui/                       # [Ring 3 - User] egui overlay + Lua runtime
│   └── src/
│       ├── main.rs               # GUI entry
│       ├── app.rs                # Frame lifecycle, DWM transparency
│       ├── ipc_client.rs         # Named Pipe client
│       ├── lua_runtime.rs        # Lua VM, scheduler, API bindings
│       └── window_util.rs        # Win32 EnumWindows + DwmGetWindowAttribute
│
├── ks-installer/                 # Elevated GUI installer (sc.exe only)
│   └── src/main.rs
│
└── driver-package/               # Deployment (flat layout)
    ├── ks-driver.sys + .pdb
    ├── ks-service.exe + .pdb
    ├── ks-gui.exe + .pdb
    ├── ks-installer.exe + .pdb
    └── scripts/
        ├── monitor.lua           # Process monitor example
        ├── search.lua            # Memory search example
        ├── draw_test.lua         # Overlay draw test
        └── debug_rva.lua         # RVA debugging
```

## Building

### Prerequisites

- Rust 1.75+
- Visual Studio 2022+ with C++ workload
- WDK 10.0.26100.0

### Workspace Check

```powershell
cargo fmt --all
cargo test --workspace
cargo check --workspace
```

### WDK Driver Build

Requires a Visual Studio Developer Command Prompt with WDK environment variables:

```powershell
$env:KS_DRIVER_WDK = '1'
$env:WDK_ROOT = 'C:\Program Files (x86)\Windows Kits\10'
$env:WDK_LIB = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\km\x64'
$env:WDK_VERSION = '10.0.26100.0'

$vs = 'C:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat'
cmd.exe /d /c "call `"$vs`" -arch=x64 -host_arch=x64 >nul && cargo build -p ks-driver --bin ks-driver --features wdk"
```

### Release Build

```powershell
cargo build --release --workspace
cargo build --profile gui-release -p ks-gui    # with unwind for catch_unwind
```

### Deploy to Package Directory

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

## Usage

### 1. Install Driver and Service

Run `ks-installer.exe` as Administrator, or manually:

```cmd
sc.exe create ks-driver type= kernel start= demand binPath= C:\path\to\ks-driver.sys
sc.exe start ks-driver
ks-service.exe --console
```

### 2. Run GUI

```cmd
ks-gui.exe
```

The GUI creates a fullscreen transparent overlay window with DWM transparency. Mouse clicks pass through to applications below unless the cursor is over a UI element.

### 3. Write Lua Scripts

Place `.lua` files in `scripts/`. Each script runs in its own Lua VM; all scripts share scalar values through `shared.set/get`.

#### Poll Pattern (Recommended)

```lua
local state = { pid_task = nil, pid = 0 }

function OnUpdate(dt)
    if state.pid_task then
        local result = memory.poll_async(state.pid_task)
        if result then
            state.pid_task = nil
            if result.error then
                print("Failed:", result.error)
            else
                state.pid = result.value
            end
        end
    end
end

function OnRender()
    ui.window("Example", function()
        if ui.button("Start") and not state.pid_task then
            state.pid_task = memory.async_get_pid("notepad.exe")
        end
        ui.label("PID: " .. tostring(state.pid))
    end)
end
```

#### Coroutine Pattern

```lua
start_async(function()
    local pid = await_async(memory.async_get_pid("notepad.exe"))
    local hp = await_async(memory.async_read_i32(pid, 0x1407FFF0))
    print("HP: " .. hp)
end)
```

#### Draw Overlay

```lua
function OnRender()
    -- Draw on transparent fullscreen overlay
    draw.rect(100, 100, 200, 150, 255, 0, 0, 200, 3.0)
    draw.filled_rect(100, 100, 200, 150, 0, 255, 0, 50)
    draw.circle(200, 175, 30.0, 255, 255, 0, 220, 2.0)
    draw.filled_circle(200, 175, 30.0, 255, 200, 0, 160)
    draw.line(100, 100, 300, 250, 255, 255, 255, 200, 1.0)
    draw.text(100, 260, "Hello!", 0, 255, 0, 255, 14.0)
end
```

## Security

- **Process Isolation**: GUI in user mode, service as SYSTEM, driver in Ring 0
- **Handle Protection**: Driver device handle held only by the service
- **DACL**: Device object uses `D:P(A;;GA;;;SY)` — SYSTEM-only access
- **Crash Isolation**: GUI crashes do not affect driver or service stability
- **Session Isolation**: Service runs in session 0; window enumeration runs in GUI process

## License

This project is for educational purposes only.
