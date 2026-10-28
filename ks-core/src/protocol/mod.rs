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
/// Packed size of one [`Request::LockList`] entry: id, pid and address as
/// little-endian u64s, then the payload length as a little-endian u32.
pub const LOCK_LIST_ENTRY_SIZE: usize = 28;
/// Convenience alias used by the Lua layer for write sizes.
pub const MAX_WRITE_SIZE: usize = MAX_DRIVER_TRANSFER_SIZE;
/// Largest single target-process allocation accepted by the driver.
pub const MAX_ALLOC_SIZE: usize = 16 * 1024 * 1024;
/// Page protections accepted by [`Request::Protect`] and — through
/// [`valid_alloc_protect`] — by [`Request::Alloc`]: `PAGE_READONLY`,
/// `PAGE_READWRITE`, `PAGE_EXECUTE_READ` and `PAGE_EXECUTE_READWRITE`.
/// `PAGE_NOACCESS` and `PAGE_GUARD` are deliberately absent: the target
/// process would fault on its own memory the moment it touched the
/// region. `PAGE_NOCACHE`/`PAGE_WRITECOMBINE` are cache hints, not
/// access rights, and stay out too.
pub const PROT_READ: u32 = 0x0000_0002;
/// `PAGE_READWRITE`.
pub const PROT_RW: u32 = 0x0000_0004;
/// `PAGE_EXECUTE_READ`.
pub const PROT_RX: u32 = 0x0000_0020;
/// `PAGE_EXECUTE_READWRITE`.
pub const PROT_RWX: u32 = 0x0000_0040;

/// Largest single [`Request::Protect`] span, in bytes. Whole images pass
/// through here (a game `.text` runs to tens of megabytes), so the cap is
/// generous; the binding limit is the target's contiguous mapping, which
/// [`Request::QueryRegion`] reports first.
pub const MAX_PROTECT_SIZE: usize = 256 * 1024 * 1024;

/// `PAGE_READWRITE`: the default protection for [`Request::Alloc`].
pub const ALLOC_PROTECT_READWRITE: u32 = PROT_RW;
/// `PAGE_EXECUTE_READWRITE`: the executable option for [`Request::Alloc`]
/// (kept explicit so a script cannot silently allocate RWX without
/// naming it).
pub const ALLOC_PROTECT_EXECUTE_READWRITE: u32 = PROT_RWX;

/// Whether `protect` is one of the four page protections the driver
/// accepts. Everything else is rejected at every trust boundary.
pub const fn valid_protect(protect: u32) -> bool {
    matches!(protect, PROT_READ | PROT_RW | PROT_RX | PROT_RWX)
}

/// Whether `protect` is valid for [`Request::Alloc`]. Deliberately one
/// value narrower than [`valid_protect`]: `PROT_READ` is excluded
/// because a read-only allocation cannot be written through the plain
/// write path (only through the MDL path), so it buys nothing — allocate
/// `PROT_RW` and [`Request::Protect`] it down if a read-only span is
/// really wanted.
pub const fn valid_alloc_protect(protect: u32) -> bool {
    matches!(protect, PROT_RW | PROT_RX | PROT_RWX)
}

/// `MEM_COMMIT`: the region's pages are resident and accessible.
pub const MEM_COMMIT: u32 = 0x0000_1000;
/// `MEM_RESERVE`: the address range is taken but not committed.
pub const MEM_RESERVE: u32 = 0x0000_2000;
/// `MEM_FREE`: nothing is mapped in the range at all.
pub const MEM_FREE: u32 = 0x0001_0000;
/// `MEM_PRIVATE`: a process-private VAD (`VirtualAlloc`, heap, stack).
pub const MEM_PRIVATE: u32 = 0x0002_0000;
/// `MEM_MAPPED`: a mapped view (section object, file mapping).
pub const MEM_MAPPED: u32 = 0x0004_0000;
/// `MEM_IMAGE`: a mapped PE image.
pub const MEM_IMAGE: u32 = 0x0100_0000;

