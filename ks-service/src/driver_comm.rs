use std::fmt;
use std::sync::Arc;

use ks_core::protocol::{
    MemoryReadRequest, MemoryWriteRequest, IOCTL_BATCH_READ_MEMORY, IOCTL_PING, IOCTL_READ_MEMORY,
    IOCTL_TRAVERSE_POINTER_CHAIN, IOCTL_WRITE_MEMORY, MAX_BATCH_ENTRIES, MAX_DRIVER_TRANSFER_SIZE,
};

const DRIVER_PATH: &str = "\\\\.\\KernelScriptProfiler";

#[derive(Debug)]
pub enum DriverError {
    ConnectionFailed,
    IoctlFailed(u32),
    ResponseParseFailed,
    DriverNotFound,
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionFailed => write!(f, "failed to open driver device"),
            Self::IoctlFailed(code) => write!(f, "IOCTL failed with NTSTATUS {:#010x}", code),
            Self::ResponseParseFailed => write!(f, "failed to parse driver response"),
            Self::DriverNotFound => write!(f, "driver device not found"),
        }
    }
}

impl std::error::Error for DriverError {}

pub struct DriverHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for DriverHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

unsafe impl Send for DriverHandle {}
unsafe impl Sync for DriverHandle {}

pub struct DriverComm {
    handle: Option<Arc<DriverHandle>>,
}

impl DriverComm {
    pub fn new() -> Self {
        Self { handle: None }
    }

    pub fn handle(&self) -> Option<Arc<DriverHandle>> {
        self.handle.clone()
    }

    pub fn connect(&mut self) -> Result<(), DriverError> {
        self.disconnect();

        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};

        let wide_path: Vec<u16> = DRIVER_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let raw_handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
                core::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                core::ptr::null_mut(),
            )
        };

        if raw_handle.is_null() || raw_handle == INVALID_HANDLE_VALUE {
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            if err == 2 {
                return Err(DriverError::DriverNotFound);
            }
            return Err(DriverError::ConnectionFailed);
        }

        let ping_input = [0u8; 1];
        let mut ping_output = [0u8; 1];
        let mut returned = 0u32;
        let ping_ok = unsafe {
            windows_sys::Win32::System::IO::DeviceIoControl(
                raw_handle,
                IOCTL_PING,
                ping_input.as_ptr() as *const _,
                ping_input.len() as u32,
                ping_output.as_mut_ptr() as *mut _,
                ping_output.len() as u32,
                &mut returned,
                core::ptr::null_mut(),
            )
        };
        if ping_ok == 0 {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            unsafe { windows_sys::Win32::Foundation::CloseHandle(raw_handle) };
            return Err(DriverError::IoctlFailed(error));
        }

        self.handle = Some(Arc::new(DriverHandle(raw_handle)));
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.handle = None;
    }
}

pub fn read_memory(
    handle: &DriverHandle,
    process_id: u64,
    address: u64,
    size: u64,
) -> Result<Vec<u8>, DriverError> {
    if process_id == 0 || address == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
        return Err(DriverError::ResponseParseFailed);
    }

    let mut bytes_returned = 0u32;
    let mut buffer = [0u8; 8 + MAX_DRIVER_TRANSFER_SIZE + 4];

    let request = MemoryReadRequest {
        process_id,
        address,
        size,
    };

    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            IOCTL_READ_MEMORY,
            &request as *const _ as *const _,
            core::mem::size_of::<MemoryReadRequest>() as u32,
            buffer.as_mut_ptr() as *mut _,
            buffer.len() as u32,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };

    if result == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(err));
    }

    if (bytes_returned as usize) < 8 + MAX_DRIVER_TRANSFER_SIZE {
        return Err(DriverError::ResponseParseFailed);
    }

    if u32::from_ne_bytes(
        buffer[0..4]
            .try_into()
            .map_err(|_| DriverError::ResponseParseFailed)?,
    ) != 0x4B53_5231
    {
        return Err(DriverError::ResponseParseFailed);
    }
    if buffer[4] == 0 {
        let error_code = u32::from_ne_bytes(
            buffer[8 + MAX_DRIVER_TRANSFER_SIZE..12 + MAX_DRIVER_TRANSFER_SIZE]
                .try_into()
                .map_err(|_| DriverError::ResponseParseFailed)?,
        );
        return Err(DriverError::IoctlFailed(error_code));
    }

    Ok(buffer[8..8 + size as usize].to_vec())
}

pub fn write_memory(
    handle: &DriverHandle,
    process_id: u64,
    address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    if process_id == 0 || address == 0 || data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
        return Err(DriverError::ResponseParseFailed);
    }

    let mut bytes_returned = 0u32;
    let mut buffer = [0u8; 8 + MAX_DRIVER_TRANSFER_SIZE + 4];

    let mut request = MemoryWriteRequest {
        process_id,
        address,
        size: data.len() as u64,
        data: [0u8; MAX_DRIVER_TRANSFER_SIZE],
    };

    request.data[..data.len()].copy_from_slice(data);

    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            IOCTL_WRITE_MEMORY,
            &request as *const _ as *const _,
            core::mem::size_of::<MemoryWriteRequest>() as u32,
            buffer.as_mut_ptr() as *mut _,
            buffer.len() as u32,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };

    if result == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(err));
    }

    Ok(())
}

