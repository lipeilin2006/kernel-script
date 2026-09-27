use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ks_core::crypto::{self, xor_u64};
use ks_core::protocol::{
    MemoryReadRequest, MemoryWriteRequest, IOCTL_BATCH_READ_MEMORY, IOCTL_PING, IOCTL_READ_MEMORY,
    IOCTL_TRAVERSE_POINTER_CHAIN, IOCTL_WRITE_MEMORY, IOCTL_WRITE_MEMORY_BATCH, MAX_BATCH_ENTRIES,
    MAX_BATCH_WRITE_ENTRIES, MAX_DRIVER_TRANSFER_SIZE, MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE,
};

const DRIVER_PATH: &str = "\\\\.\\KernelScriptProfiler";
const KEY_REGISTRY_PATH: &str = "SYSTEM\\CurrentControlSet\\Control\\KernelScript";
const KEY_VALUE_NAME: &str = "IoctlKey";

fn ioctl_key() -> &'static [u8; crypto::KEY_LEN] {
    static KEY: OnceLock<[u8; crypto::KEY_LEN]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = crypto::FALLBACK_KEY;
        let path: Vec<u16> = KEY_REGISTRY_PATH.encode_utf16().chain([0]).collect();
        let value: Vec<u16> = KEY_VALUE_NAME.encode_utf16().chain([0]).collect();
        unsafe {
            use windows_sys::Win32::System::Registry::{
                RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ,
                REG_BINARY,
            };
            let mut handle = core::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut handle) == 0 {
                let mut kind = 0;
                let mut size = crypto::KEY_LEN as u32;
                let status = RegQueryValueExW(
                    handle,
                    value.as_ptr(),
                    core::ptr::null_mut(),
                    &mut kind,
                    key.as_mut_ptr(),
                    &mut size,
                );
                if status != 0 || kind != REG_BINARY || size != crypto::KEY_LEN as u32 {
                    key = crypto::FALLBACK_KEY;
                }
                RegCloseKey(handle);
            }
        }
        key
    })
}

#[inline]
fn protect(value: u64, field: usize) -> u64 {
    xor_u64(value, ioctl_key(), field)
}
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
        process_id: protect(process_id, crypto::PID),
        address: protect(address, crypto::ADDRESS),
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
        process_id: protect(process_id, crypto::PID),
        address: protect(address, crypto::ADDRESS),
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
    let request = ks_core::protocol::ProcessBaseRequest {
        process_id: protect(process_id, crypto::PID),
    };
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
    Ok(protect(u64::from_le_bytes(output), crypto::BASE))
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
        process_id: protect(process_id, crypto::PID),
        relative_address: protect(relative_address, crypto::RVA),
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
        process_id: protect(process_id, crypto::PID),
        relative_address: protect(relative_address, crypto::RVA),
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
        process_id: protect(process_id, crypto::PID),
        address: protect(
            address,
            if ioctl == ks_core::protocol::IOCTL_READ_MEMORY_MDL_RVA {
                crypto::RVA
            } else {
                crypto::ADDRESS
            },
        ),
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
        process_id: protect(process_id, crypto::PID),
        address: protect(
            address,
            if ioctl == ks_core::protocol::IOCTL_WRITE_MEMORY_MDL_RVA {
                crypto::RVA
            } else {
                crypto::ADDRESS
            },
        ),
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
    input[..8].copy_from_slice(&protect(process_id, crypto::PID).to_le_bytes());
    input[8..12].copy_from_slice(&size.to_le_bytes());
    input[12..16].copy_from_slice(&(addresses.len() as u32).to_le_bytes());
    for (i, &addr) in addresses.iter().enumerate() {
        let off = 16 + i * 8;
        input[off..off + 8].copy_from_slice(&protect(addr, crypto::ADDRESS).to_le_bytes());
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
    input[..8].copy_from_slice(&protect(process_id, crypto::PID).to_le_bytes());
    input[8..16].copy_from_slice(&protect(base, crypto::BASE).to_le_bytes());
    input[16..20].copy_from_slice(&(offsets.len() as u32).to_le_bytes());
    for (i, &offset) in offsets.iter().enumerate() {
        let off = 20 + i * 8;
        input[off..off + 8].copy_from_slice(&protect(offset, crypto::OFFSET).to_le_bytes());
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
    Ok(protect(result_ptr, crypto::RESULT))
}

/// Service-side memory lock table.
///
/// A lock continuously rewrites a byte pattern to a target-process address.
/// The rewriter thread replays every entry through a single
/// `IOCTL_WRITE_MEMORY_BATCH` per sweep; the kernel driver keeps no lock
/// state and performs no background writes of its own.
#[derive(Clone)]
struct LockEntry {
    id: u64,
    pid: u64,
    address: u64,
    data: Vec<u8>,
}

static LOCKS: OnceLock<Mutex<Vec<LockEntry>>> = OnceLock::new();

/// Bumped on every table mutation so the rewriter can cache the encoded
/// batch buffer and only re-encode after actual changes.
static LOCKS_VERSION: AtomicU64 = AtomicU64::new(0);

fn lock_table() -> &'static Mutex<Vec<LockEntry>> {
    LOCKS.get_or_init(|| Mutex::new(Vec::new()))
}