/// Wire form of one [`Request::QueryRegion`] answer: a fixed 40-byte
/// little-endian POD packed in declaration order — the same convention as
/// [`LOCK_LIST_ENTRY_SIZE`]. The system's `MEMORY_BASIC_INFORMATION` is
/// **not** used directly: it gained a `PartitionId` field on Windows 10
/// 2004 and its layout shifts with the OS version, so the driver narrows
/// it to these stable fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionInfo {
    /// Region start, page-aligned.
    pub base: u64,
    /// Allocation base (the VAD or image base) this region belongs to.
    pub allocation_base: u64,
    /// Size of this region in bytes.
    pub region_size: u64,
    /// [`MEM_COMMIT`] / [`MEM_RESERVE`] / [`MEM_FREE`].
    pub state: u32,
    /// Current page protection; 0 when the region is free.
    pub protect: u32,
    /// Protection the VAD was created with.
    pub allocation_protect: u32,
    /// [`MEM_IMAGE`] / [`MEM_MAPPED`] / [`MEM_PRIVATE`].
    pub typ: u32,
}

/// Bytes of bulk one [`Request::QueryRegion`] answer carries.
pub const REGION_INFO_SIZE: usize = 40;

impl RegionInfo {
    /// Packs the fields little-endian in declaration order. The array
    /// length *is* [`REGION_INFO_SIZE`], so the wire size cannot drift.
    pub fn encode(&self) -> [u8; REGION_INFO_SIZE] {
        let mut out = [0u8; REGION_INFO_SIZE];
        out[0..8].copy_from_slice(&self.base.to_le_bytes());
        out[8..16].copy_from_slice(&self.allocation_base.to_le_bytes());
        out[16..24].copy_from_slice(&self.region_size.to_le_bytes());
        out[24..28].copy_from_slice(&self.state.to_le_bytes());
        out[28..32].copy_from_slice(&self.protect.to_le_bytes());
        out[32..36].copy_from_slice(&self.allocation_protect.to_le_bytes());
        out[36..40].copy_from_slice(&self.typ.to_le_bytes());
        out
    }

    /// Unpacks a [`REGION_INFO_SIZE`]-byte wire record. A short buffer is
    /// a truncated response and is rejected rather than guessed at.
    pub fn decode(bytes: &[u8]) -> Option<RegionInfo> {
        if bytes.len() < REGION_INFO_SIZE {
            return None;
        }
        let u64_at = |at: usize| -> Option<u64> {
            Some(u64::from_le_bytes(bytes[at..at + 8].try_into().ok()?))
        };
        let u32_at = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(bytes[at..at + 4].try_into().ok()?))
        };
        Some(RegionInfo {
            base: u64_at(0)?,
            allocation_base: u64_at(8)?,
            region_size: u64_at(16)?,
            state: u32_at(24)?,
            protect: u32_at(28)?,
            allocation_protect: u32_at(32)?,
            typ: u32_at(36)?,
        })
    }
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
/// Processes the driver can track as "suspended by us" at once.
pub const MAX_PROCESS_SUSPENDS: usize = 32;
/// Lowest address treated as kernel space on x64 (`0xFFFF8000_00000000`).
pub const MIN_KERNEL_ADDRESS: u64 = 0xFFFF_8000_0000_0000;

/// Whether `address` is in the x64 kernel half of the address space.
pub const fn is_kernel_address(address: u64) -> bool {
    address >= MIN_KERNEL_ADDRESS
}

/// Validates a hook target against its `pid`: `pid == 0` selects the kernel
/// half (the address must be a kernel address), anything else selects that
/// process's user half (the address must be below the kernel split). The
/// trampoline follows the same rule.
pub const fn check_hook_address(pid: u64, address: u64) -> bool {
    if pid == 0 {
        is_kernel_address(address)
    } else {
        !is_kernel_address(address) && address != 0
    }
}

/// [`HookOp::Probe`] response flag: the stolen prologue contains a
/// RIP-relative memory operand (the driver rebases it in a detour).
pub const HOOK_PROBE_RIP_RELATIVE: u64 = 1;
/// [`HookOp::Probe`] response flag: the stolen prologue contains a relative
/// branch, which the driver refuses to relocate.
pub const HOOK_PROBE_RELATIVE_BRANCH: u64 = 2;

