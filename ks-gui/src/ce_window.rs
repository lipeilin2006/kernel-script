//! Independent Cheat Engine-style memory tool window.
//!
//! A second, ordinary decorated glfw window (the overlay keeps the Lua/ESP
//! side). It owns its own `egui::Context` and `ThreeDBackend`, so its
//! rendering never mixes with the overlay pass.
//!
//! Layout: a menu bar, the process list on the left, a full-width log bar at
//! the bottom (driver phase plus the lifecycle/startup log) and a tabbed
//! centre: hex viewer, Lua script editor and the value editor. The hex view
//! is anchored at the target's module base and pages forward in a selectable
//! length; the current page is re-read every frame, so changed bytes show up
//! highlighted.
//!
//! Driver round trips never run on the render thread: the UI sends
//! [`Command`]s to one worker thread (through the gated `sync_ipc` facade)
//! and drains [`Event`]s each frame.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use egui_overlay::egui_render_three_d::{ThreeDBackend, ThreeDConfig};
use egui_overlay::egui_window_glfw_passthrough::glfw::{
    self, Action, Context as _, GlfwReceiver, Key, Modifiers as GlfwModifiers, MouseButton,
    PWindow, WindowEvent, WindowHint, WindowMode,
};
use egui_overlay::egui_window_glfw_passthrough::GlfwBackend;

use crate::sync_ipc;

/// Bytes shown by one hex page. The driver caps a single read at 4096.
const HEX_LENGTHS: [usize; 5] = [256, 512, 1024, 2048, 4096];
/// Default page length.
const DEFAULT_HEX_LEN: usize = 512;

/// One value type the address list can display and edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueType {
    I8,
    I16,
    I32,
    I64,
    U32,
    F32,
    F64,
}

impl ValueType {
    const ALL: [Self; 7] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::U32,
        Self::F32,
        Self::F64,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::I8 => "1 字节",
            Self::I16 => "2 字节",
            Self::I32 => "4 字节",
            Self::I64 => "8 字节",
            Self::U32 => "4 字节 (无符号)",
            Self::F32 => "浮点",
            Self::F64 => "双精度",
        }
    }

    fn size(self) -> usize {
        match self {
            Self::I8 => 1,
            Self::I16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
        }
    }

    fn format(self, bytes: &[u8]) -> Result<String, String> {
        let need = self.size();
        if bytes.len() < need {
            return Err(format!("short read: {} of {need} bytes", bytes.len()));
        }
        let bytes = &bytes[..need];
        Ok(match self {
            Self::I8 => (bytes[0] as i8).to_string(),
            Self::I16 => i16::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            Self::I32 => i32::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            Self::I64 => i64::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            Self::U32 => u32::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            Self::F32 => f32::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            Self::F64 => f64::from_le_bytes(bytes.try_into().unwrap()).to_string(),
        })
    }

    fn parse(self, text: &str) -> Result<Vec<u8>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("value is empty".into());
        }
        let integer = |signed: bool| -> Result<i128, String> {
            let literal = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"));
            match (literal, signed) {
                (Some(hex), _) => {
                    let raw =
                        u64::from_str_radix(hex, 16).map_err(|_| format!("bad hex {text:?}"))?;
                    Ok(raw as i128)
                }
                (None, true) => text
                    .parse::<i128>()
                    .map_err(|_| format!("bad number {text:?}")),
                (None, false) => text
                    .parse::<u64>()
                    .map(|value| value as i128)
                    .map_err(|_| format!("bad number {text:?}")),
            }
        };
        Ok(match self {
            Self::I8 => vec![(integer(true)? as i8) as u8],
            Self::I16 => (integer(true)? as i16).to_le_bytes().to_vec(),
            Self::I32 => (integer(true)? as i32).to_le_bytes().to_vec(),
            Self::I64 => (integer(true)? as i64).to_le_bytes().to_vec(),
            Self::U32 => (integer(false)? as u32).to_le_bytes().to_vec(),
            Self::F32 => text
                .parse::<f32>()
                .map_err(|_| format!("bad float {text:?}"))?
                .to_le_bytes()
                .to_vec(),
            Self::F64 => text
                .parse::<f64>()
                .map_err(|_| format!("bad double {text:?}"))?
                .to_le_bytes()
                .to_vec(),
        })
    }
}

/// One address-list entry (value editor tab).
struct AddrRow {
    id: u64,
    address: u64,
    kind: ValueType,
    value: String,
    frozen: bool,
    lock_id: u64,
    status: String,
}

/// Commands executed on the worker thread (each is one driven operation).
enum Command {
    ListProcesses,
    GetBase {
        pid: u64,
    },
    Read {
        row: u64,
        pid: u64,
        address: u64,
        kind: ValueType,
    },
    Write {
        row: u64,
        pid: u64,
        address: u64,
        data: Vec<u8>,
    },
    Freeze {
        row: u64,
        pid: u64,
        lock_id: u64,
        address: u64,
        data: Vec<u8>,
    },
    Unfreeze {
        row: u64,
        lock_id: u64,
    },
    UnlockAll {
        pid: u64,
    },
    ReadHex {
        pid: u64,
        address: u64,
        size: usize,
    },
}

/// Worker results drained by the UI thread.
enum Event {
    Processes(Result<Vec<(u32, String)>, String>),
    Base(Result<u64, String>),
    Row {
        row: u64,
        value: Option<String>,
        result: Result<String, String>,
    },
    Hex {
        address: u64,
        size: usize,
        result: Result<Vec<u8>, String>,
    },
    Message(String),
}

