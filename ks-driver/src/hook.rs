//! MinHook-style kernel inline hooks, applied through MDL mappings.
//!
//! The driver owns the whole lifecycle a user-mode hook library would own —
//! entry-prologue instruction decoding, jump construction, an
//! enabled/disabled state machine, conflict detection and LIFO teardown —
//! except trampoline generation: the caller supplies an already-executable
//! detour address (its own trampoline: relocated stolen instructions plus a
//! jump back to `target + replaced_len`).
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

use core::sync::atomic::{AtomicBool, Ordering};
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
    target: u64,
    /// Bytes replaced at `target` (an instruction boundary, >= patch_len).
    stolen_len: usize,
    /// Length of the jump actually written.
    patch_len: usize,
    patch: [u8; MAX_HOOK_STOLEN_LEN],
    original: [u8; MAX_HOOK_STOLEN_LEN],
    /// Driver-owned executable buffer holding the detour bytes, if this hook
    /// was created from shellcode. Freed by `remove`/`shutdown`.
    owned_code: u64,
    enabled: bool,
}

/// Pool tag for shellcode buffers (`kShc` in little-endian).
const SHELLCODE_TAG: u32 = 0x6368_536B;
/// Executable non-paged pool flag for `ExAllocatePool2`
/// (`POOL_FLAG_NON_PAGED_EXECUTE`).
const POOL_FLAG_NON_PAGED_EXECUTE: u64 = 0x0000_0000_0000_0080;

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

    fn find_target(&self, target: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Some(entry) if entry.target == target))
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

/// Records a hook (`disabled`). Returns `(id, replaced_len, original_bytes)`.
pub fn create(
    target: u64,
    trampoline: u64,
) -> Result<(u64, usize, [u8; MAX_HOOK_STOLEN_LEN]), NTSTATUS> {
    insert(target, trampoline, 0)
}

