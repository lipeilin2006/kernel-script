use core::{ffi::c_void, ptr};

use crate::wdm::*;
use ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE;

/// RAII owner for the reference returned by PsLookupProcessByProcessId.
struct ProcessRef(isize);
impl Drop for ProcessRef {
    fn drop(&mut self) {
        unsafe {
            ObDereferenceObject(self.0 as Pvoid);
        }
    }
}

/// RAII owner for a locked MDL. The pages are unlocked before the MDL is freed.
struct LockedMdl(*mut MDL);
impl Drop for LockedMdl {
    fn drop(&mut self) {
        unsafe {
            MmUnlockPages(self.0);
            IoFreeMdl(self.0);
        }
    }
}

/// Access target-process pages through an MDL mapped into kernel space.
///
/// The user-mode mapping is attached, probed with READ access only (probing
/// write access raises for read-only pages), and locked. The kernel mapping
/// created afterwards bypasses the user-mode page protection, so read-only
/// and execute-only pages can be written. Writing the physical page touches
/// every process that shares it (image sections are shared).
fn mdl_access(
    process_id: u64,
    address: u64,
    size: usize,
    access: impl FnOnce(Pvoid),
) -> Result<(), NTSTATUS> {
    let process = lookup(process_id)?;
    let mut apc_state: KAPC_STATE = unsafe { core::mem::zeroed() };
    unsafe { KeStackAttachProcess(process.0, &mut apc_state) };
    let mdl = unsafe {
        IoAllocateMdl(
            address as *mut c_void,
            size as u32,
            false,
            false,
            ptr::null_mut(),
        )
    };
    if mdl.is_null() {
        unsafe { KeUnstackDetachProcess(&mut apc_state) };
        return Err(STATUS_INSUFFICIENT_RESOURCES);
    }
    // This function is implemented in a tiny WDK/MSVC shim using
    // __try/__except around MmProbeAndLockPages. Rust cannot catch SEH with
    // catch_unwind, and allowing an exception across Rust frames is invalid.
    let status = unsafe { ks_probe_and_lock_pages(mdl, KernelMode as i8, IoReadAccess) };
    unsafe { KeUnstackDetachProcess(&mut apc_state) };
    if !nt_success(status) {
        unsafe {
            IoFreeMdl(mdl);
        }
        return Err(status);
    }
    let locked = LockedMdl(mdl);
    let mapped = unsafe {
        MmMapLockedPagesSpecifyCache(
            locked.0,
            KernelMode as i8,
            MmCached,
            ptr::null(),
            0,
            NormalPagePriority as u32,
        )
    };
    if mapped.is_null() {
        return Err(STATUS_INSUFFICIENT_RESOURCES);
    }
    access(mapped);
    unsafe { MmUnmapLockedPages(mapped, locked.0) };
    Ok(())
}

/// Read target-process memory through an MDL kernel mapping.
pub fn read_process_memory_mdl(
    process_id: u64,
    address: u64,
    output: &mut [u8],
) -> Result<(), NTSTATUS> {
    if output.is_empty() || output.len() > MAX_DRIVER_TRANSFER_SIZE || address == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    mdl_access(process_id, address, output.len(), |mapped| unsafe {
        ptr::copy_nonoverlapping(mapped as *const u8, output.as_mut_ptr(), output.len());
    })
}

/// Write target-process memory through an MDL kernel mapping. This bypasses
/// user-mode write protection on code pages and read-only sections.
pub fn write_process_memory_mdl(
    process_id: u64,
    address: u64,
    data: &[u8],
) -> Result<(), NTSTATUS> {
    if data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE || address == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    mdl_access(process_id, address, data.len(), |mapped| unsafe {
        ptr::copy_nonoverlapping(data.as_ptr(), mapped as *mut u8, data.len());
    })
}

pub fn read_process_memory(
    process_id: u64,
    address: u64,
    output: &mut [u8],
) -> Result<(), NTSTATUS> {
    if output.is_empty() || output.len() > MAX_DRIVER_TRANSFER_SIZE || address == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let process = lookup(process_id)?;
    let mut copied = 0usize;
    let status = unsafe {
        ks_copy_process_memory(
            process.0,
            address as Pvoid,
            output.as_mut_ptr() as Pvoid,
            output.len(),
            &mut copied,
        )
    };
    if nt_success(status) && copied == output.len() {
        Ok(())
    } else if nt_success(status) {
        Err(STATUS_ACCESS_VIOLATION)
    } else {
        Err(status)
    }
}
fn lookup(process_id: u64) -> Result<ProcessRef, NTSTATUS> {
    let mut process = 0isize;
    let status = unsafe { PsLookupProcessByProcessId(pid_handle(process_id), &mut process) };
    if !nt_success(status) || process == 0 {
        Err(status)
    } else {
        Ok(ProcessRef(process))
    }
}

pub fn write_process_memory(process_id: u64, address: u64, data: &[u8]) -> Result<(), NTSTATUS> {
    if data.is_empty() || data.len() > MAX_DRIVER_TRANSFER_SIZE || address == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let process = lookup(process_id)?;
    let mut copied = 0usize;
    let status = unsafe {
        ks_write_process_memory(
            process.0,
            data.as_ptr() as Pvoid,
            address as Pvoid,
            data.len(),
            &mut copied,
        )
    };
    if nt_success(status) && copied == data.len() {
        Ok(())
    } else if nt_success(status) {
        Err(STATUS_ACCESS_VIOLATION)
    } else {
        Err(status)
    }
}

/// Batch-read multiple memory regions from a target process with a single
/// process lookup. Each entry reads `size` bytes from `address` into the
/// output buffer contiguously (no size prefix, no padding).
///
/// Returns the total number of bytes written to `output`, or the first
/// failing NTSTATUS.
pub fn batch_read_process_memory(
    process_id: u64,
    entries: &[(u64, u32)],
    output: &mut [u8],
) -> Result<usize, NTSTATUS> {
    if entries.is_empty() || process_id == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let process = lookup(process_id)?;
    let mut out_off = 0usize;
    for &(address, size) in entries {
        if address == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE as u32 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let size = size as usize;
        if out_off + size > output.len() {
            return Err(STATUS_BUFFER_TOO_SMALL);
        }
        let data_slice = &mut output[out_off..out_off + size];
        let mut copied = 0usize;
        let status = unsafe {
            ks_copy_process_memory(
                process.0,
                address as Pvoid,
                data_slice.as_mut_ptr() as Pvoid,
                size,
                &mut copied,
            )
        };
        if nt_success(status) && copied == size {
            out_off += size;
        } else if nt_success(status) {
            return Err(STATUS_ACCESS_VIOLATION);
        } else {
            return Err(status);
        }
    }
    Ok(out_off)
}
