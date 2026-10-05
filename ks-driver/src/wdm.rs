#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    non_upper_case_globals
)]

use core::ffi::c_void;
use core::mem::size_of;

pub use windows_sys::Wdk::Foundation::{
    DRIVER_OBJECT, FAST_MUTEX, MDL, OBJECT_ATTRIBUTES, PKTHREAD,
};
pub use windows_sys::Wdk::Storage::FileSystem::{
    KeStackAttachProcess, KeUnstackDetachProcess, PsLookupProcessByProcessId, KAPC_STATE,
};
pub use windows_sys::Wdk::System::SystemServices::{
    ExAcquireFastMutex, ExReleaseFastMutex, Executive, IoAllocateMdl, IoFreeMdl, IoReadAccess,
    KeInitializeEvent, KeSetPriorityThread, KeWaitForSingleObject, KernelMode, MmCached,
    MmGetSystemRoutineAddress, MmMapLockedPagesSpecifyCache, MmMapViewInSystemSpace, MmUnlockPages,
    MmUnmapLockedPages, MmUnmapViewInSystemSpace, NormalPagePriority, ObReferenceObjectByHandle,
    PsTerminateSystemThread, UserMode,
};
pub use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, STATUS_ACCESS_VIOLATION, STATUS_BUFFER_TOO_SMALL,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_INTEGER_OVERFLOW, STATUS_INVALID_ADDRESS,
    STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_QUOTA_EXCEEDED, STATUS_SUCCESS, UNICODE_STRING,
};
pub use windows_sys::Win32::Security::{ACL, ACL_REVISION, PSID, SECURITY_DESCRIPTOR};
pub use windows_sys::Win32::System::Kernel::{SynchronizationEvent, EVENT_TYPE};

pub type Pvoid = *mut c_void;

/// Kernel-mode only: bypasses the object DACL, so only the driver may use it.
pub const OBJ_KERNEL_HANDLE: u32 = 0x0000_0200;
/// Read/write, non-executable backing for the ring section.
pub const PAGE_READWRITE: u32 = 0x0000_0004;
/// Commit the section's pages rather than reserving them.
pub const SEC_COMMIT: u32 = 0x0800_0000;
pub const SECTION_ALL_ACCESS: u32 = 0x000F_001F;
pub const EVENT_ALL_ACCESS: u32 = 0x001F_0003;
/// `SECURITY_DESCRIPTOR_REVISION`; `RtlCreateSecurityDescriptor` rejects
/// anything else.
pub const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
/// `GENERIC_ALL` for the section/event DACL entries.
pub const GENERIC_ALL_ACCESS: u32 = 0x1000_0000;
/// Handle access for the names registry key: the driver only writes values.
pub const KEY_SET_VALUE: u32 = 0x0000_0002;
/// `DELETE` access on a key handle; `ZwDeleteValueKey` requires it.
pub const KEY_DELETE: u32 = 0x0001_0000;
/// `REG_SZ` value type for the published object names.
pub const REG_SZ: u32 = 1;
/// `REG_DWORD` value type for the single-instance registry claim.
pub const REG_DWORD: u32 = 4;

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

/// `SECTION_BASIC_INFORMATION` as consumed by `ZwQuerySection` with
/// `SectionBasicInformation`.
#[repr(C)]
pub struct SectionBasicInformation {
    pub base_address: Pvoid,
    pub allocation_attributes: u32,
    pub maximum_size: i64,
}

pub type PkStartRoutine = unsafe extern "system" fn(*mut c_void);

