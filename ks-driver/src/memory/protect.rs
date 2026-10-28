//! Page-protection changes and region queries for target processes.
//!
//! Both operations open a kernel handle to the target
//! ([`open_process`], `ObOpenObjectByPointer` with `KernelMode`) instead
//! of attaching: no `KAPC_STATE`, no detach-on-every-path bookkeeping,
//! and the RAII handle closes on every return. Everything runs on the
//! ring worker at `PASSIVE_LEVEL` with no driver lock held — the two
//! documented requirements for these system calls.

use ks_core::protocol::{valid_protect, RegionInfo, MAX_PROTECT_SIZE};

use crate::wdm::*;

use super::process::open_process;

/// Changes the protection of `[address, address + size)` in `process_id`
/// to `protect`. Returns `(first page's old protection, bytes the kernel
/// protected)`.
///
/// The kernel rounds `address` down and `size` up to page boundaries and
/// writes the rounded size back, so the caller does no page arithmetic
/// of its own. The whole span must already sit inside existing mappings:
/// a range crossing a free hole fails with the kernel's status, which
/// travels back verbatim — never as a success. Probe with [`query_region`]
/// first.
///
/// `cfg_no_update` attaches `PAGE_TARGETS_NO_UPDATE`, leaving the
/// region's existing CFG bitmap entries as they are instead of letting
/// the protection change re-mark them valid indirect-call targets.
///
/// Hard limits this function does **not** detect:
/// - Dynamic Code Policy / ACG flips a private page to executable and
///   fails with `STATUS_INVALID_PAGE_PROTECTION`; image pages are
///   usually exempt.
/// - HVCI/VBS accepts the change but still refuses the write or the
///   execution that follows.
/// - `MEM_IMAGE` pages changed to read-write trigger copy-on-write and
///   break file sharing for every process mapping that image;
///   `PROT_RX`/`PROT_RWX` do not.
/// - The returned old value covers the first page only — across several
///   VADs the pages may differ, so exact restoration means one call per
///   region as reported by [`query_region`].
/// - A kernel MDL mapping taken *before* this call (`MmMapLockedPages…`)
///   stays at the old protection: user PTEs and kernel mappings are
///   independent, so an MDL mapped earlier must be unmapped and remapped
///   to see the change.
/// - The kernel flushes the TLB itself; never add a manual
///   `KeFlushEntireTb` after this call.
pub fn protect(
    process_id: u64,
    address: u64,
    size: usize,
    protect: u32,
    cfg_no_update: bool,
) -> Result<(u32, usize), NTSTATUS> {
    if process_id == 0
        || address == 0
        || size == 0
        || size > MAX_PROTECT_SIZE
        || !valid_protect(protect)
    {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let handle = open_process(process_id, PROCESS_VM_OPERATION)?;
    let mut base = address as Pvoid;
    let mut len = size;
    let mut old = 0u32;
    let mut new = protect;
    if cfg_no_update {
        new |= PAGE_TARGETS_NO_UPDATE;
    }
    let status = unsafe { ZwProtectVirtualMemory(handle.0, &mut base, &mut len, new, &mut old) };
    if nt_success(status) {
        Ok((old, len))
    } else {
        Err(status)
    }
}

/// Describes the region containing `address` in `process_id`
/// (`ZwQueryVirtualMemory` with `MemoryBasicInformation`).
///
/// The system structure gained a `PartitionId` field on Windows 10 2004,
/// so it is spelled out here with the x64 offsets the kernel writes —
/// and those offsets are stable across the split: on pre-2004 builds the
/// field is four bytes of alignment padding and `region_size` lands on
/// the same offset either way. Only the stable fields move on to the
/// wire as [`RegionInfo`].
pub fn query_region(process_id: u64, address: u64) -> Result<RegionInfo, NTSTATUS> {
    if process_id == 0 || address == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let handle = open_process(process_id, PROCESS_QUERY_INFORMATION)?;

    /// `MEMORY_BASIC_INFORMATION` as `ZwQueryVirtualMemory` writes it on
    /// x64 (48 bytes).
    #[repr(C)]
    struct Mbi {
        base: u64,
        allocation_base: u64,
        allocation_protect: u32,
        _partition: u16,
        _pad: u16,
        region_size: u64,
        state: u32,
        protect: u32,
        typ: u32,
        _tail: u32,
    }

    let mut mbi = unsafe { core::mem::zeroed::<Mbi>() };
    let mut returned = 0usize;
    let status = unsafe {
        ZwQueryVirtualMemory(
            handle.0,
            address as Pvoid,
            MEMORY_BASIC_INFORMATION,
            &mut mbi as *mut Mbi as Pvoid,
            core::mem::size_of::<Mbi>(),
            &mut returned,
        )
    };
    if !nt_success(status) {
        return Err(status);
    }
    Ok(RegionInfo {
        base: mbi.base,
        allocation_base: mbi.allocation_base,
        region_size: mbi.region_size,
        state: mbi.state,
        protect: mbi.protect,
        allocation_protect: mbi.allocation_protect,
        typ: mbi.typ,
    })
}
