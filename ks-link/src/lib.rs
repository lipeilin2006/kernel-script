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
    check_hook_address, valid_alloc_protect, BatchWriteItem, HookOp, ProtocolError, Request,
    ResponseMeta, MAX_ALLOC_SIZE, MAX_BATCH_ENTRIES, MAX_BATCH_WRITE_ENTRIES, MAX_CHAIN_OFFSETS,
    MAX_HOOK_SHELLCODE, MAX_HOOK_STOLEN_LEN,
};

pub use error::LinkError;
pub use lock::{lock, lock_list, lock_rva, unlock, unlock_all, unlock_rva};
pub use lock::{LockListEntry, MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE};
pub use names::{
    clear_stale_publication, instance_claim_present, published_object_names,
    published_object_names_strict,
};
pub use process::{find_pid, list, ProcessError, ProcessInfo};
pub use session::{close_session, shutdown_incompatible};

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

/// Freezes every thread of the process `pid` (kernel `PsSuspendProcess`).
///
/// Refuses `pid = 0` (Idle), `pid = 4` (System) and this client's own pid.
/// Idempotent: a process the driver already froze reports success without
/// freezing it twice, so a matching [`resume`] still runs it once.
///
/// Suspends survive a driver shutdown: they are *not* undone when the
/// driver unloads, so a frozen process stays frozen until resumed.
pub fn suspend(pid: u64) -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Suspend { pid }, &mut buffer, &mut []).map(|_| ())
}

/// Continues a process this driver previously suspended. A `pid` the driver
/// never froze is a no-op (`Ok(())`): the driver never decrements a suspend
/// count it did not increment.
pub fn resume(pid: u64) -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Resume { pid }, &mut buffer, &mut []).map(|_| ())
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

/// Records a MinHook-style inline hook on `target` and leaves it
/// **disabled**. `pid` selects the target space: `0` means the kernel half
/// (global kernel text), anything else means that process's user half.
/// `trampoline` must be an executable address in the *same* space that the
/// caller already owns: the driver only jumps to it, it never builds one.
///
/// The driver decodes the prologue to decide how many bytes the jump
/// replaces (never splitting an instruction) and returns them so the caller
/// can inspect/rebuild the trampoline. Call [`hook_enable`] to apply it.
///
/// The caller guarantees nothing executes the prologue during patching, and
/// must [`hook_remove`] before freeing the trampoline.
pub fn hook_create(pid: u64, target: u64, trampoline: u64) -> Result<KernelHook, LinkError> {
    if !check_hook_address(pid, target) || !check_hook_address(pid, trampoline) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    let mut original = vec![0u8; MAX_HOOK_STOLEN_LEN];
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, copied) = submit(
        &Request::Hook {
            op: HookOp::Create {
                pid,
                target,
                trampoline,
            },
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
pub fn hook_enable(pid: u64, id: u64) -> Result<(), LinkError> {
    hook_request(HookOp::Enable { pid, id }).map(|_| ())
}

/// Restores a hook's original prologue bytes, keeping the entry so it can be
/// enabled again.
pub fn hook_disable(pid: u64, id: u64) -> Result<(), LinkError> {
    hook_request(HookOp::Disable { pid, id }).map(|_| ())
}

/// Disables (if enabled) and drops a hook. Unknown ids fail.
pub fn hook_remove(pid: u64, id: u64) -> Result<(), LinkError> {
    hook_request(HookOp::Remove { pid, id }).map(|_| ())
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
    /// Address of the driver-allocated copy of `code` (kernel or the target
    /// process's user half, matching `pid`).
    pub code_address: u64,
}

/// Records an inline hook whose detour is the supplied machine code.
///
/// `pid` selects the target space: `0` means the kernel half, anything else
/// means that process's user half. The driver allocates executable memory in
/// the *same* space, copies `code` into it and points the target's jump
/// there. The buffer belongs to the hook entry: [`hook_remove`],
/// [`hook_remove_all`] and driver shutdown free it (after restoring the
/// original bytes).
///
/// `code` must be position-independent x64 that returns to the hook's caller
/// (or jumps back into the trampoline the caller built). Maximum
/// [`ks_core::protocol::MAX_HOOK_SHELLCODE`] bytes. The same safety contract
/// as [`hook_create`] applies: nothing may execute the prologue while it is
/// being patched.
pub fn hook_shellcode(pid: u64, target: u64, code: &[u8]) -> Result<ShellcodeHook, LinkError> {
    if !check_hook_address(pid, target) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
        return Err(LinkError::Encode(ProtocolError::TooLarge));
    }
    let mut bulk = vec![0u8; MAX_HOOK_STOLEN_LEN + core::mem::size_of::<u64>()];
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + code.len()];
    let (meta, copied) = submit(
        &Request::Hook {
            op: HookOp::CreateShellcode { pid, target, code },
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

/// Read-only prologue report, the planning step before a detour.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookProbe {
    /// Bytes a 5-byte relative jump must replace.
    pub replaced_len: usize,
    /// Bytes a 14-byte absolute jump needs, or 0 when the prologue is too
    /// long to cover within `MAX_HOOK_STOLEN_LEN`.
    pub absolute_len: usize,
    /// Original prologue bytes (`replaced_len` of them are valid).
    pub original: Vec<u8>,
    /// Contains a RIP-relative operand (rebased by a detour).
    pub rip_relative: bool,
    /// Contains a relative branch; `hook_detour` will refuse it.
    pub relative_branch: bool,
}

/// Decodes `target`'s prologue without modifying anything. `pid` selects the
/// target space (see [`hook_create`]).
pub fn hook_probe(pid: u64, target: u64) -> Result<HookProbe, LinkError> {
    if !check_hook_address(pid, target) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    let mut bulk = vec![0u8; MAX_HOOK_STOLEN_LEN + 4];
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, copied) = submit(
        &Request::Hook {
            op: HookOp::Probe { pid, target },
        },
        &mut buffer,
        &mut bulk,
    )?;
    let replaced_len = meta.count as usize;
    if replaced_len == 0 || copied < replaced_len + 4 {
        return Err(LinkError::Decode(ProtocolError::InvalidPayload));
    }
    let absolute_len =
        u32::from_le_bytes(bulk[replaced_len..replaced_len + 4].try_into().unwrap()) as usize;
    Ok(HookProbe {
        replaced_len,
        absolute_len,
        original: bulk[..replaced_len].to_vec(),
        rip_relative: meta.value & ks_core::HOOK_PROBE_RIP_RELATIVE != 0,
        relative_branch: meta.value & ks_core::HOOK_PROBE_RELATIVE_BRANCH != 0,
    })
}

/// A detour whose original-function trampoline the driver built.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetourHook {
    pub id: u64,
    pub replaced_len: usize,
    pub original: Vec<u8>,
    /// Address the target jumps to (the driver's copy of the code).
    pub code_address: u64,
    /// Relocated original prologue plus a jump back; also stored at
    /// `code_address - 8`, so `call qword ptr [rip-14]` reaches it.
    pub trampoline_address: u64,
}

