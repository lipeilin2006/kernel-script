//! Wire protocol shared by the driver, service and clients.
//! All integers are little-endian and no Rust layout is exposed on the wire.

#[path = "message_type.rs"]
mod message_type;
pub use message_type::MessageType;

// Existing Windows driver ABI. These are intentionally kept separate from the
// length-prefixed IPC messages below.
// CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS).
// The device type must match the type passed to IoCreateDevice.
pub const IOCTL_READ_MEMORY: u32 = 0x0022_2004;
pub const IOCTL_WRITE_MEMORY: u32 = 0x0022_2008;
pub const IOCTL_PING: u32 = 0x0022_2010;
pub const IOCTL_GET_PROCESS_BASE: u32 = 0x0022_2018;
pub const IOCTL_READ_MEMORY_RVA: u32 = 0x0022_201C;
pub const IOCTL_WRITE_MEMORY_RVA: u32 = 0x0022_2020;
pub const IOCTL_READ_MEMORY_MDL: u32 = 0x0022_2024;
pub const IOCTL_WRITE_MEMORY_MDL: u32 = 0x0022_2028;
pub const IOCTL_READ_MEMORY_MDL_RVA: u32 = 0x0022_202C;
pub const IOCTL_WRITE_MEMORY_MDL_RVA: u32 = 0x0022_2030;
pub const IOCTL_BATCH_READ_MEMORY: u32 = 0x0022_2034;
pub const IOCTL_TRAVERSE_POINTER_CHAIN: u32 = 0x0022_2038;
pub const IOCTL_WRITE_MEMORY_BATCH: u32 = 0x0022_203C;
pub const MAX_BATCH_WRITE_ENTRIES: usize = 64;
pub const MAX_MEMORY_LOCK_SIZE: usize = 4096;
pub const MAX_MEMORY_LOCKS: usize = 64;

/// `IOCTL_WRITE_MEMORY_BATCH` input wire layout (little-endian):
/// `u32 count`, `u32 reserved`, then `count` entries of
/// `u64 process_id, u64 address, u32 size, u32 _pad, size bytes of data`.
/// Output layout: `u32 count`, then `count` little-endian `u32` NTSTATUS
/// values, one per entry in input order. A malformed boundary aborts the
/// walk; all unprocessed entries report `STATUS_INVALID_PARAMETER`.

/// Validates a pipe-level batch-write entries payload (`u64 address, u32
/// size, u32 pad, data[size]` per entry) and returns the entry count. The
/// leading `u64 pid` is not part of `entries`.
#[cfg(feature = "alloc")]
fn batch_write_entry_count(entries: &[u8]) -> Option<usize> {
    if entries.len() < 16 {
        return None;
    }
    let count = u32::from_le_bytes(entries[8..12].try_into().ok()?) as usize;
    if count == 0 || count > MAX_BATCH_WRITE_ENTRIES {
        return None;
    }
    let mut offset = 16usize;
    for _ in 0..count {
        if entries.len() - offset < 16 {
            return None;
        }
        let size = u32::from_le_bytes(entries[offset + 8..offset + 12].try_into().ok()?) as usize;
        if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE {
            return None;
        }
        offset = offset.checked_add(16)?.checked_add(size)?;
    }
    if offset != entries.len() {
        return None;
    }
    Some(count)
}

#[repr(C)]
pub struct MemoryReadRequest {
    pub process_id: u64,
    pub address: u64,
    pub size: u64,
}

#[repr(C)]
pub struct MemoryWriteRequest {
    pub process_id: u64,
    pub address: u64,
    pub size: u64,
    pub data: [u8; 4096],
}

#[repr(C)]
pub struct ProcessBaseRequest {
    pub process_id: u64,
}

#[repr(C)]
pub struct MemoryRvaReadRequest {
    pub process_id: u64,
    pub relative_address: u64,
    pub size: u64,
}

#[repr(C)]
pub struct MemoryRvaWriteRequest {
    pub process_id: u64,
    pub relative_address: u64,
    pub size: u64,
    pub data: [u8; 4096],
}

#[repr(C)]
pub struct MemoryResponse {
    pub success: bool,
    pub data: [u8; 4096],
    pub error_code: u32,
}

