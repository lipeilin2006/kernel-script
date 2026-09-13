#![windows_subsystem = "windows"]
mod app;

mod lua_runtime;

mod sync_ipc;

mod window_util;
fn main() {
    if let Err(error) = app::run() {
        eprintln!("Failed to run GUI: {error}");
    }
}
