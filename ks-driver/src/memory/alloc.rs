//! Target-process virtual-memory allocation.
//!
//! Both operations attach to the target (`KeStackAttachProcess`) and use
//! the `NT_CURRENT_PROCESS` pseudo-handle, so the kernel acts on the
//! target's own address space. The attach happens with no driver lock held
//! and at `PASSIVE_LEVEL` 鈥?the ring worker's only level 鈥?which is the
//! documented requirement for `KeStackAttachProcess`; the process
//! reference keeps the address space alive for the whole call.

use core::ptr;

use crate::wdm::*;
use ks_core::protocol::{valid_alloc_protect, MAX_ALLOC_SIZE};

use super::process::lookup;

/// Runs `operation` with the target process attached, detaching on every
/// path.
fn attached<T>(
    process_id: u64,
    operation: impl FnOnce() -> Result<T, NTSTATUS>,
) -> Result<T, NTSTATUS> {
    let process = lookup(process_id)?;
    let mut apc_state: KAPC_STATE = unsafe { core::mem::zeroed() };
    unsafe { KeStackAttachProcess(process.0, &mut apc_state) };
    let result = operation();
    unsafe { KeUnstackDetachProcess(&apc_state) };
    result
}

/// Reserves, commits and zeroes `size` bytes in `process_id` with
/// `protect` — `PAGE_READWRITE`, `PAGE_EXECUTE_READ` or
/// `PAGE_EXECUTE_READWRITE` (`PAGE_READONLY` is refused: the plain write
/// path cannot touch it). Returns the base address the kernel picked; the
/// size is rounded up to whole pages.
pub fn allocate(process_id: u64, size: usize, protect: u32) -> Result<u64, NTSTATUS> {
    if process_id == 0 || size == 0 || size > MAX_ALLOC_SIZE || !valid_alloc_protect(protect) {
        return Err(STATUS_INVALID_PARAMETER);
    }
    attached(process_id, || {
        let mut base: Pvoid = ptr::null_mut();
        let mut region_size = size;
        let status = unsafe {
            ZwAllocateVirtualMemory(
                NT_CURRENT_PROCESS,
                &mut base,
                0,
                &mut region_size,
                MEM_COMMIT | MEM_RESERVE,
                protect,
            )
        };
        if !nt_success(status) {
            return Err(status);
        }
        if base.is_null() {
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        Ok(base as u64)
    })
}

/// Releases the whole VAD that contains `address` (`MEM_RELEASE`).
///
/// `ZwFreeVirtualMemory` only requires the address to *be* an allocation
/// base — it does not check who allocated it, and this driver keeps no
/// ownership table, so an address belonging to the target itself (its
/// heap, a stack, a mapped view's base) is freed just as happily and that
/// mapping comes down. Treat `free` as a dangerous operation, not a
/// guarded one.
pub fn free(process_id: u64, address: u64) -> Result<(), NTSTATUS> {
    if process_id == 0 || address == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    attached(process_id, || {
        let mut base = address as Pvoid;
        // MEM_RELEASE frees the whole allocation and requires size 0.
        let mut region_size = 0usize;
        let status = unsafe {
            ZwFreeVirtualMemory(NT_CURRENT_PROCESS, &mut base, &mut region_size, MEM_RELEASE)
        };
        if nt_success(status) {
            Ok(())
        } else {
            Err(status)
        }
    })
}
