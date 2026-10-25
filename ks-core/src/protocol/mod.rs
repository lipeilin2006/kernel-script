//! Request/response protocol carried by the shared-memory ring.
//!
//! Payloads are [`postcard`] encoded: a `no_std`, `no_alloc`, explicitly
//! little-endian (LEB128 varints + little-endian fixed fields) format whose
//! deserializer borrows directly out of the ring, so the driver never copies
//! a request payload onto the stack. Variants are append-only; adding,
//! removing or reordering fields requires bumping
//! [`crate::ring::RING_VERSION`].
//!
//! The response side is a fixed
//! [`RESPONSE_META_SIZE`](crate::ring::RESPONSE_META_SIZE)-byte postcard
//! slot followed by a raw bulk region, so the driver never needs a staging
//! buffer: read results are written straight into the bulk area.

use core::fmt;

use heapless::Vec as SmallVec;
use serde::{Deserialize, Serialize};
/// Largest single memory operation accepted by the driver.
pub const MAX_DRIVER_TRANSFER_SIZE: usize = 4096;
/// Entries in one batch read.
pub const MAX_BATCH_ENTRIES: usize = 256;
/// Entries in one batch write.
pub const MAX_BATCH_WRITE_ENTRIES: usize = 64;
/// Offsets in one pointer-chain walk.
pub const MAX_CHAIN_OFFSETS: usize = 32;
/// Slots in the service-side memory lock table.
pub const MAX_MEMORY_LOCKS: usize = 64;
/// Largest payload held by one memory lock.
pub const MAX_MEMORY_LOCK_SIZE: usize = 4096;
/// Convenience alias used by the Lua layer for write sizes.
pub const MAX_WRITE_SIZE: usize = MAX_DRIVER_TRANSFER_SIZE;
/// Largest single target-process allocation accepted by the driver.
pub const MAX_ALLOC_SIZE: usize = 16 * 1024 * 1024;
/// `PAGE_READWRITE`: the default protection for [`Request::Alloc`].
pub const ALLOC_PROTECT_READWRITE: u32 = 0x04;
/// `PAGE_EXECUTE_READWRITE`, the only executable protection the driver
/// hands out (kept explicit so a script cannot silently allocate RWX).
pub const ALLOC_PROTECT_EXECUTE_READWRITE: u32 = 0x40;

/// Whether `protect` is one of the two protections [`Request::Alloc`]
/// accepts. Everything else is rejected at every trust boundary.
pub const fn valid_alloc_protect(protect: u32) -> bool {
    protect == ALLOC_PROTECT_READWRITE || protect == ALLOC_PROTECT_EXECUTE_READWRITE
}

/// Slots in the driver-side kernel hook table.
pub const MAX_KERNEL_HOOKS: usize = 16;
/// Longest entry prologue a hook may replace, in bytes (absolute jump plus
/// alignment headroom).
pub const MAX_HOOK_STOLEN_LEN: usize = 16;
/// Smallest replacement length: a `jmp rel32` is five bytes.
pub const MIN_HOOK_STOLEN_LEN: usize = 5;
/// Largest shellcode blob [`HookOp::CreateShellcode`] accepts.
pub const MAX_HOOK_SHELLCODE: usize = 1024;
/// Lowest address treated as kernel space on x64 (`0xFFFF8000_00000000`).
pub const MIN_KERNEL_ADDRESS: u64 = 0xFFFF_8000_0000_0000;

/// Whether `address` is in the x64 kernel half of the address space.
pub const fn is_kernel_address(address: u64) -> bool {
    address >= MIN_KERNEL_ADDRESS
}

/// One kernel-hook table operation, MinHook-style: `Create` only records the
/// hook (state `disabled`), `Enable`/`Disable` toggle the patched bytes, and
/// `Remove` restores the original bytes and drops the entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HookOp<'a> {
    /// Record a hook on `target`. The driver decodes the prologue to find the
    /// replaced length and builds the jump; `trampoline` is the caller's
    /// already-executable detour target.
    Create {
        target: u64,
        trampoline: u64,
    },
    /// Record a hook whose detour is `code`: the driver allocates executable
    /// kernel memory, copies the bytes there and points the jump at that
    /// buffer. The buffer is owned by the hook entry and freed by
    /// `Remove`/`RemoveAll`/shutdown. The response carries the original
    /// prologue bytes followed by the buffer address (little-endian u64).
    CreateShellcode {
        target: u64,
        #[serde(borrow)]
        code: &'a [u8],
    },
    /// Patch the target so calls reach the trampoline again.
    Enable {
        id: u64,
    },
    /// Restore the original prologue bytes.
    Disable {
        id: u64,
    },
    /// Restore the original bytes (if enabled) and drop the entry.
    Remove {
        id: u64,
    },
    EnableAll,
    DisableAll,
    RemoveAll,
    /// Runs one create → enable → verify → disable → remove cycle against a
    /// function inside the driver itself, so the whole MDL patch path can be
    /// exercised without touching foreign kernel code. Answers
    /// `value = 1` (patched and restored) or `0` (write refused, e.g. HVCI).
    SelfTest,
    /// Same as [`HookOp::SelfTest`] for the shellcode path: allocates
    /// executable kernel memory, hooks the driver's own leaf function with a
    /// tiny `mov eax, imm32; ret` stub, calls it, disables and removes.
    /// Answers `value = 1` (verified) or `0` (allocation/protection refused,
    /// e.g. HVCI).
    ShellcodeSelfTest,
}

