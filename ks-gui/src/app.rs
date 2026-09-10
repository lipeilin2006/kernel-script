use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Instant;

use egui::FontFamily;
use egui_overlay::egui_window_glfw_passthrough::GlfwBackend;
use egui_overlay::EguiOverlay;

use crate::lua_runtime::LuaRuntimeManager;

struct KernelScriptApp {
    runtime: LuaRuntimeManager,
    fonts_installed: bool,
    fullscreen_set: bool,
}

impl KernelScriptApp {
    fn new() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            runtime: LuaRuntimeManager::new(std::path::PathBuf::from("scripts"))?,
            fonts_installed: false,
            fullscreen_set: false,
        })
    }
}

impl EguiOverlay for KernelScriptApp {
    fn gui_run(
        &mut self,
        ctx: &egui::Context,
        default_gfx_backend: &mut egui_overlay::egui_render_three_d::ThreeDBackend,
        glfw_backend: &mut GlfwBackend,
    ) {
        if !self.fullscreen_set {
            glfw_backend.glfw.with_primary_monitor(|_, monitor| {
                if let Some(monitor) = monitor {
                    if let Some(mode) = monitor.get_video_mode() {
                        glfw_backend.window.set_monitor(
                            egui_overlay::egui_window_glfw_passthrough::glfw::WindowMode::Windowed,
                            0,
                            0,
                            mode.width as u32,
                            mode.height as u32,
                            Some(mode.refresh_rate),
                        );
                    }
                }
            });
            self.fullscreen_set = true;
        }
        if !self.fonts_installed {
            let _ = install_chinese_font(ctx);
            // Make egui windows and panels transparent so the overlay shows through.
            let mut style = (*ctx.style()).clone();
            style.visuals.window_fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            style.visuals.panel_fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            ctx.set_style(style);
            self.fonts_installed = true;
        }
        // Set clear color to transparent black so the overlay background is invisible.
        unsafe {
            use glow::HasContext;
            default_gfx_backend.glow_backend.glow_context.clear_color(0.0, 0.0, 0.0, 0.0);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.frame(ctx, Instant::now());
        }));
        if result.is_err() {
            tracing::error!("panic recovered at GUI frame boundary");
            self.runtime
                .set_error("GUI frame callback panicked".to_owned());
        }
    }
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt().with_target(false).try_init();
    let app = KernelScriptApp::new()?;
    egui_overlay::start(app);
    Ok(())
}

fn install_chinese_font(ctx: &egui::Context) -> Result<(), Box<dyn Error>> {
    let candidates = [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    let Some(path) = candidates.iter().find(|path| Path::new(path).is_file()) else {
        tracing::warn!("no Chinese font found under C:\\Windows\\Fonts");
        return Ok(());
    };
    let bytes = fs::read(path)?;
    let byte_len = bytes.len();
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "kernel-script-cjk".to_owned(),
        egui::FontData::from_owned(bytes),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "kernel-script-cjk".to_owned());
    }
    fonts.families.insert(
        FontFamily::Name("Button".into()),
        vec!["kernel-script-cjk".to_owned()],
    );
    ctx.set_fonts(fonts);
    tracing::info!(font = path, bytes = byte_len, "Chinese font loaded");
    Ok(())
}
