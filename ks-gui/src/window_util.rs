use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowThreadProcessId, IsWindowVisible, GWL_STYLE, WS_VISIBLE,
};

#[derive(Clone, Debug)]
pub struct WindowRect {
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

unsafe fn get_accurate_rect(hwnd: HWND) -> Option<WindowRect> {
    let mut rect: RECT = std::mem::zeroed();
    let hr = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        &mut rect as *mut RECT as *mut c_void,
        size_of::<RECT>() as u32,
    );
    if hr != 0 {
        if windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) == 0 {
            return None;
        }
    }
    Some(WindowRect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}