/// One entry of a batch write. `data` borrows straight out of the ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatchWriteItem<'a> {
    pub pid: u64,
    pub address: u64,
    #[serde(borrow)]
    pub data: &'a [u8],
}

/// Every request the driver services.
///
/// `rva` resolves `address` against the target module base and `mdl` routes
/// the operation through the MDL-remap path; the two flags are orthogonal
/// and replace the eight former `IOCTL_*` variants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Request<'a> {
    Ping,
    GetProcessBase {
        pid: u64,
    },
    Read {
        pid: u64,
        address: u64,
        size: u32,
        rva: bool,
        mdl: bool,
    },
    Write {
        pid: u64,
        address: u64,
        #[serde(borrow)]
        data: &'a [u8],
        rva: bool,
        mdl: bool,
    },
    BatchRead {
        pid: u64,
        size: u32,
        addresses: SmallVec<u64, MAX_BATCH_ENTRIES>,
    },
    BatchWrite {
        writes: SmallVec<BatchWriteItem<'a>, MAX_BATCH_WRITE_ENTRIES>,
    },
    TraverseChain {
        pid: u64,
        base: u64,
        offsets: SmallVec<u64, MAX_CHAIN_OFFSETS>,
    },
    /// Wind down the driver's worker thread. The response is published
    /// before the worker exits, so this is an ordinary round trip; every
    /// later request times out until the driver is reloaded. It also stops
    /// the driver-side lock rewrite thread and drops the whole lock table.
    Shutdown,
    /// Install or replace one entry in the driver's lock table. The
    /// driver's dedicated rewrite thread replays the payload with plain
    /// writes until the entry is removed by [`Request::Unlock`] or
    /// [`Request::UnlockAll`]. `rva` resolves `address` against the target
    /// module base once, at insert time.
    Lock {
        id: u64,
        pid: u64,
        address: u64,
        #[serde(borrow)]
        data: &'a [u8],
        rva: bool,
    },
    /// Remove one lock entry. Removing an unknown id succeeds, so unlock
    /// stays idempotent.
    Unlock {
        id: u64,
    },
    /// Remove every lock entry held against `pid`.
    UnlockAll {
        pid: u64,
    },
    /// Reserve, commit and zero one region of target-process virtual
    /// memory. `protect` must satisfy [`valid_alloc_protect`]; the reply
    /// carries the base address in [`ResponseMeta::value`]. The driver
    /// rounds `size` up to the page size.
    Alloc {
        pid: u64,
        size: u32,
        protect: u32,
    },
    /// Release a region returned by [`Request::Alloc`]. The whole region is
    /// freed (`MEM_RELEASE`), so no size is carried; freeing an address
    /// that was not allocated by [`Request::Alloc`] fails in the driver.
    Free {
        pid: u64,
        address: u64,
    },
    /// One kernel-hook table operation (see [`HookOp`]). `Create` returns the
    /// new id in [`ResponseMeta::value`], the replaced length in
    /// [`ResponseMeta::count`] and the original prologue bytes in the bulk
    /// region; the `*All` variants return the number of affected hooks.
    Hook {
        op: HookOp<'a>,
    },
}

/// Description of a successful response payload. Failures never reach this
/// struct: they are reported through [`crate::ring::RingHeader::status`] with
/// `bulk_len == 0`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseMeta {
    /// Bytes of payload following the meta slot.
    pub bulk_len: u32,
    /// Element count for structured payloads (batch entries). Unused for
    /// plain byte payloads, where it repeats `bulk_len`.
    pub count: u32,
    /// Scalar result: process base or pointer-chain address.
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    BufferTooSmall,
    EncodeFailed,
    DecodeFailed,
    /// The decoded message did not consume the whole input. A request slot
    /// always holds exactly one message, so leftovers mean corruption or a
    /// version mismatch.
    TrailingBytes,
    InvalidPayload,
    TooLarge,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::BufferTooSmall => "buffer too small",
            Self::EncodeFailed => "encode failed",
            Self::DecodeFailed => "decode failed",
            Self::TrailingBytes => "trailing bytes after message",
            Self::InvalidPayload => "invalid payload",
            Self::TooLarge => "payload too large",
        };
        f.write_str(text)
    }
}

