use std::error::Error;
use std::time::Instant;

use ks_ui_common::install_chinese_font;

use crate::lua_runtime::{LuaRuntimeManager, UiRectHandle};

mod runtime;

#[cfg(target_os = "windows")]
mod win32 {
    use std::ffi::c_void;

    #[repr(C)]
    #[allow(non_camel_case_types)]
    #[allow(clippy::upper_case_acronyms)]
    pub struct MARGINS {
        pub cx_left_width: i32,
        pub cx_right_width: i32,
        pub cy_top_height: i32,
        pub cy_bottom_height: i32,
    }

    #[link(name = "dwmapi")]
    extern "system" {
        pub fn DwmExtendFrameIntoClientArea(hwnd: *mut c_void, margins: *const MARGINS) -> i32;
    }

    pub unsafe fn enable_dwm_transparency(hwnd: *mut c_void) {
        let margins = MARGINS {
            cx_left_width: -1,
            cx_right_width: -1,
            cy_top_height: -1,
            cy_bottom_height: -1,
        };
        DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

/// Whole-UI scale, matching the ks-td2 overlay. `Ctrl +/-/0` zoom is
/// disabled: while another window (the game) owns input, egui's keyboard
/// zoom would rescale the overlay in the middle of aiming.
pub(crate) const UI_SCALE: f32 = 0.8;

/// Overlay presentation cadence: 100 Hz. Lua `OnUpdate` runs once per
/// presented frame, i.e. at this rate.
pub(crate) const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_micros(10_000);

pub(crate) struct KernelScriptApp {
    runtime: LuaRuntimeManager,
    driver: crate::driver::DriverControl,
    fonts_installed: bool,
    ui_visible: bool,
    insert_was_down: bool,
    pub(crate) quit_requested: bool,
    /// Smoothed actual presentation rate, shown top-right.
    pub(crate) fps_ema: f32,
    last_frame_at: Instant,
}

impl KernelScriptApp {
    fn new() -> Result<Self, Box<dyn Error>> {
        let exe_dir = std::env::current_exe()?
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let script_dir = exe_dir.join("scripts");
        // The manager runs every script's OnStart before this, so no Lua
        // round trip can overlap the probe the control spawns in `new`.
        let runtime = LuaRuntimeManager::new(script_dir)?;
        // Restore the UI language before the first frame renders.
        crate::lang::restore(&runtime.config_store());
        Ok(Self {
            driver: crate::driver::DriverControl::new(),
            runtime,
            fonts_installed: false,
            ui_visible: true,
            insert_was_down: false,
            quit_requested: false,
            fps_ema: 0.0,
            last_frame_at: Instant::now(),
        })
    }

    pub(crate) fn ui_rect_handle(&self) -> UiRectHandle {
        self.runtime.ui_rect_handle()
    }

    /// Insert toggles the whole script UI (Lua windows and the compact
    /// control window) while the `draw.*` overlay layer keeps rendering.
    /// `GetAsyncKeyState` reads the physical key state of the whole desktop,
    /// so the toggle works while the game owns input focus and the
    /// click-through overlay never receives key events itself.
    pub(crate) fn poll_input(&mut self) {
        let insert_down = unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_INSERT as i32,
            ) as u16
                & 0x8000
                != 0
        };
        if insert_down && !self.insert_was_down {
            self.ui_visible = !self.ui_visible;
            tracing::info!(ui_visible = self.ui_visible, "script UI toggled");
        }
        self.insert_was_down = insert_down;
    }

    pub(crate) fn ui_visible(&self) -> bool {
        self.ui_visible
    }

    pub(crate) fn driver(&mut self) -> &mut crate::driver::DriverControl {
        &mut self.driver
    }

    /// Installs the Chinese font and the translucent window theme once. The
    /// font must be set before the first egui pass so the first frame already
    /// lays out CJK labels with the right metrics.
    pub(crate) fn ensure_chrome(&mut self, ctx: &egui::Context) {
        if self.fonts_installed {
            return;
        }
        let _ = install_chinese_font(ctx);
        ctx.all_styles_mut(|style| {
            let fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            style.visuals.window_fill = fill;
            style.visuals.panel_fill = fill;
            style.visuals.extreme_bg_color =
                egui::Color32::from_rgba_premultiplied(20, 20, 20, 240);
        });
        self.fonts_installed = true;
    }

    /// The overlay's only chrome: driver status, the language toggle and
    /// the quit button. The memory tool is a separate application
    /// (`ks-inspector`), so the overlay no longer opens or embeds it.
    pub(crate) fn control_window(&mut self, ctx: &egui::Context, ui_rect: &UiRectHandle) {
        let strings = crate::lang::s();
        let mut quit = false;
        let response = egui::Window::new(strings.overlay_title)
            .default_pos(egui::pos2(16.0, 16.0))
            .resizable(false)
            .show(ctx, |ui| {
                let (is_error, text) = self.driver.status_text();
                if is_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, text);
                } else {
                    ui.label(text);
                }
                // One compact row for the two controls. Do not wrap this in
                // a `right_to_left` layout: that child ui starts at the far
                // right of the available rect and its centered cross-axis
                // alignment stretches the auto-sized window to most of the
                // screen height.
                ui.horizontal(|ui| {
                    crate::lang::toggle_widget(ui, &self.runtime.config_store());
                    if ui.button(strings.quit).clicked() {
                        quit = true;
                    }
                });
            });
        if let Some(response) = response {
            crate::lua_runtime::ui_api::record_ui_rect(ui_rect, response.response.rect);
        }
        if quit {
            self.quit_requested = true;
        }
    }

    /// Runs one Lua frame inside the active egui pass and paints the queued
    /// `draw.*` commands on the background layer, below all windows.
    pub(crate) fn run_lua_frame(
        &mut self,
        ctx: &egui::Context,
        now: Instant,
        ui_visible: bool,
        origin: (f32, f32),
        fps: f32,
    ) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.frame(ctx, now, ui_visible);
        }));
        if result.is_err() {
            tracing::error!("panic recovered at GUI frame boundary");
            self.runtime
                .set_error("GUI frame callback panicked".to_owned());
        }
        let commands = self.runtime.take_draw_commands();
        crate::overlay::paint_commands(ctx, &commands, origin);
        crate::overlay::paint_fps(ctx, fps);
    }

    /// EMA-smoothed presentation rate, sampled once per displayed tick.
    pub(crate) fn sample_fps(&mut self) {
        let dt = self.last_frame_at.elapsed().as_secs_f32();
        self.last_frame_at = Instant::now();
        if dt > 0.0 {
            let instantaneous = 1.0 / dt;
            self.fps_ema = if self.fps_ema <= 0.0 {
                instantaneous
            } else {
                self.fps_ema * 0.9 + instantaneous * 0.1
            };
        }
    }
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::fs::File::create("ks-gui.log").unwrap_or_else(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("ks-gui.log")
                .unwrap()
        }))
        .try_init();

    let (overlay, gfx) = runtime::create_overlay_window();
    let app = KernelScriptApp::new()?;
    runtime::DisplayLoop { overlay, gfx, app }.run();
    // The loop only returns once the window is closed (the Quit button or
    // a WM_CLOSE): Lua and rendering are gone, so the driver can be shut
    // down synchronously here.
    crate::driver::finish_on_exit();
    Ok(())
}
