#![windows_subsystem = "windows"]

pub(crate) use ks_ui_common::{config_store, driver, lang, sync_ipc};

mod app;
mod lua_runtime;
mod overlay;
mod window_util;

fn main() {
    // Per-Monitor V2 must be locked in before the first window exists: the
    // awareness context is fixed once a window is created, and a later
    // GLFW call silently fails (the process stays DPI-unaware, content
    // scale reads 1.0 and every overlay coordinate drifts on scaled
    // monitors). This mirrors the ks-td2 overlay host.
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    // The overlay repaints at 100 Hz; the default 15.6 ms timer resolution
    // would floor wait_events_timeout and make that cadence unreachable.
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    let result = app::run();
    unsafe {
        windows_sys::Win32::Media::timeEndPeriod(1);
    }
    if let Err(error) = result {
        eprintln!("Failed to run GUI: {error}");
    }
}