fn worker(rx: Receiver<Command>, tx: Sender<Event>) {
    while let Ok(command) = rx.recv() {
        let event = match command {
            Command::ListProcesses => Event::Processes(
                ks_sdk::list()
                    .map(|processes| {
                        processes
                            .into_iter()
                            .map(|process| (process.pid, process.name))
                            .collect()
                    })
                    .map_err(|error| error.to_string()),
            ),
            Command::GetBase { pid } => Event::Base(sync_ipc::get_process_base(pid)),
            Command::Read {
                row,
                pid,
                address,
                kind,
            } => {
                let result = sync_ipc::read_bytes(pid, address, kind.size() as u64)
                    .and_then(|bytes| kind.format(&bytes));
                match result {
                    Ok(value) => Event::Row {
                        row,
                        value: Some(value),
                        result: Ok(String::new()),
                    },
                    Err(error) => Event::Row {
                        row,
                        value: None,
                        result: Err(error),
                    },
                }
            }
            Command::Write {
                row,
                pid,
                address,
                data,
            } => Event::Row {
                row,
                value: None,
                result: sync_ipc::write_bytes(pid, address, &data).map(|()| String::from("已写入")),
            },
            Command::Freeze {
                row,
                pid,
                lock_id,
                address,
                data,
            } => Event::Row {
                row,
                value: None,
                result: sync_ipc::lock(lock_id, pid, address, &data)
                    .map(|()| String::from("冻结中")),
            },
            Command::Unfreeze { row, lock_id } => Event::Row {
                row,
                value: None,
                result: sync_ipc::unlock(lock_id).map(|()| String::from("已解冻")),
            },
            Command::UnlockAll { pid } => Event::Message(match sync_ipc::unlock_all(pid) {
                Ok(()) => String::from("已解除全部冻结"),
                Err(error) => error,
            }),
            Command::ReadHex { pid, address, size } => Event::Hex {
                address,
                size,
                result: sync_ipc::read_bytes(pid, address, size as u64),
            },
        };
        if tx.send(event).is_err() {
            break;
        }
    }
}

/// One read of the hex page: the bytes plus which of them changed since
/// the previous frame's read.
struct HexData {
    address: u64,
    data: Vec<u8>,
    changed: Vec<bool>,
    error: Option<String>,
}

/// The centre tabs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Hex,
    Editor,
    Modify,
}

/// Lua script editor state. Files live in the overlay's `scripts` folder;
/// saving writes the file, which the overlay's hot reload then picks up.
struct LuaEditor {
    dir: PathBuf,
    files: Vec<PathBuf>,
    scanned: bool,
    selected: Option<PathBuf>,
    buffer: String,
    saved: String,
    /// Edits of files that were switched away from without saving.
    drafts: HashMap<PathBuf, String>,
    new_name: String,
    status: String,
}

impl LuaEditor {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            files: Vec::new(),
            scanned: false,
            selected: None,
            buffer: String::new(),
            saved: String::new(),
            drafts: HashMap::new(),
            new_name: String::new(),
            status: String::new(),
        }
    }

    fn dirty(&self) -> bool {
        self.selected.is_some() && self.buffer != self.saved
    }

    fn scan(&mut self) {
        self.scanned = true;
        self.files = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "lua"))
            .collect();
        self.files.sort();
        if self.selected.is_none() {
            if let Some(first) = self.files.first() {
                self.open(first.clone());
            }
        }
        self.status = format!("{} 个脚本 · {}", self.files.len(), self.dir.display());
    }

    fn open(&mut self, path: PathBuf) {
        if let Some(current) = self.selected.take() {
            if self.dirty() {
                self.drafts.insert(current, self.buffer.clone());
            }
        }
        self.buffer = match self.drafts.remove(&path) {
            Some(draft) => draft,
            None => std::fs::read_to_string(&path).unwrap_or_else(|error| {
                self.status = format!("读取失败: {error}");
                String::new()
            }),
        };
        self.saved = self.buffer.clone();
        self.selected = Some(path);
    }

    fn save(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        match std::fs::write(&path, &self.buffer) {
            Ok(()) => {
                self.saved = self.buffer.clone();
                self.drafts.remove(&path);
                self.status = format!(
                    "已保存 {}（覆盖层会自动热重载）",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
            Err(error) => self.status = format!("保存失败: {error}"),
        }
    }

    fn create(&mut self) {
        let name = self.new_name.trim();
        if name.is_empty() || name.contains(['/', '\\']) {
            self.status = "文件名无效".to_owned();
            return;
        }
        let name = if name.ends_with(".lua") {
            name.to_owned()
        } else {
            format!("{name}.lua")
        };
        let path = self.dir.join(&name);
        if path.exists() {
            self.status = "文件已存在".to_owned();
            return;
        }
        match std::fs::write(
            &path,
            "-- KernelScript Lua script\nfunction OnUpdate(dt)\nend\n",
        ) {
            Ok(()) => {
                self.new_name.clear();
                self.scan();
                self.open(path);
                self.status = format!("已创建 {name}");
            }
            Err(error) => self.status = format!("创建失败: {error}"),
        }
    }
}

/// UI-side state, all owned by the render thread.
struct ToolState {
    processes: Vec<(u32, String)>,
    filter: String,
    selected_pid: Option<u64>,
    base: Option<u64>,
    tab: Tab,
    /// Hex view: the window starts at the process base and pages forward
    /// in `hex_len`-byte chunks; every frame re-reads the current page.
    hex_page: u64,
    hex_len: usize,
    hex: Option<HexData>,
    hex_pending: bool,
    /// Value editor tab.
    rows: Vec<AddrRow>,
    next_row_id: u64,
    next_lock_id: u64,
    new_address: String,
    new_kind: ValueType,
    /// Lua editor tab.
    editor: LuaEditor,
    status: String,
}

impl ToolState {
    fn new() -> Self {
        let scripts = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("scripts")))
            .unwrap_or_else(|| PathBuf::from("scripts"));
        Self {
            processes: Vec::new(),
            filter: String::new(),
            selected_pid: None,
            base: None,
            tab: Tab::Hex,
            hex_page: 0,
            hex_len: DEFAULT_HEX_LEN,
            hex: None,
            hex_pending: false,
            rows: Vec::new(),
            next_row_id: 1,
            next_lock_id: 1,
            new_address: String::new(),
            new_kind: ValueType::I32,
            editor: LuaEditor::new(scripts),
            status: "选择左侧进程，查看它的模块内存".to_owned(),
        }
    }

    fn hex_start(&self) -> Option<u64> {
        self.base
            .map(|base| base.wrapping_add(self.hex_page.saturating_mul(self.hex_len as u64)))
    }

    fn select_process(&mut self, pid: u64, commands: &Sender<Command>) {
        if let Some(old) = self.selected_pid {
            if !self.rows.is_empty() {
                let _ = commands.send(Command::UnlockAll { pid: old });
            }
        }
        self.selected_pid = Some(pid);
        self.base = None;
        self.rows.clear();
        self.hex = None;
        self.hex_page = 0;
        let _ = commands.send(Command::GetBase { pid });
    }

    fn forget_process(&mut self, commands: &Sender<Command>) {
        if let Some(pid) = self.selected_pid.take() {
            if !self.rows.is_empty() {
                let _ = commands.send(Command::UnlockAll { pid });
            }
        }
        self.base = None;
        self.rows.clear();
        self.hex = None;
        self.hex_page = 0;
    }
}

