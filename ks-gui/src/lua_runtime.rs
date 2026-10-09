use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mlua::{Lua, Value};

mod config_api;
mod draw_api;
mod engine_api;
mod execution;
mod keyboard;
mod memory_api;
mod types;
mod ui_api;

use crate::config_store::SharedConfigStore;
use config_api::register_config_api;
use draw_api::register_draw_api;
use memory_api::register_memory_api;
use types::RuntimeControl;
pub use types::{DrawCommand, DrawCommands};
use ui_api::create_ui_module;

static CONTENT_SCALE: AtomicU32 = AtomicU32::new(100);

pub fn set_content_scale(scale: f32) {
    CONTENT_SCALE.store((scale * 100.0) as u32, Ordering::Relaxed);
}

// The whole overlay (render + OnUpdate) is capped at 60 frames per second by
// the repaint interval in app.rs. OnUpdate runs once per rendered frame, which
// keeps immediate-mode egui windows stable; a slow callback simply lowers the
// achieved frequency.
const MAX_FRAME_DELTA: Duration = Duration::from_millis(250);
const RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LUA_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const START_BUDGET: Duration = Duration::from_millis(100);
const UPDATE_BUDGET: Duration = Duration::from_millis(20);
const DESTROY_BUDGET: Duration = Duration::from_millis(50);
const UPDATE_HEALTHY_FRAMES_TO_CLEAR_ERROR: u32 = 30;

/// Overlay frame interval: the maximum GUI frame rate (60 Hz).
pub const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

pub struct LuaRuntime {
    lua: Lua,
    script_path: PathBuf,
    control: Arc<RuntimeControl>,
    deadline: Arc<AtomicU64>,
    last_error: Option<String>,
    consecutive_healthy_updates: u32,
    last_tick: Instant,
    last_reload_poll: Instant,
    script_modified: Option<SystemTime>,
    draw_commands: DrawCommands,
    config: SharedConfigStore,
    keyboard: keyboard::SharedKeyboardState,
}

pub struct LuaRuntimeManager {
    runtimes: Vec<LuaRuntime>,
    draw_commands: DrawCommands,
    config: SharedConfigStore,
}

