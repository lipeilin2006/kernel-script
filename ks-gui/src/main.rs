#![windows_subsystem = "windows"]
mod app;

mod ipc_client;

mod lua_runtime;

mod window_util;
fn main() {
    if let Err(error) = app::run() {
        eprintln!("Failed to run GUI: {error}");
    }
}
