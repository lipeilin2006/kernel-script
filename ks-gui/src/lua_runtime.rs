use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use egui;
use mlua::debug::HookTriggers;
use mlua::{FromLua, Function, IntoLuaMulti, Lua, Table, Value, VmState};

use crate::ipc_client::IpcClient;

static CONTENT_SCALE: AtomicU32 = AtomicU32::new(100);

pub fn set_content_scale(scale: f32) {
    CONTENT_SCALE.store((scale * 100.0) as u32, Ordering::Relaxed);
}

#[derive(Clone, Debug)]
pub enum DrawCommand {
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        color: [u8; 4],
        thickness: f32,
    },
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [u8; 4],
        thickness: f32,
    },
    FilledRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [u8; 4],
    },
    Circle {
        x: f32,
        y: f32,
        radius: f32,
        color: [u8; 4],
        thickness: f32,
    },
    FilledCircle {
        x: f32,
        y: f32,
        radius: f32,
        color: [u8; 4],
    },
    Text {
        x: f32,
        y: f32,
        text: String,
        color: [u8; 4],
        size: f32,
    },
}

pub type DrawCommands = Arc<Mutex<Vec<DrawCommand>>>;

#[derive(Clone, Debug)]
enum SharedValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(String),
}

type SharedGlobals = Arc<Mutex<HashMap<String, SharedValue>>>;

fn shared_value_to_lua(lua: &Lua, value: SharedValue) -> mlua::Result<Value> {
    Ok(match value {
        SharedValue::Nil => Value::Nil,
        SharedValue::Boolean(value) => Value::Boolean(value),
        SharedValue::Integer(value) => Value::Integer(value),
        SharedValue::Number(value) => Value::Number(value),
        SharedValue::String(value) => Value::String(lua.create_string(value)?),
    })
}

fn lua_to_shared_value(value: Value) -> mlua::Result<SharedValue> {
    match value {
        Value::Nil => Ok(SharedValue::Nil),
        Value::Boolean(value) => Ok(SharedValue::Boolean(value)),
        Value::Integer(value) => Ok(SharedValue::Integer(value)),
        Value::Number(value) if value.is_finite() => Ok(SharedValue::Number(value)),
        Value::String(value) => Ok(SharedValue::String(value.to_str()?.to_owned())),
        _ => Err(mlua::Error::runtime(
            "shared values must be nil, boolean, integer, number, or string",
        )),
    }
}

#[derive(Clone, Copy, Debug)]
struct Address(u64);

impl Address {
    fn new(value: u64) -> Self {
        Self(value)
    }

    fn get(self) -> u64 {
        self.0
    }
}

impl FromLua for Address {
    fn from_lua(value: Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            Value::Integer(value) if value > 0 => Ok(Address::new(value as u64)),
            Value::Integer(_) => Err(mlua::Error::runtime("address must not be negative")),
            Value::String(value) => parse_address(value.to_str()?.as_ref()),
            value => Err(mlua::Error::runtime(format!(
                "address must be a positive integer or string, got {}",
                value.type_name()
            ))),
        }
    }
}

// Lua OnUpdate targets 60 updates per second (16.667 ms per step).
const UPDATE_STEP: Duration = Duration::from_nanos(16_666_667);
const MAX_UPDATE_STEPS_PER_FRAME: usize = 4;
const MAX_FRAME_DELTA: Duration = Duration::from_millis(250);
const RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LUA_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const HOOK_INSTRUCTION_INTERVAL: u32 = 10_000;
const START_BUDGET: Duration = Duration::from_millis(100);
const UPDATE_BUDGET: Duration = Duration::from_millis(10);
const RENDER_BUDGET: Duration = Duration::from_millis(12);
const DESTROY_BUDGET: Duration = Duration::from_millis(50);

#[derive(Clone)]
struct AsyncScheduler {
    results: Arc<Mutex<mpsc::Receiver<(u64, AsyncResult)>>>,
    result_tx: mpsc::Sender<(u64, AsyncResult)>,
    next_id: Arc<AtomicU64>,
    completed: Arc<Mutex<HashMap<u64, AsyncResult>>>,
    runtime: Arc<tokio::runtime::Runtime>,
    permits: Arc<tokio::sync::Semaphore>,
}

enum AsyncRequest {
    ReadI32 {
        pid: u64,
        address: u64,
    },
    ReadBytes {
        pid: u64,
        address: u64,
        size: u64,
    },
    WriteI32 {
        pid: u64,
        address: u64,
        value: i32,
    },
    WriteBytes {
        pid: u64,
        address: u64,
        data: Vec<u8>,
    },
    GetPid {
        name: String,
    },
    GetProcessBase {
        pid: u64,
    },
    ReadRva {
        pid: u64,
        relative_address: u64,
        size: u64,
    },
    WriteRva {
        pid: u64,
        relative_address: u64,
        data: Vec<u8>,
    },
    ReadMdl {
        pid: u64,
        address: u64,
        size: u64,
    },
    WriteMdl {
        pid: u64,
        address: u64,
        data: Vec<u8>,
    },
    ReadMdlRva {
        pid: u64,
        relative_address: u64,
        size: u64,
    },
    WriteMdlRva {
        pid: u64,
        relative_address: u64,
        data: Vec<u8>,
    },
    ListProcesses,
}

enum AsyncValue {
    I32(i32),
    Bytes(Vec<u8>),
    Pid(u64),
    Processes(Vec<crate::ipc_client::ProcessInfo>),
    Unit,
}

type AsyncResult = Result<AsyncValue, String>;