impl LuaRuntimeManager {
    pub fn new(script_directory: PathBuf) -> mlua::Result<Self> {
        let draw_commands: DrawCommands = Arc::new(Mutex::new(Vec::new()));
        // config.json lives beside the executable (scripts/<dir>/..). The
        // store is manager-owned so it survives per-script hot reloads.
        let config = crate::config_store::ConfigStore::shared(
            script_directory
                .parent()
                .unwrap_or(script_directory.as_path()),
        );
        let mut paths = fs::read_dir(&script_directory)
            .map_err(mlua::Error::external)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "lua"))
            .filter(|path| {
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !name.starts_with('_'))
            })
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
            .map(|path| LuaRuntime::new(path, Arc::clone(&draw_commands), Arc::clone(&config)))
            .collect::<mlua::Result<Vec<_>>>()?;
        Ok(Self {
            runtimes,
            draw_commands,
            config,
        })
    }

    pub fn frame(&mut self, ctx: &egui::Context, now: Instant, ui_visible: bool) {
        // Every rendered frame runs OnUpdate (immediate-mode egui requires it)
        // and regenerates the draw command snapshot.
        self.draw_commands.lock().unwrap().clear();
        for runtime in &mut self.runtimes {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.frame(ctx, now, ui_visible);
            }));
            if result.is_err() {
                runtime.set_error("Lua runtime panicked; script disabled until reload".to_owned());
            }
        }
        self.config.lock().unwrap().flush_if_due();
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
        draw_commands: DrawCommands,
        config: SharedConfigStore,
    ) -> mlua::Result<Self> {
        let control = Arc::new(RuntimeControl::default());
        let keyboard = Arc::new(Mutex::new(keyboard::KeyboardState::new()));
        let (lua, deadline) = Self::build_vm(
            &script_path,
            Arc::clone(&control),
            Arc::clone(&draw_commands),
            Arc::clone(&config),
            Arc::clone(&keyboard),
        )?;
        let now = Instant::now();
        Ok(Self {
            lua,
            script_modified: script_modified(&script_path),
            draw_commands,
            script_path,
            control,
            deadline,
            last_error: None,
            consecutive_healthy_updates: 0,
            last_tick: now,
            last_reload_poll: now,
            config,
            keyboard,
        })
    }

    fn frame(&mut self, ctx: &egui::Context, now: Instant, ui_visible: bool) {
        self.check_hot_reload(now);
        // Snapshot all keys once per frame so is_key_press reports a stable
        // down-edge for the duration of OnUpdate.
        self.keyboard.lock().unwrap().begin_frame();
        self.run_updates(ctx, now, ui_visible);
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
        draw_commands: DrawCommands,
        config: SharedConfigStore,
        keyboard: keyboard::SharedKeyboardState,
    ) -> mlua::Result<(Lua, Arc<AtomicU64>)> {
        let lua = Lua::new();
        lua.set_memory_limit(LUA_MEMORY_LIMIT)?;
        // Scripts only need the registered engine and memory APIs.
        for global in ["os", "io", "package", "debug"] {
            lua.globals().set(global, Value::Nil)?;
        }
        let deadline = execution::install_hook(&lua);
        engine_api::register(&lua, control)?;
        register_memory_api(&lua)?;
        register_draw_api(&lua, draw_commands)?;
        register_config_api(&lua, config)?;
        keyboard::register(&lua, keyboard)?;

        let source = fs::read_to_string(script_path).map_err(mlua::Error::external)?;
        execution::set_deadline(&deadline, Some(START_BUDGET));
        let load_result = lua
            .load(&source)
            .set_name(script_path.to_string_lossy().as_ref())
            .exec();
        execution::set_deadline(&deadline, None);
        load_result?;
        execution::call_budgeted(&lua, &deadline, "OnStart", (), START_BUDGET)?;
        Ok((lua, deadline))
    }

    fn run_updates(&mut self, ctx: &egui::Context, now: Instant, ui_visible: bool) {
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;

        // OnUpdate runs once per rendered frame. The dt is the real frame
        // interval; a slow callback simply stretches it and lowers the
        // achieved frequency. Budget overruns are advisory warnings only.
        let result = self.lua.scope(|scope| {
            let module = create_ui_module(&self.lua, scope, ctx, ui_visible)?;
            self.lua.globals().set("ui", module)?;
            let dt = if self.control.paused.load(Ordering::Relaxed) {
                0.0
            } else {
                elapsed.min(MAX_FRAME_DELTA).as_secs_f32()
            };
            match execution::call_unbudgeted(&self.lua, "OnUpdate", dt) {
                Ok(callback_elapsed) => {
                    self.consecutive_healthy_updates =
                        self.consecutive_healthy_updates.saturating_add(1);
                    if callback_elapsed > UPDATE_BUDGET {
                        tracing::warn!(
                            script = %self.script_path.display(),
                            elapsed_us = callback_elapsed.as_micros() as u64,
                            budget_us = UPDATE_BUDGET.as_micros() as u64,
                            "Lua OnUpdate exceeded advisory budget"
                        );
                    }
                    if self.consecutive_healthy_updates >= UPDATE_HEALTHY_FRAMES_TO_CLEAR_ERROR
                        && self
                            .last_error
                            .as_deref()
                            .is_some_and(|error| error.starts_with("OnUpdate failed:"))
                    {
                        self.last_error = None;
                    }
                }
                Err(error) => {
                    self.consecutive_healthy_updates = 0;
                    self.last_error = Some(format!("OnUpdate failed: {error}"));
                }
            }
            Ok::<(), mlua::Error>(())
        });
        if let Err(error) = result {
            self.last_error = Some(format!("OnUpdate failed: {error}"));
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
            Arc::clone(&self.draw_commands),
            Arc::clone(&self.config),
            Arc::clone(&self.keyboard),
        ) {
            Ok((new_lua, new_deadline)) => {
                if let Err(error) = execution::call_budgeted(
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
                self.consecutive_healthy_updates = 0;
                self.last_tick = Instant::now();
                self.script_modified = script_modified(&self.script_path);
            }
            Err(error) => self.last_error = Some(format!("Reload failed: {error}")),
        }
    }
}

impl Drop for LuaRuntime {
    fn drop(&mut self) {
        if let Err(error) =
            execution::call_budgeted(&self.lua, &self.deadline, "OnDestroy", (), DESTROY_BUDGET)
        {
            eprintln!("OnDestroy failed during shutdown: {error}");
        }
    }
}

fn script_modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

// Coroutine suspension happens entirely on the Lua thread. The IPC worker
// only produces task results, so no Lua object crosses a thread boundary.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_and_hex_addresses_without_float_conversion() {
        assert_eq!(
            engine_api::parse_address("140702365450240").unwrap().get(),
            140702365450240
        );
        assert_eq!(
            engine_api::parse_address("0x7FF812345000").unwrap().get(),
            0x7FF8_1234_5000
        );
        assert_eq!(engine_api::parse_address("0").unwrap().get(), 0);
        assert!(engine_api::parse_address("not-an-address").is_err());
    }

    #[test]
    fn execution_hook_stops_runaway_script() {
        let lua = Lua::new();
        let deadline = execution::install_hook(&lua);
        lua.load("function OnUpdate() while true do end end")
            .exec()
            .unwrap();
        let error = execution::call_budgeted(
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
