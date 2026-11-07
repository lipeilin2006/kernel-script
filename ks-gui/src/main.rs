#![windows_subsystem = "windows"]

pub(crate) use ks_ui_common::{config_store, driver, lang, sync_ipc};

mod app;
mod lua_runtime;
mod overlay;
mod window_util;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("Failed to run GUI: {error}");
    }
}
