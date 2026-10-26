//! MinHook-style inline hooks, applied through MDL mappings.
//!
//! The driver owns the whole lifecycle a user-mode hook library would own —
//! entry-prologue instruction decoding, jump construction, an
//! enabled/disabled state machine, conflict detection and LIFO teardown —
//! except trampoline generation: the caller supplies an already-executable
//! detour address (its own trampoline: relocated stolen instructions plus a
//! jump back to `target + replaced_len`).
//!
//! Every operation names a target space through `pid`: `0` means the kernel
//! half (global kernel text, patched through a kernel MDL), anything else
//! means that process's user half — the driver reads the prologue with
//! `MmCopyVirtualMemory` and writes the patch through an MDL attach that
//! bypasses user write-protection, exactly like `memory::write_process_memory_mdl`.
//! Executable detour buffers are allocated in the same space the target
//! lives in: kernel pool for `pid = 0`, `ZwAllocateVirtualMemory` in the
//! target process otherwise.
//!
//! Safety contract (also mirrored in AGENTS.md):
//! - the caller guarantees no thread is executing the patched prologue while
//!   a hook is created/enabled/disabled; kernel code has no safe equivalent
//!   of MinHook's thread freeze;
//! - `replaced_len` comes from decoding the real bytes, so the write never
//!   splits an instruction;
//! - the caller removes hooks before freeing the detour/trampoline memory;
//! - PatchGuard-protected bytes and HVCI-enforced code pages are *not*
//!   detected — writing them bugchecks the machine;
//! - all operations run on the ring worker at `PASSIVE_LEVEL`, without any
//!   other driver lock held, and are panic-free.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::{cell::UnsafeCell, ptr};

use ks_core::protocol::{MAX_HOOK_SHELLCODE, MAX_HOOK_STOLEN_LEN, MAX_KERNEL_HOOKS};

use crate::wdm::*;

/// Longest replacement the driver writes: `FF 25 00000000 <addr64>`.
const ABSOLUTE_JUMP_LEN: usize = 14;
/// Shortest replacement: `E9 <rel32>`.
const RELATIVE_JUMP_LEN: usize = 5;

/// One installed hook. `Copy` so the static table can start as `None`.
#[derive(Clone, Copy)]
struct HookEntry {
    id: u64,
    /// `0` = kernel half; otherwise the owning process.
    pid: u64,
    target: u64,
    /// Bytes replaced at `target` (an instruction boundary, >= patch_len).
    stolen_len: usize,
    /// Length of the jump actually written.
    patch_len: usize,
    patch: [u8; MAX_HOOK_STOLEN_LEN],
    original: [u8; MAX_HOOK_STOLEN_LEN],
    /// Executable buffer holding the detour bytes, if this hook was created
    /// from shellcode/detour. Freed by `remove`/`shutdown`.
    owned_code: u64,
    enabled: bool,
}

/// Pool tag for shellcode buffers (`kShc` in little-endian).
const SHELLCODE_TAG: u32 = 0x6368_536B;
/// Executable non-paged pool flag for `ExAllocatePool2`
/// (`POOL_FLAG_NON_PAGED_EXECUTE`).
const POOL_FLAG_NON_PAGED_EXECUTE: u64 = 0x0000_0000_0000_0080;

/// Pid of the client whose request is being executed. Set by the request
/// dispatcher before any hook operation runs, so [`write_code`] can skip the
/// self-hook suspension (which would deadlock the caller).
static CLIENT_PID: AtomicU32 = AtomicU32::new(0);

/// Publishes the pid of the client that owns the in-flight request.
pub(crate) fn set_client_pid(pid: u32) {
    CLIENT_PID.store(pid, Ordering::SeqCst);
}

struct HookTable {
    entries: [Option<HookEntry>; MAX_KERNEL_HOOKS],
    next_id: u64,
}

impl HookTable {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_KERNEL_HOOKS],
            next_id: 1,
        }
    }

    /// Reserves a fresh id (never 0, never reused).
    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1).max(1);
        id
    }

    fn find(&self, id: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Some(entry) if entry.id == id))
    }

    fn find_target(&self, pid: u64, target: u64) -> Option<usize> {
        self.entries.iter().position(
            |entry| matches!(entry, Some(entry) if entry.pid == pid && entry.target == target),
        )
    }

    fn free_slot(&self) -> Option<usize> {
        self.entries.iter().position(Option::is_none)
    }
}

struct KernelFastMutex(UnsafeCell<FAST_MUTEX>);
// SAFETY: every access goes through ExAcquireFastMutex/ExReleaseFastMutex.
unsafe impl Sync for KernelFastMutex {}

struct KernelTable(UnsafeCell<HookTable>);
// SAFETY: every access goes through the table mutex.
unsafe impl Sync for KernelTable {}

static TABLE_MUTEX: KernelFastMutex =
    KernelFastMutex(UnsafeCell::new(unsafe { core::mem::zeroed() }));
static TABLE: KernelTable = KernelTable(UnsafeCell::new(HookTable::new()));
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Runs `operation` with the table mutex held. The critical section must
/// only touch table memory: no MDL allocation, no kernel code patching and
/// no waits inside it.
fn with_table<T>(operation: impl FnOnce(&mut HookTable) -> T) -> T {
    // SAFETY: every caller reaches the table only here, and the mutex
    // serialises the critical section; `init` ran before the worker exists.
    unsafe {
        let mutex = TABLE_MUTEX.0.get();
        ExAcquireFastMutex(mutex);
        let result = operation(&mut *TABLE.0.get());
        ExReleaseFastMutex(mutex);
        result
    }
}

/// Initialises the hook table. Called from `comm::start` before the worker
/// can receive a request.
pub fn init() {
    // SAFETY: first statement of the first caller; no thread exists yet.
    unsafe { init_fast_mutex(TABLE_MUTEX.0.get()) };
    INITIALIZED.store(true, Ordering::SeqCst);
}

fn initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