/// Installs a detour and lets the driver build the trampoline. `pid` selects
/// the target space (see [`hook_create`]).
///
/// The code is copied to a fresh executable buffer, and the driver replays
/// the stolen prologue in a trampoline whose address sits right before the
/// code. Calling the original from the code is therefore the fixed 6-byte
/// sequence `FF 15 F2 FF FF FF` (`call qword ptr [rip-14]`).
///
/// Fails with `STATUS_NOT_SUPPORTED` (`LinkError::NtStatus`) when the stolen
/// prologue contains a relative branch (it cannot be relocated) and with
/// `STATUS_INVALID_PAGE_PROTECTION` when executable memory is refused (HVCI).
pub fn hook_detour(pid: u64, target: u64, code: &[u8]) -> Result<DetourHook, LinkError> {
    if !check_hook_address(pid, target) {
        return Err(LinkError::Encode(ProtocolError::InvalidPayload));
    }
    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
        return Err(LinkError::Encode(ProtocolError::TooLarge));
    }
    let mut bulk = vec![0u8; MAX_HOOK_STOLEN_LEN + 16];
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + code.len()];
    let (meta, copied) = submit(
        &Request::Hook {
            op: HookOp::CreateDetour { pid, target, code },
        },
        &mut buffer,
        &mut bulk,
    )?;
    let replaced_len = meta.count as usize;
    if replaced_len == 0 || copied < replaced_len + 16 {
        return Err(LinkError::Decode(ProtocolError::InvalidPayload));
    }
    let code_address = u64::from_le_bytes(
        bulk[replaced_len..replaced_len + 8]
            .try_into()
            .map_err(|_| LinkError::Decode(ProtocolError::InvalidPayload))?,
    );
    let trampoline_address = u64::from_le_bytes(
        bulk[replaced_len + 8..replaced_len + 16]
            .try_into()
            .map_err(|_| LinkError::Decode(ProtocolError::InvalidPayload))?,
    );
    Ok(DetourHook {
        id: meta.value,
        replaced_len,
        original: bulk[..replaced_len].to_vec(),
        code_address,
        trampoline_address,
    })
}

/// Runs the driver's detour self test: installs a detour whose code calls the
/// original prologue (`call [rip-14]`) and adds one to its result, calls the
/// target, disables, calls it again and removes. Returns `1` when the detour
/// observed `truth + 1` and the restored function `truth`, `0` when the
/// platform refused the executable allocation (HVCI).
pub fn hook_detour_self_test() -> Result<u64, LinkError> {
    hook_request(HookOp::DetourSelfTest).map(|(meta, _)| meta.value)
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
