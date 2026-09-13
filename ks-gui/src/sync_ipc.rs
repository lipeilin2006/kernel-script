use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use ks_core::protocol::{
    Frame, ReadProcessMemory, Request, Response, WireDecode, WireEncode, HEADER_SIZE,
    MAX_FRAME_SIZE,
};

struct SyncPipe {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

unsafe impl Send for SyncPipe {}

impl SyncPipe {
    fn connect() -> Result<Self, String> {
        let path: Vec<u16> = OsStr::new(r"\\.\pipe\KernelScript")
            .encode_wide()
            .chain(Some(0))
            .collect();

        let handle = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                path.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
                0,
                core::ptr::null_mut(),
                windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                0,
                core::ptr::null_mut(),
            )
        };

        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err("failed to open pipe".into());
        }

        Ok(Self { handle })
    }

    fn write_all(&self, data: &[u8]) -> Result<(), String> {
        let mut written = 0u32;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                self.handle,
                data.as_ptr(),
                data.len() as u32,
                &mut written,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err("write failed".into());
        }
        Ok(())
    }

    fn read_exact(&self, buf: &mut [u8]) -> Result<(), String> {
        let mut total = 0;
        while total < buf.len() {
            let mut read = 0u32;
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(
                    self.handle,
                    buf[total..].as_mut_ptr(),
                    (buf.len() - total) as u32,
                    &mut read,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                return Err("read failed".into());
            }
            total += read as usize;
        }
        Ok(())
    }

    fn send_request(&mut self, request: &Request) -> Result<Vec<u8>, String> {
        let total = request.encoded_len().map_err(|e| format!("{e:?}"))?;
        let mut frame = vec![0u8; total];
        request.encode(&mut frame).map_err(|e| format!("{e:?}"))?;
        self.write_all(&frame)?;

        let mut header = [0u8; HEADER_SIZE];
        self.read_exact(&mut header)?;
        let payload_len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        if payload_len > MAX_FRAME_SIZE - HEADER_SIZE {
            return Err("response too large".into());
        }
        let mut resp = vec![0u8; HEADER_SIZE + payload_len];
        resp[..HEADER_SIZE].copy_from_slice(&header);
        self.read_exact(&mut resp[HEADER_SIZE..])?;
        Ok(resp)
    }
}

impl Drop for SyncPipe {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

thread_local! {
    static SYNC_PIPE: RefCell<Option<SyncPipe>> = const { RefCell::new(None) };
}

fn ipc_send(request: &Request) -> Result<Vec<u8>, String> {
    SYNC_PIPE.with(|cell| {
        let mut borrow = cell.borrow_mut();

        if borrow.is_none() {
            *borrow = Some(SyncPipe::connect()?);
        }

        let result = borrow.as_mut().unwrap().send_request(request);

        match result {
            Ok(resp) => Ok(resp),
            Err(_) => {
                *borrow = Some(SyncPipe::connect()?);
                borrow.as_mut().unwrap().send_request(request)
            }
        }
    })
}

fn decode_ok(resp_bytes: &[u8]) -> Result<Response<'_>, String> {
    let frame = Frame::parse(resp_bytes).map_err(|e| format!("{e:?}"))?;
    Response::decode(frame.message_type, frame.payload).map_err(|e| format!("{e:?}"))
}

