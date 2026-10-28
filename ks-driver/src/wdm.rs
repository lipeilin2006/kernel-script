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
    ExAcquireFastMutex, ExFreePoolWithTag, ExReleaseFastMutex, Executive, IoAllocateMdl, IoFreeMdl,
    IoReadAccess, KeInitializeEvent, KeSetPriorityThread, KeWaitForSingleObject, KernelMode,
    MmCached, MmGetSystemRoutineAddress, MmMapLockedPagesSpecifyCache, MmMapViewInSystemSpace,
    MmUnlockPages, MmUnmapLockedPages, MmUnmapViewInSystemSpace, NormalPagePriority,
    ObReferenceObjectByHandle, PsTerminateSystemThread, UserMode,
};
pub use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, STATUS_ACCESS_DENIED, STATUS_ACCESS_VIOLATION,
    STATUS_BUFFER_TOO_SMALL, STATUS_INSUFFICIENT_RESOURCES, STATUS_INTEGER_OVERFLOW,
    STATUS_INVALID_ADDRESS, STATUS_INVALID_PAGE_PROTECTION, STATUS_INVALID_PARAMETER,
    STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_QUOTA_EXCEEDED, STATUS_SUCCESS, UNICODE_STRING,
};
pub use windows_sys::Win32::Security::{ACL, ACL_REVISION, PSID, SECURITY_DESCRIPTOR};
pub use windows_sys::Win32::System::Kernel::{SynchronizationEvent, EVENT_TYPE};

pub type Pvoid = *mut c_void;

/// Kernel-mode only: bypasses the object DACL, so only the driver may use it.
pub const OBJ_KERNEL_HANDLE: u32 = 0x0000_0200;
/// Read/write, non-executable backing for the ring section.
pub const PAGE_READWRITE: u32 = 0x0000_0004;
/// Executable protection applied to driver-owned shellcode buffers.
pub const PAGE_EXECUTE_READWRITE: u32 = 0x0000_0040;
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