/// `pid == 0` must name a kernel address; anything else must name a user
/// address, so the caller cannot mix spaces.
fn target_ok(pid: u64, target: u64) -> bool {
    ks_core::protocol::check_hook_address(pid, target)
}

/// Reads up to [`MAX_HOOK_STOLEN_LEN`] prologue bytes in the selected space.
///
/// Kernel space is read directly after a page-table probe; a user space is
/// read through an MDL attach, which faults safely instead of bugchecking.
fn read_prologue(pid: u64, address: u64) -> Result<[u8; MAX_HOOK_STOLEN_LEN], NTSTATUS> {
    let mut buffer = [0u8; MAX_HOOK_STOLEN_LEN];
    if pid == 0 {
        let read = read_kernel_bytes(address, MAX_HOOK_STOLEN_LEN).ok_or(STATUS_INVALID_ADDRESS)?;
        buffer.copy_from_slice(&read);
        Ok(buffer)
    } else {
        crate::memory::read_process_memory_mdl(pid, address, &mut buffer)?;
        Ok(buffer)
    }
}

/// Writes `data` over `address` in the selected space.
///
/// Both paths go through an MDL, so user pages are locked and the kernel
/// mapping bypasses their user-mode write protection exactly like
/// `memory::write_process_memory_mdl`.
/// Writes `data` over `address` in the selected space.
///
/// Both paths go through an MDL, so user pages are locked and the kernel
/// mapping bypasses their user-mode write protection exactly like
/// `memory::write_process_memory_mdl`.
///
/// **Invariant (enforced here, not by the caller):** patching a user
/// process's executable memory is always bracketed by `suspend`/`resume` of
/// that process, so no thread of the target can be executing the bytes
/// while they change. A refusal to suspend fails the write instead of
/// patching anyway.
///
/// The one exception is a target that is the calling client itself:
/// suspending it would deadlock — the client's thread is blocked in this
/// very round trip — so the write proceeds unsuspended and the caller's
/// "nothing executes the prologue" contract is the only protection. The
/// kernel path (`pid == 0`) is never suspended either: `PsSuspendProcess`
/// does not stop kernel threads, so there is no freeze to apply.
fn write_code(pid: u64, address: u64, data: &[u8]) -> NTSTATUS {
    if pid == 0 {
        // SAFETY: the caller validated the kernel address (page probe).
        unsafe { write_kernel_code(address, data) }
    } else {
        let client_pid = CLIENT_PID.load(Ordering::SeqCst) as u64;
        // Skip only the self-hook case: freezing the process that is
        // waiting for this round trip would deadlock it.
        let mut suspended = false;
        if pid != client_pid {
            match crate::suspend::suspend(pid, client_pid) {
                // `1`: we froze it, so the resume below is ours to make.
                Ok(1) => suspended = true,
                // `0`: already frozen by us (an explicit `suspend` from the
                // client); leave it frozen, do not resume it here.
                Ok(_) => {}
                Err(status) => return status,
            }
        }
        let result = match crate::memory::write_process_memory_mdl(pid, address, data) {
            Ok(()) => STATUS_SUCCESS,
            Err(status) => status,
        };
        if suspended {
            let _ = crate::suspend::resume(pid, client_pid);
        }
        result
    }
}

/// Allocates executable memory in the selected space.
fn allocate_exec(pid: u64, size: usize) -> Result<u64, NTSTATUS> {
    if pid == 0 {
        allocate_executable(size)
    } else {
        crate::memory::allocate(pid, size, PAGE_EXECUTE_READWRITE)
    }
}

/// Frees a buffer previously returned by [`allocate_exec`].
fn free_exec(pid: u64, address: u64) {
    if address == 0 {
        return;
    }
    if pid == 0 {
        // SAFETY: allocated by `allocate_executable` with this tag.
        unsafe { ExFreePoolWithTag(address as *mut _, SHELLCODE_TAG) };
    } else {
        let _ = crate::memory::free(pid, address);
    }
}

/// Read-only prologue report, used to plan a detour before installing it.
pub struct ProbeInfo {
    pub replaced_len: usize,
    /// Bytes a 14-byte absolute jump needs, or 0 when the prologue cannot
    /// be covered within [`MAX_HOOK_STOLEN_LEN`].
    pub absolute_len: usize,
    pub original: [u8; MAX_HOOK_STOLEN_LEN],
    pub rip_relative: bool,
    pub relative_branch: bool,
}