/// One kernel-hook table operation, MinHook-style: `Create` only records the
/// hook (state `disabled`), `Enable`/`Disable` toggle the patched bytes, and
/// `Remove` restores the original bytes and drops the entry.
///
/// Every variant that names a target space carries `pid`: `0` selects the
/// kernel half (global, patch kernel text directly), any other value selects
/// that process's user half (the driver attaches to the process and patches
/// its user pages through an MDL). `pid` on `Enable`/`Disable`/`Remove` must
/// match the `pid` the hook was created with, so ids stay process-local.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HookOp<'a> {
    /// Record a hook on `target`. The driver decodes the prologue to find the
    /// replaced length and builds the jump; `trampoline` is the caller's
    /// already-executable detour target.
    Create {
        pid: u64,
        target: u64,
        trampoline: u64,
    },
    /// Record a hook whose detour is `code`: the driver allocates executable
    /// kernel memory, copies the bytes there and points the jump at that
    /// buffer. The buffer is owned by the hook entry and freed by
    /// `Remove`/`RemoveAll`/shutdown. The response carries the original
    /// prologue bytes followed by the buffer address (little-endian u64).
    CreateShellcode {
        pid: u64,
        target: u64,
        #[serde(borrow)]
        code: &'a [u8],
    },
    /// Install a detour whose trampoline the driver builds: it relocates the
    /// stolen prologue into the same executable buffer and leaves its address
    /// at `buffer + 0`, so the code can `call qword ptr [rip-14]` to reach
    /// the original. Response: `value = id`, `count = replaced_len`,
    /// bulk = original prologue, then the code address and the trampoline
    /// address (two little-endian u64s).
    CreateDetour {
        pid: u64,
        target: u64,
        #[serde(borrow)]
        code: &'a [u8],
    },
    /// Read-only prologue probe. Response: `value` = flags
    /// (bit 0 = RIP-relative operand present, bit 1 = relative branch
    /// present), `count = replaced_len`, bulk = original prologue followed by
    /// a little-endian u32 holding the length a 14-byte absolute jump would
    /// need (0 when the prologue is too long).
    Probe {
        pid: u64,
        target: u64,
    },
    /// Patch the target so calls reach the trampoline again.
    Enable {
        pid: u64,
        id: u64,
    },
    /// Restore the original prologue bytes.
    Disable {
        pid: u64,
        id: u64,
    },
    /// Restore the original bytes (if enabled) and drop the entry.
    Remove {
        pid: u64,
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
    /// Same for the driver-built trampoline ([`HookOp::CreateDetour`]):
    /// installs a detour that calls the original prologue, calls the target,
    /// disables and removes. Answers `value = 1` (verified) or `0`
    /// (allocation refused, e.g. HVCI).
    DetourSelfTest,
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
    /// Dumps the whole driver-side lock table: every entry this driver is
    /// replaying, whoever created it (Rust SDK, GUI worker, Lua script).
    /// Response: `value = count`, `count = count`, bulk = `count` packed
    /// entries of [`LOCK_LIST_ENTRY_SIZE`] bytes (id, pid, address as
    /// little-endian u64, then length as a little-endian u32).
    LockList,
    /// Reserve, commit and zero one region of target-process virtual
    /// memory. `protect` must satisfy [`valid_alloc_protect`]; the reply
    /// carries the base address in [`ResponseMeta::value`]. The driver
    /// rounds `size` up to the page size.
    Alloc {
        pid: u64,
        size: u32,
        protect: u32,
    },
    /// Release the whole VAD that contains `address`. The whole
    /// allocation is freed (`MEM_RELEASE`), so no size is carried.
    ///
    /// `ZwFreeVirtualMemory` only requires the address to *be* an
    /// allocation base — it does not check who allocated it, and the
    /// driver keeps no ownership table. An address belonging to the
    /// target itself (its heap, a stack, a mapped view's base) is
    /// therefore freed just as happily, tearing that mapping down.
    Free {
        pid: u64,
        address: u64,
    },
    /// Suspends every thread of the process `pid` (kernel
    /// `PsSuspendProcess`). Refuses `pid = 0` (Idle), `pid = 4` (System) and
    /// the calling client's own pid. Idempotent: a process this driver
    /// already suspended reports success without suspending twice.
    /// Answers `value = 1` when the suspend was applied, `0` when it was a
    /// no-op.
    Suspend {
        pid: u64,
    },
    /// Continues a process this driver previously suspended (kernel
    /// `PsResumeProcess`). A `pid` this driver never suspended is a no-op
    /// (`value = 0`): the driver never decrements a suspend count it did not
    /// increment.
    Resume {
        pid: u64,
    },
    /// One kernel-hook table operation (see [`HookOp`]). `Create` returns the
    /// new id in [`ResponseMeta::value`], the replaced length in
    /// [`ResponseMeta::count`] and the original prologue bytes in the bulk
    /// region; the `*All` variants return the number of affected hooks.
    Hook {
        op: HookOp<'a>,
    },
    /// Change the page protection of an already mapped region in the
    /// target process — image pages, the target's own allocations and
    /// [`Request::Alloc`] pages alike. The whole span must sit inside
    /// existing mappings: probe with [`Request::QueryRegion`] first,
    /// because a range crossing a free hole fails. `protect` must satisfy
    /// [`valid_protect`].
    ///
    /// Response: `value` = the first page's previous protection, `count` =
    /// the bytes the kernel actually protected (`size` rounded up to
    /// pages by `ZwProtectVirtualMemory`).
    Protect {
        pid: u64,
        /// Need not be page aligned; the kernel rounds it down.
        address: u64,
        /// Bytes, 1..=[`MAX_PROTECT_SIZE`].
        size: u32,
        /// One of [`PROT_READ`] / [`PROT_RW`] / [`PROT_RX`] / [`PROT_RWX`].
        protect: u32,
        /// Attach `PAGE_TARGETS_NO_UPDATE` so the region's existing CFG
        /// bitmap entries are left as they are instead of being re-marked
        /// valid indirect-call targets.
        cfg_no_update: bool,
    },
    /// Describe the region containing `address`
    /// (`ZwQueryVirtualMemory(MemoryBasicInformation)`): probe the range
    /// before [`Request::Protect`], and classify the pages
    /// ([`RegionInfo::typ`], [`RegionInfo::state`]) to decide whether
    /// they may change at all.
    ///
    /// Response: `value` = region base, `count` = region size, bulk =
    /// [`REGION_INFO_SIZE`] bytes of packed [`RegionInfo`].
    QueryRegion {
        pid: u64,
        address: u64,
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
            Self::LockList => Ok(()),
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
            Self::Suspend { pid } | Self::Resume { pid } => {
                // pid 0 (Idle) and 4 (System) must never be frozen: the
                // System process owns kernel worker threads.
                if *pid == 0 || *pid == 4 {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(())
            }
            Self::Hook { op } => match op {
                HookOp::Create {
                    pid,
                    target,
                    trampoline,
                } => {
                    if !check_hook_address(*pid, *target) || !check_hook_address(*pid, *trampoline)
                    {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    Ok(())
                }
                HookOp::CreateShellcode { pid, target, code } => {
                    if !check_hook_address(*pid, *target) {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    Ok(())
                }
                HookOp::CreateDetour { pid, target, code } => {
                    if !check_hook_address(*pid, *target) {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    Ok(())
                }
                HookOp::Probe { pid, target } => {
                    if !check_hook_address(*pid, *target) {
                        Err(ProtocolError::InvalidPayload)
                    } else {
                        Ok(())
                    }
                }
                HookOp::Enable { pid: _, id }
                | HookOp::Disable { pid: _, id }
                | HookOp::Remove { pid: _, id } => {
                    if *id == 0 {
                        Err(ProtocolError::InvalidPayload)
                    } else {
                        Ok(())
                    }
                }
                HookOp::EnableAll | HookOp::DisableAll | HookOp::RemoveAll => Ok(()),
                HookOp::SelfTest | HookOp::ShellcodeSelfTest | HookOp::DetourSelfTest => Ok(()),
            },
            Self::Protect {
                pid,
                address,
                size,
                protect,
                cfg_no_update: _,
            } => {
                check_pid(*pid)?;
                if *address == 0 || *size == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                if *size as usize > MAX_PROTECT_SIZE {
                    return Err(ProtocolError::TooLarge);
                }
                if !valid_protect(*protect) {
                    return Err(ProtocolError::InvalidPayload);
                }
                Ok(())
            }
            Self::QueryRegion { pid, address } => {
                check_pid(*pid)?;
                if *address == 0 {
                    Err(ProtocolError::InvalidPayload)
                } else {
                    Ok(())
                }
            }
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