impl AsyncScheduler {
    fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel::<(u64, AsyncResult)>();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(4)
                .enable_io()
                .enable_time()
                .build()
                .expect("failed to create GUI async runtime"),
        );
        Self {
            results: Arc::new(Mutex::new(result_rx)),
            result_tx,
            next_id: Arc::new(AtomicU64::new(1)),
            completed: Arc::new(Mutex::new(HashMap::new())),
            runtime,
            permits: Arc::new(tokio::sync::Semaphore::new(256)),
        }
    }

    async fn execute(request: AsyncRequest) -> AsyncResult {
        let client = IpcClient::new();
        match request {
            AsyncRequest::ReadI32 { pid, address } => {
                client.read_memory(pid, address, 4).await.and_then(|data| {
                    let bytes: [u8; 4] = data
                        .get(..4)
                        .ok_or_else(|| "read returned fewer than 4 bytes".to_owned())?
                        .try_into()
                        .map_err(|_| "invalid i32 response".to_owned())?;
                    Ok(AsyncValue::I32(i32::from_ne_bytes(bytes)))
                })
            }
            AsyncRequest::ReadBytes { pid, address, size } => client
                .read_memory(pid, address, size)
                .await
                .map(AsyncValue::Bytes),
            AsyncRequest::WriteI32 {
                pid,
                address,
                value,
            } => client
                .write_memory(pid, address, &value.to_ne_bytes())
                .await
                .map(|()| AsyncValue::Unit),
            AsyncRequest::WriteBytes { pid, address, data } => client
                .write_memory(pid, address, &data)
                .await
                .map(|()| AsyncValue::Unit),
            AsyncRequest::GetPid { name } => {
                client.get_pid(&name).await.map(AsyncValue::Pid)
            }
            AsyncRequest::GetProcessBase { pid } => {
                client.get_process_base(pid).await.map(AsyncValue::Pid)
            }
            AsyncRequest::ReadRva {
                pid,
                relative_address,
                size,
            } => client
                .read_memory_rva(pid, relative_address, size)
                .await
                .map(AsyncValue::Bytes),
            AsyncRequest::WriteRva {
                pid,
                relative_address,
                data,
            } => client
                .write_memory_rva(pid, relative_address, &data)
                .await
                .map(|()| AsyncValue::Unit),
            AsyncRequest::ReadMdl { pid, address, size } => client
                .read_memory_mdl(pid, address, size)
                .await
                .map(AsyncValue::Bytes),
            AsyncRequest::WriteMdl { pid, address, data } => client
                .write_memory_mdl(pid, address, &data)
                .await
                .map(|()| AsyncValue::Unit),
            AsyncRequest::ReadMdlRva {
                pid,
                relative_address,
                size,
            } => client
                .read_memory_mdl_rva(pid, relative_address, size)
                .await
                .map(AsyncValue::Bytes),
            AsyncRequest::WriteMdlRva {
                pid,
                relative_address,
                data,
            } => client
                .write_memory_mdl_rva(pid, relative_address, &data)
                .await
                .map(|()| AsyncValue::Unit),
            AsyncRequest::ListProcesses => client.list_processes().await.map(AsyncValue::Processes),
        }
    }

    fn submit(&self, request: AsyncRequest) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let result_tx = self.result_tx.clone();
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| "IPC task queue is full".to_owned())?;
        let task_id = id;
        tracing::info!(task_id, "submitting IPC task");
        self.runtime.spawn(async move {
            tracing::info!(task_id, "IPC task started");
            let result = Self::execute(request).await;
            tracing::info!(task_id, "IPC task completed");
            let _ = result_tx.send((task_id, result));
            drop(permit);
        });
        Ok(id)
    }

    fn poll(&self) -> Option<(u64, AsyncResult)> {
        self.results.lock().ok()?.try_recv().ok()
    }

    fn poll_id(&self, id: u64) -> Option<AsyncResult> {
        if let Ok(mut completed) = self.completed.lock() {
            if let Some(result) = completed.remove(&id) {
                return Some(result);
            }
        }
        while let Some((completed_id, result)) = self.poll() {
            if completed_id == id {
                return Some(result);
            }
            if let Ok(mut completed) = self.completed.lock() {
                completed.insert(completed_id, result);
            }
        }
        None
    }
}

pub struct LuaRuntime {
    lua: Lua,
    script_path: PathBuf,
    control: Arc<RuntimeControl>,
    deadline: Arc<Mutex<Option<Instant>>>,
    scheduler: FixedUpdateScheduler,
    last_error: Option<String>,
    last_tick: Instant,
    last_reload_poll: Instant,
    script_modified: Option<SystemTime>,
    update_stop: Arc<AtomicBool>,
    update_thread: Option<JoinHandle<()>>,
    async_scheduler: AsyncScheduler,
    shared_globals: SharedGlobals,
    draw_commands: DrawCommands,
}

pub struct LuaRuntimeManager {
    runtimes: Vec<LuaRuntime>,
    draw_commands: DrawCommands,
}

impl LuaRuntimeManager {
    pub fn new(script_directory: PathBuf) -> mlua::Result<Self> {
        let shared_globals = Arc::new(Mutex::new(HashMap::new()));
        let draw_commands: DrawCommands = Arc::new(Mutex::new(Vec::new()));
        let mut paths = fs::read_dir(&script_directory)
            .map_err(mlua::Error::external)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "lua"))
            .collect::<Vec<_>>();
        paths.sort();
        if paths.is_empty() {
            return Err(mlua::Error::external(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no Lua scripts found",
            )));
        }
        let runtimes = paths
            .into_iter()
            .map(|path| {
                LuaRuntime::new(
                    path,
                    Arc::clone(&shared_globals),
                    Arc::clone(&draw_commands),
                )
            })
            .collect::<mlua::Result<Vec<_>>>()?;
        Ok(Self { runtimes, draw_commands })
    }

    pub fn frame(&mut self, ctx: &egui::Context, now: Instant) {
        self.draw_commands.lock().unwrap().clear();
        for runtime in &mut self.runtimes {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.frame(ctx, now);
            }));
            if result.is_err() {
                runtime.set_error("Lua runtime panicked; script disabled until reload".to_owned());
            }
        }
    }

    pub fn set_error(&mut self, error: String) {
        for runtime in &mut self.runtimes {
            runtime.set_error(error.clone());
        }
    }

    pub fn take_draw_commands(&self) -> Vec<DrawCommand> {
        self.draw_commands.lock().unwrap().drain(..).collect()
    }
}

