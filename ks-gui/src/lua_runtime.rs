use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mlua::{Function, Lua, Table, Value};

mod engine_api;
mod execution;
mod keyboard;
mod types;

use crate::config_store::SharedConfigStore;
use types::RuntimeControl;
pub use types::{DrawCommand, DrawCommands};

static CONTENT_SCALE: AtomicU32 = AtomicU32::new(100);

pub fn set_content_scale(scale: f32) {
    CONTENT_SCALE.store((scale * 100.0) as u32, Ordering::Relaxed);
}

use types::Address;

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

/// Runs `draw` against a nested Ui (the child Ui handed out by container
/// widgets such as CollapsingHeader or ScrollArea) for the duration of the
/// call, then restores the previous bridge target. Without the swap, Lua body
/// callbacks would draw into the parent Ui and overlap surrounding content.
fn with_egui_ui_swapped<R>(
    bridge: &Mutex<usize>,
    nested: &mut egui::Ui,
    draw: impl FnOnce(&mut egui::Ui) -> R,
) -> Result<R, mlua::Error> {
    let previous = {
        let mut guard = bridge
            .lock()
            .map_err(|_| mlua::Error::runtime("egui bridge lock poisoned"))?;
        let previous = *guard;
        *guard = nested as *mut egui::Ui as usize;
        previous
    };
    let result = with_egui_ui(bridge, draw);
    if let Ok(mut guard) = bridge.lock() {
        *guard = previous;
    }
    result
}

