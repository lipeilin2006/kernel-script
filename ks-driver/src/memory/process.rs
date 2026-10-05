use core::{ffi::c_void, ptr};

use crate::wdm::*;
use ks_core::protocol::{BatchWriteItem, MAX_DRIVER_TRANSFER_SIZE};

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
        unsafe { KeUnstackDetachProcess(&apc_state) };
        return Err(STATUS_INSUFFICIENT_RESOURCES);
    }
    // This function is implemented in a tiny WDK/MSVC shim using
    // __try/__except around MmProbeAndLockPages. Rust cannot catch SEH with
    // catch_unwind, and allowing an exception across Rust frames is invalid.
    // The probe mode MUST be UserMode: for a user VA, UserMode probing
    // raises STATUS_ACCESS_VIOLATION on failure, which the shim catches;
    // KernelMode probing asserts the caller already validated the pages and
    // bugchecks (PAGE_FAULT_IN_NONPAGED_AREA) instead of raising.
    let status = unsafe { ks_probe_and_lock_pages(mdl, UserMode as i8, IoReadAccess) };
    unsafe { KeUnstackDetachProcess(&apc_state) };
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

/// One prepared batch-write entry. `data` borrows straight out of the ring,
/// which stays mapped for the whole request.
///
/// Writes every entry in one pass, resolving each distinct process only
/// once: `PsLookupProcessByProcessId` per entry dominated the batch cost
/// when all locks target the same game. Entries with invalid parameters or
/// a failed lookup are skipped and their status slot reports why.
pub fn batch_write_process_memory(writes: &[BatchWriteItem<'_>], statuses: &mut [NTSTATUS]) {
    let mut cached: Option<(u64, ProcessRef)> = None;
    for (item, slot) in writes.iter().zip(statuses.iter_mut()) {
        if item.pid == 0
            || item.address == 0
            || item.data.is_empty()
            || item.data.len() > MAX_DRIVER_TRANSFER_SIZE
        {
            *slot = STATUS_INVALID_PARAMETER;
            continue;
        }
        let cached_same = matches!(
            cached.as_ref(),
            Some((pid, _)) if *pid == item.pid
        );
        if !cached_same {
            match lookup(item.pid) {
                Ok(process) => cached = Some((item.pid, process)),
                Err(status) => {
                    cached = None;
                    *slot = status;
                    continue;
                }
            }
        }
        let Some((_, process)) = cached.as_ref() else {
            *slot = STATUS_INVALID_PARAMETER;
            continue;
        };
        let process = process.0;
        let mut copied = 0usize;
        let status = unsafe {
            ks_write_process_memory(
                process,
                item.data.as_ptr() as Pvoid,
                item.address as Pvoid,
                item.data.len(),
                &mut copied,
            )
        };
        *slot = if !nt_success(status) {
            status
        } else if copied == item.data.len() {
            STATUS_SUCCESS
        } else {
            STATUS_ACCESS_VIOLATION
        };
    }
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
/// process lookup. Each address reads `size` bytes into the output buffer
/// contiguously (no size prefix, no padding).
///
/// Invalid addresses (null or unreadable) are skipped and zero-filled in the
/// output buffer so that valid entries are still returned.
///
/// Returns the total number of bytes written to `output`.
pub fn batch_read_process_memory(
    process_id: u64,
    addresses: &[u64],
    size: usize,
    output: &mut [u8],
) -> Result<usize, NTSTATUS> {
    if addresses.is_empty() || process_id == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let total = addresses
        .len()
        .checked_mul(size)
        .ok_or(STATUS_INTEGER_OVERFLOW)?;
    if output.len() < total {
        return Err(STATUS_BUFFER_TOO_SMALL);
    }
    let process = lookup(process_id)?;
    for (index, &address) in addresses.iter().enumerate() {
        let data_slice = &mut output[index * size..(index + 1) * size];
        if address == 0 {
            data_slice.fill(0);
            continue;
        }
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
        if !nt_success(status) || copied != size {
            data_slice.fill(0);
        }
    }
    Ok(total)
}

/// Walk a pointer chain in a target process. Starting from `base`, read a u64
/// pointer at `base + offsets[0]`, then read at `result + offsets[1]`, etc.
///
/// Returns the final address, or 0 if any pointer is null or unreadable.
pub fn traverse_pointer_chain(
    process_id: u64,
    base: u64,
    offsets: &[u64],
) -> Result<u64, NTSTATUS> {
    if process_id == 0 || offsets.is_empty() {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let process = lookup(process_id)?;
    let mut current = base;
    for &offset in offsets {
        if current == 0 {
            return Err(STATUS_INVALID_ADDRESS);
        }
        let Some(target) = current.checked_add(offset) else {
            return Err(STATUS_INTEGER_OVERFLOW);
        };
        let mut ptr_value: u64 = 0;
        let mut copied = 0usize;
        let status = unsafe {
            ks_copy_process_memory(
                process.0,
                target as Pvoid,
                &mut ptr_value as *mut u64 as Pvoid,
                8,
                &mut copied,
            )
        };
        if !nt_success(status) || copied != 8 {
            return Err(if nt_success(status) {
                STATUS_ACCESS_VIOLATION
            } else {
                status
            });
        }
        current = ptr_value;
    }
    Ok(current)
}