/// What the caller should do with the overlay after a CE-window frame.
pub(crate) enum CeOutcome {
    Running,
    /// The user closed the memory-tool window; the overlay keeps running.
    Closed,
}

/// The CE-style window: glfw window, own egui context and renderer.
pub(crate) struct CeWindow {
    window: PWindow,
    events: GlfwReceiver<(f64, WindowEvent)>,
    backend: Option<ThreeDBackend>,
    ctx: egui::Context,
    fonts_installed: bool,
    opened: Instant,
    state: ToolState,
    commands: Sender<Command>,
    results: Receiver<Event>,
    /// Logical cursor position from the last CursorPos event.
    cursor: egui::Pos2,
    modifiers: egui::Modifiers,
    framebuffer: [u32; 2],
}

impl CeWindow {
    /// Creates the window and its renderer. The caller leaves the overlay's
    /// GL context current afterwards; [`CeWindow::frame`] guarantees the
    /// same on every return path.
    pub(crate) fn new(glfw: &mut glfw::Glfw) -> Result<Self, String> {
        glfw.window_hint(WindowHint::TransparentFramebuffer(false));
        glfw.window_hint(WindowHint::ScaleToMonitor(true));
        glfw.window_hint(WindowHint::Visible(true));
        let (mut window, events) = glfw
            .create_window(1180, 760, "KernelScript - 内存工具", WindowMode::Windowed)
            .ok_or_else(|| "create_window failed".to_string())?;
        window.set_all_polling(true);
        window.set_floating(true);
        // The primary monitor's work area is reported in physical pixels,
        // while glfw window size/position here are in content-scaled
        // units, so convert before fitting the window (a 1475x950 window
        // otherwise overruns a scaled 1080p screen and hides the log bar).
        let work_area = glfw.with_primary_monitor(|_, monitor| monitor.map(|m| m.get_workarea()));
        if let Some((area_x, area_y, area_w, area_h)) = work_area {
            let scale = window.get_content_scale().0.max(1.0);
            let to_window = |value: i32| (value as f32 / scale) as i32;
            let (area_x, area_y) = (to_window(area_x), to_window(area_y));
            let (area_w, area_h) = (to_window(area_w), to_window(area_h));
            let width = (area_w - 40).clamp(640, 1180);
            let height = (area_h - 60).clamp(420, 760);
            window.set_size(width, height);
            window.set_pos(
                area_x + (area_w - width).max(0) / 2,
                area_y + (area_h - height).max(0) / 2,
            );
        }
        // The renderer compiles its shaders for the current context, so the
        // new window must own it while the backend is built.
        window.make_current();

        let (fb_width, fb_height) = window.get_framebuffer_size();
        let framebuffer = [fb_width.max(1) as u32, fb_height.max(1) as u32];
        let mut backend = ThreeDBackend::new(
            ThreeDConfig::default(),
            |symbol| window.get_proc_address(symbol),
            framebuffer,
        );
        // The painter starts at a zero physical size and only learns it from
        // `resize_framebuffer`, which would otherwise never run because the
        // window has not changed size yet.
        backend.resize_framebuffer(framebuffer);
        {
            use egui_overlay::egui_render_three_d::glow::HasContext;
            unsafe {
                backend
                    .glow_backend
                    .glow_context
                    .clear_color(0.09, 0.09, 0.11, 1.0);
            }
        }

        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let (commands, command_rx) = channel();
        let (event_tx, results) = channel();
        std::thread::Builder::new()
            .name("ks-ce-worker".into())
            .spawn(move || worker(command_rx, event_tx))
            .map_err(|error| format!("spawn worker: {error}"))?;

        let ce = Self {
            window,
            events,
            backend: Some(backend),
            ctx,
            fonts_installed: false,
            opened: Instant::now(),
            state: ToolState::new(),
            commands,
            results,
            cursor: egui::Pos2::ZERO,
            modifiers: egui::Modifiers::default(),
            framebuffer,
        };
        let _ = ce.commands.send(Command::ListProcesses);
        Ok(ce)
    }