pub fn get_process_base(handle: &DriverHandle, process_id: u64) -> Result<u64, DriverError> {
    if process_id == 0 {
        return Err(DriverError::ResponseParseFailed);
    }
    let request = ks_core::protocol::ProcessBaseRequest { process_id };
    let mut output = [0u8; 8];
    let mut returned = 0u32;
    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            ks_core::protocol::IOCTL_GET_PROCESS_BASE,
            &request as *const _ as *const _,
            core::mem::size_of_val(&request) as u32,
            output.as_mut_ptr() as *mut _,
            output.len() as u32,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if result == 0 {
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(error));
    }
    if returned != output.len() as u32 {
        return Err(DriverError::ResponseParseFailed);
    }
    Ok(u64::from_le_bytes(output))
}

pub fn read_memory_rva(
    handle: &DriverHandle,
    process_id: u64,
    relative_address: u64,
    size: u64,
) -> Result<Vec<u8>, DriverError> {
    if process_id == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
        return Err(DriverError::ResponseParseFailed);
    }
    let request = ks_core::protocol::MemoryRvaReadRequest {
        process_id,
        relative_address,
        size,
    };
    let mut buffer = [0u8; 8 + MAX_DRIVER_TRANSFER_SIZE + 4];
    let mut returned = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            ks_core::protocol::IOCTL_READ_MEMORY_RVA,
            &request as *const _ as *const _,
            core::mem::size_of_val(&request) as u32,
            buffer.as_mut_ptr() as *mut _,
            buffer.len() as u32,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(DriverError::IoctlFailed(unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    if returned < 8 + MAX_DRIVER_TRANSFER_SIZE as u32 {
        return Err(DriverError::ResponseParseFailed);
    }
    if u32::from_ne_bytes(buffer[..4].try_into().unwrap()) != 0x4B53_5231 {
        return Err(DriverError::ResponseParseFailed);
    }
    if buffer[4] == 0 {
        let error_code = u32::from_ne_bytes(
            buffer[8 + MAX_DRIVER_TRANSFER_SIZE..12 + MAX_DRIVER_TRANSFER_SIZE]
                .try_into()
                .unwrap(),
        );
        tracing::error!(
            pid = process_id,
            relative_address,
            ntstatus = format_args!("0x{:08X}", error_code),
            "driver RVA read: target memory access failed"
        );
        return Err(DriverError::IoctlFailed(error_code));
    }
    Ok(buffer[8..8 + size as usize].to_vec())
}

pub fn write_memory_rva(
    handle: &DriverHandle,
    process_id: u64,
    relative_address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    if process_id == 0 || data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
        return Err(DriverError::ResponseParseFailed);
    }
    let mut request = ks_core::protocol::MemoryRvaWriteRequest {
        process_id,
        relative_address,
        size: data.len() as u64,
        data: [0; MAX_DRIVER_TRANSFER_SIZE],
    };
    request.data[..data.len()].copy_from_slice(data);
    let mut returned = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            ks_core::protocol::IOCTL_WRITE_MEMORY_RVA,
            &request as *const _ as *const _,
            core::mem::size_of_val(&request) as u32,
            core::ptr::null_mut(),
            0,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(DriverError::IoctlFailed(unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    Ok(())
}

pub fn read_memory_mdl(
    handle: &DriverHandle,
    process_id: u64,
    address: u64,
    size: u64,
) -> Result<Vec<u8>, DriverError> {
    read_memory_via(
        handle,
        ks_core::protocol::IOCTL_READ_MEMORY_MDL,
        process_id,
        address,
        size,
    )
}

pub fn write_memory_mdl(
    handle: &DriverHandle,
    process_id: u64,
    address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    write_memory_via(
        handle,
        ks_core::protocol::IOCTL_WRITE_MEMORY_MDL,
        process_id,
        address,
        data,
    )
}

pub fn read_memory_mdl_rva(
    handle: &DriverHandle,
    process_id: u64,
    relative_address: u64,
    size: u64,
) -> Result<Vec<u8>, DriverError> {
    read_memory_via(
        handle,
        ks_core::protocol::IOCTL_READ_MEMORY_MDL_RVA,
        process_id,
        relative_address,
        size,
    )
}

pub fn write_memory_mdl_rva(
    handle: &DriverHandle,
    process_id: u64,
    relative_address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    write_memory_via(
        handle,
        ks_core::protocol::IOCTL_WRITE_MEMORY_MDL_RVA,
        process_id,
        relative_address,
        data,
    )
}

fn read_memory_via(
    handle: &DriverHandle,
    ioctl: u32,
    process_id: u64,
    address: u64,
    size: u64,
) -> Result<Vec<u8>, DriverError> {
    if process_id == 0 || address == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
        return Err(DriverError::ResponseParseFailed);
    }
    let mut bytes_returned = 0u32;
    let mut buffer = [0u8; 8 + MAX_DRIVER_TRANSFER_SIZE + 4];
    let request = MemoryReadRequest {
        process_id,
        address,
        size,
    };
    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            ioctl,
            &request as *const _ as *const _,
            core::mem::size_of::<MemoryReadRequest>() as u32,
            buffer.as_mut_ptr() as *mut _,
            buffer.len() as u32,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };
    if result == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(err));
    }
    if (bytes_returned as usize) < 8 + MAX_DRIVER_TRANSFER_SIZE {
        return Err(DriverError::ResponseParseFailed);
    }
    if u32::from_ne_bytes(
        buffer[0..4]
            .try_into()
            .map_err(|_| DriverError::ResponseParseFailed)?,
    ) != 0x4B53_5231
    {
        return Err(DriverError::ResponseParseFailed);
    }
    if buffer[4] == 0 {
        let error_code = u32::from_ne_bytes(
            buffer[8 + MAX_DRIVER_TRANSFER_SIZE..12 + MAX_DRIVER_TRANSFER_SIZE]
                .try_into()
                .map_err(|_| DriverError::ResponseParseFailed)?,
        );
        return Err(DriverError::IoctlFailed(error_code));
    }
    Ok(buffer[8..8 + size as usize].to_vec())
}

