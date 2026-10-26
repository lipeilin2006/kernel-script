//! Suspend and continue a target process (kernel `PsSuspendProcess` /
//! `PsResumeProcess`).
//!
//! These two ntoskrnl exports freeze/thaw every thread of a process in one
//! step, which is exactly the "pause the game while I think" primitive a
//! memory tool needs. The driver adds the bookkeeping user mode cannot do
//! cheaply:
//!
//! - **Idempotence.** A process this driver already suspended is not
//!   suspended a second time (a double suspend would need two resumes), so
//!   `Suspend` reports `value = 1` when it acted and `0` when it was a
//!   no-op.
//! - **Ownership.** `Resume` only thaws processes this driver froze; it
//!   never decrements a suspend count it did not increment.
//! - **Guard rails.** `pid = 0` (Idle) and `pid = 4` (System) are rejected
//!   by ks-core — freezing the System process hangs the machine. The
//!   *client's own pid* is rejected here: freezing the process that is
//!   waiting for this round trip would deadlock it.
//!
//! Contract and caveats (mirrored in AGENTS.md):
//! - suspends are **not** undone by `Shutdown`/`stop`/unload: a frozen
//!   process stays frozen until some client resumes it;
//! - the two primitives are treated as infallible for a valid `EPROCESS`
//!   (argument validation happens in `PsLookupProcessByProcessId`), so a
//!   garbage return value from the undocumented exports cannot roll the
//!   table back into an inconsistent state; `ks-test` proves the effect
//!   behaviourally with a heartbeat child process;
//! - everything runs on the ring worker at `PASSIVE_LEVEL`; the table mutex
//!   critical sections only touch the pid array, never the `Ps*` calls.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use ks_core::protocol::MAX_PROCESS_SUSPENDS;

use crate::wdm::*;

struct SuspendData {
    pids: [u64; MAX_PROCESS_SUSPENDS],
    count: usize,
}

impl SuspendData {
    const fn new() -> Self {
        Self {
            pids: [0; MAX_PROCESS_SUSPENDS],
            count: 0,
        }
    }
}

struct KernelFastMutex(UnsafeCell<FAST_MUTEX>);
// SAFETY: every access goes through ExAcquireFastMutex/ExReleaseFastMutex.
unsafe impl Sync for KernelFastMutex {}

struct KernelTable(UnsafeCell<SuspendData>);
// SAFETY: every access goes through the table mutex.
unsafe impl Sync for KernelTable {}

static TABLE_MUTEX: KernelFastMutex =
    KernelFastMutex(UnsafeCell::new(unsafe { core::mem::zeroed() }));
static TABLE: KernelTable = KernelTable(UnsafeCell::new(SuspendData::new()));
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialises the table. Called from `comm::start` before the worker can
/// receive a request.
pub fn init() {
    // SAFETY: first statement of the first caller; no thread exists yet.
    unsafe { init_fast_mutex(TABLE_MUTEX.0.get()) };
    INITIALIZED.store(true, Ordering::SeqCst);
}

/// Runs `operation` with the table mutex held. The critical section only
/// touches the pid array: the `Ps*` calls always run outside it.
fn with_table<T>(operation: impl FnOnce(&mut SuspendData) -> T) -> T {
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

/// The caller's own pid must never be frozen: it is blocked in this very
/// round trip. `0`/`4` are refused by ks-core; re-checked here as defence
/// in depth.
fn guard(pid: u64, client_pid: u64) -> Result<(), NTSTATUS> {
    if pid == 0 || pid == 4 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    if pid == client_pid {
        return Err(STATUS_ACCESS_DENIED);
    }
    Ok(())
}

/// Reserves `pid` in the table without suspending anything yet, so a full
/// table fails *before* the process is frozen. Returns `Err(STATUS_SUCCESS)`
/// when the pid is already suspended by us (callers treat that as a no-op).
fn reserve(pid: u64) -> Result<(), NTSTATUS> {
    with_table(|table| {
        if table.pids[..table.count].contains(&pid) {
            return Err(STATUS_SUCCESS);
        }
        if table.count >= table.pids.len() {
            return Err(STATUS_QUOTA_EXCEEDED);
        }
        table.pids[table.count] = pid;
        table.count += 1;
        Ok(())
    })
}

/// Drops `pid` from the table. Returns whether it was present.
fn release(pid: u64) -> bool {
    with_table(|table| {
        let Some(index) = table.pids[..table.count]
            .iter()
            .position(|&entry| entry == pid)
        else {
            return false;
        };
        table.pids[index] = table.pids[table.count - 1];
        table.count -= 1;
        true
    })
}

fn contains(pid: u64) -> bool {
    with_table(|table| table.pids[..table.count].contains(&pid))
}

/// Looks up `pid` and runs `operation` with the `PEPROCESS`, dropping the
/// reference afterwards.
fn with_process<T>(pid: u64, operation: impl FnOnce(Pvoid) -> T) -> Result<T, NTSTATUS> {
    let mut process = 0isize;
    // SAFETY: out parameter is a valid local; a failed lookup leaves it 0.
    let status = unsafe { PsLookupProcessByProcessId(pid_handle(pid), &mut process) };
    if !nt_success(status) || process == 0 {
        return Err(status);
    }
    let result = operation(process as Pvoid);
    // SAFETY: the reference was taken by the lookup above.
    unsafe { ObDereferenceObject(process as Pvoid) };
    Ok(result)
}

/// Freezes every thread of `pid`. Answers `1` when applied, `0` when the
/// process is already suspended by this driver.
pub fn suspend(pid: u64, client_pid: u64) -> Result<u64, NTSTATUS> {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return Err(STATUS_NOT_SUPPORTED);
    }
    guard(pid, client_pid)?;
    match reserve(pid) {
        Ok(()) => {}
        // Already ours: idempotent success.
        Err(status) if status == STATUS_SUCCESS => return Ok(0),
        Err(status) => return Err(status),
    }
    let result = with_process(pid, |process| {
        // `PsSuspendProcess` walks the process's threads internally; the
        // return value is ignored on purpose (see the module contract).
        // SAFETY: `process` is a referenced `PEPROCESS` from the lookup.
        let _ = unsafe { PsSuspendProcess(process) };
    });
    if let Err(status) = result {
        // The lookup failed, so nothing was frozen: undo the reservation.
        let _ = release(pid);
        return Err(status);
    }
    Ok(1)
}

/// Continues a process this driver previously suspended. Answers `1` when
/// applied, `0` when this driver never suspended it (no-op).
pub fn resume(pid: u64, client_pid: u64) -> Result<u64, NTSTATUS> {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return Err(STATUS_NOT_SUPPORTED);
    }
    guard(pid, client_pid)?;
    if !contains(pid) {
        return Ok(0);
    }
    let result = with_process(pid, |process| {
        // Return value ignored on purpose; see the module contract.
        // SAFETY: `process` is a referenced `PEPROCESS` from the lookup.
        let _ = unsafe { PsResumeProcess(process) };
    });
    result?;
    // Only forget the pid once the resume actually ran; a failed lookup
    // keeps the entry so a later retry is still possible.
    let _ = release(pid);
    Ok(1)
}

/// Forgets every recorded suspend. Deliberately does **not** resume: a
/// driver shutdown must not make a frozen process runnable behind the
/// client's back. Called from the `Shutdown` request and the unload path.
pub fn forget_all() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    with_table(|table| {
        table.count = 0;
    });
}
