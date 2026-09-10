use std::ffi::c_void;
use std::fmt;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowThreadProcessId, IsWindowVisible, GWL_STYLE, WS_VISIBLE,
};

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub thread_count: u32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug)]
pub enum ProcessError {
    Snapshot(u32),
    Enumeration(u32),
    InvalidName,
    NotFound,
    WindowNotFound,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(code) => write!(f, "process snapshot failed: {code}"),
            Self::Enumeration(code) => write!(f, "process enumeration failed: {code}"),
            Self::InvalidName => write!(f, "process name must be 1..255 bytes and contain no NUL"),
            Self::NotFound => write!(f, "process not found"),
            Self::WindowNotFound => write!(f, "window not found for process"),
        }
    }
}

impl std::error::Error for ProcessError {}

pub fn list() -> Result<Vec<ProcessInfo>, ProcessError> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(ProcessError::Snapshot(last_error()));
    }

    let mut entry = PROCESSENTRY32W {
        dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    let first = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    if !first {
        let error = last_error();
        unsafe { CloseHandle(snapshot) };
        return Err(ProcessError::Enumeration(error));
    }

    loop {
        processes.push(ProcessInfo {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            thread_count: entry.cntThreads,
            name: utf16_name(&entry.szExeFile),
        });
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break;
        }
    }
    unsafe { CloseHandle(snapshot) };
    Ok(processes)
}

pub fn find_pid(name: &str) -> Result<u64, ProcessError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
        return Err(ProcessError::InvalidName);
    }
    list()?
        .into_iter()
        .find(|process| process.name.eq_ignore_ascii_case(name))
        .map(|process| process.pid as u64)
        .ok_or(ProcessError::NotFound)
}

fn utf16_name(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

fn last_error() -> u32 {
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

struct EnumCtx {
    target_pid: u32,
    hwnds: Vec<HWND>,
}

pub fn get_window_rects(process_name: &str) -> Result<Vec<WindowRect>, ProcessError> {
    let pid = find_pid(process_name)?;
    let mut ctx = EnumCtx {
        target_pid: pid as u32,
        hwnds: Vec::new(),
    };
    unsafe {
        EnumWindows(Some(enum_windows_callback), &mut ctx as *mut EnumCtx as isize);
    }
    if ctx.hwnds.is_empty() {
        return Err(ProcessError::WindowNotFound);
    }
    let mut rects = Vec::with_capacity(ctx.hwnds.len());
    for hwnd in ctx.hwnds {
        if let Some(rect) = unsafe { get_accurate_rect(hwnd) } {
            rects.push(rect);
        }
    }
    if rects.is_empty() {
        return Err(ProcessError::WindowNotFound);
    }
    Ok(rects)
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
