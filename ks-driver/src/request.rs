//! Decodes one ring request, runs it, and publishes the response.
//!
//! The request payload is decoded straight out of the ring (postcard borrows
//! from the request region), read results are written straight into the
//! response bulk region, and the header is the only place a status travels:
//! there is no staging buffer and no separate error channel.

use core::ffi::c_void;

use crate::memory;
use crate::wdm::*;
use ks_core::ring::{RingHeader, RESPONSE_META_SIZE, RING_VERSION, STATE_RESPONSE};
use ks_core::{
    decode_request, encode_response_meta, HookOp, Request, ResponseMeta, LOCK_LIST_ENTRY_SIZE,
    MAX_BATCH_WRITE_ENTRIES, MAX_CHAIN_OFFSETS, MAX_DRIVER_TRANSFER_SIZE,
};

/// Answer the request the client published as `STATE_REQUEST`.
///
/// On return the header carries `response_sequence`, `response_len`,
/// `status` and `STATE_RESPONSE`; the caller signals the response event.
/// Returns `false` when the request asked the worker to stop serving after
/// this response is published (`Request::Shutdown`).
pub fn process_request(header: &RingHeader, request: &[u8], response: &mut [u8]) -> bool {
    let sequence = header.sequence();
    let request_len = header.request_len() as usize;
    let mut keep_serving = true;
    let (status, meta) = if request_len == 0 || request_len > request.len() {
        crate::trace!("reject len=%lu", request_len as u32);
        (STATUS_INVALID_PARAMETER, ResponseMeta::default())
    } else {
        match decode_request(&request[..request_len]) {
            Err(_) => {
                crate::trace!("decode failed len=%lu", request_len as u32);
                (STATUS_INVALID_PARAMETER, ResponseMeta::default())
            }
            Ok(decoded) => {
                crate::trace!("decoded len=%lu", request_len as u32);
                keep_serving = !matches!(decoded, Request::Shutdown);
                execute(&decoded, response, header.client_pid())
            }
        }
    };

    let encoded = encode_response_meta(meta, response).is_ok();
    // A failed request still reports a meta slot so the client can decode a
    // zero record rather than guessing from `response_len` alone.
    let payload = if status >= 0 {
        meta.bulk_len as usize
    } else {
        0
    };
    let response_len = if encoded {
        (RESPONSE_META_SIZE + payload) as u32
    } else {
        0
    };

    header.set_response_sequence(sequence);
    header.set_response_len(response_len);
    header.set_status(status);
    header.store_state(STATE_RESPONSE);
    keep_serving
}

fn scalar(value: u64) -> ResponseMeta {
    ResponseMeta {
        bulk_len: 0,
        count: 0,
        value,
    }
}

fn failure() -> ResponseMeta {
    ResponseMeta::default()
}

/// `rva` resolves the address against the target image base, which is the
/// same resolution the former `*_RVA` IOCTLs performed in user mode's stead.
fn resolve_address(pid: u64, address: u64, rva: bool) -> Result<u64, NTSTATUS> {
    if !rva {
        return Ok(address);
    }
    let base = resolve_base(pid)?;
    base.checked_add(address).ok_or(STATUS_INVALID_PARAMETER)
}

type GetProcessSectionBaseAddress = unsafe extern "system" fn(Pvoid) -> Pvoid;

fn resolve_base(pid: u64) -> Result<u64, NTSTATUS> {
    let Some(get_base) = resolve_process_base_routine() else {
        return Err(STATUS_NOT_SUPPORTED);
    };
    let mut process = 0isize;
    let status = unsafe { PsLookupProcessByProcessId(pid_handle(pid), &mut process) };
    if !nt_success(status) || process == 0 {
        return Err(status);
    }
    let base = unsafe { get_base(process as Pvoid) } as u64;
    unsafe { ObDereferenceObject(process as Pvoid) };
    if base == 0 {
        Err(STATUS_INVALID_PARAMETER)
    } else {
        Ok(base)
    }
}