pub const MAGIC: u32 = 0x4B53_4352; // "KSCR"
pub const HEADER_SIZE: usize = 10;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
// The Windows driver ABI uses a fixed 4096-byte data area.
pub const MAX_DRIVER_TRANSFER_SIZE: usize = 4096;
pub const READ_RESPONSE_HEADER_SIZE: usize = 8;
pub const READ_RESPONSE_SIZE: usize = READ_RESPONSE_HEADER_SIZE + MAX_DRIVER_TRANSFER_SIZE + 4;
pub const MAX_PROCESS_LIST_ENTRIES: usize = 4096;
pub const MAX_PROCESS_NAME_BYTES: usize = 260;
pub const PROCESS_RECORD_HEADER_SIZE: usize = 26;
pub const MAX_PROCESS_LIST_SIZE: usize =
    4 + MAX_PROCESS_LIST_ENTRIES * (PROCESS_RECORD_HEADER_SIZE + MAX_PROCESS_NAME_BYTES);
pub const MAX_WRITE_SIZE: usize = MAX_FRAME_SIZE - HEADER_SIZE - 20;
pub const MAX_BATCH_ENTRIES: usize = 256;
pub const BATCH_READ_ENTRY_WIRE_SIZE: usize = 12; // address:8 + size:4

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    BufferTooSmall,
    InvalidMagic,
    InvalidLength,
    UnknownMessageType,
    InvalidPayload,
    TooLarge,
}

pub trait WireEncode {
    fn message_type(&self) -> MessageType;
    fn encoded_len(&self) -> Result<usize, ProtocolError>;
    fn encode(&self, output: &mut [u8]) -> Result<usize, ProtocolError>;
}

pub trait WireDecode<'a>: Sized {
    fn decode(message_type: MessageType, payload: &'a [u8]) -> Result<Self, ProtocolError>;
}

#[path = "frame.rs"]
mod frame;
pub use frame::Frame;
#[cfg(feature = "alloc")]
pub use frame::FrameDecoder;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadProcessMemory {
    pub pid: u64,
    pub target_address: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchReadEntry {
    pub address: u64,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessRecord<'a> {
    pub pid: u64,
    pub parent_pid: u64,
    pub thread_count: u32,
    pub name: &'a [u8],
}

#[cfg(feature = "alloc")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request<'a> {
    FetchProcessList,
    GetProcessId {
        name: &'a [u8],
    },
    GetProcessBase {
        pid: u64,
    },
    ReadMemoryRva {
        pid: u64,
        relative_address: u64,
        size: u64,
    },
    WriteMemoryRva {
        pid: u64,
        relative_address: u64,
        data: &'a [u8],
    },
    ReadMemoryMdl(ReadProcessMemory),
    WriteMemoryMdl {
        pid: u64,
        target_address: u64,
        data: &'a [u8],
    },
    ReadMemoryMdlRva {
        pid: u64,
        relative_address: u64,
        size: u64,
    },
    WriteMemoryMdlRva {
        pid: u64,
        relative_address: u64,
        data: &'a [u8],
    },
    ReadProcessMemory(ReadProcessMemory),
    WriteProcessMemory {
        pid: u64,
        target_address: u64,
        data: &'a [u8],
    },
    BatchReadMemory {
        pid: u64,
        size: u32,
        addresses: &'a [u8],
    },
    /// Entries payload: per entry `u64 address, u32 size, u32 _pad,
    /// data[size]`, all little-endian. All entries share `pid`.
    BatchWrite {
        pid: u64,
        entries: &'a [u8],
    },
    TraversePointerChain {
        pid: u64,
        base: u64,
        offsets: &'a [u8],
    },
    LockMemory {
        pid: u64,
        id: u64,
        address: u64,
        data: &'a [u8],
    },
    UnlockMemory {
        id: u64,
    },
    ClearMemoryLocks {
        pid: u64,
    },
    LockMemoryRva {
        pid: u64,
        id: u64,
        relative_address: u64,
        data: &'a [u8],
    },
    UnlockMemoryRva {
        id: u64,
    },
}

