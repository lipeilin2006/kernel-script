use core::{mem, ptr};

use crate::memory::{self, BatchWriteEntry};
use crate::wdm::*;
use crate::wire::{
    MemoryReadRequest, MemoryRvaReadRequest, MemoryRvaWriteRequest, MemoryWriteRequest,
    ProcessBaseRequest,
};
use ks_core::protocol::{
    IOCTL_BATCH_READ_MEMORY, IOCTL_TRAVERSE_POINTER_CHAIN, IOCTL_WRITE_MEMORY_BATCH,
    MAX_BATCH_ENTRIES, MAX_BATCH_WRITE_ENTRIES, MAX_DRIVER_TRANSFER_SIZE,
};
use zerocopy::{FromBytes, Immutable, KnownLayout};

use ks_core::crypto::{self, xor_u64};
use ks_core::protocol::READ_RESPONSE_SIZE;

use core::cell::UnsafeCell;

struct IoctlKey(UnsafeCell<[u8; crypto::KEY_LEN]>);
unsafe impl Sync for IoctlKey {}
static IOCTL_KEY: IoctlKey = IoctlKey(UnsafeCell::new(crypto::FALLBACK_KEY));

#[inline]
fn crypt(value: u64, field: usize) -> u64 {
    unsafe { xor_u64(value, &*IOCTL_KEY.0.get(), field) }
}

unsafe fn request_from_system<'a, T>(
    system: *const u8,
    input_length: u32,
) -> Result<&'a T, NTSTATUS>
where
    T: FromBytes + Immutable + KnownLayout,
{
    let bytes = core::slice::from_raw_parts(system, input_length as usize);
    T::ref_from_bytes(bytes).map_err(|_| STATUS_BUFFER_TOO_SMALL)
}

const DEVICE_NAME: &[u16] = &[
    b'\\' as u16,
    b'D' as u16,
    b'e' as u16,
    b'v' as u16,
    b'i' as u16,
    b'c' as u16,
    b'e' as u16,
    b'\\' as u16,
    b'K' as u16,
    b'e' as u16,
    b'r' as u16,
    b'n' as u16,
    b'e' as u16,
    b'l' as u16,
    b'S' as u16,
    b'c' as u16,
    b'r' as u16,
    b'i' as u16,
    b'p' as u16,
    b't' as u16,
    b'P' as u16,
    b'r' as u16,
    b'o' as u16,
    b'f' as u16,
    b'i' as u16,
    b'l' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
];
const DOS_NAME: &[u16] = &[
    b'\\' as u16,
    b'D' as u16,
    b'o' as u16,
    b's' as u16,
    b'D' as u16,
    b'e' as u16,
    b'v' as u16,
    b'i' as u16,
    b'c' as u16,
    b'e' as u16,
    b's' as u16,
    b'\\' as u16,
    b'K' as u16,
    b'e' as u16,
    b'r' as u16,
    b'n' as u16,
    b'e' as u16,
    b'l' as u16,
    b'S' as u16,
    b'c' as u16,
    b'r' as u16,
    b'i' as u16,
    b'p' as u16,
    b't' as u16,
    b'P' as u16,
    b'r' as u16,
    b'o' as u16,
    b'f' as u16,
    b'i' as u16,
    b'l' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
];

pub fn driver_entry(driver: *mut DRIVER_OBJECT, registry: *mut UNICODE_STRING) -> NTSTATUS {
    if driver.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    unsafe {
        let _ = ks_load_ioctl_key(registry, IOCTL_KEY.0.get() as *mut u8);
        let device_name = unicode_string(DEVICE_NAME);
        let dos_name = unicode_string(DOS_NAME);
        let mut device = ptr::null_mut();
        let status =
            ks_create_secure_device(driver, &device_name as *const _ as *mut _, &mut device);
        if !nt_success(status) {
            return status;
        }
        let status = IoCreateSymbolicLink(&dos_name, &device_name);
        if !nt_success(status) {
            IoDeleteDevice(device);
            return status;
        }
        (*driver).MajorFunction.fill(Some(dispatch_unsupported));
        (*driver).MajorFunction[IRP_MJ_CREATE as usize] = Some(dispatch_create_close);
        (*driver).MajorFunction[IRP_MJ_CLOSE as usize] = Some(dispatch_create_close);
        (*driver).MajorFunction[IRP_MJ_DEVICE_CONTROL as usize] = Some(dispatch_device_control);
        (*driver).DriverUnload = Some(crate::KsDriverUnload);
        (*device).Flags &= !DO_DEVICE_INITIALIZING;
    }
    STATUS_SUCCESS
}

