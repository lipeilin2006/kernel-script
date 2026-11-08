use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowLongW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_STYLE, WS_VISIBLE,
};

static CONTENT_SCALE: AtomicU32 = AtomicU32::new(100);

/// Overlay content scale (physical pixels per egui logical point), refreshed
/// from the overlay window every frame. Used as the fallback when a target
/// window's monitor DPI cannot be queried.
pub fn set_content_scale(scale: f32) {
    CONTENT_SCALE.store((scale * 100.0).max(1.0) as u32, Ordering::Relaxed);
}

pub fn content_scale() -> f32 {
    CONTENT_SCALE.load(Ordering::Relaxed) as f32 / 100.0
}

#[derive(Clone, Debug)]
pub struct WindowRect {
    /// Logical (DWM physical divided by the target monitor's effective DPI).
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

struct EnumCtx {
    target_pid: u32,
    hwnds: Vec<HWND>,
}

pub fn get_window_rects_by_pid(pid: u32) -> Vec<WindowRect> {
    let mut ctx = EnumCtx {
        target_pid: pid,
        hwnds: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(enum_windows_callback),
            &mut ctx as *mut EnumCtx as isize,
        );
    }
    let mut rects = Vec::with_capacity(ctx.hwnds.len());
    for hwnd in ctx.hwnds {
        if let Some(rect) = unsafe { get_accurate_rect(hwnd) } {
            rects.push(rect);
        }
    }
    rects
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: isize) -> i32 {
    let ctx = &mut *(lparam as *mut EnumCtx);
    let mut window_pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut window_pid);
    if window_pid == ctx.target_pid && IsWindowVisible(hwnd) != 0 {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if (style & WS_VISIBLE) != 0 {
            ctx.hwnds.push(hwnd);
        }
    }
    1
}

/// This window's monitor effective DPI scale (GetDpiForMonitor); 0 when it
/// cannot be queried. Measurement and overlay synchronization both use the
/// game window's own monitor scale, so a per-monitor DPI layout stays
/// consistent even when the overlay lives on a differently scaled screen.
pub fn monitor_dpi_for_hwnd(hwnd: *mut c_void) -> f32 {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let monitor = MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return 0.0;
        }
        let (mut dx, mut dy) = (0u32, 0u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) == 0 && dx > 0 {
            dx as f32 / 96.0
        } else {
            0.0
        }
    }
}

/// Physical screen cursor position (GetCursorPos; negative multi-monitor
/// origins are returned as-is).
///
/// The region passthrough check compares it with the settings-panel rect in
/// the same physical space: a click-through window receives no mouse-move
/// messages, so polling the OS cursor is the only way back to interactive.
pub fn cursor_pos() -> (i32, i32) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut point = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// Client-area rect (logical screen coordinates), fallback DWM extended
/// frame bounds.
///
/// The client area excludes the title bar and borders, so Lua gets the
/// content position the game draws into; the DWM frame only covers
/// borderless-style windows anyway, while a windowed window's caption would
/// otherwise shift every consumer by the caption height. Windows APIs return
/// physical pixels; the target monitor's effective DPI converts them to the
/// logical units `draw.*` uses.
unsafe fn get_accurate_rect(hwnd: HWND) -> Option<WindowRect> {
    use windows_sys::Win32::Foundation::POINT;
    let dpi = monitor_dpi_for_hwnd(hwnd);
    let scale = if dpi > 0.0 { dpi } else { content_scale() };
    let mut origin: POINT = POINT { x: 0, y: 0 };
    let mut client: RECT = std::mem::zeroed();
    if ClientToScreen(hwnd, &mut origin as *mut POINT) != 0
        && GetClientRect(hwnd, &mut client as *mut RECT) != 0
        && client.right > client.left
        && client.bottom > client.top
    {
        return Some(WindowRect {
            x: ((origin.x as f32) / scale).round() as i32,
            y: ((origin.y as f32) / scale).round() as i32,
            width: (((client.right - client.left) as f32) / scale).round() as i32,
            height: ((client.bottom - client.top) as f32 / scale).round() as i32,
        });
    }
    let mut rect: RECT = std::mem::zeroed();
    let hr = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        &mut rect as *mut RECT as *mut c_void,
        size_of::<RECT>() as u32,
    );
    if hr != 0 && windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) == 0 {
        return None;
    }
    Some(WindowRect {
        x: ((rect.left as f32) / scale).round() as i32,
        y: ((rect.top as f32) / scale).round() as i32,
        width: (((rect.right - rect.left) as f32) / scale).round() as i32,
        height: (((rect.bottom - rect.top) as f32) / scale).round() as i32,
    })
}