#[cfg(feature = "alloc")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Response<'a> {
    ProcessList(ProcessList<'a>),
    Memory(&'a [u8]),
    WriteComplete,
    Error(u32),
    ErrorDetail(&'a [u8]),
    ProcessId(u64),
    ProcessBase(u64),
    BatchReadMemory(&'a [u8]),
    /// One little-endian NTSTATUS per entry, in input order.
    BatchWriteStatuses(&'a [u8]),
    PointerChainResult(u64),
    LockComplete,
}

#[cfg(feature = "alloc")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessList<'a> {
    payload: &'a [u8],
}

#[cfg(feature = "alloc")]
impl<'a> ProcessList<'a> {
    pub fn new(payload: &'a [u8]) -> Result<Self, ProtocolError> {
        if payload.len() < 4 {
            return Err(ProtocolError::BufferTooSmall);
        }
        let count = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        if count > MAX_PROCESS_LIST_ENTRIES {
            return Err(ProtocolError::TooLarge);
        }
        let mut offset = 4usize;
        for _ in 0..count {
            if payload.len() - offset < PROCESS_RECORD_HEADER_SIZE {
                return Err(ProtocolError::BufferTooSmall);
            }
            let name_len = u16::from_le_bytes(
                payload[offset + 24..offset + PROCESS_RECORD_HEADER_SIZE]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if name_len > MAX_PROCESS_NAME_BYTES {
                return Err(ProtocolError::InvalidPayload);
            }
            offset = offset
                .checked_add(PROCESS_RECORD_HEADER_SIZE)
                .and_then(|value| value.checked_add(name_len))
                .ok_or(ProtocolError::InvalidPayload)?;
            if offset > payload.len() {
                return Err(ProtocolError::BufferTooSmall);
            }
        }
        if offset != payload.len() {
            return Err(ProtocolError::InvalidPayload);
        }
        Ok(Self { payload })
    }
    pub fn count(&self) -> usize {
        u32::from_le_bytes(self.payload[..4].try_into().unwrap()) as usize
    }
    pub fn iter(&self) -> ProcessIter<'a> {
        ProcessIter {
            bytes: &self.payload[4..],
            remaining: self.count(),
        }
    }
}

#[cfg(feature = "alloc")]
pub struct ProcessIter<'a> {
    bytes: &'a [u8],
    remaining: usize,
}
#[cfg(feature = "alloc")]
impl<'a> Iterator for ProcessIter<'a> {
    type Item = ProcessRecord<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.bytes.len() < PROCESS_RECORD_HEADER_SIZE {
            return None;
        }
        let pid = u64::from_le_bytes(self.bytes[..8].try_into().ok()?);
        let parent_pid = u64::from_le_bytes(self.bytes[8..16].try_into().ok()?);
        let thread_count = u32::from_le_bytes(self.bytes[16..20].try_into().ok()?);
        let len = u16::from_le_bytes([self.bytes[24], self.bytes[25]]) as usize;
        if len > MAX_PROCESS_NAME_BYTES || len > self.bytes.len() - PROCESS_RECORD_HEADER_SIZE {
            self.remaining = 0;
            return None;
        }
        let name = &self.bytes[PROCESS_RECORD_HEADER_SIZE..PROCESS_RECORD_HEADER_SIZE + len];
        self.bytes = &self.bytes[PROCESS_RECORD_HEADER_SIZE + len..];
        self.remaining -= 1;
        Some(ProcessRecord {
            pid,
            parent_pid,
            thread_count,
            name,
        })
    }
}

#[cfg(feature = "alloc")]
impl<'a> WireEncode for Request<'a> {
    fn message_type(&self) -> MessageType {
        match self {
            Self::FetchProcessList => MessageType::FetchProcessList,
            Self::GetProcessId { .. } => MessageType::GetProcessId,
            Self::GetProcessBase { .. } => MessageType::GetProcessBase,
            Self::ReadMemoryRva { .. } => MessageType::ReadMemoryRva,
            Self::WriteMemoryRva { .. } => MessageType::WriteMemoryRva,
            Self::ReadMemoryMdl(_) => MessageType::ReadMemoryMdl,
            Self::WriteMemoryMdl { .. } => MessageType::WriteMemoryMdl,
            Self::ReadMemoryMdlRva { .. } => MessageType::ReadMemoryMdlRva,
            Self::WriteMemoryMdlRva { .. } => MessageType::WriteMemoryMdlRva,
            Self::ReadProcessMemory(_) => MessageType::ReadProcessMemory,
            Self::WriteProcessMemory { .. } => MessageType::WriteProcessMemory,
            Self::BatchReadMemory { .. } => MessageType::BatchReadMemory,
            Self::BatchWrite { .. } => MessageType::BatchWriteMemory,
            Self::TraversePointerChain { .. } => MessageType::TraversePointerChain,
            Self::LockMemory { .. } => MessageType::LockMemory,
            Self::UnlockMemory { .. } => MessageType::UnlockMemory,
            Self::ClearMemoryLocks { .. } => MessageType::ClearMemoryLocks,
            Self::LockMemoryRva { .. } => MessageType::LockMemoryRva,
            Self::UnlockMemoryRva { .. } => MessageType::UnlockMemoryRva,
        }
    }
    fn encoded_len(&self) -> Result<usize, ProtocolError> {
        let n = match self {
            Self::FetchProcessList => 0,
            Self::GetProcessId { name } => 4usize
                .checked_add(name.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::GetProcessBase { .. } => 8,
            Self::ReadMemoryRva { .. } => 24,
            Self::WriteMemoryRva { data, .. } => 20usize
                .checked_add(data.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::ReadMemoryMdl(_) => 24,
            Self::WriteMemoryMdl { data, .. } => 20usize
                .checked_add(data.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::ReadMemoryMdlRva { .. } => 24,
            Self::WriteMemoryMdlRva { data, .. } => 20usize
                .checked_add(data.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::ReadProcessMemory(_) => 24,
            Self::WriteProcessMemory { data, .. } => 20usize
                .checked_add(data.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::BatchReadMemory { addresses, .. } => {
                if addresses.is_empty() || addresses.len() % 8 != 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                16usize
                    .checked_add(addresses.len())
                    .ok_or(ProtocolError::TooLarge)?
            }
            Self::BatchWrite { entries, .. } => {
                batch_write_entry_count(entries).ok_or(ProtocolError::InvalidPayload)?;
                16usize
                    .checked_add(entries.len())
                    .ok_or(ProtocolError::TooLarge)?
            }
            Self::TraversePointerChain { offsets, .. } => {
                if offsets.is_empty() || offsets.len() % 8 != 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                20usize
                    .checked_add(offsets.len())
                    .ok_or(ProtocolError::TooLarge)?
            }
            Self::LockMemory { data, .. } => 28usize
                .checked_add(data.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::UnlockMemory { .. } => 8,
            Self::ClearMemoryLocks { .. } => 8,
            Self::LockMemoryRva { data, .. } => 28usize
                .checked_add(data.len())
                .ok_or(ProtocolError::TooLarge)?,
            Self::UnlockMemoryRva { .. } => 8,
        };
        if n > MAX_FRAME_SIZE - HEADER_SIZE {
            Err(ProtocolError::TooLarge)
        } else {
            Ok(HEADER_SIZE + n)
        }
    }
    fn encode(&self, out: &mut [u8]) -> Result<usize, ProtocolError> {
        let total = self.encoded_len()?;
        if out.len() < total {
            return Err(ProtocolError::BufferTooSmall);
        }
        out[..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&(self.message_type() as u16).to_le_bytes());
        out[6..10].copy_from_slice(&((total - HEADER_SIZE) as u32).to_le_bytes());
        match self {
            Self::FetchProcessList => {}
            Self::GetProcessId { name } => {
                out[10..14].copy_from_slice(&(name.len() as u32).to_le_bytes());
                out[14..total].copy_from_slice(name);
            }
            Self::GetProcessBase { pid } => out[10..18].copy_from_slice(&pid.to_le_bytes()),
            Self::ReadMemoryRva {
                pid,
                relative_address,
                size,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&relative_address.to_le_bytes());
                out[26..34].copy_from_slice(&size.to_le_bytes());
            }
            Self::WriteMemoryRva {
                pid,
                relative_address,
                data,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&relative_address.to_le_bytes());
                out[26..30].copy_from_slice(&(data.len() as u32).to_le_bytes());
                out[30..total].copy_from_slice(data);
            }
            Self::ReadMemoryMdl(v) => {
                out[10..18].copy_from_slice(&v.pid.to_le_bytes());
                out[18..26].copy_from_slice(&v.target_address.to_le_bytes());
                out[26..34].copy_from_slice(&v.size.to_le_bytes());
            }
            Self::WriteMemoryMdl {
                pid,
                target_address,
                data,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&target_address.to_le_bytes());
                out[26..30].copy_from_slice(&(data.len() as u32).to_le_bytes());
                out[30..total].copy_from_slice(data);
            }
            Self::ReadMemoryMdlRva {
                pid,
                relative_address,
                size,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&relative_address.to_le_bytes());
                out[26..34].copy_from_slice(&size.to_le_bytes());
            }
            Self::WriteMemoryMdlRva {
                pid,
                relative_address,
                data,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&relative_address.to_le_bytes());
                out[26..30].copy_from_slice(&(data.len() as u32).to_le_bytes());
                out[30..total].copy_from_slice(data);
            }
            Self::ReadProcessMemory(v) => {
                out[10..18].copy_from_slice(&v.pid.to_le_bytes());
                out[18..26].copy_from_slice(&v.target_address.to_le_bytes());
                out[26..34].copy_from_slice(&v.size.to_le_bytes());
            }
            Self::WriteProcessMemory {
                pid,
                target_address,
                data,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&target_address.to_le_bytes());
                out[26..30].copy_from_slice(&(data.len() as u32).to_le_bytes());
                out[30..total].copy_from_slice(data);
            }
            Self::BatchReadMemory {
                pid,
                size,
                addresses,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..22].copy_from_slice(&size.to_le_bytes());
                let count = (addresses.len() / 8) as u32;
                out[22..26].copy_from_slice(&count.to_le_bytes());
                out[26..total].copy_from_slice(addresses);
            }
            Self::BatchWrite { pid, entries } => {
                let count =
                    batch_write_entry_count(entries).ok_or(ProtocolError::InvalidPayload)?;
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..22].copy_from_slice(&(count as u32).to_le_bytes());
                out[22..26].copy_from_slice(&0u32.to_le_bytes());
                out[26..total].copy_from_slice(entries);
            }
            Self::TraversePointerChain { pid, base, offsets } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&base.to_le_bytes());
                let count = (offsets.len() / 8) as u32;
                out[26..30].copy_from_slice(&count.to_le_bytes());
                out[30..total].copy_from_slice(offsets);
            }
            Self::LockMemory {
                pid,
                id,
                address,
                data,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&id.to_le_bytes());
                out[26..34].copy_from_slice(&address.to_le_bytes());
                out[34..38].copy_from_slice(&(data.len() as u32).to_le_bytes());
                out[38..total].copy_from_slice(data);
            }
            Self::UnlockMemory { id } => {
                out[10..18].copy_from_slice(&id.to_le_bytes());
            }
            Self::ClearMemoryLocks { pid } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
            }
            Self::LockMemoryRva {
                pid,
                id,
                relative_address,
                data,
            } => {
                out[10..18].copy_from_slice(&pid.to_le_bytes());
                out[18..26].copy_from_slice(&id.to_le_bytes());
                out[26..34].copy_from_slice(&relative_address.to_le_bytes());
                out[34..38].copy_from_slice(&(data.len() as u32).to_le_bytes());
                out[38..total].copy_from_slice(data);
            }
            Self::UnlockMemoryRva { id } => {
                out[10..18].copy_from_slice(&id.to_le_bytes());
            }
        }
        Ok(total)
    }
}