pub fn driver_unload(_driver: *mut DRIVER_OBJECT) {
    unsafe {
        let dos_name = unicode_string(DOS_NAME);
        IoDeleteSymbolicLink(&dos_name);
        // In production retain the device pointer in DriverExtension and delete
        // exactly that object. This sample has one device and uses DRIVER_OBJECT.
        // The WDK helper below obtains the device list without dereferencing it.
        delete_first_device(_driver);
    }
}

unsafe fn delete_first_device(driver: *mut DRIVER_OBJECT) {
    let device = (*driver).DeviceObject;
    if !device.is_null() {
        IoDeleteDevice(device);
    }
}

unsafe extern "system" fn dispatch_unsupported(
    _device: *const DEVICE_OBJECT,
    irp: *mut IRP,
) -> NTSTATUS {
    if irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    unsafe { complete(irp, STATUS_INVALID_DEVICE_REQUEST, 0) }
}

unsafe extern "system" fn dispatch_create_close(
    _device: *const DEVICE_OBJECT,
    irp: *mut IRP,
) -> NTSTATUS {
    if irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let status = unsafe { ks_authorize_device_request(irp) };
    unsafe { complete(irp, status, 0) }
}

unsafe extern "system" fn dispatch_device_control(
    device: *const DEVICE_OBJECT,
    irp: *mut IRP,
) -> NTSTATUS {
    if device.is_null() || irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    unsafe {
        let authorization = ks_authorize_device_request(irp);
        if !nt_success(authorization) {
            return complete(irp, authorization, 0);
        }
        let ioctl_code = ks_get_ioctl_code(irp);
        if ioctl_code == IOCTL_PING {
            return complete(irp, STATUS_SUCCESS, 0);
        }
        let input_length = ks_get_input_buffer_length(irp);
        let output_length = ks_get_output_buffer_length(irp);
        let system = ks_get_system_buffer(irp) as *mut u8;
        if system.is_null() {
            return complete(irp, STATUS_INVALID_PARAMETER, 0);
        }
        let (status, information) = match ioctl_code {
            IOCTL_READ_MEMORY => {
                if input_length < mem::size_of::<MemoryReadRequest>() as u32
                    || output_length < READ_RESPONSE_SIZE as u32
                {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryReadRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    let address = crypt(request.address, crypto::ADDRESS);
                    if process_id == 0
                        || address == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        read_ioctl(
                            &request,
                            system,
                            output_length as usize,
                            move |_, _, data| {
                                memory::read_process_memory(process_id, address, data)
                            },
                        )
                    }
                }
            }
            IOCTL_READ_MEMORY_MDL => {
                if input_length < mem::size_of::<MemoryReadRequest>() as u32
                    || output_length < READ_RESPONSE_SIZE as u32
                {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryReadRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    let address = crypt(request.address, crypto::ADDRESS);
                    if process_id == 0
                        || address == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        read_ioctl(
                            &request,
                            system,
                            output_length as usize,
                            move |_, _, data| {
                                memory::read_process_memory_mdl(process_id, address, data)
                            },
                        )
                    }
                }
            }
            IOCTL_WRITE_MEMORY => {
                if input_length < mem::size_of::<MemoryWriteRequest>() as u32 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryWriteRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    let address = crypt(request.address, crypto::ADDRESS);
                    if process_id == 0
                        || address == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        let result = memory::write_process_memory(
                            process_id,
                            address,
                            &request.data[..request.size as usize],
                        );
                        match result {
                            Ok(()) => (STATUS_SUCCESS, 0),
                            Err(status) => (status, 0),
                        }
                    }
                }
            }
            IOCTL_WRITE_MEMORY_MDL => {
                if input_length < mem::size_of::<MemoryWriteRequest>() as u32 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryWriteRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    let address = crypt(request.address, crypto::ADDRESS);
                    if process_id == 0
                        || address == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        let result = memory::write_process_memory_mdl(
                            process_id,
                            address,
                            &request.data[..request.size as usize],
                        );
                        match result {
                            Ok(()) => (STATUS_SUCCESS, 0),
                            Err(status) => (status, 0),
                        }
                    }
                }
            }
            IOCTL_GET_PROCESS_BASE => {
                if input_length < mem::size_of::<ProcessBaseRequest>() as u32 || output_length < 8 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    process_base_ioctl(system)
                }
            }
            IOCTL_READ_MEMORY_RVA => {
                if input_length < mem::size_of::<MemoryRvaReadRequest>() as u32
                    || output_length < READ_RESPONSE_SIZE as u32
                {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryRvaReadRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    if process_id == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        read_rva_ioctl(
                            &request,
                            system,
                            output_length as usize,
                            move |_, address, data| {
                                memory::read_process_memory(process_id, address, data)
                            },
                        )
                    }
                }
            }
            IOCTL_READ_MEMORY_MDL_RVA => {
                if input_length < mem::size_of::<MemoryRvaReadRequest>() as u32
                    || output_length < READ_RESPONSE_SIZE as u32
                {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryRvaReadRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    if process_id == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        read_rva_ioctl(
                            &request,
                            system,
                            output_length as usize,
                            move |_, address, data| {
                                memory::read_process_memory_mdl(process_id, address, data)
                            },
                        )
                    }
                }
            }
            IOCTL_WRITE_MEMORY_RVA => {
                if input_length < mem::size_of::<MemoryRvaWriteRequest>() as u32 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryRvaWriteRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    if process_id == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        write_rva_ioctl(&request, move |_, address, data| {
                            memory::write_process_memory(process_id, address, data)
                        })
                    }
                }
            }
            IOCTL_WRITE_MEMORY_MDL_RVA => {
                if input_length < mem::size_of::<MemoryRvaWriteRequest>() as u32 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let request =
                        match request_from_system::<MemoryRvaWriteRequest>(system, input_length) {
                            Ok(request) => request,
                            Err(status) => return complete(irp, status, 0),
                        };
                    let process_id = crypt(request.process_id, crypto::PID);
                    if process_id == 0
                        || request.size == 0
                        || request.size > MAX_DRIVER_TRANSFER_SIZE as u64
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        write_rva_ioctl(&request, move |_, address, data| {
                            memory::write_process_memory_mdl(process_id, address, data)
                        })
                    }
                }
            }
            IOCTL_BATCH_READ_MEMORY => {
                if input_length < 16 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let pid = crypt(ptr::read_unaligned(system as *const u64), crypto::PID);
                    let size = ptr::read_unaligned(system.add(8) as *const u32);
                    let count = ptr::read_unaligned(system.add(12) as *const u32) as usize;
                    let input_size = count.checked_mul(8).and_then(|value| value.checked_add(16));
                    let output_size = (size as usize).checked_mul(count);
                    if pid == 0
                        || count == 0
                        || count > MAX_BATCH_ENTRIES
                        || size == 0
                        || size > MAX_DRIVER_TRANSFER_SIZE as u32
                        || input_size.is_none()
                        || output_size.is_none()
                        || input_size.is_some_and(|value| input_length < value as u32)
                        || output_size.is_some_and(|value| (output_length as usize) < value)
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        let output_size = output_size.unwrap();
                        let mut entries = [(0u64, 0u32); MAX_BATCH_ENTRIES];
                        for i in 0..count {
                            let base = 16 + i * 8;
                            let addr = crypt(
                                ptr::read_unaligned(system.add(base) as *const u64),
                                crypto::ADDRESS,
                            );
                            entries[i] = (addr, size);
                        }
                        match memory::batch_read_process_memory(
                            pid,
                            &entries[..count],
                            core::slice::from_raw_parts_mut(system, output_size),
                        ) {
                            Ok(bytes_written) => (STATUS_SUCCESS, bytes_written),
                            Err(status) => (status, 0),
                        }
                    }
                }
            }
            IOCTL_TRAVERSE_POINTER_CHAIN => {
                if input_length < 16 || output_length < 8 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let pid = crypt(ptr::read_unaligned(system as *const u64), crypto::PID);
                    let base = crypt(
                        ptr::read_unaligned(system.add(8) as *const u64),
                        crypto::BASE,
                    );
                    let count = if input_length >= 20 {
                        ptr::read_unaligned(system.add(16) as *const u32) as usize
                    } else {
                        0
                    };
                    let input_size = count.checked_mul(8).and_then(|value| value.checked_add(20));
                    if pid == 0
                        || base == 0
                        || count == 0
                        || count > 32
                        || input_size.is_none()
                        || input_size.is_some_and(|value| input_length < value as u32)
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        let mut offsets = [0u64; 32];
                        for i in 0..count {
                            offsets[i] = crypt(
                                ptr::read_unaligned(system.add(20 + i * 8) as *const u64),
                                crypto::OFFSET,
                            );
                        }
                        match memory::traverse_pointer_chain(pid, base, &offsets[..count]) {
                            Ok(result) => {
                                ptr::write_unaligned(
                                    system as *mut u64,
                                    crypt(result, crypto::RESULT),
                                );
                                (STATUS_SUCCESS, 8)
                            }
                            Err(status) => (status, 0),
                        }
                    }
                }
            }
            IOCTL_WRITE_MEMORY_BATCH => {
                if input_length < 8 || output_length < 4 {
                    (STATUS_BUFFER_TOO_SMALL, 0)
                } else {
                    let count = ptr::read_unaligned(system as *const u32) as usize;
                    if count == 0
                        || count > MAX_BATCH_WRITE_ENTRIES
                        || output_length < (4 + count * 4) as u32
                    {
                        (STATUS_INVALID_PARAMETER, 0)
                    } else {
                        // Per-entry results are written back through the
                        // METHOD_BUFFERED system buffer, which the I/O
                        // manager copies to the caller on completion. Every
                        // slot starts INVALID: entries that fail parsing or
                        // that the write pass never reaches report why.
                        let statuses = system as *mut NTSTATUS;
                        ptr::write_unaligned(statuses, count as i32);
                        for index in 0..count {
                            ptr::write_unaligned(statuses.add(1 + index), STATUS_INVALID_PARAMETER);
                        }
                        // Parse pass: validate every boundary up front so
                        // the write pass can run from a prepared array.
                        let mut entries = [BatchWriteEntry {
                            process_id: 0,
                            address: 0,
                            data: ptr::null(),
                            len: 0,
                        }; MAX_BATCH_WRITE_ENTRIES];
                        let mut parsed = 0usize;
                        let mut offset = 8usize;
                        let input_end = input_length as usize;
                        for slot in entries.iter_mut().take(count) {
                            let Some(header) = offset.checked_add(24) else {
                                break;
                            };
                            if header > input_end {
                                break;
                            }
                            let process_id = crypt(
                                ptr::read_unaligned(system.add(offset) as *const u64),
                                crypto::PID,
                            );
                            let address = crypt(
                                ptr::read_unaligned(system.add(offset + 8) as *const u64),
                                crypto::ADDRESS,
                            );
                            let size =
                                ptr::read_unaligned(system.add(offset + 16) as *const u32) as usize;
                            offset = header;
                            let Some(end) = offset.checked_add(size) else {
                                break;
                            };
                            if end > input_end
                                || process_id == 0
                                || address == 0
                                || size == 0
                                || size > MAX_DRIVER_TRANSFER_SIZE
                            {
                                break;
                            }
                            *slot = BatchWriteEntry {
                                process_id,
                                address,
                                data: system.add(offset),
                                len: size,
                            };
                            offset = end;
                            parsed += 1;
                        }
                        // Write pass: one process lookup per distinct PID.
                        let out = core::slice::from_raw_parts_mut(statuses.add(1), parsed);
                        // Safety: entry data pointers point into the system
                        // buffer, which stays valid for this call.
                        memory::batch_write_process_memory(&entries[..parsed], out);
                        (STATUS_SUCCESS, (4 + count * 4) as usize)
                    }
                }
            }
            _ => (STATUS_INVALID_DEVICE_REQUEST, 0),
        };
        complete(irp, status, information)
    }
}

