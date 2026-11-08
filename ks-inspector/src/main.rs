#![windows_subsystem = "windows"]

//! Standalone Kernel Script inspector.
//!
//! The memory tool used to be a second window inside `ks-gui`; it now has
//! its own executable and event loop. Only the driver lifecycle, the
//! synchronous round-trip facade, the config store and the UI strings are
//! shared with the overlay, through `ks_ui_common`.

mod inspector;

use std::error::Error;

use ks_ui_common::config_store::ConfigStore;
use ks_ui_common::driver::{self, DriverControl};

fn main() {
    // Lock Per-Monitor V2 before GLFW creates its first window; otherwise the
    // process silently stays DPI-unaware and content scale reads 1.0.
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    let result = run();
    unsafe {
        windows_sys::Win32::Media::timeEndPeriod(1);
    }
    if let Err(error) = result {
        eprintln!("Failed to run inspector: {error}");
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(
            std::fs::File::create("ks-inspector.log").unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("ks-inspector.log")
                    .unwrap()
            }),
        )
        .try_init();

    let exe_dir = std::env::current_exe()?
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let config = ConfigStore::shared(&exe_dir);
    // Restore the UI language before the window title is rendered.
    ks_ui_common::lang::restore(&config);

    let mut glfw =
        glfw::init(glfw::log_errors).map_err(|error| format!("glfw init failed: {error:?}"))?;
    // The driver probe/start runs on its own worker; creating it before the
    // window means the log bar shows the startup narrative from frame one.
    let mut driver = DriverControl::new();

    let mut inspector = inspector::Inspector::new(&mut glfw, config)
        .map_err(|error| format!("create inspector window: {error}"))?;

    loop {
        glfw.poll_events();
        driver.poll();
        if matches!(inspector.frame(&driver), inspector::FrameOutcome::Closed) {
            break;
        }
    }

    // Release the renderer, then run the shared exit path (waits for an
    // in-flight start, stops the driver when no other Kernel Script app is
    // running, drops the session).
    drop(inspector);
    driver::finish_on_exit();
    Ok(())
}
