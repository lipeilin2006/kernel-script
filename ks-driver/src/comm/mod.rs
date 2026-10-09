//! Shared-memory transport: one named section plus a request/response event
//! pair, serviced by a single system thread that also replays the memory
//! lock table between requests (no dedicated rewrite thread).
//!
//! The driver creates (or reopens) all three objects with a DACL that grants
//! `GENERIC_ALL` to LocalSystem and the Administrators group, maps the
//! section into system space, and runs [`worker`] until unload. User-mode
//! clients reach the same objects through the `Global\` prefix.
//!
//! The three object names are randomized at every startup: a 16-hex-char
//! token (seeded from interrupt time, drawn with `RtlRandomEx`) is appended
//! to fixed prefixes, and the full names are published under
//! `HKLM\SOFTWARE\KernelScript` — clients resolve them from there at
//! session open. A reload therefore creates fresh objects under fresh
//! names; a client still holding the previous load's objects is orphaned.
//! Teardown ([`registry::release_instance`]) erases the whole publication
//! again, so after a clean exit the key is gone and only a crash leaves
//! values behind for the next load to overwrite.
//!
//! Single instance: before the ring exists, [`registry::claim_instance`]
//! takes a kernel-only marker event, so a second load (SCM or a manual
//! mapper) is refused with `STATUS_OBJECT_NAME_COLLISION` instead of
//! splitting the ring and the lock table across two driver images. The
//! marker name itself stays fixed — it is the liveness probe both loads
//! must agree on.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ks_core::ring::{header, request_bytes, response_bytes, STATE_PROCESSING, STATE_REQUEST};

use crate::request;
use crate::wdm::*;

mod names;
mod registry;
mod ring;
mod security;

use names::RingNames;
use registry::{claim_instance, publish_object_names, release_instance};
use ring::{create_ring, release};

/// Longest object name handled here (`\BaseNamedObjects\...` is well under
/// this), including the terminating NUL.
const OBJECT_NAME_CAPACITY: usize = 64;
/// Fixed prefix of the section name; [`RingNames`](names::RingNames)
/// appends `-{token}`.
const SECTION_NAME_PREFIX: &str = "\\BaseNamedObjects\\KernelScriptSection-";
/// Fixed prefix of the request-event name; [`RingNames`](names::RingNames)
/// appends `-{token}`.
const REQUEST_NAME_PREFIX: &str = "\\BaseNamedObjects\\KernelScriptRequest-";
/// Fixed prefix of the response-event name; [`RingNames`](names::RingNames)
/// appends `-{token}`.
const RESPONSE_NAME_PREFIX: &str = "\\BaseNamedObjects\\KernelScriptResponse-";
/// Handle access requested for the worker thread; only used for the
/// reference the unload path waits on.
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
/// Kernel priority 6: below the normal-class base 8, the mirror of Win32
/// `THREAD_PRIORITY_BELOW_NORMAL`. The merged worker spins through the
/// lock table whenever requests are absent, so it must lose every
/// scheduling contest to the game and the GUI; requests still run at once
/// whenever the CPU is free.
const WORKER_PRIORITY: i32 = 6;
/// Zero timeout: turn the request-event wait into a non-blocking poll
/// while locks are held (rewrite yields to requests, never sleeps).
const POLL_TIMEOUT: i64 = 0;

static MAPPED_BASE: AtomicUsize = AtomicUsize::new(0);
static SECTION_HANDLE: AtomicUsize = AtomicUsize::new(0);
static REQUEST_EVENT: AtomicUsize = AtomicUsize::new(0);
static RESPONSE_EVENT: AtomicUsize = AtomicUsize::new(0);
static INSTANCE_HANDLE: AtomicUsize = AtomicUsize::new(0);
static THREAD_HANDLE: AtomicUsize = AtomicUsize::new(0);
static THREAD_OBJECT: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