    /// One frame: drain worker results, feed input, draw, present. Returns
    /// what the caller should do with the overlay; the GL objects are
    /// already torn down when the window itself was closed.
    pub(crate) fn frame(
        &mut self,
        overlay: &mut GlfwBackend,
        driver: &crate::driver::DriverControl,
    ) -> CeOutcome {
        self.drain_results();
        self.pump_hex();

        let mut close_requested = false;
        let mut raw_events = Vec::new();
        for (_, event) in glfw::flush_messages(&self.events) {
            match event {
                WindowEvent::Close => close_requested = true,
                WindowEvent::CursorPos(x, y) => {
                    self.cursor = self.to_logical(x, y);
                    raw_events.push(egui::Event::PointerMoved(self.cursor));
                }
                WindowEvent::CursorEnter(entered) => {
                    if !entered {
                        raw_events.push(egui::Event::PointerGone);
                    }
                }
                WindowEvent::MouseButton(button, action, _) => {
                    if let Some(button) = egui_button(button) {
                        raw_events.push(egui::Event::PointerButton {
                            pos: self.cursor,
                            button,
                            pressed: action == Action::Press,
                            modifiers: self.modifiers,
                        });
                    }
                }
                WindowEvent::Scroll(x, y) => {
                    raw_events.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(x as f32 * 48.0, y as f32 * 48.0),
                        modifiers: self.modifiers,
                    });
                }
                WindowEvent::Key(key, _, action, modifiers) => {
                    self.modifiers = egui_modifiers(modifiers);
                    if let Some(key) = egui_key(key) {
                        raw_events.push(egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: action != Action::Release,
                            repeat: action == Action::Repeat,
                            modifiers: self.modifiers,
                        });
                    }
                }
                WindowEvent::Char(character) => {
                    raw_events.push(egui::Event::Text(character.to_string()));
                }
                WindowEvent::ContentScale(_, _) => {
                    // The per-frame scale below is authoritative; a scale
                    // change just takes effect on the next frame.
                }
                _ => {}
            }
        }

        if close_requested {
            self.shutdown();
            // Driver locks outlive this window; release them off-thread.
            if let Some(pid) = self.state.selected_pid {
                std::thread::spawn(move || {
                    let _ = sync_ipc::unlock_all(pid);
                });
            }
            overlay.window.make_current();
            return CeOutcome::Closed;
        }

        let scale = self.window.get_content_scale().0.max(1.0);
        let (fb_width, fb_height) = self.window.get_framebuffer_size();
        let framebuffer = [fb_width.max(1) as u32, fb_height.max(1) as u32];
        if framebuffer != self.framebuffer {
            self.framebuffer = framebuffer;
            if let Some(backend) = self.backend.as_mut() {
                backend.resize_framebuffer(framebuffer);
            }
        }
        let logical = [framebuffer[0] as f32 / scale, framebuffer[1] as f32 / scale];

        self.window.make_current();
        if !self.fonts_installed {
            let _ = crate::app::install_chinese_font(&self.ctx);
            self.fonts_installed = true;
        }

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(logical[0], logical[1]),
            )),
            time: Some(self.opened.elapsed().as_secs_f64()),
            modifiers: self.modifiers,
            events: raw_events,
            focused: true,
            viewports: std::iter::once((
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    native_pixels_per_point: Some(scale),
                    ..Default::default()
                },
            ))
            .collect(),
            ..Default::default()
        };

        let ctx = self.ctx.clone();
        let commands = self.commands.clone();
        let state = &mut self.state;
        let full_output = ctx.run(raw_input, |ctx| {
            draw_ui(ctx, state, &commands, driver);
        });
        let primitives = ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
        let framebuffer = self.framebuffer;
        if let Some(backend) = self.backend.as_mut() {
            // three_d's context resets the GL viewport every frame, so
            // re-assert the full window here (otherwise a resized window
            // keeps drawing the UI in the old-size viewport).
            {
                use egui_overlay::egui_render_three_d::glow::HasContext;
                unsafe {
                    backend.glow_backend.glow_context.viewport(
                        0,
                        0,
                        framebuffer[0] as i32,
                        framebuffer[1] as i32,
                    );
                }
            }
            backend.prepare_frame(|| [framebuffer[0], framebuffer[1]]);
            backend.render_egui(primitives, full_output.textures_delta, logical);
        }
        self.window.swap_buffers();
        // The overlay renders right after `gui_run`; hand the GL context
        // back so its painter sees the context it was created for.
        overlay.window.make_current();
        CeOutcome::Running
    }

    /// Queues the current hex page read when none is in flight, so the view
    /// re-reads it every frame (continuous change monitoring).
    fn pump_hex(&mut self) {
        if self.state.hex_pending {
            return;
        }
        let (Some(pid), Some(start)) = (self.state.selected_pid, self.state.hex_start()) else {
            return;
        };
        if self
            .commands
            .send(Command::ReadHex {
                pid,
                address: start,
                size: self.state.hex_len,
            })
            .is_ok()
        {
            self.state.hex_pending = true;
        }
    }

    fn drain_results(&mut self) {
        loop {
            match self.results.try_recv() {
                Ok(Event::Processes(Ok(processes))) => {
                    self.state.processes = processes;
                    self.state.status = format!("进程列表: {} 项", self.state.processes.len());
                }
                Ok(Event::Processes(Err(error))) => self.state.status = error,
                Ok(Event::Base(Ok(base))) => {
                    self.state.base = Some(base);
                    self.state.hex_page = 0;
                    self.state.hex = None;
                }
                Ok(Event::Base(Err(error))) => {
                    self.state.base = None;
                    self.state.hex = None;
                    self.state.status = format!("模块基址读取失败: {error}");
                }
                Ok(Event::Row { row, value, result }) => {
                    let error = result.as_ref().err().cloned();
                    if let Some(entry) = self.state.rows.iter_mut().find(|entry| entry.id == row) {
                        if let Some(value) = value {
                            entry.value = value;
                        }
                        entry.status = match &result {
                            Ok(message) => message.clone(),
                            Err(error) => error.clone(),
                        };
                    }
                    if let Some(error) = error {
                        self.state.status = error;
                    }
                }
                Ok(Event::Hex {
                    address,
                    size,
                    result,
                }) => {
                    self.state.hex_pending = false;
                    // A response for a page or length the user already left
                    // behind is dropped; the next pump reads the new page.
                    if self.state.hex_start() == Some(address) && size == self.state.hex_len {
                        self.state.hex = Some(match result {
                            Ok(data) => {
                                let changed = match &self.state.hex {
                                    Some(previous)
                                        if previous.address == address
                                            && previous.data.len() == data.len() =>
                                    {
                                        previous
                                            .data
                                            .iter()
                                            .zip(&data)
                                            .map(|(old, new)| old != new)
                                            .collect()
                                    }
                                    _ => vec![false; data.len()],
                                };
                                HexData {
                                    address,
                                    data,
                                    changed,
                                    error: None,
                                }
                            }
                            Err(error) => HexData {
                                address,
                                data: Vec::new(),
                                changed: Vec::new(),
                                error: Some(error),
                            },
                        });
                    }
                }
                Ok(Event::Message(message)) => self.state.status = message,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn shutdown(&mut self) {
        self.window.make_current();
        self.backend = None;
    }

    fn to_logical(&self, x: f64, y: f64) -> egui::Pos2 {
        let scale = self.window.get_content_scale().0.max(1.0);
        let (virtual_width, _) = self.window.get_size();
        let (fb_width, _) = self.window.get_framebuffer_size();
        let pixels_per_virtual_unit = if virtual_width > 0 {
            fb_width as f32 / virtual_width as f32
        } else {
            1.0
        };
        egui::pos2(
            x as f32 * pixels_per_virtual_unit / scale,
            y as f32 * pixels_per_virtual_unit / scale,
        )
    }
}

impl Drop for CeWindow {
    fn drop(&mut self) {
        // Fields drop in declaration order, so without this the glfw window
        // (and its GL context) would be destroyed before the renderer drops
        // its GL objects. `shutdown` frees the backend while the context is
        // still alive and current.
        self.shutdown();
    }
}

fn egui_button(button: MouseButton) -> Option<egui::PointerButton> {
    match button {
        MouseButton::Button1 => Some(egui::PointerButton::Primary),
        MouseButton::Button2 => Some(egui::PointerButton::Secondary),
        MouseButton::Button3 => Some(egui::PointerButton::Middle),
        _ => None,
    }
}

fn egui_modifiers(modifiers: GlfwModifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: modifiers.contains(GlfwModifiers::Alt),
        ctrl: modifiers.contains(GlfwModifiers::Control),
        shift: modifiers.contains(GlfwModifiers::Shift),
        mac_cmd: false,
        command: modifiers.contains(GlfwModifiers::Control),
    }
}

fn egui_key(key: Key) -> Option<egui::Key> {
    Some(match key {
        Key::Space => egui::Key::Space,
        Key::Apostrophe => egui::Key::Quote,
        Key::Comma => egui::Key::Comma,
        Key::Minus => egui::Key::Minus,
        Key::Period => egui::Key::Period,
        Key::Slash => egui::Key::Slash,
        Key::Semicolon => egui::Key::Semicolon,
        Key::Equal => egui::Key::Equals,
        Key::LeftBracket => egui::Key::OpenBracket,
        Key::Backslash => egui::Key::Backslash,
        Key::RightBracket => egui::Key::CloseBracket,
        Key::GraveAccent => egui::Key::Backtick,
        Key::Escape => egui::Key::Escape,
        Key::Enter => egui::Key::Enter,
        Key::Tab => egui::Key::Tab,
        Key::Backspace => egui::Key::Backspace,
        Key::Insert => egui::Key::Insert,
        Key::Delete => egui::Key::Delete,
        Key::Right => egui::Key::ArrowRight,
        Key::Left => egui::Key::ArrowLeft,
        Key::Down => egui::Key::ArrowDown,
        Key::Up => egui::Key::ArrowUp,
        Key::PageUp => egui::Key::PageUp,
        Key::PageDown => egui::Key::PageDown,
        Key::Home => egui::Key::Home,
        Key::End => egui::Key::End,
        Key::Num0 => egui::Key::Num0,
        Key::Num1 => egui::Key::Num1,
        Key::Num2 => egui::Key::Num2,
        Key::Num3 => egui::Key::Num3,
        Key::Num4 => egui::Key::Num4,
        Key::Num5 => egui::Key::Num5,
        Key::Num6 => egui::Key::Num6,
        Key::Num7 => egui::Key::Num7,
        Key::Num8 => egui::Key::Num8,
        Key::Num9 => egui::Key::Num9,
        Key::A => egui::Key::A,
        Key::B => egui::Key::B,
        Key::C => egui::Key::C,
        Key::D => egui::Key::D,
        Key::E => egui::Key::E,
        Key::F => egui::Key::F,
        Key::G => egui::Key::G,
        Key::H => egui::Key::H,
        Key::I => egui::Key::I,
        Key::J => egui::Key::J,
        Key::K => egui::Key::K,
        Key::L => egui::Key::L,
        Key::M => egui::Key::M,
        Key::N => egui::Key::N,
        Key::O => egui::Key::O,
        Key::P => egui::Key::P,
        Key::Q => egui::Key::Q,
        Key::R => egui::Key::R,
        Key::S => egui::Key::S,
        Key::T => egui::Key::T,
        Key::U => egui::Key::U,
        Key::V => egui::Key::V,
        Key::W => egui::Key::W,
        Key::X => egui::Key::X,
        Key::Y => egui::Key::Y,
        Key::Z => egui::Key::Z,
        _ => return None,
    })
}

fn parse_address(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (text, 10),
    };
    let value = u64::from_str_radix(digits, radix).map_err(|_| format!("bad address {text:?}"))?;
    if value == 0 {
        return Err("address must not be 0".into());
    }
    Ok(value)
}

