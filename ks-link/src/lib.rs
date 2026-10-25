//! User-mode client for the KernelScript driver's shared-memory ring.
//!
//! A `session::Session` opens the section, the two events and the client
//! mutex the driver publishes in `\BaseNamedObjects`, maps the section into
//! the process, and serializes every request through its round trip. The
//! module-level free functions are the stable surface consumed by the GUI
//! and the test harness: each one connects on demand, submits one request
//! and copies the answer out of the ring before returning.

mod error;
mod lock;
mod names;
mod process;
mod session;

use heapless::Vec as SmallVec;
use ks_core::protocol::{
    is_kernel_address, valid_alloc_protect, BatchWriteItem, HookOp, ProtocolError, Request,
    ResponseMeta, MAX_ALLOC_SIZE, MAX_BATCH_ENTRIES, MAX_BATCH_WRITE_ENTRIES, MAX_CHAIN_OFFSETS,
    MAX_HOOK_SHELLCODE, MAX_HOOK_STOLEN_LEN,
};

pub use error::LinkError;
pub use lock::{lock, lock_rva, unlock, unlock_all, unlock_rva};
pub use lock::{MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE};
pub use names::{instance_claim_present, published_object_names, published_object_names_strict};
pub use process::{find_pid, list, ProcessError, ProcessInfo};
pub use session::close_session;

use session::submit;

/// Stack headroom for the largest fixed-size request: a batch read with
/// [`MAX_BATCH_ENTRIES`] addresses.
const SMALL_REQUEST_BUFFER: usize = 4096;

pub fn ping() -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Ping, &mut buffer, &mut []).map(|_| ())
}

/// Asks the driver's worker thread to stop serving after it publishes this
/// response. Every later request times out until the driver is reloaded;
/// stopping the service afterwards takes the usual unload path (the worker
/// has already exited, so `DriverUnload` just releases the objects).
pub fn shutdown() -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Shutdown, &mut buffer, &mut []).map(|_| ())
}

pub fn get_process_base(pid: u64) -> Result<u64, LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, _) = submit(&Request::GetProcessBase { pid }, &mut buffer, &mut [])?;
    Ok(meta.value)
}

pub fn read_bytes(
    pid: u64,
    address: u64,
    size: usize,
    rva: bool,
    mdl: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let mut out = vec![0u8; size];
    let (_, copied) = submit(
        &Request::Read {
            pid,
            address,
            size: size as u32,
            rva,
            mdl,
        },
        &mut buffer,
        &mut out,
    )?;
    out.truncate(copied);
    Ok(out)
}

pub fn write_bytes(
    pid: u64,
    address: u64,
    data: &[u8],
    rva: bool,
    mdl: bool,
) -> Result<(), LinkError> {
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + data.len()];
    submit(
        &Request::Write {
            pid,
            address,
            data,
            rva,
            mdl,
        },
        &mut buffer,
        &mut [],
    )?;
    Ok(())
}

pub fn batch_read(pid: u64, size: u32, addresses: &[u64]) -> Result<Vec<u8>, LinkError> {
    let addresses = addresses_small_vec(addresses, MAX_BATCH_ENTRIES)?;
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let mut out = vec![0u8; addresses.len() * size as usize];
    let (_, copied) = submit(
        &Request::BatchRead {
            pid,
            size,
            addresses,
        },
        &mut buffer,
        &mut out,
    )?;
    out.truncate(copied);
    Ok(out)
}

/// One NTSTATUS per entry, in request order.
pub fn batch_write(pid: u64, entries: &[(u64, Vec<u8>)]) -> Result<Vec<i32>, LinkError> {
    if entries.len() > MAX_BATCH_WRITE_ENTRIES {
        return Err(LinkError::TooManyEntries {
            limit: MAX_BATCH_WRITE_ENTRIES,
        });
    }
    let mut writes = SmallVec::<BatchWriteItem<'_>, MAX_BATCH_WRITE_ENTRIES>::new();
    for (address, data) in entries {
        writes
            .push(BatchWriteItem {
                pid,
                address: *address,
                data,
            })
            .map_err(|_| LinkError::TooManyEntries {
                limit: MAX_BATCH_WRITE_ENTRIES,
            })?;
    }
    let data_len: usize = entries.iter().map(|(_, data)| data.len()).sum();
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + data_len];
    let mut out = [0u8; MAX_BATCH_WRITE_ENTRIES * 4];
    let (_, copied) = submit(&Request::BatchWrite { writes }, &mut buffer, &mut out)?;
    Ok(out[..copied]
        .chunks_exact(4)
        .map(|status| i32::from_le_bytes(status.try_into().unwrap()))
        .collect())
}

