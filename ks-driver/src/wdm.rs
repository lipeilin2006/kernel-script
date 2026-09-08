#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    non_upper_case_globals
)]

use core::{ffi::c_void, ptr};

pub use windows_sys::Wdk::Foundation::{
    DEVICE_OBJECT, DRIVER_DISPATCH, DRIVER_OBJECT, IO_STACK_LOCATION, IRP, MDL,
};
pub use windows_sys::Wdk::Storage::FileSystem::IO_NO_INCREMENT;
pub use windows_sys::Wdk::Storage::FileSystem::{
    KeStackAttachProcess, KeUnstackDetachProcess, PsLookupProcessByProcessId,
    DO_DEVICE_INITIALIZING, KAPC_STATE,
};
pub use windows_sys::Wdk::System::SystemServices::{
    IoAllocateMdl, IoCreateSymbolicLink, IoDeleteDevice, IoDeleteSymbolicLink, IoFreeMdl,
    IoReadAccess, KernelMode, MmCached, MmMapLockedPagesSpecifyCache, MmUnlockPages,
    MmUnmapLockedPages, NormalPagePriority, IRP_MJ_CLOSE, IRP_MJ_CREATE, IRP_MJ_DEVICE_CONTROL,
};
pub use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, STATUS_ACCESS_VIOLATION, STATUS_BUFFER_TOO_SMALL,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER,
    STATUS_SUCCESS, UNICODE_STRING,
};

pub type Pvoid = *mut c_void;
pub type Dispatch = DRIVER_DISPATCH;
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

pub const fn unicode_string(value: &[u16]) -> UNICODE_STRING {
    UNICODE_STRING {
        Length: ((value.len() - 1) * 2) as u16,
        MaximumLength: (value.len() * 2) as u16,
        Buffer: value.as_ptr() as *mut u16,
    }
}

pub fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}
pub const fn pid_handle(pid: u64) -> HANDLE {
    pid as usize as HANDLE
}
pub const fn null<T>() -> *mut T {
    ptr::null_mut()
}

extern "system" {
    pub fn MmGetSystemRoutineAddress(name: *mut UNICODE_STRING) -> Pvoid;
    pub fn ks_authorize_device_request(irp: *mut IRP) -> NTSTATUS;
    pub fn ks_create_secure_device(
        driver: *mut DRIVER_OBJECT,
        device_name: *mut UNICODE_STRING,
        device: *mut *mut DEVICE_OBJECT,
    ) -> NTSTATUS;
    // These are project-local wrappers implemented in seh_shim.c.
    pub fn ks_get_current_irp_stack_location(irp: *mut IRP) -> *mut IO_STACK_LOCATION;
    pub fn ks_get_ioctl_code(irp: *mut IRP) -> u32;
    pub fn ks_get_input_buffer_length(irp: *mut IRP) -> u32;
    pub fn ks_get_output_buffer_length(irp: *mut IRP) -> u32;
    pub fn ks_get_system_buffer(irp: *mut IRP) -> Pvoid;
    pub fn ks_copy_process_memory(
        source_process: isize,
        source_address: Pvoid,
        target_address: Pvoid,
        size: usize,
        copied: *mut usize,
    ) -> NTSTATUS;
    pub fn ks_write_process_memory(
        target_process: isize,
        source_address: Pvoid,
        target_address: Pvoid,
        size: usize,
        copied: *mut usize,
    ) -> NTSTATUS;
    pub fn ks_get_system_address_for_mdl_safe(mdl: *mut MDL, priority: u32) -> Pvoid;
    pub fn ks_probe_and_lock_pages(mdl: *mut MDL, mode: i8, access: i32) -> NTSTATUS;

    // These are implemented in the WDK C shim because the WDK exposes the
    // completion operation as a macro rather than a stable Rust symbol.
    pub fn ks_complete_irp(irp: *mut IRP, status: NTSTATUS, information: usize, priority: i8);
    pub fn ObDereferenceObject(object: Pvoid);
}
