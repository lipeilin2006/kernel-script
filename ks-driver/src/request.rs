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
    decode_request, encode_response_meta, Request, ResponseMeta, MAX_BATCH_WRITE_ENTRIES,
    MAX_CHAIN_OFFSETS, MAX_DRIVER_TRANSFER_SIZE,
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
                execute(&decoded, response)
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

fn execute(request: &Request<'_>, response: &mut [u8]) -> (NTSTATUS, ResponseMeta) {
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
    }
}