pub fn traverse_pointer_chain(pid: u64, base: u64, offsets: &[u64]) -> Result<u64, LinkError> {
    let offsets = addresses_small_vec(offsets, MAX_CHAIN_OFFSETS)?;
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, _) = submit(
        &Request::TraverseChain { pid, base, offsets },
        &mut buffer,
        &mut [],
    )?;
    Ok(meta.value)
}

/// Allocates `size` bytes in `pid` and returns the base address. `protect`
/// must be [`ks_core::protocol::ALLOC_PROTECT_READWRITE`] or
/// [`ks_core::protocol::ALLOC_PROTECT_EXECUTE_READWRITE`]; the driver
/// rounds the size up to whole pages.
pub fn alloc(pid: u64, size: usize, protect: u32) -> Result<u64, LinkError> {
    if size == 0 || size > MAX_ALLOC_SIZE {
        return Err(LinkError::Encode(ProtocolError::TooLarge));
    }
    if !valid_alloc_protect(protect) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, _) = submit(
        &Request::Alloc {
            pid,
            size: size as u32,
            protect,
        },
        &mut buffer,
        &mut [],
    )?;
    Ok(meta.value)
}

/// Frees a whole region previously returned by [`alloc`].
pub fn free(pid: u64, address: u64) -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Free { pid, address }, &mut buffer, &mut []).map(|_| ())
}

/// A recorded kernel hook, as returned by [`hook_create`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelHook {
    /// Table id used by the enable/disable/remove calls (never 0).
    pub id: u64,
    /// Number of instruction bytes the jump replaces.
    pub replaced_len: usize,
    /// Original bytes at the target, in order.
    pub original: Vec<u8>,
}

fn hook_request(op: HookOp) -> Result<(ResponseMeta, usize), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Hook { op }, &mut buffer, &mut [])
}

/// Records a MinHook-style kernel inline hook on `target` and leaves it
/// **disabled**. `trampoline` must be an executable kernel address the
/// caller already owns: the driver only jumps to it, it never builds one.
///
/// The driver decodes the prologue to decide how many bytes the jump
/// replaces (never splitting an instruction) and returns them so the caller
/// can inspect/rebuild the trampoline. Call [`hook_enable`] to apply it.
///
/// Both addresses must be in the kernel half (`>= 0xFFFF8000_00000000`).
/// The caller guarantees nothing executes the prologue during patching, and
/// must [`hook_remove`] before freeing the trampoline.
pub fn hook_create(target: u64, trampoline: u64) -> Result<KernelHook, LinkError> {
    if !is_kernel_address(target) || !is_kernel_address(trampoline) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    let mut original = vec![0u8; MAX_HOOK_STOLEN_LEN];
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, copied) = submit(
        &Request::Hook {
            op: HookOp::Create { target, trampoline },
        },
        &mut buffer,
        &mut original,
    )?;
    original.truncate(copied);
    Ok(KernelHook {
        id: meta.value,
        replaced_len: meta.count as usize,
        original,
    })
}

/// Applies a recorded hook's jump.
pub fn hook_enable(id: u64) -> Result<(), LinkError> {
    hook_request(HookOp::Enable { id }).map(|_| ())
}

/// Restores a hook's original prologue bytes, keeping the entry so it can be
/// enabled again.
pub fn hook_disable(id: u64) -> Result<(), LinkError> {
    hook_request(HookOp::Disable { id }).map(|_| ())
}

/// Disables (if enabled) and drops a hook. Unknown ids fail.
pub fn hook_remove(id: u64) -> Result<(), LinkError> {
    hook_request(HookOp::Remove { id }).map(|_| ())
}

/// Enables every recorded hook; returns how many were touched.
pub fn hook_enable_all() -> Result<u64, LinkError> {
    hook_request(HookOp::EnableAll).map(|(meta, _)| meta.value)
}

/// Restores every patched hook; returns how many were touched.
pub fn hook_disable_all() -> Result<u64, LinkError> {
    hook_request(HookOp::DisableAll).map(|(meta, _)| meta.value)
}