fn draw_ui(
    ctx: &egui::Context,
    state: &mut ToolState,
    commands: &Sender<Command>,
    driver: &crate::driver::DriverControl,
) {
    // Full-width log bar: driver phase + lifecycle/startup log. Added before
    // the side panels so it spans the whole window.
    egui::TopBottomPanel::bottom("ce_logs")
        .resizable(true)
        .default_height(150.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("日志").strong());
                ui.separator();
                ui.label(egui::RichText::new(&state.status).monospace());
            });
            let logs = driver.logs_snapshot();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for line in &logs {
                        ui.monospace(line);
                    }
                });
        });

    egui::SidePanel::left("ce_processes")
        .default_width(200.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("进程");
                ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .desired_width(90.0)
                        .hint_text("过滤"),
                );
                if ui.small_button("刷新").clicked() {
                    let _ = commands.send(Command::ListProcesses);
                }
                if ui
                    .add_enabled(state.selected_pid.is_some(), egui::Button::new("关闭"))
                    .clicked()
                {
                    state.forget_process(commands);
                }
            });
            ui.separator();
            let filter = state.filter.to_lowercase();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let processes = state.processes.clone();
                    for (pid, name) in processes {
                        if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                            continue;
                        }
                        let selected = state.selected_pid == Some(pid as u64);
                        if ui
                            .selectable_label(selected, format!("{name}  ({pid})"))
                            .clicked()
                            && !selected
                        {
                            state.select_process(pid as u64, commands);
                        }
                    }
                });
        });

    // Ctrl+1/2/3 switch the centre tabs without the mouse.
    let (to_hex, to_editor, to_modify) = ctx.input(|input| {
        let command = input.modifiers.command;
        (
            command && input.key_pressed(egui::Key::Num1),
            command && input.key_pressed(egui::Key::Num2),
            command && input.key_pressed(egui::Key::Num3),
        )
    });
    if to_hex {
        state.tab = Tab::Hex;
    }
    if to_editor {
        state.tab = Tab::Editor;
    }
    if to_modify {
        state.tab = Tab::Modify;
    }

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut state.tab, Tab::Hex, "内存查看 (Ctrl+1)");
            ui.selectable_value(&mut state.tab, Tab::Editor, "Lua 脚本 (Ctrl+2)");
            ui.selectable_value(&mut state.tab, Tab::Modify, "修改 (Ctrl+3)");
        });
        ui.separator();
        match state.tab {
            Tab::Hex => draw_hex(ui, state, commands),
            Tab::Editor => draw_editor(ui, state),
            Tab::Modify => draw_modify(ui, state, commands),
        }
    });
}