fn lock_insert(id: u64, pid: u64, address: u64, data: &[u8]) -> Result<(), DriverError> {
    if id == 0 || pid == 0 || address == 0 || data.is_empty() || data.len() > MAX_MEMORY_LOCK_SIZE {
        return Err(DriverError::ResponseParseFailed);
    }
    let mut table = lock_table().lock().expect("lock table poisoned");
    if let Some(entry) = table.iter_mut().find(|entry| entry.id == id) {
        entry.data = data.to_vec();
    } else {
        if table.len() >= MAX_MEMORY_LOCKS {
            tracing::warn!(pid, "memory lock table full");
            return Err(DriverError::ResponseParseFailed);
        }
        table.push(LockEntry {
            id,
            pid,
            address,
            data: data.to_vec(),
        });
    }
    LOCKS_VERSION.fetch_add(1, Ordering::Release);
    Ok(())
}

fn lock_remove(id: u64) {
    lock_table()
        .lock()
        .expect("lock table poisoned")
        .retain(|entry| entry.id != id);
    LOCKS_VERSION.fetch_add(1, Ordering::Release);
}

fn lock_clear(pid: u64) {
    lock_table()
        .lock()
        .expect("lock table poisoned")
        .retain(|entry| entry.pid != pid);
    LOCKS_VERSION.fetch_add(1, Ordering::Release);
}

/// Rewrites every active lock entry through the batch write IOCTL in a
/// continuous spin. Runs on its own OS thread; it also runs at BELOW_NORMAL
/// priority so the spinning thread can never starve the game or the service.
/// Exits when the service shuts down. Write failures are ignored: a vanished
/// process or freed page simply skips the entry.
pub async fn run_lock_worker(
    handle: Arc<DriverHandle>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("ks-lock-rewriter".to_owned())
        .spawn(move || {
            // BELOW_NORMAL keeps this spinning thread scheduler-friendly:
            // any normal-priority ready thread preempts it immediately.
            unsafe {
                windows_sys::Win32::System::Threading::SetThreadPriority(
                    windows_sys::Win32::System::Threading::GetCurrentThread(),
                    windows_sys::Win32::System::Threading::THREAD_PRIORITY_BELOW_NORMAL,
                );
            }
            lock_rewrite_loop(Arc::clone(&handle), thread_stop)
        })
        .expect("failed to spawn lock rewriter thread");
    let _ = shutdown.recv().await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    let _ = thread.join();
}

fn lock_rewrite_loop(handle: Arc<DriverHandle>, stop: Arc<std::sync::atomic::AtomicBool>) {
    let mut buffers = BatchWriteBuffers::new();
    while !stop.load(Ordering::Acquire) {
        // Fast path: the table is unchanged, so the previously encoded
        // request is submitted as-is — no clone, no re-encode, no table
        // lock. Only actual lock mutations trigger a re-encode.
        buffers.sync();
        if buffers.count() > 0 {
            buffers.submit(&handle);
        }
    }
}

const BATCH_WRITE_HEADER: usize = 8;
const BATCH_WRITE_ENTRY_HEADER: usize = 24;

/// Builds a batch-write request body (count + per-entry
/// `pid, address, size, pad, data`).
fn encode_batch_write(entries: &[(u64, u64, &[u8])]) -> Vec<u8> {
    let data_bytes: usize = entries.iter().map(|(_, _, data)| data.len()).sum();
    let mut input = Vec::with_capacity(
        BATCH_WRITE_HEADER + BATCH_WRITE_ENTRY_HEADER * entries.len() + data_bytes,
    );
    input.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    input.extend_from_slice(&0u32.to_le_bytes());
    for (pid, address, data) in entries {
        input.extend_from_slice(&protect(*pid, crypto::PID).to_le_bytes());
        input.extend_from_slice(&protect(*address, crypto::ADDRESS).to_le_bytes());
        input.extend_from_slice(&(data.len() as u32).to_le_bytes());
        input.extend_from_slice(&0u32.to_le_bytes());
        input.extend_from_slice(data);
    }
    input
}