fn resolve_process_base_routine() -> Option<GetProcessSectionBaseAddress> {
    let name = b"PsGetProcessSectionBaseAddress\0";
    let mut wide = [0u16; 40];
    let length = name.len().checked_sub(1)?;
    if length >= wide.len() {
        return None;
    }
    for (index, byte) in name[..length].iter().copied().enumerate() {
        wide[index] = byte as u16;
    }
    let unicode = UNICODE_STRING {
        Length: (length * 2) as u16,
        MaximumLength: (length * 2) as u16,
        Buffer: wide.as_mut_ptr(),
    };
    let address = unsafe { MmGetSystemRoutineAddress(&unicode) };
    (!address.is_null()).then(|| unsafe { core::mem::transmute(address) })
}

fn execute(
    request: &Request<'_>,
    response: &mut [u8],
    client_pid: u32,
) -> (NTSTATUS, ResponseMeta) {
    // Publish the caller's pid for the hook write path: patching a user
    // process's executable memory suspends that process first, which must
    // skip a self-hook (see `hook::write_code`).
    crate::hook::set_client_pid(client_pid);
    let bulk = &mut response[RESPONSE_META_SIZE..];
    match request {
        Request::Ping => (STATUS_SUCCESS, scalar(RING_VERSION as u64)),
        Request::GetProcessBase { pid } => {
            crate::trace!("getbase pid=%llu", *pid);
            match resolve_base(*pid) {
                Ok(base) => (STATUS_SUCCESS, scalar(base)),
                Err(status) => (status, failure()),
            }
        }
        Request::Read {
            pid,
            address,
            size,
            rva,
            mdl,
        } => {
            crate::trace!(
                "read pid=%llu a=%p s=%lu mdl=%lu",
                *pid,
                *address as *mut c_void,
                *size,
                *mdl as u32
            );
            let size = *size as usize;
            if size == 0 || size > MAX_DRIVER_TRANSFER_SIZE || bulk.len() < size {
                return (STATUS_INVALID_PARAMETER, failure());
            }
            let address = match resolve_address(*pid, *address, *rva) {
                Ok(address) => address,
                Err(status) => return (status, failure()),
            };
            let output = &mut bulk[..size];
            let result = if *mdl {
                memory::read_process_memory_mdl(*pid, address, output)
            } else {
                memory::read_process_memory(*pid, address, output)
            };
            match result {
                Ok(()) => (
                    STATUS_SUCCESS,
                    ResponseMeta {
                        bulk_len: size as u32,
                        count: size as u32,
                        value: 0,
                    },
                ),
                Err(status) => (status, failure()),
            }
        }
        Request::Write {
            pid,
            address,
            data,
            rva,
            mdl,
        } => {
            crate::trace!(
                "write pid=%llu a=%p n=%lu mdl=%lu",
                *pid,
                *address as *mut c_void,
                data.len() as u32,
                *mdl as u32
            );
            let address = match resolve_address(*pid, *address, *rva) {
                Ok(address) => address,
                Err(status) => return (status, failure()),
            };
            let result = if *mdl {
                memory::write_process_memory_mdl(*pid, address, data)
            } else {
                memory::write_process_memory(*pid, address, data)
            };
            match result {
                Ok(()) => (STATUS_SUCCESS, failure()),
                Err(status) => (status, failure()),
            }
        }
        Request::BatchRead {
            pid,
            size,
            addresses,
        } => {
            crate::trace!(
                "batchread pid=%llu n=%lu s=%lu",
                *pid,
                addresses.len() as u32,
                *size
            );
            let count = addresses.len();
            let size = *size as usize;
            let total = count.saturating_mul(size);
            if count == 0 || size == 0 || size > MAX_DRIVER_TRANSFER_SIZE || bulk.len() < total {
                return (STATUS_INVALID_PARAMETER, failure());
            }
            let output = &mut bulk[..total];
            match memory::batch_read_process_memory(*pid, addresses.as_slice(), size, output) {
                Ok(written) => (
                    STATUS_SUCCESS,
                    ResponseMeta {
                        bulk_len: written as u32,
                        count: count as u32,
                        value: 0,
                    },
                ),
                Err(status) => (status, failure()),
            }
        }
        Request::BatchWrite { writes } => {
            crate::trace!("batchwrite n=%lu", writes.len() as u32);
            let count = writes.len();
            if count == 0 || count > MAX_BATCH_WRITE_ENTRIES || bulk.len() < count * 4 {
                return (STATUS_INVALID_PARAMETER, failure());
            }
            let mut statuses = [STATUS_INVALID_PARAMETER; MAX_BATCH_WRITE_ENTRIES];
            memory::batch_write_process_memory(writes.as_slice(), &mut statuses[..count]);
            for (index, status) in statuses[..count].iter().enumerate() {
                let start = index * 4;
                bulk[start..start + 4].copy_from_slice(&(*status as u32).to_le_bytes());
            }
            // The transport succeeded; per-entry outcomes travel in the bulk
            // payload exactly as they did through the batch IOCTL.
            (
                STATUS_SUCCESS,
                ResponseMeta {
                    bulk_len: (count * 4) as u32,
                    count: count as u32,
                    value: 0,
                },
            )
        }
        Request::TraverseChain { pid, base, offsets } => {
            crate::trace!(
                "chain pid=%llu b=%p n=%lu",
                *pid,
                *base as *mut c_void,
                offsets.len() as u32
            );
            if offsets.is_empty() || offsets.len() > MAX_CHAIN_OFFSETS {
                return (STATUS_INVALID_PARAMETER, failure());
            }
            match memory::traverse_pointer_chain(*pid, *base, offsets.as_slice()) {
                Ok(value) => (STATUS_SUCCESS, scalar(value)),
                Err(status) => (status, failure()),
            }
        }
        Request::Shutdown => {
            crate::trace!("shutdown command decoded");
            (STATUS_SUCCESS, scalar(1))
        }
        Request::Lock {
            id,
            pid,
            address,
            data,
            rva,
        } => {
            crate::trace!(
                "lock id=%llu pid=%llu a=%p n=%lu rva=%lu",
                *id,
                *pid,
                *address as *mut c_void,
                data.len() as u32,
                *rva as u32
            );
            // RVA locks resolve once, at insert time, exactly like an RVA
            // read or write; the table only ever holds absolute addresses.
            let address = match resolve_address(*pid, *address, *rva) {
                Ok(address) => address,
                Err(status) => return (status, failure()),
            };
            match crate::lock::insert(*id, *pid, address, data) {
                Ok(()) => (STATUS_SUCCESS, scalar(0)),
                Err(status) => (status, failure()),
            }
        }
        Request::Unlock { id } => {
            crate::trace!("unlock id=%llu", *id);
            match crate::lock::remove(*id) {
                Ok(()) => (STATUS_SUCCESS, scalar(0)),
                Err(status) => (status, failure()),
            }
        }
        Request::UnlockAll { pid } => {
            crate::trace!("unlockall pid=%llu", *pid);
            match crate::lock::clear(*pid) {
                Ok(()) => (STATUS_SUCCESS, scalar(0)),
                Err(status) => (status, failure()),
            }
        }
        Request::LockList => {
            crate::trace!("locklist");
            let (entries, count) = crate::lock::list();
            let total = count * LOCK_LIST_ENTRY_SIZE;
            if bulk.len() < total {
                return (STATUS_BUFFER_TOO_SMALL, failure());
            }
            for (index, entry) in entries[..count].iter().enumerate() {
                let at = index * LOCK_LIST_ENTRY_SIZE;
                bulk[at..at + 8].copy_from_slice(&entry.id.to_le_bytes());
                bulk[at + 8..at + 16].copy_from_slice(&entry.pid.to_le_bytes());
                bulk[at + 16..at + 24].copy_from_slice(&entry.address.to_le_bytes());
                bulk[at + 24..at + 28].copy_from_slice(&(entry.len as u32).to_le_bytes());
            }
            (
                STATUS_SUCCESS,
                ResponseMeta {
                    bulk_len: total as u32,
                    count: count as u32,
                    value: count as u64,
                },
            )
        }
        Request::Alloc { pid, size, protect } => {
            crate::trace!("alloc pid=%llu s=%lu p=0x%lx", *pid, *size, *protect);
            match memory::allocate(*pid, *size as usize, *protect) {
                Ok(base) => (STATUS_SUCCESS, scalar(base)),
                Err(status) => (status, failure()),
            }
        }
        Request::Free { pid, address } => {
            crate::trace!("free pid=%llu a=%p", *pid, *address as *mut c_void);
            match memory::free(*pid, *address) {
                Ok(()) => (STATUS_SUCCESS, scalar(0)),
                Err(status) => (status, failure()),
            }
        }
        Request::Hook { op } => execute_hook(op, bulk),
        Request::Suspend { pid } => {
            crate::trace!("process suspend pid=%llu", *pid);
            match crate::suspend::suspend(*pid, client_pid as u64) {
                Ok(value) => (STATUS_SUCCESS, scalar(value)),
                Err(status) => (status, failure()),
            }
        }
        Request::Resume { pid } => {
            crate::trace!("process resume pid=%llu", *pid);
            match crate::suspend::resume(*pid, client_pid as u64) {
                Ok(value) => (STATUS_SUCCESS, scalar(value)),
                Err(status) => (status, failure()),
            }
        }
    }
}

