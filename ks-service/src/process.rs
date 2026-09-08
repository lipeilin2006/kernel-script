use std::fmt;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub thread_count: u32,
    pub name: String,
}

#[derive(Debug)]
pub enum ProcessError {
    Snapshot(u32),
    Enumeration(u32),
    InvalidName,
    NotFound,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(code) => write!(f, "process snapshot failed: {code}"),
            Self::Enumeration(code) => write!(f, "process enumeration failed: {code}"),
            Self::InvalidName => write!(f, "process name must be 1..255 bytes and contain no NUL"),
            Self::NotFound => write!(f, "process not found"),
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
