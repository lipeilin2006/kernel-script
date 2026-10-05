//! Driver-side memory lock table, replayed by the ring worker's own loop.
//!
//! There is no dedicated rewrite thread: [`crate::comm::worker`] services
//! requests and replays locks in one merged loop. While the table is
//! non-empty the loop only *polls* the request event (`timeout = 0`), so a
//! pending request always wins over the next rewrite entry, and each pass
//! rewrites exactly one entry ([`sweep_step`]); while the table is empty
//! the loop blocks on the request event and burns no CPU — a `Lock`
//! request is itself what wakes it, so no private wake event exists.
//! Only the worker ever mutates or replays the table, so the sweep cursor
//! and scratch buffer need no synchronisation of their own.
//!
//! An earlier kernel lock worker bugchecked repeatedly in the field and was
//! removed. The invariants that keep this one from repeating that history:
//!
//! - `comm::stop` flags the worker, signals the request event and waits on
//!   the thread object, then drops the table: the worker is joined before
//!   the ring objects or the table go away. `Request::Shutdown` clears the
//!   table before the worker exits, so a shut-down driver stops writing
//!   immediately.
//! - Everything runs at `PASSIVE_LEVEL` and nothing can panic: the table
//!   is only mutated through bounds-checked indexing and validated lengths,
//!   because the workspace builds with `panic = "abort"` and any panic is
//!   a bugcheck.
//! - The fast mutex is only ever held across plain byte copies — no waits
//!   and no IRQL-sensitive calls inside a critical section. Target memory
//!   is written after the mutex is released.
//! - The loop checks the stop flag once per entry, so unload can never
//!   race a write into a process that is going away.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ks_core::protocol::{MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE};

use crate::wdm::*;

/// One table entry. `id == 0` is invalid by protocol, so an empty slot is
/// simply `None`.
struct LockEntry {
    id: u64,
    pid: u64,
    address: u64,
    len: usize,
    data: [u8; MAX_MEMORY_LOCK_SIZE],
}

/// The whole lock table: at most [`MAX_MEMORY_LOCKS`] entries, each holding
/// at most [`MAX_MEMORY_LOCK_SIZE`] bytes. Static storage, never paged, so
/// the sweep never waits on the pool.
struct LockTable {
    entries: [Option<LockEntry>; MAX_MEMORY_LOCKS],
}

impl LockTable {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_MEMORY_LOCKS],
        }
    }

    /// Inserts a new id or replaces the payload of an existing one.
    /// Re-validates everything the protocol already checked: this is the
    /// last trust boundary before the bytes land in the table.
    fn insert(&mut self, id: u64, pid: u64, address: u64, data: &[u8]) -> Result<(), NTSTATUS> {
        if id == 0
            || pid == 0
            || address == 0
            || data.is_empty()
            || data.len() > MAX_MEMORY_LOCK_SIZE
        {
            return Err(STATUS_INVALID_PARAMETER);
        }
        for entry in self.entries.iter_mut().flatten() {
            if entry.id == id {
                entry.pid = pid;
                entry.address = address;
                entry.len = data.len();
                entry.data[..data.len()].copy_from_slice(data);
                return Ok(());
            }
        }
        for slot in self.entries.iter_mut() {
            if slot.is_none() {
                let mut entry = LockEntry {
                    id,
                    pid,
                    address,
                    len: data.len(),
                    data: [0u8; MAX_MEMORY_LOCK_SIZE],
                };
                entry.data[..data.len()].copy_from_slice(data);
                *slot = Some(entry);
                return Ok(());
            }
        }
        Err(STATUS_QUOTA_EXCEEDED)
    }

    /// Removes one id; an unknown id is a successful no-op so unlock stays
    /// idempotent. Id 0 is rejected because the protocol rejects it.
    fn remove(&mut self, id: u64) -> Result<(), NTSTATUS> {
        if id == 0 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        for slot in self.entries.iter_mut() {
            let hit = matches!(slot, Some(entry) if entry.id == id);
            if hit {
                *slot = None;
                break;
            }
        }
        Ok(())
    }

    /// Removes every entry owned by `pid`.
    fn clear(&mut self, pid: u64) -> Result<(), NTSTATUS> {
        if pid == 0 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        for slot in self.entries.iter_mut() {
            let hit = matches!(slot, Some(entry) if entry.pid == pid);
            if hit {
                *slot = None;
            }
        }
        Ok(())
    }

    /// Drops every entry (driver shutdown and unload).
    fn clear_all(&mut self) {
        for slot in self.entries.iter_mut() {
            *slot = None;
        }
    }

    /// Copies one entry into the caller's scratch buffer and reports its
    /// target. Runs entirely under the table mutex so the worker never
    /// holds a reference into the table.
    fn snapshot(
        &self,
        index: usize,
        scratch: &mut [u8; MAX_MEMORY_LOCK_SIZE],
    ) -> Option<(u64, u64, usize)> {
        let entry = self.entries.get(index)?.as_ref()?;
        let len = core::cmp::min(entry.len, MAX_MEMORY_LOCK_SIZE);
        scratch[..len].copy_from_slice(&entry.data[..len]);
        Some((entry.pid, entry.address, len))
    }
}

