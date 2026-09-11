use std::fmt;

use ks_core::protocol::{
    MemoryReadRequest, MemoryWriteRequest, IOCTL_PING, IOCTL_READ_MEMORY, IOCTL_WRITE_MEMORY,
    MAX_DRIVER_TRANSFER_SIZE,
};

const DRIVER_PATH: &str = "\\\\.\\KernelScriptProfiler";

#[derive(Debug)]
pub enum DriverError {
    NotConnected,
    ConnectionFailed,
    IoctlFailed(u32),
    ResponseParseFailed,
    DriverNotFound,
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => write!(f, "not connected to driver"),
            Self::ConnectionFailed => write!(f, "failed to open driver device"),
            Self::IoctlFailed(code) => write!(f, "IOCTL failed with NTSTATUS {:#010x}", code),
            Self::ResponseParseFailed => write!(f, "failed to parse driver response"),
            Self::DriverNotFound => write!(f, "driver device not found"),
        }
    }
}

impl std::error::Error for DriverError {}

pub struct DriverComm {
    handle: Option<DriverHandle>,
    reconnect_attempts: u32,
}

struct DriverHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for DriverHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

unsafe impl Send for DriverHandle {}

impl DriverComm {
    pub fn new() -> Self {
        Self {
            handle: None,
            reconnect_attempts: 0,
        }
    }

    pub fn connect(&mut self) -> Result<(), DriverError> {
        self.disconnect();

        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};

        let wide_path: Vec<u16> = DRIVER_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
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

        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
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
                handle,
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
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(DriverError::IoctlFailed(error));
        }

        self.handle = Some(DriverHandle(handle));
        self.reconnect_attempts = 0;
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.handle = None;
    }

    pub fn is_connected(&self) -> bool {
        self.handle.is_some()
    }

    pub fn try_reconnect(&mut self) -> Result<(), DriverError> {
        if self.is_connected() {
            return Ok(());
        }
        self.reconnect_attempts += 1;
        if self.reconnect_attempts > 5 {
            return Err(DriverError::ConnectionFailed);
        }
        self.connect()
    }

    pub fn read_memory(
        &self,
        process_id: u64,
        address: u64,
        size: u64,
    ) -> Result<Vec<u8>, DriverError> {
        if process_id == 0 || address == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;

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
        &self,
        process_id: u64,
        address: u64,
        data: &[u8],
    ) -> Result<(), DriverError> {
        if process_id == 0 || address == 0 || data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;

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

    pub fn get_process_base(&self, process_id: u64) -> Result<u64, DriverError> {
        if process_id == 0 {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;
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
        &self,
        process_id: u64,
        relative_address: u64,
        size: u64,
    ) -> Result<Vec<u8>, DriverError> {
        if process_id == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;
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
        &self,
        process_id: u64,
        relative_address: u64,
        data: &[u8],
    ) -> Result<(), DriverError> {
        if process_id == 0 || data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;
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
        &self,
        process_id: u64,
        address: u64,
        size: u64,
    ) -> Result<Vec<u8>, DriverError> {
        self.read_memory_via(
            ks_core::protocol::IOCTL_READ_MEMORY_MDL,
            process_id,
            address,
            size,
        )
    }

    pub fn write_memory_mdl(
        &self,
        process_id: u64,
        address: u64,
        data: &[u8],
    ) -> Result<(), DriverError> {
        self.write_memory_via(
            ks_core::protocol::IOCTL_WRITE_MEMORY_MDL,
            process_id,
            address,
            data,
        )
    }

    pub fn read_memory_mdl_rva(
        &self,
        process_id: u64,
        relative_address: u64,
        size: u64,
    ) -> Result<Vec<u8>, DriverError> {
        self.read_memory_via(
            ks_core::protocol::IOCTL_READ_MEMORY_MDL_RVA,
            process_id,
            relative_address,
            size,
        )
    }

    pub fn write_memory_mdl_rva(
        &self,
        process_id: u64,
        relative_address: u64,
        data: &[u8],
    ) -> Result<(), DriverError> {
        self.write_memory_via(
            ks_core::protocol::IOCTL_WRITE_MEMORY_MDL_RVA,
            process_id,
            relative_address,
            data,
        )
    }

    fn read_memory_via(
        &self,
        ioctl: u32,
        process_id: u64,
        address: u64,
        size: u64,
    ) -> Result<Vec<u8>, DriverError> {
        if process_id == 0 || address == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u64 {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;
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
        &self,
        ioctl: u32,
        process_id: u64,
        address: u64,
        data: &[u8],
    ) -> Result<(), DriverError> {
        if process_id == 0 || address == 0 || data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE {
            return Err(DriverError::ResponseParseFailed);
        }
        let handle = self.handle.as_ref().ok_or(DriverError::NotConnected)?;
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
}