/// Hex viewer tab.
fn draw_hex(ui: &mut egui::Ui, state: &mut ToolState, commands: &Sender<Command>) {
    ui.horizontal_wrapped(|ui| {
        ui.label("起始");
        match (state.base, state.hex_start()) {
            (Some(base), Some(start)) => {
                ui.monospace(format!("0x{base:X}  →  0x{start:X}"));
            }
            _ => {
                ui.monospace("进程基址（未选择进程）");
            }
        }
        ui.separator();
        ui.label("长度");
        let previous_len = state.hex_len;
        egui::ComboBox::from_id_salt("ce_hex_len")
            .selected_text(format!("{} 字节", state.hex_len))
            .show_ui(ui, |ui| {
                for len in HEX_LENGTHS {
                    ui.selectable_value(&mut state.hex_len, len, format!("{len} 字节"));
                }
            });
        if state.hex_len != previous_len {
            state.hex = None;
        }
        ui.separator();
        if ui
            .add_enabled(state.hex_page > 0, egui::Button::new("◀ 上一页"))
            .clicked()
        {
            state.hex_page -= 1;
            state.hex = None;
        }
        ui.label(format!("第 {} 页", state.hex_page + 1));
        if ui.button("下一页 ▶").clicked() {
            state.hex_page += 1;
            state.hex = None;
        }
        if ui
            .add_enabled(state.hex_page > 0, egui::Button::new("回到基址"))
            .clicked()
        {
            state.hex_page = 0;
            state.hex = None;
        }
    });
    ui.separator();

    // Keep the column header visible while a process/base/page is loading.
    // This is the fixed ImHex-style layout: address, two byte groups, ASCII.
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        // Keep horizontal origin identical to the data rows below. Only
        // vertical padding separates the header from the page content.
        .inner_margin(egui::Margin::symmetric(0.0, 4.0))
        .show(ui, |ui| {
            draw_hex_header(ui);
        });
    ui.separator();

    if state.selected_pid.is_none() {
        ui.label("从左侧选择一个进程，十六进制视图会以它的模块基址为起点。");
        return;
    }
    let Some(page) = &state.hex else {
        ui.label("读取中...");
        return;
    };
    if let Some(error) = &page.error {
        ui.colored_label(egui::Color32::LIGHT_RED, format!("读取失败: {error}"));
        return;
    }
    // One monospace label per row keeps every column aligned regardless of
    // the panel width; changed bytes get their own colored span.
    let muted = ui.visuals().weak_text_color();
    let normal = ui.visuals().text_color();
    let changed_color = egui::Color32::from_rgb(255, 96, 96);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (row_index, chunk) in page.data.chunks(16).enumerate() {
                let address = page.address + (row_index * 16) as u64;
                let mut job = egui::text::LayoutJob::default();
                let base = egui::TextFormat {
                    font_id: egui::FontId::monospace(12.0),
                    color: normal,
                    ..Default::default()
                };
                job.append(
                    &format!("{address:016X}  "),
                    0.0,
                    egui::TextFormat {
                        color: muted,
                        ..base.clone()
                    },
                );
                for column in 0..16 {
                    let Some(byte) = chunk.get(column) else {
                        job.append("   ", 0.0, base.clone());
                        continue;
                    };
                    let changed = page
                        .changed
                        .get(row_index * 16 + column)
                        .copied()
                        .unwrap_or(false);
                    let format = egui::TextFormat {
                        color: if changed { changed_color } else { normal },
                        ..base.clone()
                    };
                    job.append(&format!("{byte:02X} "), 0.0, format);
                    if column == 7 {
                        job.append(" ", 0.0, base.clone());
                    }
                }
                let ascii: String = chunk
                    .iter()
                    .map(|byte| {
                        if byte.is_ascii_graphic() {
                            *byte as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                job.append(
                    &format!(" {ascii}"),
                    0.0,
                    egui::TextFormat {
                        color: muted,
                        ..base
                    },
                );
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            }
        });

    let _ = commands;
}

/// Draw the ImHex-style column header with the exact same character widths
/// as the data rows below it. Building this as a `LayoutJob` avoids relying
/// on proportional spaces in a font fallback and keeps every byte column
/// aligned with its `00..0F` label.
fn draw_hex_header(ui: &mut egui::Ui) {
    let format = egui::TextFormat {
        font_id: egui::FontId::monospace(12.0),
        color: ui.visuals().strong_text_color(),
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(&format!("{:<16}  ", "Address"), 0.0, format.clone());
    for column in 0..16 {
        job.append(&format!("{column:02X} "), 0.0, format.clone());
        if column == 7 {
            job.append(" ", 0.0, format.clone());
        }
    }
    job.append(" ASCII", 0.0, format);
    ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
}

/// Lua script editor tab: pick a script from the overlay's `scripts`
/// directory, edit it, save it (Ctrl+S). Saving triggers the overlay's
/// file-watch hot reload.
fn draw_editor(ui: &mut egui::Ui, state: &mut ToolState) {
    if !state.editor.scanned {
        state.editor.scan();
    }
    let save_requested =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::S));

    ui.horizontal(|ui| {
        ui.label("脚本目录");
        ui.monospace(state.editor.dir.display().to_string());
        if ui.button("刷新").clicked() {
            state.editor.scan();
        }
        if ui.button("重新加载").clicked() {
            if let Some(path) = state.editor.selected.clone() {
                state.editor.open(path);
                state.editor.status = "已从磁盘重新加载".to_owned();
            }
        }
        ui.add(
            egui::TextEdit::singleline(&mut state.editor.new_name)
                .desired_width(120.0)
                .hint_text("新文件名"),
        );
        if ui.button("新建").clicked() {
            state.editor.create();
        }
        if ui
            .add_enabled(state.editor.dirty(), egui::Button::new("保存 (Ctrl+S)"))
            .clicked()
            || (save_requested && state.editor.dirty())
        {
            state.editor.save();
        }
        if state.editor.dirty() {
            ui.colored_label(egui::Color32::YELLOW, "● 未保存");
        }
    });
    if !state.editor.status.is_empty() {
        ui.label(egui::RichText::new(&state.editor.status).small().weak());
    }
    ui.separator();

    ui.horizontal_top(|ui| {
        // File list.
        egui::ScrollArea::vertical()
            .id_salt("ce_script_list")
            .max_width(200.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let files = state.editor.files.clone();
                for path in files {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let selected = state.editor.selected.as_deref() == Some(path.as_path());
                    if ui.selectable_label(selected, name).clicked() && !selected {
                        state.editor.open(path);
                    }
                }
            });
        ui.separator();
        // Editor.
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut layouter = lua_layouter;
                ui.add(
                    egui::TextEdit::multiline(&mut state.editor.buffer)
                        .code_editor()
                        .layouter(&mut layouter)
                        .desired_width(f32::INFINITY)
                        .desired_rows(24),
                );
            });
    });
}