/// One-shot batch write used by the script-facing `batch_write` API: every
/// entry shares `pid` and is written in a single kernel transition.
/// Returns one NTSTATUS per entry.
pub fn batch_write_entries(
    handle: &DriverHandle,
    pid: u64,
    entries: &[(u64, Vec<u8>)],
) -> Result<Vec<u32>, DriverError> {
    if entries.is_empty() || entries.len() > MAX_BATCH_WRITE_ENTRIES || pid == 0 {
        return Err(DriverError::ResponseParseFailed);
    }
    let borrowed: Vec<(u64, u64, &[u8])> = entries
        .iter()
        .map(|(address, data)| (pid, *address, data.as_slice()))
        .collect();
    let input = encode_batch_write(&borrowed);
    let mut output = vec![0u8; 4 + entries.len() * 4];
    let mut returned = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.0,
            IOCTL_WRITE_MEMORY_BATCH,
            input.as_ptr() as *const _,
            input.len() as u32,
            output.as_mut_ptr() as *mut _,
            output.len() as u32,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(DriverError::IoctlFailed(unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    let count = u32::from_le_bytes(output[..4].try_into().unwrap()) as usize;
    Ok(output[4..4 + count * 4]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect())
}

/// Persistent re-encode cache for the lock sweep. The encoded
/// `IOCTL_WRITE_MEMORY_BATCH` request only depends on the lock table
/// contents, so it is rebuilt lazily after a version bump and reused for
/// every sweep in between.
struct BatchWriteBuffers {
    encoded_version: u64,
    count: usize,
    input: Vec<u8>,
    output: Vec<u8>,
}

impl BatchWriteBuffers {
    fn new() -> Self {
        Self {
            encoded_version: u64::MAX,
            count: 0,
            input: Vec::new(),
            output: Vec::new(),
        }
    }

    fn count(&self) -> usize {
        self.count
    }

    /// Re-encodes the request when the lock table changed since the last
    /// encode. The table mutex is only taken on that slow path.
    fn sync(&mut self) {
        let current = LOCKS_VERSION.load(Ordering::Acquire);
        if current == self.encoded_version {
            return;
        }
        let table = lock_table().lock().expect("lock table poisoned");
        self.count = table.len().min(MAX_BATCH_WRITE_ENTRIES);
        let data_bytes: usize = table.iter().take(self.count).map(|e| e.data.len()).sum();
        self.input.clear();
        self.input
            .reserve(BATCH_WRITE_HEADER + BATCH_WRITE_ENTRY_HEADER * self.count + data_bytes);
        self.input
            .extend_from_slice(&(self.count as u32).to_le_bytes());
        self.input.extend_from_slice(&0u32.to_le_bytes());
        for entry in table.iter().take(self.count) {
            self.input
                .extend_from_slice(&protect(entry.pid, crypto::PID).to_le_bytes());
            self.input
                .extend_from_slice(&protect(entry.address, crypto::ADDRESS).to_le_bytes());
            self.input
                .extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            self.input.extend_from_slice(&0u32.to_le_bytes());
            self.input.extend_from_slice(&entry.data);
        }
        self.output.clear();
        self.output.resize(4 + self.count * 4, 0);
        // Read the version again after encoding: a mutation that raced this
        // encode bumped it and the next sweep re-encodes.
        self.encoded_version = LOCKS_VERSION.load(Ordering::Acquire);
    }

    /// Submits the cached request. Per-entry write failures are ignored by
    /// the sweep (a vanished process or freed page simply skips the entry),
    /// but a transport-level failure is logged.
    fn submit(&mut self, handle: &DriverHandle) {
        let mut returned = 0u32;
        let ok = unsafe {
            windows_sys::Win32::System::IO::DeviceIoControl(
                handle.0,
                IOCTL_WRITE_MEMORY_BATCH,
                self.input.as_ptr() as *const _,
                self.input.len() as u32,
                self.output.as_mut_ptr() as *mut _,
                self.output.len() as u32,
                &mut returned,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            tracing::debug!(error, entries = self.count, "lock batch write failed");
        }
    }
}

pub fn lock_memory(
    _handle: &DriverHandle,
    id: u64,
    pid: u64,
    address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    lock_insert(id, pid, address, data)
}

pub fn lock_memory_rva(
    handle: &DriverHandle,
    id: u64,
    pid: u64,
    relative_address: u64,
    data: &[u8],
) -> Result<(), DriverError> {
    let base = get_process_base(handle, pid)?;
    let Some(address) = base.checked_add(relative_address) else {
        return Err(DriverError::ResponseParseFailed);
    };
    lock_insert(id, pid, address, data)
}

pub fn unlock_memory(_handle: &DriverHandle, id: u64) -> Result<(), DriverError> {
    lock_remove(id);
    Ok(())
}

pub fn unlock_memory_rva(_handle: &DriverHandle, id: u64) -> Result<(), DriverError> {
    lock_remove(id);
    Ok(())
}

pub fn clear_memory_locks(_handle: &DriverHandle, pid: u64) -> Result<(), DriverError> {
    lock_clear(pid);
    Ok(())
}