fn create_ui_module<'scope>(
    lua: &Lua,
    scope: &'scope mlua::Scope<'scope, '_>,
    ctx: &'scope egui::Context,
    ui_visible: bool,
) -> mlua::Result<Table> {
    let module = lua.create_table()?;
    let bridge = Arc::new(Mutex::new(0usize));
    let bridge_for_window = Arc::clone(&bridge);
    module.set(
        "window",
        scope.create_function(move |_, (title, body): (String, Function)| {
            // While the script UI is hidden the window body is skipped entirely:
            // OnUpdate keeps running so draw.* overlay output is unaffected, and
            // scripts never observe a missing ui.window function.
            if !ui_visible {
                return Ok(());
            }
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
                        .show(ui, |inner_ui| {
                            if let Err(error) =
                                with_egui_ui_swapped(&bridge, inner_ui, |_ui| body.call::<()>(()))
                            {
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
                        .show(ui, |inner_ui| {
                            if let Err(error) =
                                with_egui_ui_swapped(&bridge, inner_ui, |_ui| body.call::<()>(()))
                            {
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
fn register_memory_api(lua: &Lua) -> mlua::Result<()> {
    let module = lua.create_table()?;

    module.set(
        "get_pid",
        lua.create_function(|_, name: String| -> mlua::Result<u64> {
            crate::sync_ipc::get_pid(&name).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "get_process_base",
        lua.create_function(|_, pid: u64| -> mlua::Result<u64> {
            crate::sync_ipc::get_process_base(pid).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "read_i32",
        lua.create_function(|_, (pid, address): (u64, Address)| -> mlua::Result<i32> {
            crate::sync_ipc::read_i32(pid, address.get()).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "read_bytes",
        lua.create_function(
            |_, (pid, address, size): (u64, Address, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid read size"));
                }
                crate::sync_ipc::read_bytes(pid, address.get(), size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_i32",
        lua.create_function(
            |_, (pid, address, value): (u64, Address, i32)| -> mlua::Result<()> {
                crate::sync_ipc::write_i32(pid, address.get(), value).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_bytes",
        lua.create_function(
            |_, (pid, address, data): (u64, Address, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid write size"));
                }
                crate::sync_ipc::write_bytes(pid, address.get(), &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "read_rva",
        lua.create_function(
            |_, (pid, relative_address, size): (u64, u64, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid RVA read size"));
                }
                crate::sync_ipc::read_rva(pid, relative_address, size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_rva",
        lua.create_function(
            |_, (pid, relative_address, data): (u64, u64, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid RVA write size"));
                }
                crate::sync_ipc::write_rva(pid, relative_address, &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "read_mdl",
        lua.create_function(
            |_, (pid, address, size): (u64, Address, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL read size"));
                }
                crate::sync_ipc::read_mdl(pid, address.get(), size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_mdl",
        lua.create_function(
            |_, (pid, address, data): (u64, Address, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL write size"));
                }
                crate::sync_ipc::write_mdl(pid, address.get(), &data).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "read_mdl_rva",
        lua.create_function(
            |_, (pid, relative_address, size): (u64, u64, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL RVA read size"));
                }
                crate::sync_ipc::read_mdl_rva(pid, relative_address, size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_mdl_rva",
        lua.create_function(
            |_, (pid, relative_address, data): (u64, u64, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL RVA write size"));
                }
                crate::sync_ipc::write_mdl_rva(pid, relative_address, &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "batch_read",
        lua.create_function(
            |_, (pid, size, addresses_table): (u64, u32, mlua::Table)| -> mlua::Result<Vec<u8>> {
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE as u32 {
                    return Err(mlua::Error::runtime("invalid batch entry size"));
                }
                let count = addresses_table.len()? as usize;
                if count > ks_core::protocol::MAX_BATCH_ENTRIES {
                    return Err(mlua::Error::runtime("too many batch entries"));
                }
                let mut addresses = Vec::with_capacity(count);
                for i in 1..=count {
                    let addr: u64 = addresses_table.get(i)?;
                    addresses.push(addr);
                }
                crate::sync_ipc::batch_read(pid, size, &addresses).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    // memory.batch_write(pid, writes) -> {bool, ...}
    // `writes` is an array of {address, data} tables; `data` is a byte
    // table. All entries are applied in a single ring round trip and a
    // single kernel transition; the returned table holds one success flag
    // per entry, in input order.
    module.set(
        "batch_write",
        lua.create_function(
            |lua, (pid, writes_table): (u64, mlua::Table)| -> mlua::Result<mlua::Table> {
                let count = writes_table.len()? as usize;
                if count == 0 || count > ks_core::protocol::MAX_BATCH_WRITE_ENTRIES {
                    return Err(mlua::Error::runtime("batch_write expects 1-64 entries"));
                }
                let mut entries = Vec::with_capacity(count);
                for i in 1..=count {
                    let item: mlua::Table = writes_table.get(i)?;
                    let address: Address = item.get("address")?;
                    let data: Vec<u8> = item.get("data")?;
                    if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                        return Err(mlua::Error::runtime(format!(
                            "invalid batch_write data size at entry {i}"
                        )));
                    }
                    entries.push((address.get(), data));
                }
                let flags =
                    crate::sync_ipc::batch_write(pid, &entries).map_err(mlua::Error::runtime)?;
                let result = lua.create_table()?;
                for (i, ok) in flags.iter().enumerate() {
                    result.set(i + 1, *ok)?;
                }
                Ok(result)
            },
        )?,
    )?;

    module.set(
        "batch_offset",
        lua.create_function(|lua, sizes: mlua::Table| -> mlua::Result<mlua::Table> {
            let count = sizes.len()? as usize;
            let offsets = lua.create_table()?;
            let mut acc = 0u64;
            for i in 1..=count {
                offsets.set(i, acc)?;
                let size: u64 = sizes.get(i)?;
                acc += size;
            }
            offsets.set("total", acc)?;
            Ok(offsets)
        })?,
    )?;

    module.set(
        "traverse_pointer_chain",
        lua.create_function(
            |_, (pid, base, offsets_table): (u64, u64, mlua::Table)| -> mlua::Result<u64> {
                let count = offsets_table.len()? as usize;
                if count > 32 {
                    return Err(mlua::Error::runtime("pointer chain: max 32 offsets"));
                }
                let mut offsets = Vec::with_capacity(count);
                for i in 1..=count {
                    let offset: u64 = offsets_table.get(i)?;
                    offsets.push(offset);
                }
                crate::sync_ipc::traverse_pointer_chain(pid, base, &offsets)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "lock",
        lua.create_function(
            |_, (id, pid, address, data): (u64, u64, Address, Vec<u8>)| {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid lock size"));
                }
                crate::sync_ipc::lock(id, pid, address.get(), &data).map_err(mlua::Error::runtime)
            },
        )?,
    )?;
    module.set(
        "unlock",
        lua.create_function(|_, id: u64| {
            crate::sync_ipc::unlock(id).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "unlock_all",
        lua.create_function(|_, pid: u64| {
            crate::sync_ipc::unlock_all(pid).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "lock_rva",
        lua.create_function(
            |_, (id, pid, relative_address, data): (u64, u64, u64, Vec<u8>)| {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid lock size"));
                }
                crate::sync_ipc::lock_rva(id, pid, relative_address, &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;
    module.set(
        "unlock_rva",
        lua.create_function(|_, id: u64| {
            crate::sync_ipc::unlock_rva(id).map_err(mlua::Error::runtime)
        })?,
    )?;

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

    lua.globals().set("memory", module)
}

/// Converts a Lua value into a storable config entry. Only JSON-mappable
/// scalars are accepted; tables, functions and userdata are rejected.
fn config_value_from_lua(value: &Value) -> Option<crate::config_store::ConfigValue> {
    use crate::config_store::ConfigValue;
    match value {
        Value::Boolean(flag) => Some(ConfigValue::Bool(*flag)),
        Value::Integer(int) => Some(ConfigValue::Int(*int)),
        Value::Number(number) => Some(ConfigValue::Float(*number)),
        Value::String(text) => text
            .to_str()
            .ok()
            .map(|text| ConfigValue::Str(text.to_owned())),
        _ => None,
    }
}

fn config_value_into_lua(lua: &Lua, value: crate::config_store::ConfigValue) -> Value {
    use crate::config_store::ConfigValue;
    match value {
        ConfigValue::Bool(flag) => Value::Boolean(flag),
        ConfigValue::Int(int) => Value::Integer(int),
        // Non-finite floats cannot be represented in JSON; surface nil instead
        // of letting the flush fail on serialization.
        ConfigValue::Float(float) if float.is_finite() => Value::Number(float),
        ConfigValue::Float(_) => Value::Nil,
        ConfigValue::Str(text) => match lua.create_string(text.as_bytes()) {
            Ok(string) => Value::String(string),
            Err(_) => Value::Nil,
        },
    }
}

fn register_config_api(
    lua: &Lua,
    config: crate::config_store::SharedConfigStore,
) -> mlua::Result<()> {
    let module = lua.create_table()?;

    // config.set(key, value) -> bool
    // Accepts boolean, integer, number or string values. Returns false for
    // unsupported types or oversized keys/values; the entry is only queued in
    // memory and written to config.json by the debounced flush.
    {
        let config = Arc::clone(&config);
        module.set(
            "set",
            lua.create_function(move |_, (key, value): (String, Value)| {
                let Some(entry) = config_value_from_lua(&value) else {
                    return Ok(false);
                };
                Ok(config.lock().unwrap().set(&key, entry))
            })?,
        )?;
    }

    // config.get(key, default?) -> value
    // Returns the stored entry, or `default` (nil when omitted) when missing.
    {
        let config = Arc::clone(&config);
        module.set(
            "get",
            lua.create_function(move |lua, (key, default): (String, Option<Value>)| {
                let stored = config
                    .lock()
                    .unwrap()
                    .get(&key)
                    .map(|entry| config_value_into_lua(lua, entry));
                Ok(stored.unwrap_or_else(|| default.unwrap_or(Value::Nil)))
            })?,
        )?;
    }

    // config.remove(key) -> bool
    {
        let config = Arc::clone(&config);
        module.set(
            "remove",
            lua.create_function(move |_, key: String| Ok(config.lock().unwrap().remove(&key)))?,
        )?;
    }

    // config.save() -> bool
    // Forces an immediate write of the pending changes to config.json.
    {
        let config = Arc::clone(&config);
        module.set(
            "save",
            lua.create_function(move |_, ()| {
                let mut store = config.lock().unwrap();
                store.flush();
                Ok(!store.is_dirty())
            })?,
        )?;
    }

    lua.globals().set("config", module)
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
                move |_, (x, y, w, h, r, g, b, a): (f32, f32, f32, f32, u8, u8, u8, u8)| {
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
                move |_,
                      (x, y, radius, r, g, b, a, thickness): (
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