fn lua_layouter(ui: &egui::Ui, text: &str, wrap_width: f32) -> Arc<egui::Galley> {
    let normal = ui.visuals().text_color();
    let comment = egui::Color32::from_rgb(110, 150, 110);
    let string = egui::Color32::from_rgb(220, 170, 100);
    let number = egui::Color32::from_rgb(100, 190, 220);
    let keyword = egui::Color32::from_rgb(210, 130, 230);
    let builtin = egui::Color32::from_rgb(100, 170, 245);
    let format = |color| egui::TextFormat {
        font_id: egui::FontId::monospace(13.0),
        color,
        ..Default::default()
    };
    let keywords = [
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];
    let builtins = [
        "memory", "ui", "draw", "config", "keyboard", "print", "math", "string", "table",
    ];
    let mut job = egui::text::LayoutJob::default();
    let mut pos = 0;
    while pos < text.len() {
        let rest = &text[pos..];
        if rest.starts_with("--") {
            let end = rest.find('\n').map_or(text.len(), |offset| pos + offset);
            job.append(&text[pos..end], 0.0, format(comment));
            pos = end;
            continue;
        }
        let ch = rest.chars().next().unwrap();
        if ch == '\'' || ch == '"' {
            let quote = ch;
            let mut end = pos + ch.len_utf8();
            let mut escaped = false;
            for (offset, current) in text[end..].char_indices() {
                end = end.max(pos + ch.len_utf8() + offset + current.len_utf8());
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == quote {
                    break;
                }
            }
            job.append(&text[pos..end.min(text.len())], 0.0, format(string));
            pos = end.min(text.len());
            continue;
        }
        if ch.is_ascii_digit() {
            let end = rest
                .char_indices()
                .take_while(|(_, current)| {
                    current.is_ascii_alphanumeric() || matches!(current, '.' | '_')
                })
                .last()
                .map_or(pos + ch.len_utf8(), |(offset, current)| {
                    pos + offset + current.len_utf8()
                });
            job.append(&text[pos..end], 0.0, format(number));
            pos = end;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let end = rest
                .char_indices()
                .take_while(|(_, current)| current.is_ascii_alphanumeric() || *current == '_')
                .last()
                .map_or(pos + ch.len_utf8(), |(offset, current)| {
                    pos + offset + current.len_utf8()
                });
            let word = &text[pos..end];
            let color = if keywords.contains(&word) {
                keyword
            } else if builtins.contains(&word) {
                builtin
            } else {
                normal
            };
            job.append(word, 0.0, format(color));
            pos = end;
            continue;
        }
        let end = pos + ch.len_utf8();
        job.append(&text[pos..end], 0.0, format(normal));
        pos = end;
    }
    job.wrap.max_width = wrap_width;
    ui.fonts(|fonts| fonts.layout_job(job))
}

