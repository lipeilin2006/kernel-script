use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use ks_core::protocol::{Frame, ReadProcessMemory, Request, Response, WireDecode, WireEncode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

const PIPE_NAME: &str = r"\\.\pipe\KernelScript";
const IPC_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_millis(5);
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

type ResponseMap = std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>>;

#[derive(Clone)]
pub struct IpcClient {
    sender: tokio::sync::mpsc::UnboundedSender<(u64, Vec<u8>)>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    responses: Arc<tokio::sync::Mutex<ResponseMap>>,
}

enum OwnedResponse {
    Memory(Vec<u8>),
    ProcessBase(u64),
    ProcessList(Vec<ProcessInfo>),
    ProcessId(u64),
    WriteComplete,
    Error(u32),
    ErrorDetail(String),
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
        let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Vec<u8>)>();
        let responses: Arc<tokio::sync::Mutex<ResponseMap>> =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let responses_clone = responses.clone();

        tokio::spawn(connection_task(req_rx, responses_clone));

        Self {
            sender: req_tx,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            responses,
        }
    }

    async fn send_request(&self, request: &Request<'_>) -> Result<OwnedResponse, String> {
        let mut output = vec![
            0u8;
            request
                .encoded_len()
                .map_err(|e| format!("encode length: {e:?}"))?
        ];
        request
            .encode(&mut output)
            .map_err(|e| format!("encode request: {e:?}"))?;

        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

        {
            let mut map = self.responses.lock().await;
            map.insert(id, resp_tx);
        }

        self.sender
            .send((id, output))
            .map_err(|_| "connection task closed".to_owned())?;

        let result = tokio::time::timeout(IPC_TIMEOUT, resp_rx)
            .await
            .map_err(|_| {
                tokio::spawn({
                    let responses = self.responses.clone();
                    async move {
                        let mut map = responses.lock().await;
                        map.remove(&id);
                    }
                });
                "IPC response timed out".to_owned()
            })?
            .map_err(|_| "connection task dropped sender".to_owned())?;

        let response_bytes = result?;
        let frame =
            Frame::parse(&response_bytes).map_err(|e| format!("parse response: {e:?}"))?;
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

    pub async fn write_memory(
        &self,
        pid: u64,
        address: u64,
        data: &[u8],
    ) -> Result<(), String> {
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

    pub async fn get_pid(&self, name: &str) -> Result<u64, String> {
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

    pub async fn get_process_base(&self, pid: u64) -> Result<u64, String> {
        match self
            .send_request(&Request::GetProcessBase { pid })
            .await?
        {
            OwnedResponse::ProcessBase(base) => Ok(base),
            OwnedResponse::Error(code) => Err(format!("service error: {code}")),
            OwnedResponse::ErrorDetail(detail) => Err(detail),
            _ => Err("unexpected response".into()),
        }
    }
}

impl Default for IpcClient {
    fn default() -> Self {
        Self::new()
    }
}

async fn connect() -> Result<NamedPipeClient, String> {
    let deadline = Instant::now() + IPC_TIMEOUT;
    loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(stream) => return Ok(stream),
            Err(_error) if Instant::now() < deadline => {
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(error) => return Err(format!("connect failed: {error}")),
        }
    }
}

async fn connection_task(
    mut req_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    responses: Arc<tokio::sync::Mutex<ResponseMap>>,
) {
    let mut stream = match connect().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("initial connection failed: {e}");
            return;
        }
    };

    while let Some((id, request_bytes)) = req_rx.recv().await {
        if let Err(e) = stream.write_all(&request_bytes).await {
            tracing::warn!("write failed: {e}, reconnecting");
            stream = match reconnect_or_die(&responses, id, &e).await {
                Some(s) => s,
                None => return,
            };
            // Retry the failed request once.
            if let Err(e) = stream.write_all(&request_bytes).await {
                tracing::error!("retry write also failed: {e}");
                respond_error(&responses, id, "write failed".into()).await;
                continue;
            }
        }

        let mut header = [0u8; ks_core::protocol::HEADER_SIZE];
        if let Err(e) = stream.read_exact(&mut header).await {
            tracing::warn!("header read failed: {e}, reconnecting");
            stream = match reconnect_or_die(&responses, id, &e).await {
                Some(s) => s,
                None => return,
            };
            respond_error(&responses, id, "read header failed".into()).await;
            continue;
        }

        let payload_len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        if payload_len > MAX_MESSAGE_SIZE - HEADER_SIZE {
            respond_error(&responses, id, "response too large".into()).await;
            continue;
        }

        let mut response = vec![0u8; HEADER_SIZE + payload_len];
        response[..HEADER_SIZE].copy_from_slice(&header);
        if let Err(e) = stream.read_exact(&mut response[HEADER_SIZE..]).await {
            tracing::warn!("payload read failed: {e}, reconnecting");
            stream = match reconnect_or_die(&responses, id, &e).await {
                Some(s) => s,
                None => return,
            };
            respond_error(&responses, id, "read payload failed".into()).await;
            continue;
        }

        let mut map = responses.lock().await;
        if let Some(tx) = map.remove(&id) {
            let _ = tx.send(Ok(response));
        }
    }
}

async fn reconnect_or_die(
    responses: &Arc<tokio::sync::Mutex<ResponseMap>>,
    failed_id: u64,
    _error: &std::io::Error,
) -> Option<NamedPipeClient> {
    respond_error(responses, failed_id, "connection lost".into()).await;
    match connect().await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!("reconnect failed: {e}");
            None
        }
    }
}

async fn respond_error(responses: &Arc<tokio::sync::Mutex<ResponseMap>>, id: u64, error: String) {
    let mut map = responses.lock().await;
    if let Some(tx) = map.remove(&id) {
        let _ = tx.send(Err(error));
    }
}

use ks_core::protocol::HEADER_SIZE;
