use std::cell::Cell;
use std::error::Error;
use std::rc::Rc;
use std::time::Instant;

use egui_overlay::egui_render_three_d::{ThreeDBackend, ThreeDConfig};
use egui_overlay::egui_window_glfw_passthrough::{
    self as glfw_passthrough, GlfwBackend, GlfwConfig,
};
use egui_overlay::EguiOverlay;

use ks_ui_common::install_chinese_font;

use crate::lua_runtime::LuaRuntimeManager;

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

struct KernelScriptApp {
    runtime: LuaRuntimeManager,
    driver: crate::driver::DriverControl,
    fonts_installed: bool,
    ui_visible: bool,
    insert_was_down: bool,
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
        })
    }
}

impl EguiOverlay for KernelScriptApp {
    fn gui_run(
        &mut self,
        ctx: &egui::Context,
        default_gfx_backend: &mut ThreeDBackend,
        glfw_backend: &mut GlfwBackend,
    ) {
        // The overlay has no ordinary widget invalidation to drive repainting.
        // Cap the whole overlay (render + OnUpdate) at 60 FPS so egui windows
        // stay stable and slow Lua callbacks simply lower the frame rate.
        ctx.request_repaint_after(crate::lua_runtime::FRAME_INTERVAL);
        // Insert toggles the whole script UI (Lua windows and the small
        // control window) while the draw.* overlay layer keeps rendering.
        // GetAsyncKeyState reads the physical key state of the whole desktop,
        // so the toggle works while another window (the game) owns input
        // focus and the unfocused overlay never receives key events.
        let insert_down = unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_INSERT as i32,
            ) as u16
                & 0x8000
                != 0
        };
        let toggle_requested = insert_down && !self.insert_was_down;
        self.insert_was_down = insert_down;
        if toggle_requested {
            self.ui_visible = !self.ui_visible;
            tracing::info!(ui_visible = self.ui_visible, "script UI toggled");
        }
        if !self.fonts_installed {
            let _ = install_chinese_font(ctx);
            let mut style = (*ctx.style()).clone();
            style.visuals.window_fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            style.visuals.panel_fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            ctx.set_style(style);
            self.fonts_installed = true;
            unsafe {
                use glow::HasContext;
                default_gfx_backend
                    .glow_backend
                    .glow_context
                    .clear_color(0.0, 0.0, 0.0, 0.0);
            }
        }
        // Driver lifecycle first: apply finished worker jobs (this flips
        // the sync_ipc gate) before this frame's Lua runs.
        self.driver.poll();
        if self.ui_visible {
            self.control_window(ctx, glfw_backend);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.frame(ctx, Instant::now(), self.ui_visible);
        }));
        if result.is_err() {
            tracing::error!("panic recovered at GUI frame boundary");
            self.runtime
                .set_error("GUI frame callback panicked".to_owned());
        }
        let wants_input = ctx.is_pointer_over_area();
        glfw_backend.set_passthrough(!wants_input);

        crate::overlay::paint_commands(ctx, self.runtime.take_draw_commands());
    }
}

impl KernelScriptApp {
    /// The overlay's only chrome: driver status, the language toggle and
    /// the quit button. The memory tool is a separate application
    /// (`ks-inspector`), so the overlay no longer opens or embeds it.
    fn control_window(&mut self, ctx: &egui::Context, glfw_backend: &mut GlfwBackend) {
        egui::Window::new(crate::lang::s().overlay_title)
            .default_pos(egui::pos2(16.0, 16.0))
            .resizable(false)
            .show(ctx, |ui| {
                let (is_error, text) = self.driver.status_text();
                if is_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, text);
                } else {
                    ui.label(text);
                }
                ui.horizontal(|ui| {
                    if ui.button(crate::lang::s().quit).clicked() {
                        glfw_backend.window.set_should_close(true);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::lang::toggle_widget(ui, &self.runtime.config_store());
                });
            });
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

    let monitor_size: Rc<Cell<[u32; 2]>> = Rc::new(Cell::new([1920, 1080]));
    let monitor_size_clone = monitor_size.clone();

    let mut glfw_backend = GlfwBackend::new(GlfwConfig {
        transparent_window: Some(true),
        opengl_window: Some(true),
        glfw_callback: Box::new(move |gtx| {
            (glfw_passthrough::GlfwConfig::default().glfw_callback)(gtx);
            gtx.window_hint(glfw_passthrough::glfw::WindowHint::ScaleToMonitor(true));
            gtx.with_primary_monitor(|_, monitor| {
                if let Some(monitor) = monitor {
                    if let Some(video_mode) = monitor.get_video_mode() {
                        monitor_size_clone.set([video_mode.width, video_mode.height]);
                    }
                }
            });
        }),
        window_callback: Box::new(|window: &mut glfw_passthrough::glfw::Window| {
            window.set_floating(true);
            window.set_decorated(false);
        }),
        ..Default::default()
    });

    let [w, h] = monitor_size.get();
    let h = h - 10;
    glfw_backend.window.set_size(w as i32, h as i32);
    glfw_backend.window.set_pos(0, 0);

    {
        let (sx, _sy) = glfw_backend.window.get_content_scale();
        crate::lua_runtime::set_content_scale(sx);
        tracing::info!(content_scale = sx, "content scale set");
    }

    #[cfg(target_os = "windows")]
    unsafe {
        use glfw_passthrough::glfw::ffi::glfwGetWin32Window;
        let hwnd = glfwGetWin32Window(&mut glfw_backend.window as *mut _ as *mut _);
        win32::enable_dwm_transparency(hwnd as *mut _);
    }

    let fb_size = glfw_backend.window.get_framebuffer_size();
    let latest_size = [fb_size.0 as _, fb_size.1 as _];

    let default_gfx_backend = ThreeDBackend::new(
        ThreeDConfig::default(),
        |s| glfw_backend.get_proc_address(s),
        latest_size,
    );

    let app = KernelScriptApp::new()?;
    let overlap_app = egui_overlay::OverlayApp {
        user_data: app,
        egui_context: Default::default(),
        default_gfx_backend,
        glfw_backend,
    };
    overlap_app.enter_event_loop();
    // The loop only returns once the window is closed (the Quit button or
    // a WM_CLOSE): Lua and rendering are gone, so the driver can be shut
    // down synchronously here.
    crate::driver::finish_on_exit();
    Ok(())
}