unsafe fn spawn_worker() -> NTSTATUS {
    let mut thread: HANDLE = ptr::null_mut();
    // Kernel handle again: the join handle must outlive whatever process
    // `DriverEntry` happened to run in (under KDU that is the mapper, which
    // exits right after the load).
    let attributes = OBJECT_ATTRIBUTES {
        Length: OBJECT_ATTRIBUTES_LENGTH,
        RootDirectory: ptr::null_mut(),
        ObjectName: ptr::null(),
        Attributes: OBJ_KERNEL_HANDLE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let status = PsCreateSystemThread(
        &mut thread,
        SYNCHRONIZE_ACCESS,
        &attributes,
        ptr::null_mut(),
        ptr::null_mut(),
        worker,
        ptr::null(),
    );
    if !nt_success(status) {
        return status;
    }
    THREAD_HANDLE.store(thread as usize, Ordering::SeqCst);

    let mut object: *mut c_void = ptr::null_mut();
    let status = ObReferenceObjectByHandle(
        thread,
        SYNCHRONIZE_ACCESS,
        0,
        KernelMode as i8,
        &mut object,
        ptr::null_mut(),
    );
    if !nt_success(status) || object.is_null() {
        let _ = ZwClose(thread);
        THREAD_HANDLE.store(0, Ordering::SeqCst);
        return if nt_success(status) {
            STATUS_INVALID_PARAMETER
        } else {
            status
        };
    }
    THREAD_OBJECT.store(object as usize, Ordering::SeqCst);
    STATUS_SUCCESS
}

/// Called from `DriverEntry`. On failure every partially created object has
/// already been released before the status is returned.
pub fn start() -> NTSTATUS {
    unsafe {
        // Single instance: refuse a second load before any ring object
        // exists, so two images can never split the ring or the lock table.
        let claimed = claim_instance();
        if !nt_success(claimed) {
            crate::trace!("single-instance claim refused 0x%lx", claimed as u32);
            return claimed;
        }
        // Fresh random names for this load: one token shared by the three
        // objects, published to the registry after the ring exists.
        let names = RingNames::generate();
        let objects = match create_ring(&names) {
            Ok(objects) => objects,
            Err(status) => {
                // Releases the claim taken above; with no objects created
                // yet, everything else in `stop` is a no-op.
                stop();
                return status;
            }
        };
        MAPPED_BASE.store(objects.mapped as usize, Ordering::SeqCst);
        SECTION_HANDLE.store(objects.section as usize, Ordering::SeqCst);
        REQUEST_EVENT.store(objects.request_event as usize, Ordering::SeqCst);
        RESPONSE_EVENT.store(objects.response_event as usize, Ordering::SeqCst);
        core::mem::forget(objects);

        crate::trace!(
            "ring ready mapped=%p",
            MAPPED_BASE.load(Ordering::SeqCst) as *mut u8
        );
        // The lock table's fast mutex must exist before the first `Lock`
        // request can arrive; initialisation cannot fail.
        crate::lock::init();
        let status = spawn_worker();
        if !nt_success(status) {
            crate::trace!("worker spawn failed 0x%lx", status as u32);
            stop();
            return status;
        }
        crate::trace!("worker spawned");

        // With randomized names the registry publication is the only way a
        // client can learn them — no fallback exists — so a failed write
        // fails the whole load instead of leaving an unreachable driver.
        let published = publish_object_names(&names);
        if nt_success(published) {
            crate::trace!("object names published to registry");
        } else {
            crate::trace!("registry publish failed 0x%lx", published as u32);
            stop();
            return published;
        }
        STATUS_SUCCESS
    }
}

/// Releases everything [`start`] acquired, from the worker itself when a
/// `Shutdown` request arrives. Every handle is swapped out of its static
/// first, so this and [`stop`] — whichever runs first — owns each handle
/// and the other observes NULL: no handle is ever closed twice.
///
/// `THREAD_OBJECT` is deliberately left untouched: [`stop`] must still be
/// able to join through it after a `Shutdown` + `sc stop` sequence. A
/// manually mapped image never unloads, so its thread object and the
/// worker's kernel stack are never collected — an accepted leak of one
/// thread per mapped instance.
///
/// # Safety
/// Runs on the worker thread itself with no concurrent sweep (this *is*
/// the worker); the ring handles it swaps are only used by this thread
/// from here on, and the client still holds its own references to the
/// shared objects.
unsafe fn self_teardown() {
    let thread_handle = THREAD_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    if !thread_handle.is_null() {
        let _ = ZwClose(thread_handle);
    }
    let request_event = REQUEST_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    let response_event = RESPONSE_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    let section = SECTION_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    let mapped = MAPPED_BASE.swap(0, Ordering::SeqCst) as *mut u8;
    release(mapped, section, request_event, response_event);
    // Last: the marker object must outlive the ring teardown so a racing
    // second load stays rejected until this instance is fully gone.
    release_instance();
}

/// Called from `DriverUnload`. Idempotent: every handle is swapped out of
/// its static before it is closed, so a second call — or a [`self_teardown`]
/// that already ran — is a no-op.
///
/// # Safety
/// No client may be inside [`worker`] when this runs; the wait on the thread
/// object guarantees that.
pub unsafe fn stop() {
    STOP.store(true, Ordering::SeqCst);

    let request_event = REQUEST_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    if !request_event.is_null() {
        let _ = ZwSetEvent(request_event, ptr::null_mut());
    }

    let thread_object = THREAD_OBJECT.swap(0, Ordering::SeqCst);
    if thread_object != 0 {
        let object = thread_object as Pvoid;
        let _ = KeWaitForSingleObject(
            object as *const c_void,
            Executive,
            KernelMode as i8,
            false,
            ptr::null(),
        );
        ObDereferenceObject(object);
    }

    let thread_handle = THREAD_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    if !thread_handle.is_null() {
        let _ = ZwClose(thread_handle);
    }
    // The ring worker is joined, so no sweep can race the final table
    // clear; dropping the table keeps every target write inside the
    // driver's lifetime.
    crate::lock::clear_table();
    let response_event = RESPONSE_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    let section = SECTION_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    let mapped = MAPPED_BASE.swap(0, Ordering::SeqCst) as *mut u8;
    release(mapped, section, request_event, response_event);
    // Last: the marker object must outlive the ring teardown so a racing
    // second load stays rejected until this instance is fully gone.
    release_instance();
    STOP.store(false, Ordering::SeqCst);
}

unsafe extern "system" fn worker(_context: *mut c_void) {
    crate::trace!("worker enter");
    let _ = KeSetPriorityThread(PsGetCurrentThread(), WORKER_PRIORITY);
    let base = MAPPED_BASE.load(Ordering::SeqCst) as *mut u8;
    let request_event = REQUEST_EVENT.load(Ordering::SeqCst) as HANDLE;
    let response_event = RESPONSE_EVENT.load(Ordering::SeqCst) as HANDLE;
    loop {
        // Merged request/lock loop: with locks held the wait is a pure
        // poll (timeout 0), so a pending request always wins over the
        // next rewrite entry; with the table empty the wait blocks, so an
        // idle driver burns no CPU — the `Lock` request itself is what
        // wakes it, and no private wake event exists.
        let timeout: *const i64 = if crate::lock::has_entries() {
            &POLL_TIMEOUT
        } else {
            ptr::null()
        };
        let status = ZwWaitForSingleObject(request_event, false, timeout);
        if !nt_success(status) {
            crate::trace!("wait failed 0x%lx", status as u32);
            break;
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        let Some(head) = header(base) else {
            break;
        };
        // The client only publishes REQUEST after it has finished writing
        // the payload, and only the driver may move it to PROCESSING, so a
        // spurious wakeup just loops back to the wait.
        if head.state() == STATE_REQUEST && head.cas_state(STATE_REQUEST, STATE_PROCESSING) {
            crate::trace!(
                "request seq=%llu len=%lu",
                head.sequence(),
                head.request_len()
            );
            let request = request_bytes(base);
            let response = response_bytes(base);
            let keep_serving = request::process_request(head, request, response);
            crate::trace!(
                "respond st=0x%lx seq=%llu",
                head.status() as u32,
                head.response_sequence()
            );
            let _ = ZwSetEvent(response_event, ptr::null_mut());
            if !keep_serving {
                crate::trace!("shutdown requested - worker exiting");
                // The response is already published; drop the lock table
                // so a shut-down driver performs no further target writes.
                crate::lock::clear_table();
                // Then release everything `start` acquired. A manually
                // mapped image (KDU) never runs `DriverUnload`, so this is
                // its only chance to free the ring objects, the registry
                // claim and the single-instance marker; under the SCM load
                // path `stop` repeats the same swaps after joining this
                // thread and finds them already NULL.
                self_teardown();
                break;
            }
            // Queued requests are served before any further rewriting.
            continue;
        }
        // No request pending: rewrite at most one lock entry per pass,
        // polling the event again immediately afterwards. A table that
        // became empty just loops back to the blocking wait.
        let _ = crate::lock::sweep_step();
    }
    crate::trace!("worker exit");
    let _ = PsTerminateSystemThread(STATUS_SUCCESS);
}