unsafe fn resolve_base(pid: u64) -> Result<u64, NTSTATUS> {
    let Some(get_base) = resolve_process_base_routine() else {
        return Err(STATUS_INVALID_DEVICE_REQUEST);
    };
    let mut process = 0isize;
    let status = PsLookupProcessByProcessId(pid_handle(pid), &mut process);
    if !nt_success(status) || process == 0 {
        return Err(status);
    }
    let base = get_base(process as Pvoid) as u64;
    ObDereferenceObject(process as Pvoid);
    if base == 0 {
        Err(STATUS_INVALID_PARAMETER)
    } else {
        Ok(base)
    }
}

unsafe fn read_rva_ioctl(
    request: &MemoryRvaReadRequest,
    output: *mut u8,
    output_len: usize,
    reader: impl FnOnce(u64, u64, &mut [u8]) -> Result<(), NTSTATUS>,
) -> (NTSTATUS, usize) {
    let process_id = crypt(request.process_id, crypto::PID);
    let relative_address = crypt(request.relative_address, crypto::RVA);
    let base = match resolve_base(process_id) {
        Ok(base) => base,
        Err(status) => return (status, 0),
    };
    let Some(address) = base.checked_add(relative_address) else {
        return (STATUS_INVALID_PARAMETER, 0);
    };
    let absolute = MemoryReadRequest {
        process_id,
        address,
        size: request.size,
    };
    read_ioctl(&absolute, output, output_len, reader)
}