fn write_memory_via(
    handle: &DriverHandle,
    ioctl: u32,
    process_id: u64,
    address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    if process_id == 0 || address == 0 || data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
        return Err(DriverError::ResponseParseFailed);
    }
    let mut bytes_returned = 0u32;
    let mut buffer = [0u8; 8 + MAX_DRIVER_TRANSFER_SIZE + 4];
    let mut request = MemoryWriteRequest {
        process_id,
        address,
        size: data.len() as u64,
        data: [0u8; MAX_DRIVER_TRANSFER_SIZE],
    };
    request.data[..data.len()].copy_from_slice(data);
    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            ioctl,
            &request as *const _ as *const _,
            core::mem::size_of::<MemoryWriteRequest>() as u32,
            buffer.as_mut_ptr() as *mut _,
            buffer.len() as u32,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };
    if result == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(err));
    }
    Ok(())
}

pub fn batch_read_memory(
    handle: &DriverHandle,
    process_id: u64,
    size: u32,
    addresses: &[u64],
) -> Result<Vec<u8>, DriverError> {
    if process_id == 0
        || addresses.is_empty()
        || addresses.len() > MAX_BATCH_ENTRIES
        || size == 0
        || size > MAX_DRIVER_TRANSFER_SIZE as u32
    {
        return Err(DriverError::ResponseParseFailed);
    }
    let input_size = 16 + addresses.len() * 8;
    let mut input = vec![0u8; input_size];
    input[..8].copy_from_slice(&process_id.to_le_bytes());
    input[8..12].copy_from_slice(&size.to_le_bytes());
    input[12..16].copy_from_slice(&(addresses.len() as u32).to_le_bytes());
    for (i, &addr) in addresses.iter().enumerate() {
        let off = 16 + i * 8;
        input[off..off + 8].copy_from_slice(&addr.to_le_bytes());
    }

    let mut bytes_returned = 0u32;
    let output_size = addresses.len() * size as usize;
    let mut output = vec![0u8; output_size];

    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            IOCTL_BATCH_READ_MEMORY,
            input.as_ptr() as *const _,
            input_size as u32,
            output.as_mut_ptr() as *mut _,
            output.len() as u32,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };
    if result == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(err));
    }
    if bytes_returned as usize != output_size {
        return Err(DriverError::ResponseParseFailed);
    }
    output.truncate(bytes_returned as usize);
    Ok(output)
}

pub fn traverse_pointer_chain(
    handle: &DriverHandle,
    process_id: u64,
    base: u64,
    offsets: &[u64],
) -> Result<u64, DriverError> {
    if process_id == 0 || offsets.len() > 32 {
        return Err(DriverError::ResponseParseFailed);
    }
    let input_size = 20 + offsets.len() * 8;
    let mut input = vec![0u8; input_size];
    input[..8].copy_from_slice(&process_id.to_le_bytes());
    input[8..16].copy_from_slice(&base.to_le_bytes());
    input[16..20].copy_from_slice(&(offsets.len() as u32).to_le_bytes());
    for (i, &offset) in offsets.iter().enumerate() {
        let off = 20 + i * 8;
        input[off..off + 8].copy_from_slice(&offset.to_le_bytes());
    }

    let mut result_ptr: u64 = 0;
    let mut bytes_returned = 0u32;

    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            IOCTL_TRAVERSE_POINTER_CHAIN,
            input.as_ptr() as *const _,
            input_size as u32,
            &mut result_ptr as *mut u64 as *mut _,
            8,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };
    if result == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(DriverError::IoctlFailed(err));
    }
    if bytes_returned != 8 {
        return Err(DriverError::ResponseParseFailed);
    }
    Ok(result_ptr)
}