#[link(name = "ntoskrnl.exe", kind = "raw-dylib", modifiers = "+verbatim")]
extern "system" {
    pub fn ZwCreateSection(
        section_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
        maximum_size: *const i64,
        section_page_protection: u32,
        allocation_attributes: u32,
        file_handle: HANDLE,
    ) -> NTSTATUS;
    pub fn ZwOpenSection(
        section_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;
    pub fn ZwQuerySection(
        section_handle: HANDLE,
        section_information_class: i32,
        section_information: Pvoid,
        section_information_length: u32,
        return_length: *mut u32,
    ) -> NTSTATUS;
    pub fn ZwCreateEvent(
        event_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
        event_type: EVENT_TYPE,
        initial_state: bool,
    ) -> NTSTATUS;
    pub fn ZwOpenEvent(
        event_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;
    pub fn ZwSetEvent(event_handle: HANDLE, previous_state: *mut i32) -> NTSTATUS;
    pub fn ZwWaitForSingleObject(handle: HANDLE, alertable: bool, timeout: *const i64) -> NTSTATUS;
    pub fn ZwClose(handle: HANDLE) -> NTSTATUS;
    pub fn RtlCreateSecurityDescriptor(
        security_descriptor: *mut SECURITY_DESCRIPTOR,
        revision: u32,
    ) -> NTSTATUS;
    pub fn RtlSetDaclSecurityDescriptor(
        security_descriptor: *mut SECURITY_DESCRIPTOR,
        dacl_present: bool,
        dacl: *const ACL,
        dacl_defaulted: bool,
    ) -> NTSTATUS;
    pub fn RtlCreateAcl(acl: *mut ACL, acl_length: u32, acl_revision: u32) -> NTSTATUS;
    pub fn RtlAddAccessAllowedAce(
        acl: *mut ACL,
        ace_revision: u32,
        access_mask: u32,
        sid: PSID,
    ) -> NTSTATUS;

    pub fn ZwCreateKey(
        key_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
        title_index: u32,
        class: *mut UNICODE_STRING,
        create_options: u32,
        disposition: *mut u32,
    ) -> NTSTATUS;
    /// Opens an existing key; unlike [`ZwCreateKey`] a missing key fails
    /// with `STATUS_OBJECT_NAME_NOT_FOUND` instead of being created — what
    /// every delete path needs, so cleanup can never recreate the key it
    /// is trying to erase.
    pub fn ZwOpenKey(
        key_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;
    pub fn ZwSetValueKey(
        key_handle: HANDLE,
        value_name: *const UNICODE_STRING,
        title_index: u32,
        value_type: u32,
        data: Pvoid,
        data_size: u32,
    ) -> NTSTATUS;
    pub fn ZwDeleteValueKey(key_handle: HANDLE, value_name: *const UNICODE_STRING) -> NTSTATUS;
    /// Deletes a registry key (with every value left in it) through a
    /// handle opened with [`KEY_DELETE`]; the key goes away once all
    /// handles to it are closed. Used by the teardown path to erase the
    /// publication this load made.
    pub fn ZwDeleteKey(key_handle: HANDLE) -> NTSTATUS;

    /// Kernel PRNG backing the startup-randomized ring object names: each
    /// call draws one `u32` and updates `seed` in place (ntoskrnl export
    /// `9DB`, not provided by `windows-sys`).
    pub fn RtlRandomEx(seed: *mut u32) -> u32;
    /// Interrupt-time source (`100 ns` units) for the PRNG seed; the out
    /// parameter receives the same value for callers that want both.
    pub fn KeQueryInterruptTimePrecise(last_time: *mut u64) -> u64;

    pub fn PsCreateSystemThread(
        thread_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const OBJECT_ATTRIBUTES,
        process_handle: HANDLE,
        client_id: *mut c_void,
        start_routine: PkStartRoutine,
        start_context: *const c_void,
    ) -> NTSTATUS;

    /// The calling thread's `ETHREAD`/`KTHREAD`, used to set the lock
    /// worker's priority from inside its own start routine. Not exported by
    /// `windows-sys`, so it lives in this `ntoskrnl.exe` block like every
    /// other kernel-only extern.
    pub fn PsGetCurrentThread() -> PKTHREAD;

    pub fn ObDereferenceObject(object: Pvoid);

    /// `DbgPrint` from ntoskrnl.exe: varargs printf into the kernel debug
    /// pipe. The format string is a NUL-terminated UTF-8 literal; the
    /// trace! macro is the only caller and passes width-matched scalars.
    pub fn DbgPrint(format: *const u8, ...) -> u32;
}

// Implemented in seh_shim.c: MmCopyVirtualMemory and MmProbeAndLockPages
// need an SEH boundary that Rust frames cannot provide. These stay in a
// plain extern block so the references resolve against the bundled shim
// object instead of a generated import table.
extern "system" {
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
    pub fn ks_probe_and_lock_pages(mdl: *mut MDL, mode: i8, access: i32) -> NTSTATUS;
}

/// Size of `OBJECT_ATTRIBUTES` as the kernel expects it in `Length`.
pub const OBJECT_ATTRIBUTES_LENGTH: u32 = size_of::<OBJECT_ATTRIBUTES>() as u32;

/// One-time initialisation of a `FAST_MUTEX`, mirroring the WDK's
/// `ExInitializeFastMutex` (`Count = 1`, no owner, no contention, reset
/// `SynchronizationEvent`). ntoskrnl does **not** export
/// `ExInitializeFastMutex` — the WDK ships it header-only — so importing
/// it fails driver load with error 127 /
/// `STATUS_ENTRYPOINT_NOT_FOUND`; the fields are set directly instead.
///
/// # Safety
///
/// `fast_mutex` must point to valid storage shared by no thread yet; call
/// it exactly once before the first acquire (a zeroed `Count` would
/// deadlock the first acquirer).
pub unsafe fn init_fast_mutex(fast_mutex: *mut FAST_MUTEX) {
    (*fast_mutex).Count = 1;
    (*fast_mutex).Owner = core::ptr::null_mut();
    (*fast_mutex).Contention = 0;
    KeInitializeEvent(&mut (*fast_mutex).Event, SynchronizationEvent, false);
}