/// Pseudo-handle naming the current process. [`crate::memory::allocate`]
/// uses it while attached to the target, so the allocation lands in the
/// target's address space.
pub const NT_CURRENT_PROCESS: HANDLE = -1isize as HANDLE;
/// Commit the backing pages of an allocation.
pub const MEM_COMMIT: u32 = 0x1000;
/// Reserve (without committing) the address range.
pub const MEM_RESERVE: u32 = 0x2000;
/// Release a whole allocation; `ZwFreeVirtualMemory` then requires size 0.
pub const MEM_RELEASE: u32 = 0x8000;
/// `PAGE_TARGETS_NO_UPDATE`: a `ZwProtectVirtualMemory` *modifier* that
/// keeps the region's CFG bitmap entries as they are (a plain protection
/// change re-marks them valid indirect-call targets). Not a protection —
/// which is why it lives here instead of beside the protocol's `PROT_*`.
pub const PAGE_TARGETS_NO_UPDATE: u32 = 0x4000_0000;
/// Handle access `ZwProtectVirtualMemory` needs on the target process.
pub const PROCESS_VM_OPERATION: u32 = 0x0000_0008;
/// Handle access `ZwQueryVirtualMemory` needs on the target process.
pub const PROCESS_QUERY_INFORMATION: u32 = 0x0000_0400;
/// `MemoryBasicInformation`: the `ZwQueryVirtualMemory` information
/// class that fills a `MEMORY_BASIC_INFORMATION`.
pub const MEMORY_BASIC_INFORMATION: u32 = 0;

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

    /// Freezes every thread of the process (`ntoskrnl` export, undocumented
    /// but stable since Windows 7). The return value is ignored by the
    /// caller; see `suspend.rs` for the contract.
    pub fn PsSuspendProcess(process: Pvoid) -> NTSTATUS;

    /// Continues a process frozen by [`PsSuspendProcess`].
    pub fn PsResumeProcess(process: Pvoid) -> NTSTATUS;

    /// Executable non-paged pool (`ExAllocatePool2` with
    /// `POOL_FLAG_NON_PAGED_EXECUTE`). Returns NULL when policy refuses
    /// executable pool; freed with [`ExFreePoolWithTag`].
    pub fn ExAllocatePool2(flags: u64, number_of_bytes: usize, tag: u32) -> Pvoid;

    /// Page-table probe used before patching kernel code: every page the
    /// hook write touches is checked first, so a bad address fails instead
    /// of bugchecking inside `MmProbeAndLockPages`.
    pub fn MmIsAddressValid(virtual_address: Pvoid) -> u8;

    /// Target-process virtual-memory allocation through the attached
    /// address space (`NT_CURRENT_PROCESS`). The kernel rounds `region_size`
    /// up to whole pages and writes the rounded size back.
    pub fn ZwAllocateVirtualMemory(
        process_handle: HANDLE,
        base_address: *mut Pvoid,
        zero_bits: usize,
        region_size: *mut usize,
        allocation_type: u32,
        page_protection: u32,
    ) -> NTSTATUS;
    /// Releases a region previously returned by
    /// [`ZwAllocateVirtualMemory`] (`MEM_RELEASE`, size 0).
    pub fn ZwFreeVirtualMemory(
        process_handle: HANDLE,
        base_address: *mut Pvoid,
        region_size: *mut usize,
        free_type: u32,
    ) -> NTSTATUS;
    /// Changes the protection of a committed region in the given process.
    /// The kernel rounds `base_address` down and `region_size` up to page
    /// boundaries and writes both back; `old_protection` receives the
    /// first page's previous protection only.
    pub fn ZwProtectVirtualMemory(
        process_handle: HANDLE,
        base_address: *mut Pvoid,
        region_size: *mut usize,
        new_protection: u32,
        old_protection: *mut u32,
    ) -> NTSTATUS;
    /// Fills `memory_information` for `base_address` according to
    /// `memory_information_class` (0 = `MemoryBasicInformation`).
    pub fn ZwQueryVirtualMemory(
        process_handle: HANDLE,
        base_address: Pvoid,
        memory_information_class: u32,
        memory_information: Pvoid,
        memory_information_length: usize,
        return_length: *mut usize,
    ) -> NTSTATUS;
    /// Opens a handle to an existing object. With `access_mode =
    /// KernelMode` and a null `passed_access_state` the requested access
    /// is granted outright — the object's DACL is never consulted — which
    /// is how the driver obtains a `PROCESS_VM_OPERATION` handle to a
    /// target it does not own.
    pub fn ObOpenObjectByPointer(
        object: Pvoid,
        handle_attributes: u32,
        passed_access_state: *mut c_void,
        desired_access: u32,
        object_type: *mut c_void,
        access_mode: i32,
        handle: *mut HANDLE,
    ) -> NTSTATUS;

    /// The `Process` object type, exported as **data**. `ntddk.h`
    /// declares it `extern POBJECT_TYPE *PsProcessType;` — one pointer
    /// *more* than the type itself — so the value handed to
    /// [`ObOpenObjectByPointer`] is `*PsProcessType` (a load), never the
    /// address of this cell.
    pub static mut PsProcessType: *mut *mut c_void;

    /// `DbgPrint` from ntoskrnl.exe: varargs printf into the kernel debug
    /// pipe. The format string is a NUL-terminated UTF-8 literal; the
    /// trace! macro is the only caller and passes width-matched scalars.
    pub fn DbgPrint(format: *const u8, ...) -> u32;
}

// Implemented in seh_shim.c: MmCopyVirtualMemory and MmProbeAndLockPages
// need an SEH boundary that Rust frames cannot provide. These stay in a
// plain extern block so the references resolve against the bundled shim
// object instead of a generated import table. Framework builds (plain
// `cargo check`/`cargo test`, no WDK) get non-functional Rust stubs below so
// the lib test binary links without the C object.
#[cfg(feature = "wdk")]
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

/// Framework-mode stub: never reached by a real driver image.
#[cfg(not(feature = "wdk"))]
pub unsafe extern "system" fn ks_copy_process_memory(
    _source_process: isize,
    _source_address: Pvoid,
    _target_address: Pvoid,
    _size: usize,
    _copied: *mut usize,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

/// Framework-mode stub: never reached by a real driver image.
#[cfg(not(feature = "wdk"))]
pub unsafe extern "system" fn ks_write_process_memory(
    _target_process: isize,
    _source_address: Pvoid,
    _target_address: Pvoid,
    _size: usize,
    _copied: *mut usize,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

/// Framework-mode stub: never reached by a real driver image.
#[cfg(not(feature = "wdk"))]
pub unsafe extern "system" fn ks_probe_and_lock_pages(
    _mdl: *mut MDL,
    _mode: i8,
    _access: i32,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
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