/// Mutex cell for the table: plain `FAST_MUTEX` storage with exclusive
/// access serialised by [`with_table`].
struct KernelFastMutex(UnsafeCell<FAST_MUTEX>);
// SAFETY: every access goes through ExAcquireFastMutex/ExReleaseFastMutex.
unsafe impl Sync for KernelFastMutex {}

/// Static table instance; see [`KernelFastMutex`] for the access rule.
struct KernelTable(UnsafeCell<LockTable>);
// SAFETY: every access goes through [`with_table`].
unsafe impl Sync for KernelTable {}

/// Copy target for [`sweep_step`]: one entry is snapshotted under the
/// table mutex and written after the mutex is released, so the worker
/// never points into the table during target I/O. Stack storage would eat
/// too much of the ~16 KiB kernel stack, so it lives here instead.
struct KernelScratch(UnsafeCell<[u8; MAX_MEMORY_LOCK_SIZE]>);
// SAFETY: only the ring worker touches it — [`sweep_step`] has a single
// caller and that caller cannot re-enter itself.
unsafe impl Sync for KernelScratch {}

/// Zero-initialised on purpose: [`init`] runs [`init_fast_mutex`] before
/// the worker exists, and a zeroed `Count` would otherwise deadlock.
static TABLE_MUTEX: KernelFastMutex =
    KernelFastMutex(UnsafeCell::new(unsafe { core::mem::zeroed() }));
static TABLE: KernelTable = KernelTable(UnsafeCell::new(LockTable::new()));
static SWEEP_SCRATCH: KernelScratch = KernelScratch(UnsafeCell::new([0; MAX_MEMORY_LOCK_SIZE]));
/// Rotating start slot so [`sweep_step`] visits entries in round-robin
/// order instead of hammering slot 0. Only the worker moves it (always
/// under the table mutex).
static SWEEP_CURSOR: AtomicUsize = AtomicUsize::new(0);
/// Set once [`init`] has run, so [`clear_table`] before a failed start is
/// a no-op instead of an acquire on a zeroed mutex.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Runs `f` with exclusive access to the table. Callers must hold nothing
/// else: the fast mutex blocks (at `PASSIVE_LEVEL`) on contention, and the
/// closure must not wait itself — only copies and bookkeeping.
fn with_table<R>(f: impl FnOnce(&mut LockTable) -> R) -> R {
    debug_assert!(
        INITIALIZED.load(Ordering::SeqCst),
        "lock table used before lock::init"
    );
    // SAFETY: initialisation happens in `init` before the worker exists;
    // ExReleaseFastMutex restores the IRQL ExAcquireFastMutex raised.
    unsafe { ExAcquireFastMutex(TABLE_MUTEX.0.get()) };
    // SAFETY: exclusive access is held, so the single mutable borrow is unique.
    let result = f(unsafe { &mut *TABLE.0.get() });
    // SAFETY: paired with the acquire above; nothing in `f` can unwind
    // (the workspace builds with panic = "abort", and f is panic-free).
    unsafe { ExReleaseFastMutex(TABLE_MUTEX.0.get()) };
    result
}