#[cfg(feature = "alloc")]
impl<'a> WireDecode<'a> for Request<'a> {
    fn decode(ty: MessageType, p: &'a [u8]) -> Result<Self, ProtocolError> {
        match ty {
            MessageType::FetchProcessList if p.is_empty() => Ok(Self::FetchProcessList),
            MessageType::GetProcessId if p.len() >= 4 => {
                let len = u32::from_le_bytes(p[..4].try_into().unwrap()) as usize;
                if len != p.len() - 4 || len == 0 || len > 255 {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::GetProcessId { name: &p[4..] })
            }
            MessageType::GetProcessBase if p.len() == 8 => Ok(Self::GetProcessBase {
                pid: u64::from_le_bytes(p.try_into().unwrap()),
            }),
            MessageType::ReadMemoryRva if p.len() == 24 => Ok(Self::ReadMemoryRva {
                pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                relative_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                size: u64::from_le_bytes(p[16..24].try_into().unwrap()),
            }),
            MessageType::WriteMemoryRva if p.len() >= 20 => {
                let len = u32::from_le_bytes(p[16..20].try_into().unwrap()) as usize;
                if len != p.len() - 20 || len > MAX_WRITE_SIZE {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::WriteMemoryRva {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    relative_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    data: &p[20..],
                })
            }
            MessageType::ReadMemoryMdl if p.len() == 24 => {
                Ok(Self::ReadMemoryMdl(ReadProcessMemory {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    target_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    size: u64::from_le_bytes(p[16..24].try_into().unwrap()),
                }))
            }
            MessageType::WriteMemoryMdl if p.len() >= 20 => {
                let len = u32::from_le_bytes(p[16..20].try_into().unwrap()) as usize;
                if len != p.len() - 20 || len > MAX_WRITE_SIZE {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::WriteMemoryMdl {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    target_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    data: &p[20..],
                })
            }
            MessageType::ReadMemoryMdlRva if p.len() == 24 => Ok(Self::ReadMemoryMdlRva {
                pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                relative_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                size: u64::from_le_bytes(p[16..24].try_into().unwrap()),
            }),
            MessageType::WriteMemoryMdlRva if p.len() >= 20 => {
                let len = u32::from_le_bytes(p[16..20].try_into().unwrap()) as usize;
                if len != p.len() - 20 || len > MAX_WRITE_SIZE {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::WriteMemoryMdlRva {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    relative_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    data: &p[20..],
                })
            }
            MessageType::ReadProcessMemory if p.len() == 24 => {
                Ok(Self::ReadProcessMemory(ReadProcessMemory {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    target_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    size: u64::from_le_bytes(p[16..24].try_into().unwrap()),
                }))
            }
            MessageType::WriteProcessMemory if p.len() >= 20 => {
                let len = u32::from_le_bytes(p[16..20].try_into().unwrap()) as usize;
                if len != p.len() - 20 || len > MAX_WRITE_SIZE {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::WriteProcessMemory {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    target_address: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    data: &p[20..],
                })
            }
            MessageType::BatchReadMemory if p.len() >= 16 => {
                let pid = u64::from_le_bytes(p[..8].try_into().unwrap());
                let size = u32::from_le_bytes(p[8..12].try_into().unwrap());
                let count = u32::from_le_bytes(p[12..16].try_into().unwrap()) as usize;
                let expected = 16 + count * 8;
                if p.len() != expected
                    || count > MAX_BATCH_ENTRIES
                    || size == 0
                    || size > MAX_DRIVER_TRANSFER_SIZE as u32
                {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::BatchReadMemory {
                    pid,
                    size,
                    addresses: &p[16..],
                })
            }
            MessageType::BatchWriteMemory if p.len() >= 16 => {
                batch_write_entry_count(p).ok_or(ProtocolError::InvalidPayload)?;
                Ok(Self::BatchWrite {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    entries: &p[16..],
                })
            }
            MessageType::TraversePointerChain if p.len() >= 20 => {
                let pid = u64::from_le_bytes(p[..8].try_into().unwrap());
                let base = u64::from_le_bytes(p[8..16].try_into().unwrap());
                let count = u32::from_le_bytes(p[16..20].try_into().unwrap()) as usize;
                let expected = 20 + count * 8;
                if p.len() != expected || count > 32 {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::TraversePointerChain {
                    pid,
                    base,
                    offsets: if count > 0 { &p[20..] } else { &[] },
                })
            }
            MessageType::LockMemory if p.len() >= 28 => {
                let size = u32::from_le_bytes(p[24..28].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE || p.len() != 28 + size {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::LockMemory {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    id: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    address: u64::from_le_bytes(p[16..24].try_into().unwrap()),
                    data: &p[28..],
                })
            }
            MessageType::UnlockMemory if p.len() == 8 => Ok(Self::UnlockMemory {
                id: u64::from_le_bytes(p.try_into().unwrap()),
            }),
            MessageType::ClearMemoryLocks if p.len() == 8 => Ok(Self::ClearMemoryLocks {
                pid: u64::from_le_bytes(p.try_into().unwrap()),
            }),
            MessageType::LockMemoryRva if p.len() >= 28 => {
                let size = u32::from_le_bytes(p[24..28].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE || p.len() != 28 + size {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(Self::LockMemoryRva {
                    pid: u64::from_le_bytes(p[..8].try_into().unwrap()),
                    id: u64::from_le_bytes(p[8..16].try_into().unwrap()),
                    relative_address: u64::from_le_bytes(p[16..24].try_into().unwrap()),
                    data: &p[28..],
                })
            }
            MessageType::UnlockMemoryRva if p.len() == 8 => Ok(Self::UnlockMemoryRva {
                id: u64::from_le_bytes(p.try_into().unwrap()),
            }),
            _ => Err(ProtocolError::InvalidPayload),
        }
    }
}

#[cfg(feature = "alloc")]
impl<'a> WireEncode for Response<'a> {
    fn message_type(&self) -> MessageType {
        match self {
            Self::ProcessList(_) => MessageType::ProcessList,
            Self::Memory(_) => MessageType::ReadProcessMemoryResponse,
            Self::WriteComplete => MessageType::WriteProcessMemoryResponse,
            Self::Error(_) => MessageType::Error,
            Self::ErrorDetail(_) => MessageType::ErrorDetail,
            Self::ProcessId(_) => MessageType::GetProcessIdResponse,
            Self::ProcessBase(_) => MessageType::GetProcessBaseResponse,
            Self::BatchReadMemory(_) => MessageType::BatchReadMemoryResponse,
            Self::BatchWriteStatuses(_) => MessageType::BatchWriteMemoryResponse,
            Self::PointerChainResult(_) => MessageType::TraversePointerChainResponse,
            Self::LockComplete => MessageType::LockMemoryResponse,
        }
    }
    fn encoded_len(&self) -> Result<usize, ProtocolError> {
        let payload_len = match self {
            Self::ProcessList(v) => v.payload.len(),
            Self::Memory(v) => v.len(),
            Self::WriteComplete => 0,
            Self::Error(_) => 4,
            Self::ErrorDetail(v) => v.len(),
            Self::ProcessId(_) => 8,
            Self::ProcessBase(_) => 8,
            Self::BatchReadMemory(v) => v.len(),
            Self::BatchWriteStatuses(v) => v.len(),
            Self::PointerChainResult(_) => 8,
            Self::LockComplete => 0,
        };
        let total = HEADER_SIZE
            .checked_add(payload_len)
            .ok_or(ProtocolError::TooLarge)?;
        if total > MAX_FRAME_SIZE {
            Err(ProtocolError::TooLarge)
        } else {
            Ok(total)
        }
    }
    fn encode(&self, out: &mut [u8]) -> Result<usize, ProtocolError> {
        let total = self.encoded_len()?;
        if out.len() < total {
            return Err(ProtocolError::BufferTooSmall);
        }
        out[..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&(self.message_type() as u16).to_le_bytes());
        out[6..10].copy_from_slice(&((total - HEADER_SIZE) as u32).to_le_bytes());
        match self {
            Self::ProcessList(v) => out[10..total].copy_from_slice(v.payload),
            Self::Memory(v) => out[10..total].copy_from_slice(v),
            Self::WriteComplete => {}
            Self::Error(code) => out[10..14].copy_from_slice(&code.to_le_bytes()),
            Self::ErrorDetail(v) => out[10..total].copy_from_slice(v),
            Self::ProcessId(pid) => out[10..18].copy_from_slice(&pid.to_le_bytes()),
            Self::ProcessBase(base) => out[10..18].copy_from_slice(&base.to_le_bytes()),
            Self::BatchReadMemory(v) => out[10..total].copy_from_slice(v),
            Self::BatchWriteStatuses(v) => out[10..total].copy_from_slice(v),
            Self::PointerChainResult(addr) => out[10..18].copy_from_slice(&addr.to_le_bytes()),
            Self::LockComplete => {}
        }
        Ok(total)
    }
}

#[cfg(feature = "alloc")]
impl<'a> WireDecode<'a> for Response<'a> {
    fn decode(ty: MessageType, payload: &'a [u8]) -> Result<Self, ProtocolError> {
        match ty {
            MessageType::ProcessList => Ok(Self::ProcessList(ProcessList::new(payload)?)),
            MessageType::ReadProcessMemoryResponse => Ok(Self::Memory(payload)),
            MessageType::WriteProcessMemoryResponse if payload.is_empty() => {
                Ok(Self::WriteComplete)
            }
            MessageType::Error if payload.len() == 4 => {
                Ok(Self::Error(u32::from_le_bytes(payload.try_into().unwrap())))
            }
            MessageType::ErrorDetail => Ok(Self::ErrorDetail(payload)),
            MessageType::GetProcessIdResponse if payload.len() == 8 => Ok(Self::ProcessId(
                u64::from_le_bytes(payload.try_into().unwrap()),
            )),
            MessageType::GetProcessBaseResponse if payload.len() == 8 => Ok(Self::ProcessBase(
                u64::from_le_bytes(payload.try_into().unwrap()),
            )),
            MessageType::BatchReadMemoryResponse => Ok(Self::BatchReadMemory(payload)),
            MessageType::BatchWriteMemoryResponse
                if !payload.is_empty() && payload.len() % 4 == 0 =>
            {
                Ok(Self::BatchWriteStatuses(payload))
            }
            MessageType::TraversePointerChainResponse if payload.len() == 8 => Ok(
                Self::PointerChainResult(u64::from_le_bytes(payload.try_into().unwrap())),
            ),
            MessageType::LockMemoryResponse if payload.is_empty() => Ok(Self::LockComplete),
            _ => Err(ProtocolError::InvalidPayload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let request = Request::WriteProcessMemory {
            pid: 42,
            target_address: 0x1000,
            data: b"abc",
        };
        let mut bytes = [0u8; 64];
        let len = request.encode(&mut bytes).unwrap();
        let frame = Frame::parse(&bytes[..len]).unwrap();
        assert_eq!(
            Request::decode(frame.message_type, frame.payload),
            Ok(request)
        );
    }

    #[test]
    fn process_lookup_request_round_trip() {
        let request = Request::GetProcessId {
            name: b"notepad.exe",
        };
        let mut bytes = [0u8; 64];
        let len = request.encode(&mut bytes).unwrap();
        let frame = Frame::parse(&bytes[..len]).unwrap();
        assert_eq!(
            Request::decode(frame.message_type, frame.payload),
            Ok(request)
        );
    }

    #[test]
    fn rva_write_request_round_trip() {
        let request = Request::WriteMemoryRva {
            pid: 42,
            relative_address: 0x1234,
            data: b"abc",
        };
        let mut bytes = [0u8; 64];
        let len = request.encode(&mut bytes).unwrap();
        let frame = Frame::parse(&bytes[..len]).unwrap();
        assert_eq!(
            Request::decode(frame.message_type, frame.payload),
            Ok(request)
        );
    }

    #[test]
    fn mdl_request_round_trip() {
        let read = Request::ReadMemoryMdl(ReadProcessMemory {
            pid: 7,
            target_address: 0x1000,
            size: 4,
        });
        let write = Request::WriteMemoryMdl {
            pid: 7,
            target_address: 0x1000,
            data: b"xy",
        };
        let read_rva = Request::ReadMemoryMdlRva {
            pid: 7,
            relative_address: 0x2000,
            size: 8,
        };
        let write_rva = Request::WriteMemoryMdlRva {
            pid: 7,
            relative_address: 0x2000,
            data: b"xyz",
        };
        for request in [read, write, read_rva, write_rva] {
            let mut bytes = [0u8; 64];
            let len = request.encode(&mut bytes).unwrap();
            let frame = Frame::parse(&bytes[..len]).unwrap();
            assert_eq!(
                Request::decode(frame.message_type, frame.payload),
                Ok(request)
            );
        }
    }

    #[test]
    fn decoder_handles_half_and_sticky_frames() {
        let request = Request::FetchProcessList;
        let mut first = [0u8; HEADER_SIZE];
        let first_len = request.encode(&mut first).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&first[..3]).unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder.push(&first[3..]).unwrap();
        assert_eq!(
            decoder.next().unwrap().unwrap().message_type,
            MessageType::FetchProcessList
        );
        decoder.consume().unwrap();
        assert_eq!(decoder.buffered_len(), 0);
        assert_eq!(first_len, HEADER_SIZE);
    }

    #[test]
    fn malformed_lengths_never_panic() {
        assert_eq!(Frame::parse(&[]), Err(ProtocolError::BufferTooSmall));
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..6].copy_from_slice(&(MessageType::FetchProcessList as u16).to_le_bytes());
        bytes[6..10].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(Frame::parse(&bytes), Err(ProtocolError::BufferTooSmall));
        let mut decoder = FrameDecoder::with_capacity(1);
        decoder.push(&bytes).unwrap();
        assert_eq!(decoder.next(), Err(ProtocolError::TooLarge));
    }

    #[test]
    fn write_length_and_output_are_checked() {
        let too_large = Request::WriteProcessMemory {
            pid: 1,
            target_address: 2,
            data: &[0u8; MAX_WRITE_SIZE + 1],
        };
        assert_eq!(too_large.encoded_len(), Err(ProtocolError::TooLarge));
        let read = Request::ReadProcessMemory(ReadProcessMemory {
            pid: 1,
            target_address: 2,
            size: 3,
        });
        assert_eq!(
            read.encode(&mut [0u8; 10]),
            Err(ProtocolError::BufferTooSmall)
        );
    }
}
