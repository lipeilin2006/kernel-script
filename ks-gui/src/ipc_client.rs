use std::time::Duration;
use std::time::Instant;

use ks_core::protocol::{Frame, ReadProcessMemory, Request, Response, WireDecode, WireEncode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

const PIPE_NAME: &str = r"\\.\pipe\KernelScript";
const IPC_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

#[derive(Clone, Default)]
pub struct IpcClient;

enum OwnedResponse {
    Memory(Vec<u8>),
    ProcessBase(u64),
    ProcessList(Vec<ProcessInfo>),
    ProcessId(u64),
    WriteComplete,
    Error(u32),
    ErrorDetail(String),
    WindowRect(i32, i32, i32, i32),
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u64,
    pub parent_pid: u64,
    pub thread_count: u32,
    pub name: String,
}

impl IpcClient {
    pub fn new() -> Self {
        Self
    }

    async fn send_request(&self, request: &Request<'_>) -> Result<OwnedResponse, String> {
        let deadline = Instant::now() + IPC_TIMEOUT;
        let mut stream: NamedPipeClient = loop {
            match ClientOptions::new().open(PIPE_NAME) {
                Ok(stream) => break stream,
                Err(_error) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(format!("connect failed: {error}")),
            }
        };

        let mut output = vec![
            0u8;
            request
                .encoded_len()
                .map_err(|e| format!("encode length: {e:?}"))?
        ];
        request
            .encode(&mut output)
            .map_err(|e| format!("encode request: {e:?}"))?;
        tokio::time::timeout(IPC_TIMEOUT, stream.write_all(&output))
            .await
            .map_err(|_| "IPC write timed out".to_owned())?
            .map_err(|error| format!("write request failed: {error}"))?;

        let mut header = [0u8; ks_core::protocol::HEADER_SIZE];
        tokio::time::timeout(IPC_TIMEOUT, stream.read_exact(&mut header))
            .await
            .map_err(|_| "IPC header read timed out".to_owned())?
            .map_err(|error| format!("read response header failed: {error}"))?;
        let payload_len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        if payload_len > MAX_MESSAGE_SIZE - ks_core::protocol::HEADER_SIZE {
            return Err("response too large".into());
        }
        let mut response = vec![0u8; ks_core::protocol::HEADER_SIZE + payload_len];
        response[..ks_core::protocol::HEADER_SIZE].copy_from_slice(&header);
        tokio::time::timeout(
            IPC_TIMEOUT,
            stream.read_exact(&mut response[ks_core::protocol::HEADER_SIZE..]),
        )
        .await
        .map_err(|_| "IPC payload read timed out".to_owned())?
        .map_err(|error| format!("read response payload failed: {error}"))?;

        let frame = Frame::parse(&response).map_err(|e| format!("parse response: {e:?}"))?;
        match Response::decode(frame.message_type, frame.payload)
            .map_err(|e| format!("decode response: {e:?}"))?
        {
            Response::Memory(data) => Ok(OwnedResponse::Memory(data.to_vec())),
            Response::ProcessBase(base) => Ok(OwnedResponse::ProcessBase(base)),
            Response::ProcessId(pid) => Ok(OwnedResponse::ProcessId(pid)),
            Response::ProcessList(list) => Ok(OwnedResponse::ProcessList(
                list.iter()
                    .map(|p| ProcessInfo {
                        pid: p.pid,
                        parent_pid: p.parent_pid,
                        thread_count: p.thread_count,
                        name: String::from_utf8_lossy(p.name).into_owned(),
                    })
                    .collect(),
            )),
            Response::WriteComplete => Ok(OwnedResponse::WriteComplete),
            Response::Error(code) => Ok(OwnedResponse::Error(code)),
            Response::ErrorDetail(detail) => Ok(OwnedResponse::ErrorDetail(
                String::from_utf8_lossy(detail).into_owned(),
            )),
            Response::WindowRect {
                x,
                y,
                width,
                height,
            } => Ok(OwnedResponse::WindowRect(x, y, width, height)),
        }
    }

    pub async fn list_processes(&self) -> Result<Vec<ProcessInfo>, String> {
        match self.send_request(&Request::FetchProcessList).await? {
            OwnedResponse::ProcessList(value) => Ok(value),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn read_memory(&self, pid: u64, address: u64, size: u64) -> Result<Vec<u8>, String> {
        match self
            .send_request(&Request::ReadProcessMemory(ReadProcessMemory {
                pid,
                target_address: address,
                size,
            }))
            .await?
        {
            OwnedResponse::Memory(value) => Ok(value),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn read_memory_mdl(
        &self,
        pid: u64,
        address: u64,
        size: u64,
    ) -> Result<Vec<u8>, String> {
        match self
            .send_request(&Request::ReadMemoryMdl(ReadProcessMemory {
                pid,
                target_address: address,
                size,
            }))
            .await?
        {
            OwnedResponse::Memory(value) => Ok(value),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn write_memory_mdl(
        &self,
        pid: u64,
        address: u64,
        data: &[u8],
    ) -> Result<(), String> {
        match self
            .send_request(&Request::WriteMemoryMdl {
                pid,
                target_address: address,
                data,
            })
            .await?
        {
            OwnedResponse::WriteComplete => Ok(()),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn read_memory_mdl_rva(
        &self,
        pid: u64,
        relative_address: u64,
        size: u64,
    ) -> Result<Vec<u8>, String> {
        match self
            .send_request(&Request::ReadMemoryMdlRva {
                pid,
                relative_address,
                size,
            })
            .await?
        {
            OwnedResponse::Memory(value) => Ok(value),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn write_memory_mdl_rva(
        &self,
        pid: u64,
        relative_address: u64,
        data: &[u8],
    ) -> Result<(), String> {
        match self
            .send_request(&Request::WriteMemoryMdlRva {
                pid,
                relative_address,
                data,
            })
            .await?
        {
            OwnedResponse::WriteComplete => Ok(()),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn get_process_base(&self, pid: u64) -> Result<u64, String> {
        match self.send_request(&Request::GetProcessBase { pid }).await? {
            OwnedResponse::ProcessBase(base) => Ok(base),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn read_memory_rva(
        &self,
        pid: u64,
        relative_address: u64,
        size: u64,
    ) -> Result<Vec<u8>, String> {
        match self
            .send_request(&Request::ReadMemoryRva {
                pid,
                relative_address,
                size,
            })
            .await?
        {
            OwnedResponse::Memory(value) => Ok(value),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn write_memory_rva(
        &self,
        pid: u64,
        relative_address: u64,
        data: &[u8],
    ) -> Result<(), String> {
        match self
            .send_request(&Request::WriteMemoryRva {
                pid,
                relative_address,
                data,
            })
            .await?
        {
            OwnedResponse::WriteComplete => Ok(()),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn write_memory(&self, pid: u64, address: u64, data: &[u8]) -> Result<(), String> {
        match self
            .send_request(&Request::WriteProcessMemory {
                pid,
                target_address: address,
                data,
            })
            .await?
        {
            OwnedResponse::WriteComplete => Ok(()),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn get_process_id(&self, name: &str) -> Result<u64, String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
            return Err("process name must be 1..255 bytes and contain no NUL".into());
        }
        match self
            .send_request(&Request::GetProcessId {
                name: name.as_bytes(),
            })
            .await?
        {
            OwnedResponse::ProcessId(pid) if pid != 0 => Ok(pid),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }

    pub async fn get_window_rect(
        &self,
        process_name: &str,
    ) -> Result<(i32, i32, i32, i32), String> {
        let name = process_name.trim();
        if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
            return Err("process name must be 1..255 bytes and contain no NUL".into());
        }
        match self
            .send_request(&Request::GetWindowRect {
                name: name.as_bytes(),
            })
            .await?
        {
            OwnedResponse::WindowRect(x, y, w, h) => Ok((x, y, w, h)),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }
}