/// Kernel hook table operations. `Create` reports the id, the replaced
/// length and the original prologue bytes (`bulk`); the `*All` variants
/// report how many hooks they touched.
fn execute_hook(op: &HookOp<'_>, bulk: &mut [u8]) -> (NTSTATUS, ResponseMeta) {
    match op {
        HookOp::Create {
            pid,
            target,
            trampoline,
        } => {
            crate::trace!(
                "hook create pid=%llu t=%p d=%p",
                *pid,
                *target as *mut c_void,
                *trampoline as *mut c_void
            );
            match crate::hook::create(*pid, *target, *trampoline) {
                Ok((id, stolen_len, original)) => {
                    if bulk.len() < stolen_len {
                        return (STATUS_BUFFER_TOO_SMALL, failure());
                    }
                    bulk[..stolen_len].copy_from_slice(&original[..stolen_len]);
                    (
                        STATUS_SUCCESS,
                        ResponseMeta {
                            bulk_len: stolen_len as u32,
                            count: stolen_len as u32,
                            value: id,
                        },
                    )
                }
                Err(status) => (status, failure()),
            }
        }
        HookOp::CreateShellcode { pid, target, code } => {
            crate::trace!(
                "hook shellcode pid=%llu t=%p n=%lu",
                *pid,
                *target as *mut c_void,
                code.len() as u32
            );
            match crate::hook::create_shellcode(*pid, *target, code) {
                Ok((id, stolen_len, original, code_address)) => {
                    // bulk = original prologue bytes, then the little-endian
                    // buffer address for diagnostics.
                    let total = stolen_len + core::mem::size_of::<u64>();
                    if bulk.len() < total {
                        // The table entry exists now; drop it so a short ring
                        // buffer cannot leak an enabled shellcode buffer.
                        let _ = crate::hook::remove(*pid, id);
                        return (STATUS_BUFFER_TOO_SMALL, failure());
                    }
                    bulk[..stolen_len].copy_from_slice(&original[..stolen_len]);
                    bulk[stolen_len..total].copy_from_slice(&code_address.to_le_bytes());
                    (
                        STATUS_SUCCESS,
                        ResponseMeta {
                            bulk_len: total as u32,
                            count: stolen_len as u32,
                            value: id,
                        },
                    )
                }
                Err(status) => (status, failure()),
            }
        }
        HookOp::CreateDetour { pid, target, code } => {
            crate::trace!(
                "hook detour pid=%llu t=%p n=%lu",
                *pid,
                *target as *mut c_void,
                code.len() as u32
            );
            match crate::hook::create_detour(*pid, *target, code) {
                Ok(info) => {
                    // bulk = original prologue, then code address and
                    // trampoline address.
                    let total = info.replaced_len + 16;
                    if bulk.len() < total {
                        let _ = crate::hook::remove(*pid, info.id);
                        return (STATUS_BUFFER_TOO_SMALL, failure());
                    }
                    bulk[..info.replaced_len].copy_from_slice(&info.original[..info.replaced_len]);
                    bulk[info.replaced_len..info.replaced_len + 8]
                        .copy_from_slice(&info.code_address.to_le_bytes());
                    bulk[info.replaced_len + 8..total]
                        .copy_from_slice(&info.trampoline_address.to_le_bytes());
                    (
                        STATUS_SUCCESS,
                        ResponseMeta {
                            bulk_len: total as u32,
                            count: info.replaced_len as u32,
                            value: info.id,
                        },
                    )
                }
                Err(status) => (status, failure()),
            }
        }
        HookOp::Probe { pid, target } => {
            crate::trace!("hook probe pid=%llu t=%p", *pid, *target as *mut c_void);
            match crate::hook::probe(*pid, *target) {
                Ok(info) => {
                    let total = info.replaced_len + 4;
                    if bulk.len() < total {
                        return (STATUS_BUFFER_TOO_SMALL, failure());
                    }
                    bulk[..info.replaced_len].copy_from_slice(&info.original[..info.replaced_len]);
                    bulk[info.replaced_len..total]
                        .copy_from_slice(&(info.absolute_len as u32).to_le_bytes());
                    let mut flags = 0u64;
                    if info.rip_relative {
                        flags |= ks_core::HOOK_PROBE_RIP_RELATIVE;
                    }
                    if info.relative_branch {
                        flags |= ks_core::HOOK_PROBE_RELATIVE_BRANCH;
                    }
                    (
                        STATUS_SUCCESS,
                        ResponseMeta {
                            bulk_len: total as u32,
                            count: info.replaced_len as u32,
                            value: flags,
                        },
                    )
                }
                Err(status) => (status, failure()),
            }
        }
        HookOp::Enable { pid, id } => {
            crate::trace!("hook enable pid=%llu id=%llu", *pid, *id);
            match crate::hook::enable(*pid, *id) {
                status if nt_success(status) => (STATUS_SUCCESS, scalar(0)),
                status => (status, failure()),
            }
        }
        HookOp::Disable { pid, id } => {
            crate::trace!("hook disable pid=%llu id=%llu", *pid, *id);
            match crate::hook::disable(*pid, *id) {
                status if nt_success(status) => (STATUS_SUCCESS, scalar(0)),
                status => (status, failure()),
            }
        }
        HookOp::Remove { pid, id } => {
            crate::trace!("hook remove pid=%llu id=%llu", *pid, *id);
            match crate::hook::remove(*pid, *id) {
                status if nt_success(status) => (STATUS_SUCCESS, scalar(0)),
                status => (status, failure()),
            }
        }
        HookOp::EnableAll => match crate::hook::enable_all() {
            Ok(count) => (STATUS_SUCCESS, scalar(count)),
            Err(status) => (status, failure()),
        },
        HookOp::DisableAll => match crate::hook::disable_all() {
            Ok(count) => (STATUS_SUCCESS, scalar(count)),
            Err(status) => (status, failure()),
        },
        HookOp::RemoveAll => match crate::hook::remove_all() {
            Ok(count) => (STATUS_SUCCESS, scalar(count)),
            Err(status) => (status, failure()),
        },
        HookOp::SelfTest => {
            crate::trace!("hook selftest");
            match crate::hook::self_test() {
                Ok(value) => (STATUS_SUCCESS, scalar(value)),
                Err(status) => (status, failure()),
            }
        }
        HookOp::ShellcodeSelfTest => {
            crate::trace!("hook shellcode selftest");
            match crate::hook::shellcode_self_test() {
                Ok(value) => (STATUS_SUCCESS, scalar(value)),
                Err(status) => (status, failure()),
            }
        }
        HookOp::DetourSelfTest => {
            crate::trace!("hook detour selftest");
            match crate::hook::detour_self_test() {
                Ok(value) => (STATUS_SUCCESS, scalar(value)),
                Err(status) => (status, failure()),
            }
        }
    }
}