impl LuaRuntime {
    fn new(
        script_path: PathBuf,
        shared_globals: SharedGlobals,
        draw_commands: DrawCommands,
    ) -> mlua::Result<Self> {
        let control = Arc::new(RuntimeControl::default());
        let async_scheduler = AsyncScheduler::new();
        let (lua, deadline) = Self::build_vm(
            &script_path,
            Arc::clone(&control),
            async_scheduler.clone(),
            Arc::clone(&shared_globals),
            Arc::clone(&draw_commands),
        )?;
        let now = Instant::now();
        let update_stop = Arc::new(AtomicBool::new(false));
        let update_stop_thread = Arc::clone(&update_stop);
        let update_control = Arc::clone(&control);
        let update_thread = thread::spawn(move || {
            while !update_stop_thread.load(Ordering::Relaxed) {
                thread::sleep(UPDATE_STEP);
                update_control.pending_steps.fetch_add(1, Ordering::Relaxed);
            }
        });
        Ok(Self {
            lua,
            script_modified: script_modified(&script_path),
            update_stop,
            update_thread: Some(update_thread),
            async_scheduler,
            shared_globals,
            draw_commands,
            script_path,
            control,
            deadline,
            scheduler: FixedUpdateScheduler::new(
                UPDATE_STEP,
                MAX_FRAME_DELTA,
                MAX_UPDATE_STEPS_PER_FRAME,
            ),
            last_error: None,
            last_tick: now,
            last_reload_poll: now,
        })
    }

    pub fn frame(&mut self, ctx: &egui::Context, now: Instant) {
        self.check_hot_reload(now);
        self.run_updates(now);
        self.render(ctx);
        if let Err(error) = self.lua.gc_step() {
            self.last_error = Some(format!("Lua GC error: {error}"));
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.last_error = Some(error);
    }

    fn build_vm(
        script_path: &Path,
        control: Arc<RuntimeControl>,
        async_scheduler: AsyncScheduler,
        shared_globals: SharedGlobals,
        draw_commands: DrawCommands,
    ) -> mlua::Result<(Lua, Arc<Mutex<Option<Instant>>>)> {
        let lua = Lua::new();
        lua.set_memory_limit(LUA_MEMORY_LIMIT)?;
        // Scripts only need the registered engine and memory APIs.
        for global in ["os", "io", "package", "debug"] {
            lua.globals().set(global, Value::Nil)?;
        }
        let deadline = install_execution_hook(&lua)?;
        register_engine_api(&lua, control)?;
        register_memory_api(&lua, async_scheduler)?;
        register_shared_api(&lua, shared_globals)?;
        register_draw_api(&lua, draw_commands)?;
        install_async_helpers(&lua)?;

        let source = fs::read_to_string(script_path).map_err(mlua::Error::external)?;
        set_deadline(&deadline, Some(Instant::now() + START_BUDGET))?;
        let load_result = lua
            .load(&source)
            .set_name(script_path.to_string_lossy().as_ref())
            .exec();
        set_deadline(&deadline, None)?;
        load_result?;
        call_optional_budgeted(&lua, &deadline, "OnStart", (), START_BUDGET)?;
        Ok((lua, deadline))
    }

    fn run_updates(&mut self, now: Instant) {
        let _elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;

        let steps = if self.control.paused.load(Ordering::Relaxed) {
            self.scheduler.reset();
            self.control.pending_steps.swap(0, Ordering::Relaxed).min(1)
        } else {
            self.control
                .pending_steps
                .swap(0, Ordering::Relaxed)
                .min(MAX_UPDATE_STEPS_PER_FRAME)
        };

        for _ in 0..steps {
            if let Err(error) = call_optional_budgeted(
                &self.lua,
                &self.deadline,
                "__pump_async_tasks",
                (),
                UPDATE_BUDGET,
            ) {
                self.last_error = Some(format!("async task failed: {error}"));
                break;
            }
            if let Err(error) = call_optional_budgeted(
                &self.lua,
                &self.deadline,
                "OnUpdate",
                UPDATE_STEP.as_secs_f32(),
                UPDATE_BUDGET,
            ) {
                self.last_error = Some(format!("OnUpdate failed: {error}"));
                self.scheduler.reset();
                break;
            }
        }
    }

    fn render(&mut self, ctx: &egui::Context) {
        let result = self.lua.scope(|scope| {
            let module = create_ui_module(&self.lua, scope, ctx)?;
            self.lua.globals().set("ui", module)?;
            call_optional_budgeted(&self.lua, &self.deadline, "OnRender", (), RENDER_BUDGET)
        });
        if let Err(error) = result {
            self.last_error = Some(format!("OnRender failed: {error}"));
        }

        if let Some(error) = self.last_error.clone() {
            let mut reload = false;
            egui::Window::new("Lua error").show(ctx, |ui| {
                ui.label(error);
                if ui.button("Reload script").clicked() {
                    reload = true;
                }
            });
            if reload {
                self.reload();
            }
        }
    }

    fn check_hot_reload(&mut self, now: Instant) {
        if now.duration_since(self.last_reload_poll) < RELOAD_POLL_INTERVAL {
            return;
        }
        self.last_reload_poll = now;
        let modified = script_modified(&self.script_path);
        if modified.is_some() && modified != self.script_modified {
            self.script_modified = modified;
            self.reload();
        }
    }

    fn reload(&mut self) {
        match Self::build_vm(
            &self.script_path,
            Arc::clone(&self.control),
            self.async_scheduler.clone(),
            Arc::clone(&self.shared_globals),
            Arc::clone(&self.draw_commands),
        ) {
            Ok((new_lua, new_deadline)) => {
                if let Err(error) = call_optional_budgeted(
                    &self.lua,
                    &self.deadline,
                    "OnDestroy",
                    (),
                    DESTROY_BUDGET,
                ) {
                    eprintln!("OnDestroy failed during reload: {error}");
                }
                self.lua = new_lua;
                self.deadline = new_deadline;
                self.last_error = None;
                self.scheduler.reset();
                self.last_tick = Instant::now();
                self.script_modified = script_modified(&self.script_path);
            }
            Err(error) => self.last_error = Some(format!("Reload failed: {error}")),
        }
    }
}

impl Drop for LuaRuntime {
    fn drop(&mut self) {
        self.update_stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.update_thread.take() {
            let _ = thread.join();
        }
        if let Err(error) =
            call_optional_budgeted(&self.lua, &self.deadline, "OnDestroy", (), DESTROY_BUDGET)
        {
            eprintln!("OnDestroy failed during shutdown: {error}");
        }
    }
}

#[derive(Default)]
struct RuntimeControl {
    paused: AtomicBool,
    pending_steps: AtomicUsize,
}

struct FixedUpdateScheduler {
    step: Duration,
    max_elapsed: Duration,
    max_steps: usize,
    accumulator: Duration,
}

impl FixedUpdateScheduler {
    fn new(step: Duration, max_elapsed: Duration, max_steps: usize) -> Self {
        Self {
            step,
            max_elapsed,
            max_steps,
            accumulator: Duration::ZERO,
        }
    }

