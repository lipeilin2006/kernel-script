mod app;
mod ipc_client;
mod lua_runtime;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("Failed to run GUI: {error}");
    }
}