/// Value editor tab: manual address list with read/write/freeze.
fn draw_modify(ui: &mut egui::Ui, state: &mut ToolState, commands: &Sender<Command>) {
    let pid = state.selected_pid;
    let mut next_row_id = state.next_row_id;
    let mut next_lock_id = state.next_lock_id;
    let mut status = None;
    let mut jump_to = None;

    ui.horizontal_wrapped(|ui| {
        ui.label("地址");
        ui.add(
            egui::TextEdit::singleline(&mut state.new_address)
                .desired_width(130.0)
                .hint_text("0x7FF6..."),
        );
        egui::ComboBox::from_id_salt("ce_new_kind")
            .selected_text(state.new_kind.label())
            .show_ui(ui, |ui| {
                for kind in ValueType::ALL {
                    ui.selectable_value(&mut state.new_kind, kind, kind.label());
                }
            });
        if ui.button("添加").clicked() {
            match parse_address(&state.new_address) {
                Ok(address) => {
                    let row = next_row_id;
                    next_row_id += 1;
                    state.rows.push(AddrRow {
                        id: row,
                        address,
                        kind: state.new_kind,
                        value: String::new(),
                        frozen: false,
                        lock_id: 0,
                        status: String::new(),
                    });
                    state.new_address.clear();
                    if let Some(pid) = pid {
                        let _ = commands.send(Command::Read {
                            row,
                            pid,
                            address,
                            kind: state.new_kind,
                        });
                    }
                }
                Err(error) => status = Some(error),
            }
        }
        if ui.button("刷新数值").clicked() {
            if let Some(pid) = pid {
                for row in &state.rows {
                    let _ = commands.send(Command::Read {
                        row: row.id,
                        pid,
                        address: row.address,
                        kind: row.kind,
                    });
                }
            }
        }
        if ui.button("写入修改").clicked() {
            if let Some(pid) = pid {
                for row in &state.rows {
                    match row.kind.parse(&row.value) {
                        Ok(data) => {
                            let _ = commands.send(Command::Write {
                                row: row.id,
                                pid,
                                address: row.address,
                                data,
                            });
                        }
                        Err(error) => {
                            status = Some(format!("0x{:X}: {error}", row.address));
                        }
                    }
                }
            }
        }
        if ui.button("解除全部冻结").clicked() {
            for row in &mut state.rows {
                if row.frozen {
                    row.frozen = false;
                    let lock_id = std::mem::take(&mut row.lock_id);
                    if lock_id != 0 {
                        let _ = commands.send(Command::Unfreeze {
                            row: row.id,
                            lock_id,
                        });
                    }
                }
            }
        }
    });
    ui.separator();

    let mut remove_row = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("ce_rows")
                .striped(true)
                .num_columns(5)
                .show(ui, |ui| {
                    ui.label("冻结");
                    ui.label("地址");
                    ui.label("类型");
                    ui.label("数值");
                    ui.label("");
                    ui.end_row();

                    for row in &mut state.rows {
                        let mut frozen = row.frozen;
                        if ui.checkbox(&mut frozen, "").changed() {
                            if frozen {
                                match (pid, row.kind.parse(&row.value)) {
                                    (Some(pid), Ok(data)) => {
                                        let lock_id = next_lock_id;
                                        next_lock_id += 1;
                                        row.lock_id = lock_id;
                                        row.frozen = true;
                                        row.status = "冻结中".into();
                                        let _ = commands.send(Command::Freeze {
                                            row: row.id,
                                            pid,
                                            lock_id,
                                            address: row.address,
                                            data,
                                        });
                                    }
                                    (Some(_), Err(error)) => row.status = error,
                                    (None, _) => row.status = "未选择进程".into(),
                                }
                            } else {
                                row.frozen = false;
                                let lock_id = std::mem::take(&mut row.lock_id);
                                if lock_id != 0 {
                                    let _ = commands.send(Command::Unfreeze {
                                        row: row.id,
                                        lock_id,
                                    });
                                }
                            }
                        }

                        if ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(format!("0x{:X}", row.address)).monospace(),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .clicked()
                        {
                            jump_to = Some(row.address);
                        }

                        let previous_kind = row.kind;
                        egui::ComboBox::from_id_salt(("ce_row_kind", row.id))
                            .selected_text(row.kind.label())
                            .show_ui(ui, |ui| {
                                for kind in ValueType::ALL {
                                    ui.selectable_value(&mut row.kind, kind, kind.label());
                                }
                            });
                        if row.kind != previous_kind && row.frozen {
                            row.frozen = false;
                            let lock_id = std::mem::take(&mut row.lock_id);
                            if lock_id != 0 {
                                let _ = commands.send(Command::Unfreeze {
                                    row: row.id,
                                    lock_id,
                                });
                            }
                        }

                        let edited = ui
                            .add(egui::TextEdit::singleline(&mut row.value).desired_width(120.0))
                            .lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        if edited {
                            match (pid, row.kind.parse(&row.value)) {
                                (Some(pid), Ok(data)) => {
                                    let _ = commands.send(Command::Write {
                                        row: row.id,
                                        pid,
                                        address: row.address,
                                        data,
                                    });
                                }
                                (Some(_), Err(error)) => {
                                    status = Some(format!("0x{:X}: {error}", row.address));
                                }
                                (None, _) => status = Some("未选择进程".into()),
                            }
                        }

                        ui.horizontal(|ui| {
                            if !row.status.is_empty() {
                                ui.label(egui::RichText::new(&row.status).small());
                            }
                            if ui.small_button("删").clicked() {
                                remove_row = Some(row.id);
                            }
                        });
                        ui.end_row();
                    }
                });
        });

    state.next_row_id = next_row_id;
    state.next_lock_id = next_lock_id;
    if let Some(error) = status {
        state.status = error;
    }
    if let Some(row) = remove_row {
        if let Some(index) = state.rows.iter().position(|entry| entry.id == row) {
            let entry = state.rows.remove(index);
            if entry.lock_id != 0 {
                let _ = commands.send(Command::Unfreeze {
                    row: entry.id,
                    lock_id: entry.lock_id,
                });
            }
        }
    }
    if let Some(address) = jump_to {
        if let Some(base) = state.base {
            if address >= base {
                state.hex_page = (address - base) / state.hex_len as u64;
                state.hex = None;
                state.tab = Tab::Hex;
            }
        }
    }
}