/// Removes every hook in reverse creation order; returns how many were
/// touched. The `Shutdown` request and the unload path do this implicitly.
pub fn hook_remove_all() -> Result<u64, LinkError> {
    hook_request(HookOp::RemoveAll).map(|(meta, _)| meta.value)
}

/// Exercises the whole MDL patch path on a function inside the driver itself
/// (create → enable → verify → disable → verify → remove). Returns `1` when
/// a jump was applied and the original bytes were restored, `0` when the
/// platform refused the write (HVCI keeps driver text read-only).
pub fn hook_self_test() -> Result<u64, LinkError> {
    hook_request(HookOp::SelfTest).map(|(meta, _)| meta.value)
}

/// A shellcode hook: the driver owns the executable detour buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellcodeHook {
    /// Table id used by `hook_enable`/`hook_disable`/`hook_remove`.
    pub id: u64,
    /// Number of instruction bytes the jump replaces.
    pub replaced_len: usize,
    /// Original bytes at the target, in order.
    pub original: Vec<u8>,
    /// Kernel address of the driver-allocated copy of `code`.
    pub code_address: u64,
}

/// Records a kernel inline hook whose detour is the supplied machine code.
///
/// The driver allocates non-paged kernel memory, copies `code` into it, marks
/// it executable (`MmProtectMdlSystemAddress`) and points the target's jump
/// there. The buffer belongs to the hook entry: [`hook_remove`],
/// [`hook_remove_all`] and driver shutdown free it (after restoring the
/// original bytes).
///
/// `code` must be position-independent x64 that returns to the hook's caller
/// (or jumps back into the trampoline the caller built). Maximum
/// [`ks_core::protocol::MAX_HOOK_SHELLCODE`] bytes. The same safety contract
/// as [`hook_create`] applies: nothing may execute the prologue while it is
/// being patched, and the target must be a real kernel address.
pub fn hook_shellcode(target: u64, code: &[u8]) -> Result<ShellcodeHook, LinkError> {
    if !is_kernel_address(target) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
        return Err(LinkError::Encode(ProtocolError::TooLarge));
    }
    let mut bulk = vec![0u8; MAX_HOOK_STOLEN_LEN + core::mem::size_of::<u64>()];
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + code.len()];
    let (meta, copied) = submit(
        &Request::Hook {
            op: HookOp::CreateShellcode { target, code },
        },
        &mut buffer,
        &mut bulk,
    )?;
    let replaced_len = meta.count as usize;
    if copied < replaced_len + core::mem::size_of::<u64>() {
        return Err(LinkError::Decode(ProtocolError::InvalidPayload));
    }
    let code_address = u64::from_le_bytes(
        bulk[replaced_len..replaced_len + 8]
            .try_into()
            .map_err(|_| LinkError::Decode(ProtocolError::InvalidPayload))?,
    );
    Ok(ShellcodeHook {
        id: meta.value,
        replaced_len,
        original: bulk[..replaced_len].to_vec(),
        code_address,
    })
}

/// Runs the driver's shellcode self test: allocates executable kernel memory,
/// hooks an internal leaf function with a `mov eax, imm32; ret` stub, calls
/// through the hook, disables, calls again and removes. Returns `1` when the
/// call observed the stub value and the original value afterwards, `0` when
/// the platform refused the executable allocation (HVCI).
pub fn hook_shellcode_self_test() -> Result<u64, LinkError> {
    hook_request(HookOp::ShellcodeSelfTest).map(|(meta, _)| meta.value)
}

fn addresses_small_vec<const N: usize>(
    items: &[u64],
    limit: usize,
) -> Result<SmallVec<u64, N>, LinkError> {
    if items.len() > limit {
        return Err(LinkError::TooManyEntries { limit });
    }
    let mut vec = SmallVec::new();
    for item in items {
        vec.push(*item)
            .map_err(|_| LinkError::TooManyEntries { limit })?;
    }
    Ok(vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_vectors_reject_overflows() {
        assert_eq!(
            addresses_small_vec::<1>(&[1, 2], 1),
            Err(LinkError::TooManyEntries { limit: 1 })
        );
        assert!(addresses_small_vec::<4>(&[1, 2], 4).is_ok());
    }
}