fn check_len(len: usize) -> Result<(), ProtocolError> {
    if len == 0 {
        Err(ProtocolError::InvalidPayload)
    } else if len > MAX_DRIVER_TRANSFER_SIZE {
        Err(ProtocolError::TooLarge)
    } else {
        Ok(())
    }
}

fn check_pid(pid: u64) -> Result<(), ProtocolError> {
    if pid == 0 {
        Err(ProtocolError::InvalidPayload)
    } else {
        Ok(())
    }
}

impl<'a> Request<'a> {
    /// Validate everything the driver is allowed to assume about a decoded
    /// request. The driver re-checks the same rules at the memory layer, but
    /// rejecting bad requests here keeps them out of the operation code.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Ping => Ok(()),
            Self::GetProcessBase { pid } => check_pid(*pid),
            Self::Read {
                pid,
                address,
                size,
                rva,
                ..
            } => {
                check_pid(*pid)?;
                if *address == 0 && !*rva {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_len(*size as usize)
            }
            Self::Write {
                pid,
                address,
                data,
                rva,
                ..
            } => {
                check_pid(*pid)?;
                if *address == 0 && !*rva {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_len(data.len())
            }
            Self::BatchRead {
                pid,
                size,
                addresses,
            } => {
                check_pid(*pid)?;
                check_len(*size as usize)?;
                if addresses.is_empty() {
                    return Err(ProtocolError::InvalidPayload);
                }
                let total = (*size as usize)
                    .checked_mul(addresses.len())
                    .ok_or(ProtocolError::TooLarge)?;
                if total > crate::ring::RESPONSE_BULK_SIZE {
                    return Err(ProtocolError::TooLarge);
                }
                Ok(())
            }
            Self::BatchWrite { writes } => {
                if writes.is_empty() {
                    return Err(ProtocolError::InvalidPayload);
                }
                for item in writes {
                    check_pid(item.pid)?;
                    if item.address == 0 {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    check_len(item.data.len())?;
                }
                Ok(())
            }
            Self::TraverseChain { pid, base, offsets } => {
                check_pid(*pid)?;
                if *base == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                if offsets.len() > MAX_CHAIN_OFFSETS {
                    return Err(ProtocolError::TooLarge);
                }
                Ok(())
            }
            Self::Shutdown => Ok(()),
            Self::Lock {
                id,
                pid,
                address,
                data,
                rva,
            } => {
                if *id == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_pid(*pid)?;
                if *address == 0 && !*rva {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_len(data.len())?;
                if data.len() > MAX_MEMORY_LOCK_SIZE {
                    return Err(ProtocolError::TooLarge);
                }
                Ok(())
            }
            Self::Unlock { id } => {
                if *id == 0 {
                    Err(ProtocolError::InvalidPayload)
                } else {
                    Ok(())
                }
            }
            Self::UnlockAll { pid } => check_pid(*pid),
            Self::Alloc { pid, size, protect } => {
                check_pid(*pid)?;
                if *size == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                if *size as usize > MAX_ALLOC_SIZE {
                    return Err(ProtocolError::TooLarge);
                }
                if !valid_alloc_protect(*protect) {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(())
            }
            Self::Free { pid, address } => {
                check_pid(*pid)?;
                if *address == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(())
            }
            Self::Hook { op } => match op {
                HookOp::Create { target, trampoline } => {
                    if !is_kernel_address(*target) || !is_kernel_address(*trampoline) {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    Ok(())
                }
                HookOp::CreateShellcode { target, code } => {
                    if !is_kernel_address(*target) {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    Ok(())
                }
                HookOp::Enable { id } | HookOp::Disable { id } | HookOp::Remove { id } => {
                    if *id == 0 {
                        Err(ProtocolError::InvalidPayload)
                    } else {
                        Ok(())
                    }
                }
                HookOp::EnableAll | HookOp::DisableAll | HookOp::RemoveAll => Ok(()),
                HookOp::SelfTest | HookOp::ShellcodeSelfTest => Ok(()),
            },
        }
    }
}

mod codec;
pub use codec::{decode_request, decode_response_meta, encode_request, encode_response_meta};

/// The largest legal batch read must fit the response bulk region exactly
/// or below it, otherwise a valid request could not be answered.
const _: () =
    assert!(MAX_BATCH_ENTRIES * MAX_DRIVER_TRANSFER_SIZE <= crate::ring::RESPONSE_BULK_SIZE);

/// A lock payload travels in one request and is re-checked with the same
/// single-transfer rule in the driver, so the two limits must not drift.
const _: () = assert!(MAX_MEMORY_LOCK_SIZE <= MAX_DRIVER_TRANSFER_SIZE);

#[cfg(test)]
mod tests;
