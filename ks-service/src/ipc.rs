use std::sync::Arc;
use std::{ffi::c_void, ptr};

use bytes::{Bytes, BytesMut};
use ks_core::protocol::{
    Frame, ProcessList, ProtocolError, HEADER_SIZE, MAGIC, MAX_DRIVER_TRANSFER_SIZE, MAX_FRAME_SIZE,
};
use ks_core::protocol::{Request, Response, WireDecode, WireEncode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, Mutex};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

use crate::driver_comm::DriverComm;
use crate::process;

const PIPE_NAME: &str = r"\\.\pipe\KernelScript";
const PIPE_SDDL: &[u16] = &[
    'D' as u16, ':' as u16, 'P' as u16, '(' as u16, 'A' as u16, ';' as u16, ';' as u16, 'G' as u16,
    'A' as u16, ';' as u16, ';' as u16, ';' as u16, 'S' as u16, 'Y' as u16, ')' as u16, '(' as u16,
    'A' as u16, ';' as u16, ';' as u16, 'G' as u16, 'R' as u16, 'G' as u16, 'W' as u16, ';' as u16,
    ';' as u16, ';' as u16, 'I' as u16, 'U' as u16, ')' as u16, 0,
];
pub type SharedDriver = Arc<Mutex<DriverComm>>;

// split_to transfers complete frames without copying their payload.
struct BytesFrameDecoder {
    buffer: BytesMut,
}

impl BytesFrameDecoder {
    fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(16 * 1024),
        }
    }

    fn push(&mut self, input: &[u8]) -> Result<(), ProtocolError> {
        if self
            .buffer
            .len()
            .checked_add(input.len())
            .ok_or(ProtocolError::TooLarge)?
            > MAX_FRAME_SIZE
        {
            return Err(ProtocolError::TooLarge);
        }
        self.buffer.extend_from_slice(input);
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Bytes>, ProtocolError> {
        if self.buffer.len() < HEADER_SIZE {
            return Ok(None);
        }
        if self.buffer[..4] != MAGIC.to_le_bytes() {
            return Err(ProtocolError::InvalidMagic);
        }
        let length = u32::from_le_bytes(
            self.buffer[6..10]
                .try_into()
                .map_err(|_| ProtocolError::InvalidLength)?,
        ) as usize;
        if length > MAX_FRAME_SIZE - HEADER_SIZE {
            return Err(ProtocolError::TooLarge);
        }
        let total = HEADER_SIZE
            .checked_add(length)
            .ok_or(ProtocolError::InvalidLength)?;
        if self.buffer.len() < total {
            return Ok(None);
        }
        Ok(Some(self.buffer.split_to(total).freeze()))
    }
}

pub async fn start_server(
    driver: SharedDriver,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut server = create_pipe_server()?;
    tracing::info!(pipe = PIPE_NAME, "IPC named pipe server listening");
    loop {
        tokio::select! {
            result = server.connect() => {
                result?;
                tracing::info!(pipe = PIPE_NAME, "IPC named pipe client connected");
                let connected = server;
                server = create_pipe_server()?;
                let driver = Arc::clone(&driver);
                let stop = shutdown.resubscribe();
                tokio::spawn(async move { handle_client(connected, driver, stop).await; });
            }
            _ = shutdown.recv() => break,
        }
    }
    Ok(())
}

fn create_pipe_server() -> Result<NamedPipeServer, Box<dyn std::error::Error + Send + Sync>> {
    let mut descriptor: *mut c_void = ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PIPE_SDDL.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let result = unsafe {
        ServerOptions::new()
            .reject_remote_clients(true)
            .max_instances(4)
            .create_with_security_attributes_raw(
                PIPE_NAME,
                &mut attributes as *mut _ as *mut c_void,
            )
    };
    unsafe {
        LocalFree(descriptor);
    }
    Ok(result?)
}

