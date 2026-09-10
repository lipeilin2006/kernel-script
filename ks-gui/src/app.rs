use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Instant;

use egui::FontFamily;
use egui_overlay::egui_render_three_d::{ThreeDConfig, ThreeDBackend};
use egui_overlay::egui_window_glfw_passthrough::{self as glfw_passthrough, GlfwBackend, GlfwConfig};
use egui_overlay::EguiOverlay;

use crate::lua_runtime::LuaRuntimeManager;

struct KernelScriptApp {
    runtime: LuaRuntimeManager,
    fonts_installed: bool,
}

impl KernelScriptApp {
    fn new() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            runtime: LuaRuntimeManager::new(std::path::PathBuf::from("scripts"))?,
            fonts_installed: false,
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
        if !self.fonts_installed {
            let _ = install_chinese_font(ctx);
            let mut style = (*ctx.style()).clone();
            style.visuals.window_fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            style.visuals.panel_fill = egui::Color32::from_rgba_premultiplied(20, 20, 20, 200);
            ctx.set_style(style);
            self.fonts_installed = true;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.frame(ctx, Instant::now());
        }));
        if result.is_err() {
            tracing::error!("panic recovered at GUI frame boundary");
            self.runtime
                .set_error("GUI frame callback panicked".to_owned());
        }
        let wants_input = ctx.is_pointer_over_area();
        glfw_backend.set_passthrough(!wants_input);
    }
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt().with_target(false).try_init();

    let mut glfw_backend = GlfwBackend::new(GlfwConfig {
        size: [1920, 1080],
        transparent_window: Some(true),
        opengl_window: Some(true),
        glfw_callback: Box::new(|gtx| {
            (glfw_passthrough::GlfwConfig::default().glfw_callback)(gtx);
            gtx.window_hint(glfw_passthrough::glfw::WindowHint::ScaleToMonitor(true));
        }),
        window_callback: Box::new(|window: &mut glfw_passthrough::glfw::Window| {
            window.set_floating(true);
            window.set_decorated(false);
            window.set_pos(0, 0);
        }),
        ..Default::default()
    });

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