pub fn get_pid(name: &str) -> Result<u64, String> {
    let resp = ipc_send(&Request::GetProcessId {
        name: name.as_bytes(),
    })?;
    match decode_ok(&resp)? {
        Response::ProcessId(pid) => Ok(pid),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn get_process_base(pid: u64) -> Result<u64, String> {
    let resp = ipc_send(&Request::GetProcessBase { pid })?;
    match decode_ok(&resp)? {
        Response::ProcessBase(base) => Ok(base),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn read_i32(pid: u64, address: u64) -> Result<i32, String> {
    let resp = ipc_send(&Request::ReadProcessMemory(ReadProcessMemory {
        pid,
        target_address: address,
        size: 4,
    }))?;
    match decode_ok(&resp)? {
        Response::Memory(data) => {
            if data.len() >= 4 {
                Ok(i32::from_le_bytes(data[..4].try_into().unwrap()))
            } else {
                Err("short read".into())
            }
        }
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn read_bytes(pid: u64, address: u64, size: u64) -> Result<Vec<u8>, String> {
    let resp = ipc_send(&Request::ReadProcessMemory(ReadProcessMemory {
        pid,
        target_address: address,
        size,
    }))?;
    match decode_ok(&resp)? {
        Response::Memory(data) => Ok(data.to_vec()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn write_i32(pid: u64, address: u64, value: i32) -> Result<(), String> {
    let data = value.to_le_bytes();
    let resp = ipc_send(&Request::WriteProcessMemory {
        pid,
        target_address: address,
        data: &data,
    })?;
    match decode_ok(&resp)? {
        Response::WriteComplete => Ok(()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn write_bytes(pid: u64, address: u64, data: &[u8]) -> Result<(), String> {
    let resp = ipc_send(&Request::WriteProcessMemory {
        pid,
        target_address: address,
        data,
    })?;
    match decode_ok(&resp)? {
        Response::WriteComplete => Ok(()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn read_rva(pid: u64, relative_address: u64, size: u64) -> Result<Vec<u8>, String> {
    let resp = ipc_send(&Request::ReadMemoryRva {
        pid,
        relative_address,
        size,
    })?;
    match decode_ok(&resp)? {
        Response::Memory(data) => Ok(data.to_vec()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn write_rva(pid: u64, relative_address: u64, data: &[u8]) -> Result<(), String> {
    let resp = ipc_send(&Request::WriteMemoryRva {
        pid,
        relative_address,
        data,
    })?;
    match decode_ok(&resp)? {
        Response::WriteComplete => Ok(()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn read_mdl(pid: u64, address: u64, size: u64) -> Result<Vec<u8>, String> {
    let resp = ipc_send(&Request::ReadMemoryMdl(ReadProcessMemory {
        pid,
        target_address: address,
        size,
    }))?;
    match decode_ok(&resp)? {
        Response::Memory(data) => Ok(data.to_vec()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn write_mdl(pid: u64, address: u64, data: &[u8]) -> Result<(), String> {
    let resp = ipc_send(&Request::WriteMemoryMdl {
        pid,
        target_address: address,
        data,
    })?;
    match decode_ok(&resp)? {
        Response::WriteComplete => Ok(()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn read_mdl_rva(pid: u64, relative_address: u64, size: u64) -> Result<Vec<u8>, String> {
    let resp = ipc_send(&Request::ReadMemoryMdlRva {
        pid,
        relative_address,
        size,
    })?;
    match decode_ok(&resp)? {
        Response::Memory(data) => Ok(data.to_vec()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn write_mdl_rva(pid: u64, relative_address: u64, data: &[u8]) -> Result<(), String> {
    let resp = ipc_send(&Request::WriteMemoryMdlRva {
        pid,
        relative_address,
        data,
    })?;
    match decode_ok(&resp)? {
        Response::WriteComplete => Ok(()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn batch_read(pid: u64, size: u32, addresses: &[u64]) -> Result<Vec<u8>, String> {
    let mut addrs_raw = Vec::with_capacity(addresses.len() * 8);
    for &addr in addresses {
        addrs_raw.extend_from_slice(&addr.to_le_bytes());
    }
    let resp = ipc_send(&Request::BatchReadMemory {
        pid,
        size,
        addresses: &addrs_raw,
    })?;
    match decode_ok(&resp)? {
        Response::BatchReadMemory(data) => Ok(data.to_vec()),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}

pub fn traverse_pointer_chain(pid: u64, base: u64, offsets: &[u64]) -> Result<u64, String> {
    let mut offsets_raw = Vec::with_capacity(offsets.len() * 8);
    for &off in offsets {
        offsets_raw.extend_from_slice(&off.to_le_bytes());
    }
    let resp = ipc_send(&Request::TraversePointerChain {
        pid,
        base,
        offsets: &offsets_raw,
    })?;
    match decode_ok(&resp)? {
        Response::PointerChainResult(addr) => Ok(addr),
        Response::Error(code) => Err(format!("service error: {code}")),
        _ => Err("unexpected response".into()),
    }
}