async fn handle_client(
    mut socket: NamedPipeServer,
    driver: SharedDriver,
    mut shutdown: broadcast::Receiver<()>,
) {
    tracing::info!("handle_client: new client connected");
    let mut decoder = BytesFrameDecoder::new();
    let mut read_buffer = [0u8; 16 * 1024];
    loop {
        tokio::select! {
            result = socket.read(&mut read_buffer) => {
                let count = match result { Ok(0) | Err(_) => return, Ok(value) => value };
                if let Err(error) = decoder.push(&read_buffer[..count]) {
                    tracing::warn!(?error, "invalid IPC input");
                    return;
                }
                loop {
                    let frame_bytes = match decoder.next() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(?error, "invalid IPC frame");
                            return;
                        }
                    };
                    let frame = match Frame::parse(&frame_bytes) {
                        Ok(frame) => frame,
                        Err(error) => {
                            tracing::warn!(?error, "invalid IPC frame header");
                            return;
                        }
                    };
                    let output = dispatch(frame.message_type, frame.payload, &driver).await;
                    tracing::info!(response_len = output.len(), "sending response");
                    if socket.write_all(&output).await.is_err() {
                        tracing::warn!("client write failed, disconnecting");
                        return;
                    }
                }
            }
            _ = shutdown.recv() => return,
        }
    }
}