unsafe fn write_rva_ioctl(
    request: &MemoryRvaWriteRequest,
    writer: impl FnOnce(u64, u64, &[u8]) -> Result<(), NTSTATUS>,
) -> (NTSTATUS, usize) {
    let process_id = crypt(request.process_id, crypto::PID);
    let relative_address = crypt(request.relative_address, crypto::RVA);
    let base = match resolve_base(process_id) {
        Ok(base) => base,
        Err(status) => return (status, 0),
    };
    let Some(address) = base.checked_add(relative_address) else {
        return (STATUS_INVALID_PARAMETER, 0);
    };
    match writer(process_id, address, &request.data[..request.size as usize]) {
        Ok(()) => (STATUS_SUCCESS, 0),
        Err(status) => (status, 0),
    }
}

type GetProcessSectionBaseAddress = unsafe extern "system" fn(Pvoid) -> Pvoid;

unsafe fn process_base_ioctl(system: *mut u8) -> (NTSTATUS, usize) {
    let Some(get_base) = resolve_process_base_routine() else {
        return (STATUS_INVALID_DEVICE_REQUEST, 0);
    };
    let request = match request_from_system::<ProcessBaseRequest>(
        system,
        mem::size_of::<ProcessBaseRequest>() as u32,
    ) {
        Ok(request) => request,
        Err(status) => return (status, 0),
    };
    let process_id = crypt(request.process_id, crypto::PID);
    if process_id == 0 {
        return (STATUS_INVALID_PARAMETER, 0);
    }
    let mut process = 0isize;
    let status = PsLookupProcessByProcessId(pid_handle(process_id), &mut process);
    if !nt_success(status) || process == 0 {
        return (status, 0);
    }
    let base = get_base(process as Pvoid) as u64;
    ObDereferenceObject(process as Pvoid);
    if base == 0 {
        return (STATUS_INVALID_PARAMETER, 0);
    }
    ptr::write_unaligned(system as *mut u64, crypt(base, crypto::BASE));
    (STATUS_SUCCESS, 8)
}

