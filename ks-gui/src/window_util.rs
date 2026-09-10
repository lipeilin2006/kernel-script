use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    GWL_STYLE, WS_VISIBLE,
};

struct EnumData {
    target_pid: u32,
    result_hwnd: Option<HWND>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let data = &mut *(lparam as *mut EnumData);

    let mut window_pid = 0;
    GetWindowThreadProcessId(hwnd, &mut window_pid);

    if window_pid == data.target_pid && IsWindowVisible(hwnd) != 0 {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if (style & WS_VISIBLE) != 0 {
            data.result_hwnd = Some(hwnd);
            return 0;
        }
    }
    1
}

pub fn get_window_rect_by_pid(pid: u32) -> Option<(i32, i32, i32, i32)> {
    let mut data = EnumData {
        target_pid: pid,
        result_hwnd: None,
    };

    unsafe {
        EnumWindows(Some(enum_proc), &mut data as *mut EnumData as LPARAM);
    }

    let hwnd = data.result_hwnd?;

    let mut rect: RECT = unsafe { std::mem::zeroed() };

    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
    };

    if hr != 0 {
        if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            return None;
        }
    }

    Some((rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top))
}