async fn dispatch(
    message_type: ks_core::protocol::MessageType,
    payload: &[u8],
    driver: &SharedDriver,
) -> Vec<u8> {
    tracing::info!(?message_type, payload_len = payload.len(), "dispatching request");
    let request = match Request::decode(message_type, payload) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(?error, "request decode failed");
            return encode_response(Response::Error(1));
        }
    };
    if let Request::ReadProcessMemory(value) | Request::ReadMemoryMdl(value) = &request {
        if value.size == 0
            || usize::try_from(value.size).map_or(true, |size| size > MAX_DRIVER_TRANSFER_SIZE)
        {
            return encode_response(Response::Error(6));
        }
    }
    // DeviceIoControl is synchronous. Move it to the blocking pool so one
    // slow driver call cannot occupy a Tokio worker thread.
    let request = match request {
        Request::FetchProcessList => OwnedRequest::GetProcessList,
        Request::GetProcessId { name } => {
            let Ok(name) = core::str::from_utf8(name) else {
                return encode_response(Response::Error(1));
            };
            if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
                return encode_response(Response::Error(1));
            }
            OwnedRequest::GetProcessId {
                name: name.to_owned(),
            }
        }
        Request::GetProcessBase { pid } => OwnedRequest::GetProcessBase { pid },
        Request::ReadMemoryRva {
            pid,
            relative_address,
            size,
        } => {
            if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
                return encode_response(Response::Error(6));
            }
            OwnedRequest::ReadRva {
                pid,
                relative_address,
                size,
            }
        }
        Request::WriteMemoryRva {
            pid,
            relative_address,
            data,
        } => {
            if data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
                return encode_response(Response::Error(6));
            }
            OwnedRequest::WriteRva {
                pid,
                relative_address,
                data: data.to_vec(),
            }
        }
        Request::ReadProcessMemory(value) => OwnedRequest::Read(value),
        Request::ReadMemoryMdl(value) => OwnedRequest::ReadMdl(value),
        Request::WriteMemoryMdl {
            pid,
            target_address,
            data,
        } => {
            if data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
                return encode_response(Response::Error(6));
            }
            OwnedRequest::WriteMdl {
                pid,
                target_address,
                data: data.to_vec(),
            }
        }
        Request::ReadMemoryMdlRva {
            pid,
            relative_address,
            size,
        } => {
            if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
                return encode_response(Response::Error(6));
            }
            OwnedRequest::ReadMdlRva {
                pid,
                relative_address,
                size,
            }
        }
        Request::WriteMemoryMdlRva {
            pid,
            relative_address,
            data,
        } => {
            if data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
                return encode_response(Response::Error(6));
            }
            OwnedRequest::WriteMdlRva {
                pid,
                relative_address,
                data: data.to_vec(),
            }
        }
        Request::WriteProcessMemory {
            pid,
            target_address,
            data,
        } => {
            if data.len() > MAX_DRIVER_TRANSFER_SIZE {
                return encode_response(Response::Error(6));
            }
            OwnedRequest::Write {
                pid,
                target_address,
                data: data.to_vec(),
            }
        }
        Request::GetWindowRect { name } => {
            let Ok(name) = core::str::from_utf8(name) else {
                return encode_response(Response::Error(1));
            };
            if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
                return encode_response(Response::Error(1));
            }
            OwnedRequest::GetWindowRect {
                name: name.to_owned(),
            }
        }
    };
    let driver = Arc::clone(driver);
    tokio::task::spawn_blocking(move || {
        match request {
            OwnedRequest::GetProcessList => match process::list() {
                Ok(processes) => encode_process_list(&processes),
                Err(error) => {
                    tracing::error!(%error, "process list query failed");
                    encode_response(Response::Error(9))
                }
            },
            OwnedRequest::GetProcessId { name } => match process::find_pid(&name) {
                Ok(pid) => encode_response(Response::ProcessId(pid)),
                Err(error) => {
                    tracing::warn!(process = %name, %error, "process lookup failed");
                    encode_response(Response::Error(8))
                }
            },
            OwnedRequest::GetProcessBase { pid } => match driver
                .blocking_lock()
                .get_process_base(pid)
            {
                Ok(base) => encode_response(Response::ProcessBase(base)),
                Err(error) => {
                    tracing::warn!(pid, %error, "process base lookup failed");
                    encode_response(Response::Error(10))
                }
            },
            OwnedRequest::ReadRva { pid, relative_address, size } => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.read_memory_rva(pid, relative_address, size) {
                        Ok(data) => encode_response(Response::Memory(&data)),
                        Err(error) => {
                            tracing::error!(pid, relative_address, size, %error, "driver RVA read failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::WriteRva { pid, relative_address, data } => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.write_memory_rva(pid, relative_address, &data) {
                        Ok(()) => encode_response(Response::WriteComplete),
                        Err(error) => {
                            tracing::error!(pid, relative_address, size = data.len(), %error, "driver RVA write failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::Read(value) => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.read_memory(value.pid, value.target_address, value.size) {
                        Ok(data) => encode_response(Response::Memory(&data)),
                        Err(error) => {
                            tracing::error!(pid = value.pid, address = format_args!("0x{:X}", value.target_address), size = value.size, %error, "driver read failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::ReadMdl(value) => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.read_memory_mdl(value.pid, value.target_address, value.size) {
                        Ok(data) => encode_response(Response::Memory(&data)),
                        Err(error) => {
                            tracing::error!(pid = value.pid, address = format_args!("0x{:X}", value.target_address), size = value.size, %error, "driver MDL read failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::ReadMdlRva { pid, relative_address, size } => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.read_memory_mdl_rva(pid, relative_address, size) {
                        Ok(data) => encode_response(Response::Memory(&data)),
                        Err(error) => {
                            tracing::error!(pid, relative_address, size, %error, "driver MDL RVA read failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::WriteMdl { pid, target_address, data } => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.write_memory_mdl(pid, target_address, &data) {
                        Ok(()) => encode_response(Response::WriteComplete),
                        Err(error) => {
                            tracing::error!(pid, address = format_args!("0x{:X}", target_address), size = data.len(), %error, "driver MDL write failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::WriteMdlRva { pid, relative_address, data } => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.write_memory_mdl_rva(pid, relative_address, &data) {
                        Ok(()) => encode_response(Response::WriteComplete),
                        Err(error) => {
                            tracing::error!(pid, relative_address, size = data.len(), %error, "driver MDL RVA write failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::Write { pid, target_address, data } => {
                let mut driver = driver.blocking_lock();
                if !driver.is_connected() && driver.try_reconnect().is_err() {
                    encode_response(Response::Error(2))
                } else {
                    match driver.write_memory(pid, target_address, &data) {
                        Ok(()) => encode_response(Response::WriteComplete),
                        Err(error) => {
                            tracing::error!(pid, address = format_args!("0x{:X}", target_address), size = data.len(), %error, "driver write failed");
                            encode_error_detail(&error)
                        }
                    }
                }
            }
            OwnedRequest::GetWindowRect { name } => match process::get_window_rect(&name) {
                Ok(rect) => encode_response(Response::WindowRect {
                    x: rect.0,
                    y: rect.1,
                    width: rect.2,
                    height: rect.3,
                }),
                Err(error) => {
                    tracing::warn!(window = %name, %error, "window rect lookup failed");
                    encode_response(Response::Error(11))
                }
            },
        }
    })
    .await
    .unwrap_or_else(|error| {
        tracing::error!(%error, "driver blocking task failed");
        encode_response(Response::Error(7))
    })
}

fn encode_process_list(processes: &[process::ProcessInfo]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(
        4 + processes.len() * (ks_core::protocol::PROCESS_RECORD_HEADER_SIZE + 15),
    );
    payload.extend_from_slice(&(processes.len() as u32).to_le_bytes());
    for process in processes {
        let name = process.name.as_bytes();
        let name = &name[..name.len().min(ks_core::protocol::MAX_PROCESS_NAME_BYTES)];
        payload.extend_from_slice(&(process.pid as u64).to_le_bytes());
        payload.extend_from_slice(&(process.parent_pid as u64).to_le_bytes());
        payload.extend_from_slice(&process.thread_count.to_le_bytes());
        payload.extend_from_slice(&[0u8; 4]);
        payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
        payload.extend_from_slice(name);
    }
    encode_response(Response::ProcessList(
        ProcessList::new(&payload).expect("encoded process list is valid"),
    ))
}

enum OwnedRequest {
    GetProcessList,
    GetProcessId {
        name: String,
    },
    GetProcessBase {
        pid: u64,
    },
    ReadRva {
        pid: u64,
        relative_address: u64,
        size: u64,
    },
    WriteRva {
        pid: u64,
        relative_address: u64,
        data: Vec<u8>,
    },
    ReadMdl(ks_core::protocol::ReadProcessMemory),
    WriteMdl {
        pid: u64,
        target_address: u64,
        data: Vec<u8>,
    },
    ReadMdlRva {
        pid: u64,
        relative_address: u64,
        size: u64,
    },
    WriteMdlRva {
        pid: u64,
        relative_address: u64,
        data: Vec<u8>,
    },
    Read(ks_core::protocol::ReadProcessMemory),
    Write {
        pid: u64,
        target_address: u64,
        data: Vec<u8>,
    },
    GetWindowRect {
        name: String,
    },
}

fn encode_response(response: Response<'_>) -> Vec<u8> {
    let Ok(size) = response.encoded_len() else {
        return Vec::new();
    };
    let mut bytes = vec![0u8; size];
    if response.encode(&mut bytes).is_err() {
        return Vec::new();
    }
    bytes
}

fn encode_error_detail(error: &crate::driver_comm::DriverError) -> Vec<u8> {
    encode_response(Response::ErrorDetail(format!("{error}").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ks_core::protocol::{Request, WireEncode};

    #[test]
    fn bytes_decoder_handles_partial_and_sticky_frames_without_payload_copy() {
        let mut encoded = [0u8; HEADER_SIZE];
        Request::FetchProcessList.encode(&mut encoded).unwrap();

        let mut decoder = BytesFrameDecoder::new();
        decoder.push(&encoded[..3]).unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder.push(&encoded[3..]).unwrap();
        let frame = decoder.next().unwrap().unwrap();
        assert_eq!(frame.len(), HEADER_SIZE);
        assert_eq!(
            Frame::parse(&frame).unwrap().message_type,
            ks_core::protocol::MessageType::FetchProcessList
        );
        assert!(decoder.next().unwrap().is_none());
    }
}
