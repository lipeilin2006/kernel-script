//! Chinese font installation shared by both desktop apps.
//!
//! Neither app bundles a CJK font: the first available `simhei.ttf`,
//! `msyh.ttc` or `simsun.ttc` under `C:\Windows\Fonts` is loaded into the
//! app's own egui context and prepended to both default families, so Lua and
//! UI text render on a stock Windows install.

use std::error::Error;
use std::fs;
use std::path::Path;

use egui::FontFamily;

pub fn install_chinese_font(ctx: &egui::Context) -> Result<(), Box<dyn Error>> {
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
        egui::FontData::from_owned(bytes).into(),
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