/// Installs or replaces one lock entry. Returns `STATUS_QUOTA_EXCEEDED`
/// when all [`MAX_MEMORY_LOCKS`] slots are taken by other ids (ks-link
/// maps it to `TooManyEntries`). Wakes nothing: the `Lock` request itself
/// is running on the worker, whose next loop pass sees the new entry.
pub fn insert(id: u64, pid: u64, address: u64, data: &[u8]) -> Result<(), NTSTATUS> {
    with_table(|table| table.insert(id, pid, address, data))
}

/// Removes one entry. Unknown ids succeed; id 0 is invalid.
pub fn remove(id: u64) -> Result<(), NTSTATUS> {
    with_table(|table| table.remove(id))
}

/// Removes every entry held against `pid`.
pub fn clear(pid: u64) -> Result<(), NTSTATUS> {
    with_table(|table| table.clear(pid))
}

/// Drops the whole table (shutdown, unload, start-failure cleanup). A
/// no-op before [`init`], when no mutex exists yet.
pub fn clear_table() {
    if INITIALIZED.load(Ordering::SeqCst) {
        with_table(|table| table.clear_all());
    }
}

/// True when at least one entry is held. The worker uses it to choose
/// between polling the request event (non-empty: rewrite must yield to
/// requests) and blocking on it (empty: sleep). Table state can only
/// change on this same thread, so the answer cannot go stale between the
/// check and the wait.
pub fn has_entries() -> bool {
    with_table(|table| table.entries.iter().any(Option::is_some))
}

/// Replays exactly one table entry: snapshot under the mutex into
/// [`SWEEP_SCRATCH`], then write with the mutex released. Returns `false`
/// when the table is empty, which is the worker's cue to block on the
/// request event instead of polling it.
pub fn sweep_step() -> bool {
    let target = with_table(|table| {
        let start = SWEEP_CURSOR.load(Ordering::Relaxed);
        for offset in 0..MAX_MEMORY_LOCKS {
            let index = (start + offset) % MAX_MEMORY_LOCKS;
            // SAFETY: single caller (the ring worker), which cannot
            // re-enter while the mutex is held.
            let scratch = unsafe { &mut *SWEEP_SCRATCH.0.get() };
            let Some(hit) = table.snapshot(index, scratch) else {
                continue;
            };
            SWEEP_CURSOR.store((index + 1) % MAX_MEMORY_LOCKS, Ordering::Relaxed);
            return Some(hit);
        }
        None
    });
    let Some((pid, address, len)) = target else {
        return false;
    };
    // SAFETY: single consumer; the buffer was filled above and nothing
    // else can touch it before this write completes.
    let scratch = unsafe { &*SWEEP_SCRATCH.0.get() };
    // Sweeps ignore per-entry failures: a vanished process or a freed
    // page simply skips that entry until the next pass.
    let _ = crate::memory::write_process_memory(pid, address, &scratch[..len]);
    true
}