/// Decodes the target prologue without touching it.
pub fn probe(pid: u64, target: u64) -> Result<ProbeInfo, NTSTATUS> {
    if !initialized() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if !target_ok(pid, target) {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let original = read_prologue(pid, target)?;
    let replaced_len = decode_prologue(&original, RELATIVE_JUMP_LEN)?;
    let absolute_len = decode_prologue(&original, ABSOLUTE_JUMP_LEN).unwrap_or(0);

    let mut rip_relative = false;
    let mut relative_branch = false;
    let mut cursor = 0usize;
    while cursor < replaced_len {
        let Some(decoded) = decode(&original[cursor..]) else {
            return Err(STATUS_NOT_SUPPORTED);
        };
        rip_relative |= decoded.rip_disp.is_some();
        relative_branch |= decoded.relative_branch;
        cursor += decoded.len;
    }
    Ok(ProbeInfo {
        replaced_len,
        absolute_len,
        original,
        rip_relative,
        relative_branch,
    })
}

/// Layout of a driver-built detour buffer:
///
/// ```text
/// +0                  : trampoline address (filled in by the driver)
/// +8                  : user code (execution starts here)
/// +8+align8(code_len) : trampoline (relocated prologue + jump back)
/// ```
pub struct DetourInfo {
    pub id: u64,
    pub replaced_len: usize,
    pub original: [u8; MAX_HOOK_STOLEN_LEN],
    /// Address the target now jumps to (buffer + 8).
    pub code_address: u64,
    /// Address of the relocated-original trampoline; also stored at
    /// `buffer + 0` so the shellcode can `call [rip-14]`.
    pub trampoline_address: u64,
}

/// Byte offset of the trampoline pointer inside the detour buffer.
const DETOUR_HEADER: usize = 8;
/// `FF 25 00000000 <addr64>` used to jump back from the trampoline.
const MAX_TRAMPOLINE: usize = MAX_HOOK_STOLEN_LEN + ABSOLUTE_JUMP_LEN;

const fn align8(value: usize) -> usize {
    (value + 7) & !7
}

/// Installs a detour whose original prologue the driver replays for it.
///
/// The driver copies `code` to `buffer + 8`, builds a trampoline from the
/// stolen instructions (fixing up RIP-relative displacements so they still
/// point at the same data), appends a jump back to `target + replaced_len`
/// and stores the trampoline address at `buffer + 0`. Code that wants to
/// call the original can therefore use the fixed 6-byte sequence
/// `call qword ptr [rip-14]` (`FF 15 F2 FF FF FF`).
///
/// Relative branches inside the stolen prologue are refused with
/// `STATUS_NOT_SUPPORTED`: relocating them needs full branch rewriting.
pub fn create_detour(pid: u64, target: u64, code: &[u8]) -> Result<DetourInfo, NTSTATUS> {
    if !initialized() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if !target_ok(pid, target) || code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let original = read_prologue(pid, target)?;

    let total = DETOUR_HEADER + align8(code.len()) + MAX_TRAMPOLINE;
    let mut last_status = STATUS_INVALID_ADDRESS;
    for _attempt in 0..DETOUR_ALLOC_ATTEMPTS {
        let buffer = allocate_exec(pid, total)?;
        let code_address = buffer + DETOUR_HEADER as u64;
        let trampoline_address = code_address + align8(code.len()) as u64;
        let (_, patch_len) = build_patch(target, code_address);
        let Ok(stolen_len) = decode_prologue(&original, patch_len) else {
            free_exec(pid, buffer);
            return Err(STATUS_NOT_SUPPORTED);
        };
        // RIP-relative operands can only be rebased within ±2 GiB; try
        // another allocation when this one landed too far away.
        if !rebase_is_possible(&original[..stolen_len], target, trampoline_address) {
            free_exec(pid, buffer);
            last_status = STATUS_NOT_SUPPORTED;
            continue;
        }
        match build_detour_at(pid, buffer, target, code, &original[..stolen_len]) {
            Ok(info) => {
                return match insert(pid, target, info.code_address, buffer) {
                    Ok((id, replaced_len, stored)) => Ok(DetourInfo {
                        id,
                        replaced_len,
                        original: stored,
                        code_address: info.code_address,
                        trampoline_address: info.trampoline_address,
                    }),
                    Err(status) => {
                        free_exec(pid, buffer);
                        Err(status)
                    }
                };
            }
            Err(status) => {
                free_exec(pid, buffer);
                return Err(status);
            }
        }
    }
    Err(last_status)
}

/// How many executable buffers the driver tries before giving up on finding
/// one whose distance lets RIP-relative operands be rebased.
const DETOUR_ALLOC_ATTEMPTS: usize = 8;

/// Whether every RIP-relative operand in `stolen` stays encodable after the
/// move to `trampoline_address`, and whether relocation is possible at all.
fn rebase_is_possible(stolen: &[u8], target: u64, trampoline_address: u64) -> bool {
    let delta = (target as i64).wrapping_sub(trampoline_address as i64);
    let mut cursor = 0usize;
    while cursor < stolen.len() {
        let Some(decoded) = decode(&stolen[cursor..]) else {
            return false;
        };
        if decoded.relative_branch {
            return false;
        }
        if let Some(offset) = decoded.rip_disp {
            let at = cursor + offset;
            let Some(bytes) = stolen.get(at..at + 4) else {
                return false;
            };
            let displacement = i32::from_le_bytes(bytes.try_into().unwrap()) as i64;
            if i32::try_from(displacement + delta).is_err() {
                return false;
            }
        }
        cursor += decoded.len;
    }
    true
}

struct BuiltDetour {
    code_address: u64,
    trampoline_address: u64,
}

/// Fills the executable buffer: header, user code, relocated trampoline.
fn build_detour_at(
    pid: u64,
    buffer: u64,
    target: u64,
    code: &[u8],
    stolen: &[u8],
) -> Result<BuiltDetour, NTSTATUS> {
    let code_address = buffer + DETOUR_HEADER as u64;
    let trampoline_address = code_address + align8(code.len()) as u64;

    let mut trampoline = [0u8; MAX_TRAMPOLINE];
    let trampoline_len = build_trampoline(target, stolen, trampoline_address, &mut trampoline)?;

    // The buffer lives in the target space; write it through the same
    // MDL-dispatching path so user allocations are handled by an attach.
    let mut header = [0u8; DETOUR_HEADER];
    header.copy_from_slice(&trampoline_address.to_le_bytes());
    let status = write_code(pid, buffer, &header);
    if !nt_success(status) {
        return Err(status);
    }
    let status = write_code(pid, buffer + DETOUR_HEADER as u64, code);
    if !nt_success(status) {
        return Err(status);
    }
    let status = write_code(
        pid,
        buffer + DETOUR_HEADER as u64 + align8(code.len()) as u64,
        &trampoline[..trampoline_len],
    );
    if !nt_success(status) {
        return Err(status);
    }
    Ok(BuiltDetour {
        code_address,
        trampoline_address,
    })
}
/// Copies the stolen instructions into `out`, rebases RIP-relative operands
/// and appends an absolute jump back to `target + stolen.len()`.
fn build_trampoline(
    target: u64,
    stolen: &[u8],
    trampoline_address: u64,
    out: &mut [u8; MAX_TRAMPOLINE],
) -> Result<usize, NTSTATUS> {
    out[..stolen.len()].copy_from_slice(stolen);
    let delta = (target as i64).wrapping_sub(trampoline_address as i64);

    let mut cursor = 0usize;
    while cursor < stolen.len() {
        let Some(decoded) = decode(&stolen[cursor..]) else {
            return Err(STATUS_NOT_SUPPORTED);
        };
        if decoded.relative_branch {
            return Err(STATUS_NOT_SUPPORTED);
        }
        if let Some(offset) = decoded.rip_disp {
            let at = cursor + offset;
            let displacement = i32::from_le_bytes(
                out[at..at + 4]
                    .try_into()
                    .map_err(|_| STATUS_NOT_SUPPORTED)?,
            );
            let rebased = i32::try_from((displacement as i64).wrapping_add(delta))
                .map_err(|_| STATUS_NOT_SUPPORTED)?;
            out[at..at + 4].copy_from_slice(&rebased.to_le_bytes());
        }
        cursor += decoded.len;
    }

    // FF 25 00000000 <target + stolen_len>
    out[cursor] = 0xFF;
    out[cursor + 1] = 0x25;
    out[cursor + 2..cursor + 6].copy_from_slice(&0u32.to_le_bytes());
    let back = target.wrapping_add(stolen.len() as u64);
    out[cursor + 6..cursor + 14].copy_from_slice(&back.to_le_bytes());
    Ok(cursor + ABSOLUTE_JUMP_LEN)
}

/// Records a hook (`disabled`). Returns `(id, replaced_len, original_bytes)`.
pub fn create(
    pid: u64,
    target: u64,
    trampoline: u64,
) -> Result<(u64, usize, [u8; MAX_HOOK_STOLEN_LEN]), NTSTATUS> {
    insert(pid, target, trampoline, 0)
}

/// Allocates executable memory in the target's space, copies `code` there
/// and records a hook whose detour is that buffer. Returns
/// `(id, replaced_len, original_bytes, code_address)`. The buffer belongs to
/// the hook entry from here on: `remove`, `remove_all` and shutdown free it.
pub fn create_shellcode(
    pid: u64,
    target: u64,
    code: &[u8],
) -> Result<(u64, usize, [u8; MAX_HOOK_STOLEN_LEN], u64), NTSTATUS> {
    if !initialized() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if !target_ok(pid, target) || code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let buffer = allocate_exec(pid, code.len())?;
    let copied = write_code(pid, buffer, code);
    if !nt_success(copied) {
        free_exec(pid, buffer);
        return Err(copied);
    }
    match insert(pid, target, buffer, buffer) {
        Ok((id, stolen_len, original)) => Ok((id, stolen_len, original, buffer)),
        Err(status) => {
            free_exec(pid, buffer);
            Err(status)
        }
    }
}

/// Shared create path: `owned_code` is 0 for caller-provided trampolines.
fn insert(
    pid: u64,
    target: u64,
    trampoline: u64,
    owned_code: u64,
) -> Result<(u64, usize, [u8; MAX_HOOK_STOLEN_LEN]), NTSTATUS> {
    if !initialized() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if !target_ok(pid, target) || !target_ok(pid, trampoline) {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let original = read_prologue(pid, target)?;

    let (patch, patch_len) = build_patch(target, trampoline);
    let stolen_len = decode_prologue(&original, patch_len)?;
    let mut stored = [0u8; MAX_HOOK_STOLEN_LEN];
    stored[..stolen_len].copy_from_slice(&original[..stolen_len]);

    let mut patch_bytes = [0u8; MAX_HOOK_STOLEN_LEN];
    patch_bytes[..patch_len].copy_from_slice(&patch[..patch_len]);

    with_table(|table| {
        if table.find_target(pid, target).is_some() {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        let Some(slot) = table.free_slot() else {
            return Err(STATUS_QUOTA_EXCEEDED);
        };
        let id = table.allocate_id();
        table.entries[slot] = Some(HookEntry {
            id,
            pid,
            target,
            stolen_len,
            patch_len,
            patch: patch_bytes,
            original: stored,
            owned_code,
            enabled: false,
        });
        Ok(id)
    })
    .map(|id| (id, stolen_len, stored))
}

/// Non-paged buffer of `size` bytes made executable through its MDL. Returns
/// `STATUS_INVALID_PAGE_PROTECTION` when the platform refuses (HVCI), which
/// the self test treats as "skipped".
fn allocate_executable(size: usize) -> Result<u64, NTSTATUS> {
    // SAFETY: fixed flags/tag; the returned block is owned by the hook entry
    // and freed on the error paths below.
    let memory = unsafe { ExAllocatePool2(POOL_FLAG_NON_PAGED_EXECUTE, size, SHELLCODE_TAG) };
    if memory.is_null() {
        // The realistic cause is a policy that forbids executable pool
        // (HVCI enforces W^X for kernel pool).
        return Err(STATUS_INVALID_PAGE_PROTECTION);
    }
    Ok(memory as u64)
}

/// Applies the jump. Already-enabled hooks report success.
pub fn enable(pid: u64, id: u64) -> NTSTATUS {
    if !initialized() {
        return STATUS_NOT_SUPPORTED;
    }
    let entry = with_table(|table| table.find(id).and_then(|slot| table.entries[slot]));
    let Some(entry) = entry else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    if entry.pid != pid {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    }
    if entry.enabled {
        return STATUS_SUCCESS;
    }
    let status = write_code(pid, entry.target, &entry.patch[..entry.patch_len]);
    if !nt_success(status) {
        return status;
    }
    with_table(|table| {
        if let Some(slot) = table.find(id) {
            if let Some(entry) = table.entries[slot].as_mut() {
                entry.enabled = true;
            }
        }
    });
    STATUS_SUCCESS
}

/// Restores the original prologue. A disabled hook reports success.
pub fn disable(pid: u64, id: u64) -> NTSTATUS {
    if !initialized() {
        return STATUS_NOT_SUPPORTED;
    }
    let entry = with_table(|table| table.find(id).and_then(|slot| table.entries[slot]));
    let Some(entry) = entry else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    if entry.pid != pid {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    }
    if !entry.enabled {
        return STATUS_SUCCESS;
    }
    let status = write_code(pid, entry.target, &entry.original[..entry.stolen_len]);
    if !nt_success(status) {
        return status;
    }
    with_table(|table| {
        if let Some(slot) = table.find(id) {
            if let Some(entry) = table.entries[slot].as_mut() {
                entry.enabled = false;
            }
        }
    });
    STATUS_SUCCESS
}

/// Disables (if needed) and forgets the hook. Unknown ids fail.
pub fn remove(pid: u64, id: u64) -> NTSTATUS {
    let status = disable(pid, id);
    if !nt_success(status) {
        return status;
    }
    // Remove the entry and extract its owned buffer, if any.
    let removed = with_table(|table| match table.find(id) {
        Some(slot) => {
            let entry = table.entries[slot];
            if entry.map(|entry| entry.pid) != Some(pid) {
                return None;
            }
            table.entries[slot] = None;
            entry
        }
        None => None,
    });
    match removed {
        Some(entry) => {
            // The detour buffer is released only after the jump is gone, so
            // no core can fetch instructions from a freed page.
            free_exec(entry.pid, entry.owned_code);
            STATUS_SUCCESS
        }
        None => {
            if with_table(|table| table.find(id).is_some()) {
                STATUS_INVALID_PARAMETER
            } else {
                STATUS_OBJECT_NAME_NOT_FOUND
            }
        }
    }
}

/// Enables every recorded hook; stops at the first patch failure.
pub fn enable_all() -> Result<u64, NTSTATUS> {
    let (entries, count) = collect_entries();
    let mut touched = 0u64;
    for &(pid, id) in &entries[..count] {
        let status = enable(pid, id);
        if !nt_success(status) {
            return Err(status);
        }
        touched += 1;
    }
    Ok(touched)
}

/// Restores every patched hook.
pub fn disable_all() -> Result<u64, NTSTATUS> {
    let (entries, count) = collect_entries();
    let mut touched = 0u64;
    for &(pid, id) in &entries[..count] {
        let status = disable(pid, id);
        if !nt_success(status) {
            return Err(status);
        }
        touched += 1;
    }
    Ok(touched)
}

/// Disables and forgets every hook, newest first (LIFO), so nested patches
/// restore in the reverse order they were applied.
pub fn remove_all() -> Result<u64, NTSTATUS> {
    let (mut entries, count) = collect_entries();
    entries[..count].reverse();
    let mut touched = 0u64;
    for &(pid, id) in &entries[..count] {
        let status = remove(pid, id);
        if !nt_success(status) {
            return Err(status);
        }
        touched += 1;
    }
    Ok(touched)
}

/// Snapshot of the live `(pid, id)` pairs, in table order. Ids are never 0,
/// so unused slots stay clearly distinguishable.
fn collect_entries() -> ([(u64, u64); MAX_KERNEL_HOOKS], usize) {
    let mut entries = [(0u64, 0u64); MAX_KERNEL_HOOKS];
    let mut count = 0usize;
    with_table(|table| {
        for entry in table.entries.iter().flatten() {
            if count < entries.len() {
                entries[count] = (entry.pid, entry.id);
                count += 1;
            }
        }
    });
    (entries, count)
}

/// Restores every patch and drops the table. Called from the `Shutdown`
/// request and from the unload path: kernel code must never keep jumping at
/// memory that is about to disappear.
pub fn shutdown() {
    if !initialized() {
        return;
    }
    let (entries, count) = collect_entries();
    for &(pid, id) in &entries[..count] {
        let _ = disable(pid, id);
    }
    let mut owned = [0u64; MAX_KERNEL_HOOKS];
    let mut pids = [0u64; MAX_KERNEL_HOOKS];
    with_table(|table| {
        for (index, entry) in table.entries.iter().enumerate() {
            if let Some(entry) = entry {
                owned[index] = entry.owned_code;
                pids[index] = entry.pid;
            }
        }
        table.entries = [None; MAX_KERNEL_HOOKS];
    });
    for (index, buffer) in owned.iter().enumerate() {
        if *buffer != 0 {
            free_exec(pids[index], *buffer);
        }
    }
}

/// Target of the hook self test: a leaf function in the driver's own `.text`
/// that nothing calls, so patching it cannot disturb the running driver.
/// Scratch byte the self-test target writes so its prologue is long enough
/// for either jump form (two 7-byte volatile stores: 7 and 14 bytes tile
/// both a 5-byte and a 14-byte patch without running past the function).
static SELFTEST_PAD: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

#[no_mangle]
#[inline(never)]
pub extern "system" fn ks_hook_selftest_target() -> u64 {
    // SAFETY: the byte lives in the driver's own data section.
    unsafe {
        ptr::write_volatile(SELFTEST_PAD.as_ptr(), 0x5A);
        ptr::write_volatile(SELFTEST_PAD.as_ptr(), 0xA5);
    }
    0x1111_1111
}

/// Detour of the hook self test. It does not reproduce the stolen prologue
/// because the target is never executed while the patch is in place.
#[no_mangle]
#[inline(never)]
pub extern "system" fn ks_hook_selftest_detour() -> u64 {
    0x2222_2222
}

/// Exercises the real patch path end to end on the driver's own code:
/// create, enable, verify the jump, disable, verify the restored bytes,
/// remove. Returns 1 when the patch was applied and restored, 0 when the
/// platform refused the write (HVCI keeps driver `.text` read-only).
pub fn self_test() -> Result<u64, NTSTATUS> {
    let target = ks_hook_selftest_target as *const () as u64;
    let detour = ks_hook_selftest_detour as *const () as u64;
    let (id, replaced_len, original) = create(0, target, detour)?;

    let status = enable(0, id);
    if !nt_success(status) {
        let _ = remove(0, id);
        return if status == STATUS_INVALID_PAGE_PROTECTION {
            Ok(0)
        } else {
            Err(status)
        };
    }

    let mut patched = [0u8; 2];
    // SAFETY: `target` is this driver's own executable text.
    unsafe { ptr::copy_nonoverlapping(target as *const u8, patched.as_mut_ptr(), 2) };
    let applied = patched[0] == 0xE9 || (patched[0] == 0xFF && patched[1] == 0x25);

    let status = disable(0, id);
    if !nt_success(status) {
        return Err(status);
    }

    let mut restored = [0u8; MAX_HOOK_STOLEN_LEN];
    // SAFETY: same function text; `replaced_len` came from our own decode.
    unsafe {
        ptr::copy_nonoverlapping(target as *const u8, restored.as_mut_ptr(), replaced_len);
    }
    let restored_ok = restored[..replaced_len] == original[..replaced_len];

    let status = remove(0, id);
    if !nt_success(status) {
        return Err(status);
    }
    if applied && restored_ok {
        Ok(1)
    } else {
        Err(STATUS_INVALID_PARAMETER)
    }
}

/// Shellcode detour used by [`shellcode_self_test`]: `mov eax, 0x22222222;
/// ret`. The target returns `u64`, so eax zero-extends.
const SELFTEST_SHELLCODE: [u8; 6] = [0xB8, 0x22, 0x22, 0x22, 0x22, 0xC3];

/// Exercises the shellcode path end to end on the driver's own code:
/// allocate executable memory, copy the stub, hook the leaf function, call
/// through the hook, disable, call again, remove. Returns 1 when the call
/// observed the stub's value before and the original value after, 0 when the
/// platform refused the executable allocation (HVCI).
pub fn shellcode_self_test() -> Result<u64, NTSTATUS> {
    let target_fn: extern "system" fn() -> u64 = ks_hook_selftest_target;
    let target = ks_hook_selftest_target as *const () as u64;

    let (id, _replaced_len, _original, _code) =
        match create_shellcode(0, target, &SELFTEST_SHELLCODE) {
            Ok(created) => created,
            Err(status) if status == STATUS_INVALID_PAGE_PROTECTION => return Ok(0),
            Err(status) => return Err(status),
        };

    let status = enable(0, id);
    if !nt_success(status) {
        let _ = remove(0, id);
        return if status == STATUS_INVALID_PAGE_PROTECTION {
            Ok(0)
        } else {
            Err(status)
        };
    }

    // SAFETY: the target is this driver's own leaf function; while patched it
    // jumps to the stub above, which returns without touching any state.
    let through_hook = (core::hint::black_box(target_fn))();
    let status = disable(0, id);
    if !nt_success(status) {
        return Err(status);
    }
    // SAFETY: after `disable` the prologue is restored.
    let restored = (core::hint::black_box(target_fn))();
    let status = remove(0, id);
    if !nt_success(status) {
        return Err(status);
    }

    if through_hook == 0x2222_2222 && restored == 0x1111_1111 {
        Ok(1)
    } else {
        Err(STATUS_INVALID_PARAMETER)
    }
}

/// Detour self-test code: `call qword ptr [rip-14]` (replays the original
/// prologue through the driver-built trampoline), `add rax, 1`, `ret`.
/// Uses `48 83 C0 01` (`add rax, imm8`) so the full 64-bit return value is
/// preserved, matching the client-side `ShellcodeBuilder` output.
const DETOUR_SELFTEST_CODE: [u8; 11] = [
    0xFF, 0x15, 0xF2, 0xFF, 0xFF, 0xFF, 0x48, 0x83, 0xC0, 0x01, 0xC3,
];

/// Exercises the driver-built trampoline end to end: install a detour that
/// calls the original prologue and adds one, call through it, disable, call
/// again, remove. Returns 1 when the detour observed `original + 1` and the
/// restored function the original value, 0 when executable pool was refused
/// (HVCI).
pub fn detour_self_test() -> Result<u64, NTSTATUS> {
    let target_fn: extern "system" fn() -> u64 = ks_hook_selftest_target;
    let target = ks_hook_selftest_target as *const () as u64;

    let info = match create_detour(0, target, &DETOUR_SELFTEST_CODE) {
        Ok(info) => info,
        Err(status) if status == STATUS_INVALID_PAGE_PROTECTION => return Ok(0),
        Err(status) => return Err(status),
    };

    let status = enable(0, info.id);
    if !nt_success(status) {
        let _ = remove(0, info.id);
        return if status == STATUS_INVALID_PAGE_PROTECTION {
            Ok(0)
        } else {
            Err(status)
        };
    }

    // SAFETY: the target is the driver's own function; the detour calls only
    // the trampoline and returns.
    let through_hook = (core::hint::black_box(target_fn))();
    let status = disable(0, info.id);
    if !nt_success(status) {
        return Err(status);
    }
    // SAFETY: the original prologue is restored at this point.
    let restored = (core::hint::black_box(target_fn))();
    let status = remove(0, info.id);
    if !nt_success(status) {
        return Err(status);
    }

    if through_hook == 0x1111_1112 && restored == 0x1111_1111 {
        Ok(1)
    } else {
        Err(STATUS_INVALID_PARAMETER)
    }
}

/// Builds the replacement jump. `E9 rel32` when the trampoline is within
/// ±2 GiB of the instruction following the jump, otherwise the 14-byte
/// absolute `FF 25` form.
fn build_patch(target: u64, trampoline: u64) -> ([u8; MAX_HOOK_STOLEN_LEN], usize) {
    let mut patch = [0u8; MAX_HOOK_STOLEN_LEN];
    let next = target.wrapping_add(RELATIVE_JUMP_LEN as u64);
    let delta = (trampoline as i64).wrapping_sub(next as i64);
    if let Ok(relative) = i32::try_from(delta) {
        patch[0] = 0xE9;
        patch[1..5].copy_from_slice(&relative.to_le_bytes());
        (patch, RELATIVE_JUMP_LEN)
    } else {
        patch[0] = 0xFF;
        patch[1] = 0x25;
        // RIP-relative pointer to the absolute address stored right after
        // the instruction.
        patch[2..6].copy_from_slice(&0u32.to_le_bytes());
        patch[6..14].copy_from_slice(&trampoline.to_le_bytes());
        (patch, ABSOLUTE_JUMP_LEN)
    }
}

/// Decodes whole instructions from the prologue until at least
/// `patch_len` bytes are covered, and returns that covered length.
fn decode_prologue(
    original: &[u8; MAX_HOOK_STOLEN_LEN],
    patch_len: usize,
) -> Result<usize, NTSTATUS> {
    let mut covered = 0usize;
    while covered < patch_len {
        if covered >= MAX_HOOK_STOLEN_LEN {
            return Err(STATUS_NOT_SUPPORTED);
        }
        let Some(length) = instruction_len(&original[covered..]) else {
            return Err(STATUS_NOT_SUPPORTED);
        };
        if length == 0 {
            return Err(STATUS_NOT_SUPPORTED);
        }
        covered += length;
    }
    if covered > MAX_HOOK_STOLEN_LEN {
        return Err(STATUS_NOT_SUPPORTED);
    }
    Ok(covered)
}

/// Reads `size` bytes from a kernel address after verifying every touched
/// page. Reads never need the write-protect dance.
fn read_kernel_bytes(address: u64, size: usize) -> Option<[u8; MAX_HOOK_STOLEN_LEN]> {
    let mut buffer = [0u8; MAX_HOOK_STOLEN_LEN];
    let mut offset = 0usize;
    while offset < size {
        let page = address.wrapping_add(offset as u64) & !0xFFFu64;
        if !page_readable(page) {
            return None;
        }
        let chunk = core::cmp::min(
            size - offset,
            0x1000 - ((address as usize + offset) & 0xFFF),
        );
        unsafe {
            ptr::copy_nonoverlapping(
                (address as usize + offset) as *const u8,
                buffer.as_mut_ptr().add(offset),
                chunk,
            );
        }
        offset += chunk;
    }
    Some(buffer)
}

fn page_readable(page: u64) -> bool {
    // SAFETY: `MmIsAddressValid` only inspects page tables.
    unsafe { MmIsAddressValid(page as *mut _) != 0 }
}

/// Writes `data` into kernel code. Locking the pages through an MDL gives a
/// writable system mapping without touching CR0.WP; HVCI-protected pages
/// refuse the write and the status is returned unchanged.
unsafe fn write_kernel_code(address: u64, data: &[u8]) -> NTSTATUS {
    // Bounded by the largest thing a hook ever writes into one place: the
    // shellcode buffer (trampoline writes are smaller, patches are smaller).
    if data.is_empty() || data.len() > MAX_HOOK_SHELLCODE {
        return STATUS_INVALID_PARAMETER;
    }
    let mut offset = 0usize;
    while offset < data.len() {
        let page = address.wrapping_add(offset as u64) & !0xFFFu64;
        if !page_readable(page) {
            return STATUS_INVALID_ADDRESS;
        }
        offset += 0x1000;
    }

    let mdl = IoAllocateMdl(
        address as *mut core::ffi::c_void,
        data.len() as u32,
        false,
        false,
        ptr::null_mut(),
    );
    if mdl.is_null() {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    // KernelMode probing asserts the pages must be valid; the page walk
    // above is what keeps a bad address from turning into a bugcheck.
    let status = ks_probe_and_lock_pages(mdl, KernelMode as i8, IoReadAccess);
    if !nt_success(status) {
        IoFreeMdl(mdl);
        return status;
    }
    let mapped = MmMapLockedPagesSpecifyCache(
        mdl,
        KernelMode as i8,
        MmCached,
        ptr::null(),
        0,
        NormalPagePriority as u32,
    );
    if mapped.is_null() {
        MmUnlockPages(mdl);
        IoFreeMdl(mdl);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    ptr::copy_nonoverlapping(data.as_ptr(), mapped as *mut u8, data.len());
    MmUnmapLockedPages(mapped, mdl);
    MmUnlockPages(mdl);
    IoFreeMdl(mdl);
    STATUS_SUCCESS
}
mod decoder;

use decoder::{decode, instruction_len};

#[cfg(test)]
mod tests {
    use super::*;

    fn len(bytes: &[u8]) -> usize {
        instruction_len(bytes).expect("decoder")
    }

    #[test]
    fn decodes_common_prologues() {
        // push rbp
        assert_eq!(len(&[0x55]), 1);
        // mov [rsp+8], rbx
        assert_eq!(len(&[0x48, 0x89, 0x5C, 0x24, 0x08]), 5);
        // mov rax, rsp
        assert_eq!(len(&[0x48, 0x8B, 0xC4]), 3);
        // sub rsp, 0x28
        assert_eq!(len(&[0x48, 0x83, 0xEC, 0x28]), 4);
        // mov rbx, [rip+disp32]
        assert_eq!(len(&[0x48, 0x8B, 0x1D, 0x00, 0x00, 0x00, 0x00]), 7);
        // lea rax, [rip+disp32]
        assert_eq!(len(&[0x48, 0x8D, 0x05, 0x00, 0x00, 0x00, 0x00]), 7);
        // mov rax, imm64
        assert_eq!(len(&[0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0]), 10);
        // endbr64
        assert_eq!(len(&[0xF3, 0x0F, 0x1E, 0xFA]), 4);
        // nop dword [rax+rax]
        assert_eq!(len(&[0x0F, 0x1F, 0x44, 0x00, 0x00]), 5);
        // jmp rel32 / jmp [rip] / jmp rax
        assert_eq!(len(&[0xE9, 0, 0, 0, 0]), 5);
        assert_eq!(len(&[0xFF, 0x25, 0, 0, 0, 0]), 6);
        assert_eq!(len(&[0x48, 0xFF, 0x25, 0, 0, 0, 0]), 7);
        assert_eq!(len(&[0xFF, 0xE0]), 2);
        // call rel32 / jcc rel8 / jcc rel32
        assert_eq!(len(&[0xE8, 0, 0, 0, 0]), 5);
        assert_eq!(len(&[0x74, 0x10]), 2);
        assert_eq!(len(&[0x0F, 0x84, 0, 0, 0, 0]), 6);
        // xor eax, eax / nop / int3
        assert_eq!(len(&[0x33, 0xC0]), 2);
        assert_eq!(len(&[0x90]), 1);
        assert_eq!(len(&[0xCC]), 1);
        // test rax, rax (Rex.W + F7 /0 with imm32)
        assert_eq!(len(&[0x48, 0xF7, 0xC0, 0, 0, 0, 0]), 7);
        // div ecx (F7 /6, no immediate)
        assert_eq!(len(&[0xF7, 0xF1]), 2);
    }

    #[test]
    fn rejects_unsupported_encodings() {
        // VEX / EVEX prefixes
        assert_eq!(instruction_len(&[0xC5, 0xF8, 0x77]), None);
        assert_eq!(instruction_len(&[0x62, 0xF1, 0x7C, 0x48]), None);
        // 3-byte opcode map
        assert_eq!(instruction_len(&[0x0F, 0x38, 0xF0, 0xC0]), None);
        // Truncated input
        assert_eq!(instruction_len(&[0x48]), None);
        assert_eq!(instruction_len(&[0xE9, 0x00]), None);
    }

    #[test]
    fn prologue_decoding_covers_the_jump() {
        let prologue = [
            0x48, 0x89, 0x5C, 0x24, 0x08, // mov [rsp+8], rbx (5)
            0x57, // push rdi
            0x48, 0x83, 0xEC, 0x20, // sub rsp, 0x20
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
        ];
        // A five-byte patch needs exactly five covered bytes.
        assert_eq!(decode_prologue(&prologue, 5), Ok(5));
        // The absolute form needs at least 14 covered bytes.
        assert_eq!(decode_prologue(&prologue, 14), Ok(14));
    }

    #[test]
    fn builds_relative_and_absolute_jumps() {
        let (patch, length) = build_patch(0xFFFF_F800_0000_1000, 0xFFFF_F800_0000_2000);
        assert_eq!(length, RELATIVE_JUMP_LEN);
        assert_eq!(patch[0], 0xE9);

        let (patch, length) = build_patch(0xFFFF_F800_0000_1000, 0xFFFF_F900_0000_2000);
        assert_eq!(length, ABSOLUTE_JUMP_LEN);
        assert_eq!(&patch[..2], &[0xFF, 0x25]);
        assert_eq!(
            u64::from_le_bytes(patch[6..14].try_into().unwrap()),
            0xFFFF_F900_0000_2000
        );
    }

    #[test]
    fn table_allocates_unique_nonzero_ids() {
        let mut table = HookTable::new();
        let first = table.allocate_id();
        let second = table.allocate_id();
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_ne!(first, second);
    }

    #[test]
    fn decoder_reports_relocation_metadata() {
        // mov rax, [rip+0x10]
        let decoded = decode(&[0x48, 0x8B, 0x05, 0x10, 0, 0, 0]).unwrap();
        assert_eq!(decoded.len, 7);
        assert_eq!(decoded.rip_disp, Some(3));
        assert!(!decoded.relative_branch);

        // Address-size prefix turns the same ModRM into absolute disp32.
        let decoded = decode(&[0x67, 0x8B, 0x05, 0x10, 0, 0, 0]).unwrap();
        assert_eq!(decoded.rip_disp, None);

        // Relative branches are flagged.
        assert!(decode(&[0xE9, 0, 0, 0, 0]).unwrap().relative_branch);
        assert!(decode(&[0xEB, 0x10]).unwrap().relative_branch);
        assert!(decode(&[0x0F, 0x84, 0, 0, 0, 0]).unwrap().relative_branch);

        // SIB with a base register is not RIP-relative; base-less SIB is.
        assert_eq!(decode(&[0x8B, 0x04, 0x24]).unwrap().rip_disp, None);
        assert_eq!(
            decode(&[0x8B, 0x04, 0x25, 0, 0, 0, 0]).unwrap().rip_disp,
            None
        );
    }

    #[test]
    fn trampoline_rebases_rip_relative_operands() {
        // mov rax, [rip+0x10]; ret
        let stolen = [0x48u8, 0x8B, 0x05, 0x10, 0, 0, 0, 0xC3];
        let target = 0xFFFF_F800_0000_1000u64;
        // Within ±2 GiB: RIP-relative operands are only rebasable that far.
        let trampoline = 0xFFFF_F800_1000_0000u64;
        let mut out = [0u8; MAX_TRAMPOLINE];
        let len = build_trampoline(target, &stolen, trampoline, &mut out).unwrap();

        assert_eq!(&out[..3], &[0x48, 0x8B, 0x05]);
        let expected = (0x10i64 + (target as i64 - trampoline as i64)) as i32;
        assert_eq!(i32::from_le_bytes(out[3..7].try_into().unwrap()), expected);
        // Jump back to target + stolen_len.
        assert_eq!(out[8], 0xFF);
        assert_eq!(out[9], 0x25);
        assert_eq!(
            u64::from_le_bytes(out[14..22].try_into().unwrap()),
            target + stolen.len() as u64
        );
        assert_eq!(len, stolen.len() + ABSOLUTE_JUMP_LEN);
    }

    #[test]
    fn trampoline_rejects_relative_branches() {
        let mut out = [0u8; MAX_TRAMPOLINE];
        assert_eq!(
            build_trampoline(
                0xFFFF_F800_0000_1000,
                &[0xE9, 0, 0, 0, 0],
                0xFFFF_F900_0000_0000,
                &mut out
            ),
            Err(STATUS_NOT_SUPPORTED)
        );
    }
}