/// Allocates executable kernel memory, copies `code` there and records a
/// hook whose detour is that buffer. Returns
/// `(id, replaced_len, original_bytes, code_address)`. The buffer belongs to
/// the hook entry from here on: `remove`, `remove_all` and shutdown free it.
pub fn create_shellcode(
    target: u64,
    code: &[u8],
) -> Result<(u64, usize, [u8; MAX_HOOK_STOLEN_LEN], u64), NTSTATUS> {
    if !initialized() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if code.is_empty() || code.len() > MAX_HOOK_SHELLCODE {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let buffer = allocate_executable(code.len())?;
    // SAFETY: `buffer` is a fresh non-paged allocation of `code.len()` bytes
    // that has just been made executable.
    unsafe {
        ptr::copy_nonoverlapping(code.as_ptr(), buffer as *mut u8, code.len());
    }
    match insert(target, buffer, buffer) {
        Ok((id, stolen_len, original)) => Ok((id, stolen_len, original, buffer)),
        Err(status) => {
            // SAFETY: allocation created above and not stored in the table.
            unsafe { ExFreePoolWithTag(buffer as *mut _, SHELLCODE_TAG) };
            Err(status)
        }
    }
}

/// Shared create path: `owned_code` is 0 for caller-provided trampolines.
fn insert(
    target: u64,
    trampoline: u64,
    owned_code: u64,
) -> Result<(u64, usize, [u8; MAX_HOOK_STOLEN_LEN]), NTSTATUS> {
    if !initialized() {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if target == 0 || trampoline == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    // The caller-supplied trampoline is only ever jumped to, so an
    // unreachable far jump costs bytes but is not an error.
    let Some(original) = read_kernel_bytes(target, MAX_HOOK_STOLEN_LEN) else {
        return Err(STATUS_INVALID_ADDRESS);
    };

    let (patch, patch_len) = build_patch(target, trampoline);
    let stolen_len = decode_prologue(&original, patch_len)?;
    let mut stored = [0u8; MAX_HOOK_STOLEN_LEN];
    stored[..stolen_len].copy_from_slice(&original[..stolen_len]);

    let mut patch_bytes = [0u8; MAX_HOOK_STOLEN_LEN];
    patch_bytes[..patch_len].copy_from_slice(&patch[..patch_len]);

    with_table(|table| {
        if table.find_target(target).is_some() {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        let Some(slot) = table.free_slot() else {
            return Err(STATUS_QUOTA_EXCEEDED);
        };
        let id = table.allocate_id();
        table.entries[slot] = Some(HookEntry {
            id,
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
pub fn enable(id: u64) -> NTSTATUS {
    if !initialized() {
        return STATUS_NOT_SUPPORTED;
    }
    let entry = with_table(|table| table.find(id).and_then(|slot| table.entries[slot]));
    let Some(entry) = entry else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    if entry.enabled {
        return STATUS_SUCCESS;
    }
    // SAFETY: patch bytes were built by `create` from the same entry.
    let status = unsafe { write_kernel_code(entry.target, &entry.patch[..entry.patch_len]) };
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
pub fn disable(id: u64) -> NTSTATUS {
    if !initialized() {
        return STATUS_NOT_SUPPORTED;
    }
    let entry = with_table(|table| table.find(id).and_then(|slot| table.entries[slot]));
    let Some(entry) = entry else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    if !entry.enabled {
        return STATUS_SUCCESS;
    }
    // SAFETY: `original` holds the bytes that were at `target`.
    let status = unsafe { write_kernel_code(entry.target, &entry.original[..entry.stolen_len]) };
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
pub fn remove(id: u64) -> NTSTATUS {
    let status = disable(id);
    if !nt_success(status) {
        return status;
    }
    let owned = with_table(|table| match table.find(id) {
        Some(slot) => {
            let owned = table.entries[slot].map(|entry| entry.owned_code);
            table.entries[slot] = None;
            owned
        }
        None => None,
    });
    let Some(owned) = owned else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    // The detour buffer is released only after the jump is gone, so no core
    // can fetch instructions from a freed page.
    if owned != 0 {
        // SAFETY: the buffer was allocated by `allocate_executable` and is no
        // longer referenced by any table entry.
        unsafe { ExFreePoolWithTag(owned as *mut _, SHELLCODE_TAG) };
    }
    STATUS_SUCCESS
}

/// Enables every recorded hook; stops at the first patch failure.
pub fn enable_all() -> Result<u64, NTSTATUS> {
    let (ids, count) = collect_ids();
    let mut touched = 0u64;
    for id in &ids[..count] {
        let status = enable(*id);
        if !nt_success(status) {
            return Err(status);
        }
        touched += 1;
    }
    Ok(touched)
}

/// Restores every patched hook.
pub fn disable_all() -> Result<u64, NTSTATUS> {
    let (ids, count) = collect_ids();
    let mut touched = 0u64;
    for id in &ids[..count] {
        let status = disable(*id);
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
    let (mut ids, count) = collect_ids();
    ids[..count].reverse();
    let mut touched = 0u64;
    for id in &ids[..count] {
        let status = remove(*id);
        if !nt_success(status) {
            return Err(status);
        }
        touched += 1;
    }
    Ok(touched)
}

/// Snapshot of the live ids plus how many slots are valid. Ids are never 0,
/// so the unused tail of the array stays clearly distinguishable.
fn collect_ids() -> ([u64; MAX_KERNEL_HOOKS], usize) {
    let mut ids = [0u64; MAX_KERNEL_HOOKS];
    let mut count = 0usize;
    with_table(|table| {
        for entry in table.entries.iter().flatten() {
            if count < ids.len() {
                ids[count] = entry.id;
                count += 1;
            }
        }
    });
    (ids, count)
}

/// Restores every patch and drops the table. Called from the `Shutdown`
/// request and from the unload path: kernel code must never keep jumping at
/// memory that is about to disappear.
pub fn shutdown() {
    if !initialized() {
        return;
    }
    let (ids, count) = collect_ids();
    for id in &ids[..count] {
        let _ = disable(*id);
    }
    let mut owned = [0u64; MAX_KERNEL_HOOKS];
    with_table(|table| {
        for (index, entry) in table.entries.iter().enumerate() {
            if let Some(entry) = entry {
                owned[index] = entry.owned_code;
            }
        }
        table.entries = [None; MAX_KERNEL_HOOKS];
    });
    for buffer in owned {
        if buffer != 0 {
            // SAFETY: allocation owned by the entry that was just dropped;
            // its jump is already restored.
            unsafe { ExFreePoolWithTag(buffer as *mut _, SHELLCODE_TAG) };
        }
    }
}

/// Target of the hook self test: a leaf function in the driver's own `.text`
/// that nothing calls, so patching it cannot disturb the running driver.
#[no_mangle]
#[inline(never)]
pub extern "system" fn ks_hook_selftest_target() -> u64 {
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
    let (id, replaced_len, original) = create(target, detour)?;

    let status = enable(id);
    if !nt_success(status) {
        let _ = remove(id);
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

    let status = disable(id);
    if !nt_success(status) {
        return Err(status);
    }

    let mut restored = [0u8; MAX_HOOK_STOLEN_LEN];
    // SAFETY: same function text; `replaced_len` came from our own decode.
    unsafe {
        ptr::copy_nonoverlapping(target as *const u8, restored.as_mut_ptr(), replaced_len);
    }
    let restored_ok = restored[..replaced_len] == original[..replaced_len];

    let status = remove(id);
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

    let (id, _replaced_len, _original, _code) = match create_shellcode(target, &SELFTEST_SHELLCODE)
    {
        Ok(created) => created,
        Err(status) if status == STATUS_INVALID_PAGE_PROTECTION => return Ok(0),
        Err(status) => return Err(status),
    };

    let status = enable(id);
    if !nt_success(status) {
        let _ = remove(id);
        return if status == STATUS_INVALID_PAGE_PROTECTION {
            Ok(0)
        } else {
            Err(status)
        };
    }

    // SAFETY: the target is this driver's own leaf function; while patched it
    // jumps to the stub above, which returns without touching any state.
    let through_hook = (core::hint::black_box(target_fn))();
    let status = disable(id);
    if !nt_success(status) {
        return Err(status);
    }
    // SAFETY: after `disable` the prologue is restored.
    let restored = (core::hint::black_box(target_fn))();
    let status = remove(id);
    if !nt_success(status) {
        return Err(status);
    }

    if through_hook == 0x2222_2222 && restored == 0x1111_1111 {
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
    if data.is_empty() || data.len() > MAX_HOOK_STOLEN_LEN {
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

// ---------------------------------------------------------------------------
// x64 instruction length decoder
//
// A deliberately conservative subset of HDE64: it returns the instruction
// length for the opcode families that appear in function prologues/epilogs,
// and `None` for anything it does not understand (VEX/EVEX, 3-byte maps,
// LOOP/JCXZ edge cases). A wrong length would split an instruction, so
// refusing is always better than guessing.
// ---------------------------------------------------------------------------

/// Returns the length of the instruction at the start of `code`.
pub(crate) fn instruction_len(code: &[u8]) -> Option<usize> {
    let mut index = 0usize;
    let mut rex_w = false;
    let mut opsize = false;
    let mut addrsize = false;

    // Legacy prefixes, then at most one REX.
    loop {
        let byte = *code.get(index)?;
        match byte {
            0x66 => opsize = true,
            0x67 => addrsize = true,
            0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {}
            _ => break,
        }
        index += 1;
        if index > 14 {
            return None;
        }
    }
    if let Some(&byte) = code.get(index) {
        if (0x40..=0x4F).contains(&byte) {
            rex_w = byte & 0x08 != 0;
            index += 1;
        }
    }

    let opcode = *code.get(index)?;
    index += 1;

    let length = match opcode {
        0x0F => two_byte(code, &mut index, rex_w, opsize),
        0xC4 | 0xC5 | 0x62 => None, // VEX / EVEX: unsupported on purpose
        0x9A | 0xEA => None,        // far call/jmp, invalid in long mode
        0xE8 | 0xE9 => Some(index + 4),
        0xEB | 0x70..=0x7F | 0xE0..=0xE3 => Some(index + 1),
        0xA0..=0xA3 => Some(index + if addrsize { 4 } else { 8 }),
        0xA8 | 0xCD | 0xD4 | 0xD5 | 0xE4..=0xE7 => Some(index + 1),
        0xA9 => Some(index + imm_32(opsize)),
        0xB0..=0xB7 => Some(index + 1),
        0xB8..=0xBF => Some(index + if rex_w { 8 } else { imm_16_32(opsize) }),
        0xC2 | 0xCA => Some(index + 2),
        0xC8 => Some(index + 3),
        0x68 => Some(index + imm_16_32(opsize)),
        0x6A => Some(index + 1),
        0x69 | 0x6B => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            let immediate = if opcode == 0x6B { 1 } else { imm_16_32(opsize) };
            Some(index + modrm_len + immediate)
        }
        0x80 | 0x82 | 0x83 => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            Some(index + modrm_len + 1)
        }
        0x81 => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            Some(index + modrm_len + imm_16_32(opsize))
        }
        0xC0 | 0xC1 => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            Some(index + modrm_len + 1)
        }
        0xC6 => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            Some(index + modrm_len + 1)
        }
        0xC7 => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            Some(index + modrm_len + imm_16_32(opsize))
        }
        0xF6 | 0xF7 => {
            let (modrm_len, register) = modrm_len(&code[index..], addrsize)?;
            let immediate = if register <= 1 {
                if opcode == 0xF6 {
                    1
                } else {
                    imm_16_32(opsize)
                }
            } else {
                0
            };
            Some(index + modrm_len + immediate)
        }
        0x00..=0x3F => {
            if (opcode & 0x07) > 3 {
                // Segment push/pop, daa/das, aaa/aas, and the segment
                // prefix slots when they appear as an opcode: no operands.
                Some(index)
            } else {
                let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
                Some(index + modrm_len)
            }
        }
        0x50..=0x5F => Some(index), // push/pop r64
        0x63 | 0x84..=0x8F | 0xD0..=0xD3 | 0xD8..=0xDF | 0xFE | 0xFF => {
            let (modrm_len, _) = modrm_len(&code[index..], addrsize)?;
            Some(index + modrm_len)
        }
        // Remaining single-byte forms take no ModRM and no immediate.
        0x60 | 0x61 => None,
        0x90..=0x99
        | 0x9B..=0x9F
        | 0xC3
        | 0xC9
        | 0xCB
        | 0xCC
        | 0xCE
        | 0xCF
        | 0xD7
        | 0xEC..=0xEF
        | 0xF1
        | 0xF4
        | 0xF5
        | 0xF8..=0xFD => Some(index),
        _ => None,
    }?;

    // A length is only usable when the whole instruction is present in the
    // buffer; otherwise the caller could believe in bytes it never read.
    (code.len() >= length && length <= 15).then_some(length)
}

fn imm_16_32(opsize: bool) -> usize {
    if opsize {
        2
    } else {
        4
    }
}

fn imm_32(_opsize: bool) -> usize {
    4
}

fn two_byte(code: &[u8], index: &mut usize, rex_w: bool, opsize: bool) -> Option<usize> {
    let opcode = *code.get(*index)?;
    *index += 1;
    match opcode {
        0x05
        | 0x06
        | 0x07
        | 0x08
        | 0x09
        | 0x0B
        | 0x0E
        | 0x30..=0x37
        | 0x77
        | 0xA0..=0xA2
        | 0xA8
        | 0xA9
        | 0xAA
        | 0xC8..=0xCF => Some(*index),
        0x80..=0x8F => Some(*index + 4),
        0x38 => None, // 3-byte map: immediate sizes vary
        0x3A => {
            let (modrm_len, _) = modrm_len(&code[*index..], false)?;
            Some(*index + modrm_len + 1)
        }
        0x0F => {
            let (modrm_len, _) = modrm_len(&code[*index..], false)?;
            Some(*index + modrm_len + 1)
        }
        0x70 | 0x71 | 0x72 | 0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4 | 0xC5 | 0xC6 => {
            let (modrm_len, _) = modrm_len(&code[*index..], false)?;
            Some(*index + modrm_len + 1)
        }
        0x0D
        | 0x10..=0x17
        | 0x18..=0x1F
        | 0x28..=0x2F
        | 0x40..=0x4F
        | 0x50..=0x6F
        | 0x74..=0x76
        | 0x78
        | 0x79
        | 0x7C..=0x7F
        | 0x90..=0x9F
        | 0xA3
        | 0xA5
        | 0xAB
        | 0xAD
        | 0xAE
        | 0xAF
        | 0xB0..=0xB9
        | 0xBB..=0xC1
        | 0xC3
        | 0xC7
        | 0xD0..=0xFF => {
            let (modrm_len, _) = modrm_len(&code[*index..], false)?;
            Some(*index + modrm_len)
        }
        _ => {
            let _ = (rex_w, opsize);
            None
        }
    }
}

/// Decodes ModRM/SIB/displacement bytes and returns
/// `(total_len, reg_field)`.
fn modrm_len(code: &[u8], addrsize: bool) -> Option<(usize, u8)> {
    let modrm = *code.first()?;
    let mode = modrm >> 6;
    let register = (modrm >> 3) & 0x07;
    let rm = modrm & 0x07;
    let mut length = 1usize;

    if mode != 0b11 {
        let needs_sib = rm == 0b100;
        if needs_sib {
            let sib = *code.get(length)?;
            length += 1;
            if mode == 0 && (sib & 0x07) == 0b101 {
                length += 4;
            }
        } else if mode == 0 && rm == 0b101 {
            // RIP-relative (or absolute with 0x67): always disp32.
            length += 4;
        }
        match mode {
            0b01 => length += 1,
            0b10 => length += 4,
            _ => {}
        }
    }
    let _ = addrsize;
    Some((length, register))
}

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
}