/// Initialises the table mutex. Called from [`crate::comm::start`] before
/// the worker is spawned, so no `Lock` request can ever reach the table
/// first. Nothing here can fail.
pub fn init() {
    // SAFETY: first statement of the first caller; no thread exists yet.
    unsafe { init_fast_mutex(TABLE_MUTEX.0.get()) };
    INITIALIZED.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table behaviour is exercised on a plain stack instance: the static
    /// instance needs the kernel fast mutex, and [`sweep_step`] itself only
    /// runs inside a loaded driver (ks-test covers it end to end).
    #[test]
    fn lock_table_lifecycle() {
        let mut table = LockTable::new();

        table.insert(1, 7, 0x1000, b"abcd").expect("first lock");
        table.insert(2, 7, 0x2000, b"efgh").expect("second lock");
        // Re-locking an existing id replaces the payload, not a new slot.
        table.insert(1, 7, 0x1000, b"zz").expect("replace");
        assert_eq!(
            table.snapshot(0, &mut [0u8; MAX_MEMORY_LOCK_SIZE]),
            Some((7, 0x1000, 2))
        );
        {
            let mut scratch = [0u8; MAX_MEMORY_LOCK_SIZE];
            table.snapshot(0, &mut scratch).expect("slot 0");
            assert_eq!(&scratch[..2], b"zz");
        }

        table.remove(1).expect("unlock");
        assert!(table
            .snapshot(0, &mut [0u8; MAX_MEMORY_LOCK_SIZE])
            .is_none());
        assert!(table
            .snapshot(1, &mut [0u8; MAX_MEMORY_LOCK_SIZE])
            .is_some());
        // Unlocking an unknown id is a successful no-op.
        table.remove(42).expect("idempotent unlock");
        assert!(table
            .snapshot(1, &mut [0u8; MAX_MEMORY_LOCK_SIZE])
            .is_some());

        // Validation mirrors the protocol boundary.
        assert_eq!(
            table.insert(0, 7, 0x1000, b"a"),
            Err(STATUS_INVALID_PARAMETER)
        );
        assert_eq!(
            table.insert(3, 0, 0x1000, b"a"),
            Err(STATUS_INVALID_PARAMETER)
        );
        assert_eq!(table.insert(3, 7, 0, b"a"), Err(STATUS_INVALID_PARAMETER));
        assert_eq!(
            table.insert(3, 7, 0x1000, b""),
            Err(STATUS_INVALID_PARAMETER)
        );
        assert_eq!(
            table.insert(3, 7, 0x1000, &[0u8; MAX_MEMORY_LOCK_SIZE + 1]),
            Err(STATUS_INVALID_PARAMETER)
        );
        assert_eq!(table.remove(0), Err(STATUS_INVALID_PARAMETER));
        assert_eq!(table.clear(0), Err(STATUS_INVALID_PARAMETER));

        // A pid-scoped clear removes only that pid's entries: the pid-7
        // entry (slot 1) is gone, while the pid-9 entry reused slot 0.
        table.insert(3, 9, 0x3000, b"wxyz").expect("third lock");
        table.clear(7).expect("clear pid 7");
        assert!(table
            .snapshot(1, &mut [0u8; MAX_MEMORY_LOCK_SIZE])
            .is_none());
        assert_eq!(
            table.snapshot(0, &mut [0u8; MAX_MEMORY_LOCK_SIZE]),
            Some((9, 0x3000, 4))
        );

        table.clear_all();
        for index in 0..MAX_MEMORY_LOCKS {
            assert!(table
                .snapshot(index, &mut [0u8; MAX_MEMORY_LOCK_SIZE])
                .is_none());
        }
    }

    #[test]
    fn lock_table_reports_quota_when_full() {
        let mut table = LockTable::new();
        for id in 1..=MAX_MEMORY_LOCKS as u64 {
            table
                .insert(id, 7, 0x1000 + id * 8, b"xxxx")
                .unwrap_or_else(|status| panic!("lock {id} rejected: {status:#x}"));
        }
        assert_eq!(
            table.insert(MAX_MEMORY_LOCKS as u64 + 1, 7, 0x5000, b"xxxx"),
            Err(STATUS_QUOTA_EXCEEDED)
        );
        // Replacing an existing id still works while the table is full.
        table
            .insert(1, 7, 0x1000, b"yyyy")
            .expect("replace at capacity");
        let mut scratch = [0u8; MAX_MEMORY_LOCK_SIZE];
        let (pid, address, len) = table.snapshot(0, &mut scratch).expect("slot 0");
        assert_eq!((pid, address, len), (7, 0x1000, 4));
        assert_eq!(&scratch[..4], b"yyyy");
    }
}
