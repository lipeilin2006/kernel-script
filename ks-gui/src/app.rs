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
        _default_gfx_backend: &mut egui_overlay::egui_render_three_d::ThreeDBackend,
        _glfw_backend: &mut GlfwBackend,
    ) {
        if !self.fonts_installed {
            let _ = install_chinese_font(ctx);
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