unsafe fn resolve_process_base_routine() -> Option<GetProcessSectionBaseAddress> {
    let name = b"PsGetProcessSectionBaseAddress\0";
    let mut wide = [0u16; 40];
    let length = name.len().checked_sub(1)?;
    if length >= wide.len() {
        return None;
    }
    for (index, byte) in name[..length].iter().copied().enumerate() {
        wide[index] = byte as u16;
    }
    let mut unicode = UNICODE_STRING {
        Length: (length * 2) as u16,
        MaximumLength: (length * 2) as u16,
        Buffer: wide.as_mut_ptr(),
    };
    let address = MmGetSystemRoutineAddress(&mut unicode);
    (!address.is_null()).then(|| mem::transmute(address))
}

unsafe fn read_ioctl(
    request: &MemoryReadRequest,
    output: *mut u8,
    output_len: usize,
    reader: impl FnOnce(u64, u64, &mut [u8]) -> Result<(), NTSTATUS>,
) -> (NTSTATUS, usize) {
    // Write directly into the I/O manager system buffer to avoid a 4 KB
    // stack allocation. The output buffer is already allocated for the IRP.
    // Layout: [4-byte magic][1-byte success][3-byte pad][data up to 4096][4-byte error]
    let mut success = false;
    let mut error_code = 0u32;
    let data_slice = core::slice::from_raw_parts_mut(output.add(8), request.size as usize);
    match reader(request.process_id, request.address, data_slice) {
        Ok(()) => {
            success = true;
        }
        Err(status) => {
            error_code = status as u32;
        }
    }
    ptr::write_unaligned(output as *mut u32, 0x4B53_5231);
    *output.add(4) = success as u8;
    ptr::write_unaligned(
        output.add(4 + 1 + 3 + MAX_DRIVER_TRANSFER_SIZE) as *mut u32,
        error_code,
    );
    // The IOCTL transport succeeded even when the target memory operation did
    // not. The operation status is carried in error_code so user mode does not
    // lose the original NTSTATUS through GetLastError mapping.
    (STATUS_SUCCESS, READ_RESPONSE_SIZE.min(output_len))
}

unsafe fn complete(irp: *mut IRP, status: NTSTATUS, information: usize) -> NTSTATUS {
    ks_complete_irp(irp, status, information, IO_NO_INCREMENT as i8);
    status
}
