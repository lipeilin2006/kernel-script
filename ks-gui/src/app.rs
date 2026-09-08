use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eframe::egui;
use egui::FontFamily;

use crate::lua_runtime::LuaRuntimeManager;

struct KernelScriptApp {
    runtime: LuaRuntimeManager,
}

impl KernelScriptApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, Box<dyn Error>> {
        install_chinese_font(&cc.egui_ctx)?;
        Ok(Self {
            runtime: LuaRuntimeManager::new(PathBuf::from("scripts"))?,
        })
    }
}

impl eframe::App for KernelScriptApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.frame(ctx, Instant::now());
        }));
        if result.is_err() {
            tracing::error!("panic recovered at GUI frame boundary");
            self.runtime
                .set_error("GUI frame callback panicked".to_owned());
        }
        ctx.request_repaint();
    }
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt().with_target(false).try_init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kernel Script")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Kernel Script",
        options,
        Box::new(|creation_context| {
            Box::new(
                KernelScriptApp::new(creation_context)
                    .expect("failed to initialize Kernel Script GUI"),
            )
        }),
    )?;
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
