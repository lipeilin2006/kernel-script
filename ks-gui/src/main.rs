#![windows_subsystem = "windows"]
mod app;
mod ce_window;
mod config_store;
mod driver;
mod lang;
mod overlay;

mod lua_runtime;

mod sync_ipc;

mod window_util;
fn main() {
    if let Err(error) = app::run() {
        eprintln!("Failed to run GUI: {error}");
    }
}