    fn advance(&mut self, elapsed: Duration) -> usize {
        self.accumulator += elapsed.min(self.max_elapsed);
        let available = (self.accumulator.as_nanos() / self.step.as_nanos()) as usize;
        let steps = available.min(self.max_steps);
        self.accumulator = if available > self.max_steps {
            Duration::ZERO
        } else {
            self.accumulator - self.step * steps as u32
        };
        steps
    }

    fn reset(&mut self) {
        self.accumulator = Duration::ZERO;
    }
}

fn install_execution_hook(lua: &Lua) -> mlua::Result<Arc<Mutex<Option<Instant>>>> {
    let deadline = Arc::new(Mutex::new(None));
    let hook_deadline = Arc::clone(&deadline);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INSTRUCTION_INTERVAL),
        move |_, _| {
            if hook_deadline
                .lock()
                .map_err(|_| mlua::Error::runtime("execution deadline lock poisoned"))?
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err(mlua::Error::runtime("script execution budget exceeded"));
            }
            Ok(VmState::Continue)
        },
    )?;
    Ok(deadline)
}

fn call_optional_budgeted<A>(
    lua: &Lua,
    deadline: &Mutex<Option<Instant>>,
    name: &str,
    args: A,
    budget: Duration,
) -> mlua::Result<()>
where
    A: IntoLuaMulti,
{
    let callback = lua.globals().get::<Option<Function>>(name)?;
    let Some(callback) = callback else {
        return Ok(());
    };

    set_deadline(deadline, Some(Instant::now() + budget))?;
    let result = callback.call::<()>(args);
    set_deadline(deadline, None)?;
    result
}

fn set_deadline(deadline: &Mutex<Option<Instant>>, value: Option<Instant>) -> mlua::Result<()> {
    *deadline
        .lock()
        .map_err(|_| mlua::Error::runtime("execution deadline lock poisoned"))? = value;
    Ok(())
}

fn register_engine_api(lua: &Lua, control: Arc<RuntimeControl>) -> mlua::Result<()> {
    let module = lua.create_table()?;
    let paused = Arc::clone(&control);
    module.set(
        "is_paused",
        lua.create_function(move |_, ()| Ok(paused.paused.load(Ordering::Relaxed)))?,
    )?;
    let pause = Arc::clone(&control);
    module.set(
        "pause",
        lua.create_function(move |_, ()| {
            pause.paused.store(true, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    let resume = Arc::clone(&control);
    module.set(
        "resume",
        lua.create_function(move |_, ()| {
            resume.paused.store(false, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    module.set(
        "step",
        lua.create_function(move |_, ()| {
            control.paused.store(true, Ordering::Relaxed);
            control.pending_steps.store(1, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    module.set(
        "memory_used",
        lua.create_function(|lua, ()| Ok(lua.used_memory()))?,
    )?;
    lua.globals().set("engine", module)
}

fn script_modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn with_egui_ui<R>(
    bridge: &Mutex<usize>,
    draw: impl FnOnce(&mut egui::Ui) -> R,
) -> Result<R, mlua::Error> {
    let pointer = *bridge
        .lock()
        .map_err(|_| mlua::Error::runtime("egui bridge lock poisoned"))?
        as *mut egui::Ui;
    if pointer.is_null() {
        return Err(mlua::Error::runtime(
            "UI operation called outside ui.window",
        ));
    }
    // The bridge is created and consumed synchronously on the GUI thread. Lua
    // cannot yield from an egui callback, so the pointer is valid for this call.
    Ok(unsafe { draw(&mut *pointer) })
}

fn create_ui_module<'scope>(
    lua: &Lua,
    scope: &'scope mlua::Scope<'scope, '_>,
    ctx: &'scope egui::Context,
) -> mlua::Result<Table> {
    let module = lua.create_table()?;
    let bridge = Arc::new(Mutex::new(0usize));
    let bridge_for_window = Arc::clone(&bridge);
    module.set(
        "window",
        scope.create_function(move |_, (title, body): (String, Function)| {
            let callback_error = std::cell::RefCell::new(None);
            egui::Window::new(title).show(ctx, |ui| {
                *bridge_for_window.lock().unwrap() = ui as *mut egui::Ui as usize;
                if let Err(error) = body.call::<()>(()) {
                    *callback_error.borrow_mut() = Some(error);
                }
                *bridge_for_window.lock().unwrap() = 0;
            });
            match callback_error.into_inner() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })?,
    )?;
    module.set("label", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, text: String| {
            with_egui_ui(&bridge, |ui| ui.label(text))?;
            Ok(())
        })?
    })?;
    module.set("button", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, label: String| {
            with_egui_ui(&bridge, |ui| ui.button(label).clicked())
        })?
    })?;
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "separator",
            scope.create_function(move |_, ()| {
                with_egui_ui(&bridge, |ui| ui.separator())?;
                Ok(())
            })?,
        )?;
    }
    module.set("checkbox", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, (label, mut value): (String, bool)| {
            with_egui_ui(&bridge, |ui| {
                let changed = ui.checkbox(&mut value, label).changed();
                (value, changed)
            })
        })?
    })?;
    module.set("drag_value", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, (label, mut value): (String, f64)| {
            with_egui_ui(&bridge, |ui| {
                let changed = ui
                    .add(egui::DragValue::new(&mut value).prefix(format!("{}: ", label)))
                    .changed();
                (value, changed)
            })
        })?
    })?;
    macro_rules! drag_value {
        ($name:literal, $ty:ty) => {{
            let bridge = Arc::clone(&bridge);
            module.set(
                $name,
                scope.create_function(move |_, (label, mut value): (String, $ty)| {
                    with_egui_ui(&bridge, |ui| {
                        let changed = ui
                            .add(egui::DragValue::new(&mut value).prefix(format!("{}: ", label)))
                            .changed();
                        (value, changed)
                    })
                })?,
            )?;
        }};
    }
    drag_value!("drag_value_i8", i8);
    drag_value!("drag_value_u8", u8);
    drag_value!("drag_value_i16", i16);
    drag_value!("drag_value_u16", u16);
    drag_value!("drag_value_i32", i32);
    drag_value!("drag_value_u32", u32);
    drag_value!("drag_value_i64", i64);
    drag_value!("drag_value_u64", u64);
    drag_value!("drag_value_f32", f32);
    module.set("text_edit", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(
            move |_, (mut value, multiline, password, code): (String, bool, bool, bool)| {
                with_egui_ui(&bridge, |ui| {
                    let mut editor = if multiline {
                        egui::TextEdit::multiline(&mut value).desired_rows(8)
                    } else {
                        egui::TextEdit::singleline(&mut value)
                    };
                    editor = editor.password(password);
                    if code {
                        editor = editor.code_editor();
                    }
                    let changed = ui.add(editor).changed();
                    (value, changed)
                })
            },
        )?
    })?;
    module.set("color_edit_button_srgba", {
        let bridge = Arc::clone(&bridge);
        scope.create_function(move |_, (r, g, b, a): (u8, u8, u8, u8)| {
            with_egui_ui(&bridge, |ui| {
                let mut color = egui::Color32::from_rgba_unmultiplied(r, g, b, a);
                let changed = ui.color_edit_button_srgba(&mut color).changed();
                let [r, g, b, a] = color.to_array();
                (r, g, b, a, changed)
            })
        })?
    })?;

    // Sliders
    macro_rules! slider {
        ($name:literal, $ty:ty) => {{
            let bridge = Arc::clone(&bridge);
            module.set(
                $name,
                scope.create_function(
                    move |_, (label, mut value, min, max): (String, $ty, $ty, $ty)| {
                        with_egui_ui(&bridge, |ui| {
                            let changed = ui
                                .add(egui::Slider::new(&mut value, min..=max).text(&label))
                                .changed();
                            (value, changed)
                        })
                    },
                )?,
            )?;
        }};
    }
    slider!("slider_i8", i8);
    slider!("slider_u8", u8);
    slider!("slider_i16", i16);
    slider!("slider_u16", u16);
    slider!("slider_i32", i32);
    slider!("slider_u32", u32);
    slider!("slider_i64", i64);
    slider!("slider_u64", u64);
    slider!("slider_f32", f32);
    slider!("slider_f64", f64);

    // Combo box (dropdown)
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "combo_box",
            scope.create_function(
                move |_, (label, mut selected, options): (String, String, Vec<String>)| {
                    with_egui_ui(&bridge, |ui| {
                        let mut changed = false;
                        egui::ComboBox::from_label(&label)
                            .selected_text(&selected)
                            .show_ui(ui, |ui| {
                                for option in &options {
                                    let is_selected = *option == selected;
                                    if ui.selectable_label(is_selected, option).clicked()
                                        && !is_selected
                                    {
                                        selected = option.clone();
                                        changed = true;
                                    }
                                }
                            });
                        (selected, changed)
                    })
                },
            )?,
        )?;
    }

    // Simpler radio: pass label + current selected, return whether this label is now selected
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "radio",
            scope.create_function(move |_, (label, group_selected): (String, String)| {
                with_egui_ui(&bridge, |ui| {
                    let response = ui.radio(label == group_selected, &label);
                    (
                        label == group_selected,
                        response.clicked() && label != group_selected,
                    )
                })
            })?,
        )?;
    }

    // Progress bar
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "progress_bar",
            scope.create_function(move |_, (fraction, label): (f32, String)| {
                with_egui_ui(&bridge, |ui| {
                    ui.add(
                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                            .show_percentage()
                            .text(&label),
                    );
                })?;
                Ok(())
            })?,
        )?;
    }

    // Collapsing header
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "collapsing_header",
            scope.create_function(move |_, (title, body): (String, Function)| {
                let callback_error = std::cell::RefCell::new(None);
                with_egui_ui(&bridge, |ui| {
                    egui::CollapsingHeader::new(&title)
                        .default_open(false)
                        .show(ui, |_ui| {
                            if let Err(error) = body.call::<()>(()) {
                                *callback_error.borrow_mut() = Some(error);
                            }
                        });
                })?;
                match callback_error.into_inner() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })?,
        )?;
    }

    // Scroll area
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "scroll_area",
            scope.create_function(move |_, (height, body): (f32, Function)| {
                let callback_error = std::cell::RefCell::new(None);
                with_egui_ui(&bridge, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(height.max(32.0))
                        .show(ui, |_ui| {
                            if let Err(error) = body.call::<()>(()) {
                                *callback_error.borrow_mut() = Some(error);
                            }
                        });
                })?;
                match callback_error.into_inner() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })?,
        )?;
    }

    // Selectable label (list item)
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "selectable_label",
            scope.create_function(move |_, (label, selected): (String, bool)| {
                with_egui_ui(&bridge, |ui| {
                    let response = ui.selectable_label(selected, &label);
                    (response.clicked(), selected)
                })
            })?,
        )?;
    }

    // Heading text
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "heading",
            scope.create_function(move |_, text: String| {
                with_egui_ui(&bridge, |ui| {
                    ui.heading(&text);
                })?;
                Ok(())
            })?,
        )?;
    }

    // Monospace text
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "monospace",
            scope.create_function(move |_, text: String| {
                with_egui_ui(&bridge, |ui| {
                    ui.monospace(&text);
                })?;
                Ok(())
            })?,
        )?;
    }

    // Hyperlink
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "hyperlink_to",
            scope.create_function(move |_, (label, url): (String, String)| {
                with_egui_ui(&bridge, |ui| {
                    ui.hyperlink_to(&label, &url);
                })?;
                Ok(())
            })?,
        )?;
    }

    for (name, kind) in [("small", 0u8), ("weak", 1u8), ("code", 2u8)] {
        let bridge = Arc::clone(&bridge);
        module.set(
            name,
            scope.create_function(move |_, text: String| {
                with_egui_ui(&bridge, |ui| match kind {
                    0 => ui.small(text),
                    1 => ui.weak(text),
                    _ => ui.code(text),
                })?;
                Ok(())
            })?,
        )?;
    }

    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "add_space",
            scope.create_function(move |_, amount: f32| {
                with_egui_ui(&bridge, |ui| ui.add_space(amount.max(0.0)))?;
                Ok(())
            })?,
        )?;
    }

    // Spinner
    {
        let bridge = Arc::clone(&bridge);
        module.set(
            "spinner",
            scope.create_function(move |_, ()| {
                with_egui_ui(&bridge, |ui| {
                    ui.spinner();
                })?;
                Ok(())
            })?,
        )?;
    }

    Ok(module)
}
/*
                    if let Err(error) = body.call::<()>(()) {
                        *callback_error.borrow_mut() = Some(error);
                    }
                });
            match callback_error.into_inner() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })?,
    )?;
    module.set(
        "text",
        scope.create_function(|_, text: String| {
            ui.text(text);
            Ok(())
        })?,
    )?;
    module.set(
        "text_wrapped",
        scope.create_function(|_, text: String| {
            ui.text_wrapped(text);
            Ok(())
        })?,
    )?;
    module.set(
        "button",
        scope.create_function(|_, label: String| Ok(ui.button(label)))?,
    )?;
    module.set(
        "separator",
        scope.create_function(|_, ()| {
            ui.separator();
            Ok(())
        })?,
    )?;
    module.set(
        "same_line",
        scope.create_function(|_, ()| {
            ui.same_line();
            Ok(())
        })?,
    )?;
    module.set(
        "checkbox",
        scope.create_function(|_, (label, mut value): (String, bool)| {
            let changed = ui.checkbox(label, &mut value);
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_int",
        scope.create_function(|_, (label, mut value): (String, i32)| {
            let changed = ui.input_int(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_i64",
        scope.create_function(|_, (label, mut value): (String, i64)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u64",
        scope.create_function(|_, (label, mut value): (String, u64)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_i8",
        scope.create_function(|_, (label, mut value): (String, i8)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u8",
        scope.create_function(|_, (label, mut value): (String, u8)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_i16",
        scope.create_function(|_, (label, mut value): (String, i16)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u16",
        scope.create_function(|_, (label, mut value): (String, u16)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_u32",
        scope.create_function(|_, (label, mut value): (String, u32)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_f32",
        scope.create_function(|_, (label, mut value): (String, f32)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_f64",
        scope.create_function(|_, (label, mut value): (String, f64)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_isize",
        scope.create_function(|_, (label, mut value): (String, isize)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_usize",
        scope.create_function(|_, (label, mut value): (String, usize)| {
            let changed = ui.input_scalar(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    module.set(
        "input_text",
        scope.create_function(|_, (label, mut value): (String, String)| {
            let changed = ui.input_text(label, &mut value).build();
            Ok((value, changed))
        })?,
    )?;
    Ok(module)
}

*/
fn register_memory_api(lua: &Lua, async_scheduler: AsyncScheduler) -> mlua::Result<()> {
    let module = lua.create_table()?;

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_read_i32",
            lua.create_function(move |_, (pid, address): (u64, Address)| {
                scheduler
                    .submit(AsyncRequest::ReadI32 {
                        pid,
                        address: address.get(),
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_read_bytes",
            lua.create_function(move |_, (pid, address, size): (u64, Address, u64)| {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid pid or read size"));
                }
                scheduler
                    .submit(AsyncRequest::ReadBytes {
                        pid,
                        address: address.get(),
                        size: size as u64,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_write_i32",
            lua.create_function(move |_, (pid, address, value): (u64, Address, i32)| {
                scheduler
                    .submit(AsyncRequest::WriteI32 {
                        pid,
                        address: address.get(),
                        value,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_write_bytes",
            lua.create_function(move |_, (pid, address, data): (u64, Address, Vec<u8>)| {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid pid or write size"));
                }
                scheduler
                    .submit(AsyncRequest::WriteBytes {
                        pid,
                        address: address.get(),
                        data,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_get_pid",
            lua.create_function(move |_, name: String| {
                let name = name.trim().to_owned();
                if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
                    return Err(mlua::Error::runtime(
                        "process name must be 1..255 bytes and contain no NUL",
                    ));
                }
                scheduler
                    .submit(AsyncRequest::GetPid { name })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_get_process_base",
            lua.create_function(move |_, pid: u64| {
                scheduler
                    .submit(AsyncRequest::GetProcessBase { pid })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_read_rva",
            lua.create_function(move |_, (pid, relative_address, size): (u64, u64, u64)| {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid RVA read size"));
                }
                scheduler
                    .submit(AsyncRequest::ReadRva {
                        pid,
                        relative_address,
                        size: size as u64,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_write_rva",
            lua.create_function(
                move |_, (pid, relative_address, data): (u64, u64, Vec<u8>)| {
                    if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                        return Err(mlua::Error::runtime("invalid RVA write size"));
                    }
                    scheduler
                        .submit(AsyncRequest::WriteRva {
                            pid,
                            relative_address,
                            data,
                        })
                        .map_err(mlua::Error::external)
                },
            )?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_read_mdl",
            lua.create_function(move |_, (pid, address, size): (u64, Address, u64)| {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL read size"));
                }
                scheduler
                    .submit(AsyncRequest::ReadMdl {
                        pid,
                        address: address.get(),
                        size: size as u64,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_write_mdl",
            lua.create_function(move |_, (pid, address, data): (u64, Address, Vec<u8>)| {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL write size"));
                }
                scheduler
                    .submit(AsyncRequest::WriteMdl {
                        pid,
                        address: address.get(),
                        data,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_read_mdl_rva",
            lua.create_function(move |_, (pid, relative_address, size): (u64, u64, u64)| {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL RVA read size"));
                }
                scheduler
                    .submit(AsyncRequest::ReadMdlRva {
                        pid,
                        relative_address,
                        size: size as u64,
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_write_mdl_rva",
            lua.create_function(
                move |_, (pid, relative_address, data): (u64, u64, Vec<u8>)| {
                    if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                        return Err(mlua::Error::runtime("invalid MDL RVA write size"));
                    }
                    scheduler
                        .submit(AsyncRequest::WriteMdlRva {
                            pid,
                            relative_address,
                            data,
                        })
                        .map_err(mlua::Error::external)
                },
            )?,
        )?;
    }

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "async_list_processes",
            lua.create_function(move |_, ()| {
                scheduler
                    .submit(AsyncRequest::ListProcesses)
                    .map_err(mlua::Error::external)
            })?,
        )?;
    }

    module.set(
        "get_window_rect",
        lua.create_function(|lua, pid: u64| -> mlua::Result<Option<mlua::Table>> {
            let pid32 = u32::try_from(pid).map_err(|_| mlua::Error::runtime("PID too large"))?;
            let rects = crate::window_util::get_window_rects_by_pid(pid32);
            if rects.is_empty() {
                return Ok(None);
            }
            let scale = CONTENT_SCALE.load(Ordering::Relaxed) as f32 / 100.0;
            let list = lua.create_table()?;
            for (i, r) in rects.into_iter().enumerate() {
                let t = lua.create_table()?;
                t.set("x", r.x as f32 / scale)?;
                t.set("y", r.y as f32 / scale)?;
                t.set("width", r.width as f32 / scale)?;
                t.set("height", r.height as f32 / scale)?;
                list.set(i + 1, t)?;
            }
            Ok(Some(list))
        })?,
    )?;

    {
        let scheduler = async_scheduler.clone();
        module.set(
            "poll_async",
            lua.create_function(move |lua, id: u64| -> mlua::Result<Option<mlua::Table>> {
                let Some(result) = scheduler.poll_id(id) else {
                    return Ok(None);
                };
                let output = lua.create_table()?;
                match result {
                    Ok(AsyncValue::I32(value)) => {
                        output.set("value", value)?;
                    }
                    Ok(AsyncValue::Bytes(value)) => {
                        output.set("value", value)?;
                    }
                    Ok(AsyncValue::Pid(value)) => {
                        output.set("value", value)?;
                    }
                    Ok(AsyncValue::Processes(value)) => {
                        let processes = lua.create_table_with_capacity(value.len(), 0)?;
                        for (index, process_info) in value.into_iter().enumerate() {
                            let process = lua.create_table()?;
                            process.set("pid", process_info.pid)?;
                            process.set("parent_pid", process_info.parent_pid)?;
                            process.set("thread_count", process_info.thread_count)?;
                            process.set("name", process_info.name)?;
                            processes.set(index + 1, process)?;
                        }
                        output.set("value", processes)?;
                    }
                    Ok(AsyncValue::Unit) => {}
                    Err(error) => {
                        output.set("error", error)?;
                    }
                }
                output.set("done", true)?;
                Ok(Some(output))
            })?,
        )?;
    }

    lua.globals().set("memory", module)
}

fn register_shared_api(lua: &Lua, shared: SharedGlobals) -> mlua::Result<()> {
    let module = lua.create_table()?;
    {
        let shared = Arc::clone(&shared);
        module.set(
            "get",
            lua.create_function(move |lua, key: String| {
                let value = shared
                    .lock()
                    .map_err(|_| mlua::Error::runtime("shared globals lock poisoned"))?
                    .get(&key)
                    .cloned()
                    .unwrap_or(SharedValue::Nil);
                shared_value_to_lua(lua, value)
            })?,
        )?;
    }
    {
        let shared = Arc::clone(&shared);
        module.set(
            "set",
            lua.create_function(move |_, (key, value): (String, Value)| {
                if key.is_empty() || key.len() > 128 {
                    return Err(mlua::Error::runtime("shared key must be 1..128 bytes"));
                }
                shared
                    .lock()
                    .map_err(|_| mlua::Error::runtime("shared globals lock poisoned"))?
                    .insert(key, lua_to_shared_value(value)?);
                Ok(())
            })?,
        )?;
    }
    {
        let shared = Arc::clone(&shared);
        module.set(
            "delete",
            lua.create_function(move |_, key: String| {
                shared
                    .lock()
                    .map_err(|_| mlua::Error::runtime("shared globals lock poisoned"))?
                    .remove(&key);
                Ok(())
            })?,
        )?;
    }
    lua.globals().set("shared", module)
}

fn register_draw_api(lua: &Lua, draw_commands: DrawCommands) -> mlua::Result<()> {
    let module = lua.create_table()?;

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "line",
            lua.create_function(
                move |_,
                      (x1, y1, x2, y2, r, g, b, a, thickness): (
                    f32,
                    f32,
                    f32,
                    f32,
                    u8,
                    u8,
                    u8,
                    u8,
                    f32,
                )| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Line {
                            x1,
                            y1,
                            x2,
                            y2,
                            color: [r, g, b, a],
                            thickness,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "rect",
            lua.create_function(
                move |_,
                      (x, y, w, h, r, g, b, a, thickness): (
                    f32,
                    f32,
                    f32,
                    f32,
                    u8,
                    u8,
                    u8,
                    u8,
                    f32,
                )| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Rect {
                            x,
                            y,
                            w,
                            h,
                            color: [r, g, b, a],
                            thickness,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "filled_rect",
            lua.create_function(
                move |_,
                      (x, y, w, h, r, g, b, a): (f32, f32, f32, f32, u8, u8, u8, u8)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::FilledRect {
                            x,
                            y,
                            w,
                            h,
                            color: [r, g, b, a],
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "circle",
            lua.create_function(
                move |_, (x, y, radius, r, g, b, a, thickness): (f32, f32, f32, u8, u8, u8, u8, f32)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Circle {
                            x,
                            y,
                            radius,
                            color: [r, g, b, a],
                            thickness,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "filled_circle",
            lua.create_function(
                move |_, (x, y, radius, r, g, b, a): (f32, f32, f32, u8, u8, u8, u8)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::FilledCircle {
                            x,
                            y,
                            radius,
                            color: [r, g, b, a],
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let dc = Arc::clone(&draw_commands);
        module.set(
            "text",
            lua.create_function(
                move |_, (x, y, text, r, g, b, a, size): (f32, f32, String, u8, u8, u8, u8, f32)| {
                    dc.lock()
                        .map_err(|_| mlua::Error::runtime("draw lock poisoned"))?
                        .push(DrawCommand::Text {
                            x,
                            y,
                            text,
                            color: [r, g, b, a],
                            size,
                        });
                    Ok(())
                },
            )?,
        )?;
    }

    lua.globals().set("draw", module)
}

// Coroutine suspension happens entirely on the Lua thread. The IPC worker
// only produces task results, so no Lua object crosses a thread boundary.
fn install_async_helpers(lua: &Lua) -> mlua::Result<()> {
    lua.load(
        r#"
        __async_tasks = {}

        function await_async(task_id)
            while true do
                local result = memory.poll_async(task_id)
                if result then
                    if result.error then error(result.error) end
                    return result.value
                end
                coroutine.yield()
            end
        end

        function start_async(fn, ...)
            local co = coroutine.create(fn)
            local ok, err = coroutine.resume(co, ...)
            if not ok then error(err) end
            if coroutine.status(co) ~= "dead" then
                __async_tasks[co] = true
            end
            return co
        end

        function __pump_async_tasks()
            local finished = {}
            for co in pairs(__async_tasks) do
                local ok, err = coroutine.resume(co)
                if coroutine.status(co) == "dead" then
                    finished[#finished + 1] = co
                    if not ok then
                        print("async task failed: " .. tostring(err))
                    end
                end
            end
            for _, co in ipairs(finished) do
                __async_tasks[co] = nil
            end
        end
        "#,
    )
    .exec()
}

fn parse_address(value: &str) -> mlua::Result<Address> {
    let value = value.trim();
    let (digits, radix) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or((value, 10), |digits| (digits, 16));
    let address = u64::from_str_radix(digits, radix).map_err(|_| {
        mlua::Error::runtime("address must be a valid u64 decimal or 0x hexadecimal value")
    })?;
    Ok(Address::new(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_and_hex_addresses_without_float_conversion() {
        assert_eq!(
            parse_address("140702365450240").unwrap().get(),
            140702365450240
        );
        assert_eq!(
            parse_address("0x7FF812345000").unwrap().get(),
            0x7FF8_1234_5000
        );
        assert_eq!(parse_address("0").unwrap().get(), 0);
        assert!(parse_address("not-an-address").is_err());
    }

    #[test]
    fn scheduler_preserves_fractional_time() {
        let mut scheduler =
            FixedUpdateScheduler::new(Duration::from_millis(10), Duration::from_millis(100), 4);
        assert_eq!(scheduler.advance(Duration::from_millis(6)), 0);
        assert_eq!(scheduler.advance(Duration::from_millis(6)), 1);
        assert_eq!(scheduler.advance(Duration::from_millis(8)), 1);
    }

    #[test]
    fn scheduler_drops_excessive_backlog() {
        let mut scheduler =
            FixedUpdateScheduler::new(Duration::from_millis(10), Duration::from_millis(100), 4);
        assert_eq!(scheduler.advance(Duration::from_secs(1)), 4);
        assert_eq!(scheduler.advance(Duration::ZERO), 0);
    }

    #[test]
    fn execution_hook_stops_runaway_script() {
        let lua = Lua::new();
        let deadline = install_execution_hook(&lua).unwrap();
        lua.load("function OnUpdate() while true do end end")
            .exec()
            .unwrap();
        let error = call_optional_budgeted(
            &lua,
            &deadline,
            "OnUpdate",
            0.033_f32,
            Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("execution budget exceeded"));
    }
}
