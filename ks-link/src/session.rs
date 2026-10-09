//! The process-wide ring session: open, round trip and teardown.

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::{ptr, slice};

use ks_core::protocol::{
    decode_response_meta, encode_request, ProtocolError, Request, ResponseMeta,
};
use ks_core::ring::{
    header, REQUEST_SIZE, RESPONSE_BULK_SIZE, RESPONSE_META_SIZE, RING_MUTEX_CLIENT_NAME,
    STATE_IDLE, STATE_REQUEST, STATE_RESPONSE,
};
use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcessId, OpenEventW, ReleaseMutex, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, INFINITE,
};

use super::error::{last_error, wide, win_api, LinkError};
use super::names::published_object_names;

/// How long a client waits for the driver to answer before it cancels a
/// request the driver never picked up (`STATE_REQUEST`). The cancel window
/// is deliberately generous: target-process reads can stall on paged-out
/// memory.
const RESPONSE_TIMEOUT_MS: u32 = 5_000;

/// An open connection to the driver: section mapping, both events and the
/// client mutex. Names are randomized per driver load and resolved once at
/// open, so the session belongs to the load it opened against: a driver
/// reload creates fresh objects under fresh names, orphaning this session
/// (its round trips time out) — the client must drop it with
/// [`close_session`] (or start a new process) to open one against the
/// current load. A session never reconnects on its own.
pub(crate) struct Session {
    base: *mut u8,
    section: windows_sys::Win32::Foundation::HANDLE,
    request_event: windows_sys::Win32::Foundation::HANDLE,
    response_event: windows_sys::Win32::Foundation::HANDLE,
    mutex: windows_sys::Win32::Foundation::HANDLE,
    client_pid: u32,
}

unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    /// Opens every shared object the driver publishes. All-or-nothing: on
    /// failure everything opened so far is released again.
    fn open() -> Result<Self, LinkError> {
        let names = published_object_names();
        let section_name = wide(&names[0]);
        let request_name = wide(&names[1]);
        let response_name = wide(&names[2]);
        let mutex_name = wide(RING_MUTEX_CLIENT_NAME);

        // SAFETY: name buffers are NUL terminated and outlive the call.
        let section = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, section_name.as_ptr()) };
        if section.is_null() {
            return Err(LinkError::DriverNotReady {
                object: "section",
                code: last_error(),
            });
        }

        // SAFETY: the mapping covers the whole section; every access is
        // bounded by the ks-core ring layout constants.
        let view = unsafe {
            MapViewOfFile(
                section,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                ks_core::ring::RING_TOTAL_SIZE,
            )
        };
        if view.Value.is_null() {
            let error = win_api("MapViewOfFile");
            unsafe { CloseHandle(section) };
            return Err(error);
        }
        let base = view.Value as *mut u8;

        if let Err(error) = check_ring(base) {
            release_mapping(base, section);
            return Err(error);
        }

        // SAFETY: name buffer is NUL terminated.
        let request_event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, request_name.as_ptr()) };
        if request_event.is_null() {
            let error = LinkError::DriverNotReady {
                object: "request event",
                code: last_error(),
            };
            release_mapping(base, section);
            return Err(error);
        }

        // SAFETY: name buffer is NUL terminated.
        let response_event = unsafe { OpenEventW(SYNCHRONIZE, 0, response_name.as_ptr()) };
        if response_event.is_null() {
            let error = LinkError::DriverNotReady {
                object: "response event",
                code: last_error(),
            };
            unsafe { CloseHandle(request_event) };
            release_mapping(base, section);
            return Err(error);
        }

        // SAFETY: name buffer is NUL terminated; no security attributes, so
        // the mutex uses the creator's default DACL (this process is
        // elevated).
        let mutex = unsafe { CreateMutexW(ptr::null(), 0, mutex_name.as_ptr()) };
        if mutex.is_null() {
            let error = win_api("CreateMutexW");
            unsafe { CloseHandle(response_event) };
            unsafe { CloseHandle(request_event) };
            release_mapping(base, section);
            return Err(error);
        }

        Ok(Self {
            base,
            section,
            request_event,
            response_event,
            mutex,
            client_pid: unsafe { GetCurrentProcessId() },
        })
    }

    /// The ring header view for this mapping, or `RingIncompatible`.
    fn ring(&self) -> Result<&'static ks_core::ring::RingHeader, LinkError> {
        check_ring(self.base)
    }

    /// Serialized request/response exchange. `request` is an already
    /// postcard-encoded [`Request`] (callers may cache encodings); the
    /// response payload is copied into `out`.
    fn round_trip(
        &self,
        request: &[u8],
        out: &mut [u8],
    ) -> Result<(ResponseMeta, usize), LinkError> {
        if request.is_empty() || request.len() > REQUEST_SIZE {
            return Err(LinkError::Encode(ProtocolError::TooLarge));
        }
        let head = self.ring()?;

        let guard = self.acquire_mutex()?;
        if guard.abandoned {
            self.reset_stale_state(head);
        }

        // A previous client may have timed out right before the driver
        // completed; drop that pending signal so it cannot be mistaken for
        // the answer to this request.
        // SAFETY: response_event is a live event handle.
        while unsafe { WaitForSingleObject(self.response_event, 0) } == WAIT_OBJECT_0 {}

        let sequence = head.next_sequence();
        // SAFETY: request.len() <= REQUEST_SIZE, so the copy stays inside
        // the request region. The ring mutex serializes writers.
        unsafe {
            ptr::copy_nonoverlapping(
                request.as_ptr(),
                self.base.add(ks_core::ring::REQUEST_OFFSET),
                request.len(),
            )
        };
        head.set_client_pid(self.client_pid);
        head.set_request_len(request.len() as u32);
        head.store_state(STATE_REQUEST);
        // SAFETY: request_event is a live event handle.
        if unsafe { SetEvent(self.request_event) } == 0 {
            head.store_state(STATE_IDLE);
            return Err(win_api("SetEvent(request)"));
        }

        match self.wait_for_response(head, sequence) {
            Ok(()) => {}
            Err(error) => {
                head.store_state(STATE_IDLE);
                return Err(error);
            }
        }

        let answer = self.take_response(out);
        head.store_state(STATE_IDLE);
        drop(guard);
        answer
    }

    /// Waits until the driver publishes this request's response. A request
    /// the driver never picked up is cancelled after [`RESPONSE_TIMEOUT_MS`];
    /// one it is already working on is waited out, because the driver will
    /// publish into a ring this client is about to reuse.
    fn wait_for_response(
        &self,
        head: &ks_core::ring::RingHeader,
        sequence: u64,
    ) -> Result<(), LinkError> {
        loop {
            // SAFETY: response_event is a live event handle.
            let wait = unsafe { WaitForSingleObject(self.response_event, RESPONSE_TIMEOUT_MS) };
            if head.state() == STATE_RESPONSE && head.response_sequence() == sequence {
                return Ok(());
            }
            match wait {
                WAIT_OBJECT_0 => continue, // stale signal from an earlier request
                WAIT_TIMEOUT => {
                    if head.state() == STATE_REQUEST && head.cas_state(STATE_REQUEST, STATE_IDLE) {
                        // The driver never picked the request up; take it back.
                        return Err(LinkError::TimedOut);
                    }
                    if head.state() != STATE_REQUEST {
                        // The driver owns the slot; give it the next window.
                        continue;
                    }
                    return Err(LinkError::TimedOut);
                }
                _ => {
                    return Err(LinkError::WinApi {
                        operation: "WaitForSingleObject(response)",
                        code: last_error(),
                    })
                }
            }
        }
    }

    /// Copies the published response into `out`. The caller keeps the ring
    /// mutex and must publish [`STATE_IDLE`] afterwards.
    fn take_response(&self, out: &mut [u8]) -> Result<(ResponseMeta, usize), LinkError> {
        let head = self.ring()?;
        let status = head.status();
        if status != 0 {
            return Err(LinkError::NtStatus(status));
        }
        // SAFETY: the whole response region is inside the mapping.
        let response = unsafe {
            slice::from_raw_parts(
                self.base.add(ks_core::ring::RESPONSE_OFFSET),
                RESPONSE_META_SIZE + RESPONSE_BULK_SIZE,
            )
        };
        let meta =
            decode_response_meta(&response[..RESPONSE_META_SIZE]).map_err(LinkError::Decode)?;
        let bulk_len = meta.bulk_len as usize;
        if bulk_len > RESPONSE_BULK_SIZE {
            return Err(LinkError::Decode(ProtocolError::TooLarge));
        }
        if bulk_len > out.len() {
            return Err(LinkError::ResponseTooSmall);
        }
        out[..bulk_len].copy_from_slice(&response[RESPONSE_META_SIZE..][..bulk_len]);
        Ok((meta, bulk_len))
    }

    /// Acquires the client mutex. `abandoned` marks a mutex whose previous
    /// owner died; the ring state is then repaired by
    /// [`Session::reset_stale_state`].
    fn acquire_mutex(&self) -> Result<MutexGuard<'_>, LinkError> {
        // SAFETY: mutex is a live mutex handle.
        let wait = unsafe { WaitForSingleObject(self.mutex, INFINITE) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            return Err(win_api("WaitForSingleObject(ring mutex)"));
        }
        Ok(MutexGuard {
            session: self,
            abandoned: wait == WAIT_ABANDONED,
        })
    }

    /// Repairs a ring left behind by a dead client: requests still marked
    /// `STATE_REQUEST` are rolled back and stale response markers cleared.
    fn reset_stale_state(&self, head: &ks_core::ring::RingHeader) {
        match head.state() {
            STATE_REQUEST => {
                head.cas_state(STATE_REQUEST, STATE_IDLE);
            }
            STATE_RESPONSE => {
                head.store_state(STATE_IDLE);
            }
            _ => {}
        }
    }
}

struct MutexGuard<'a> {
    session: &'a Session,
    abandoned: bool,
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex while the guard is alive.
        unsafe { ReleaseMutex(self.session.mutex) };
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: every field is a live handle created in `open`.
        unsafe {
            CloseHandle(self.mutex);
            CloseHandle(self.response_event);
            CloseHandle(self.request_event);
        }
        release_mapping(self.base, self.section);
    }
}

fn check_ring(base: *mut u8) -> Result<&'static ks_core::ring::RingHeader, LinkError> {
    // SAFETY: `base` is a live mapping of RING_TOTAL_SIZE bytes created by
    // `Session::open`, which is the only caller.
    let head = unsafe { header(base) }.ok_or(LinkError::RingIncompatible)?;
    if head.is_compatible() {
        Ok(head)
    } else {
        Err(LinkError::RingIncompatible)
    }
}

fn release_mapping(base: *mut u8, section: windows_sys::Win32::Foundation::HANDLE) {
    // SAFETY: `base` is the mapping view created in `open`.
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
            Value: base as *mut c_void,
        })
    };
    unsafe { CloseHandle(section) };
}

static SESSION: AtomicPtr<Session> = AtomicPtr::new(ptr::null_mut());

/// Returns the process-wide session, opening it on first use. Concurrent
/// callers race to publish; losers use the winner's session.
pub(crate) fn session() -> Result<&'static Session, LinkError> {
    let cached = SESSION.load(Ordering::Acquire);
    if !cached.is_null() {
        // SAFETY: sessions are leaked, so the pointer stays valid forever.
        return Ok(unsafe { &*cached });
    }
    let leaked = Box::leak(Box::new(Session::open()?));
    match SESSION.compare_exchange(
        ptr::null_mut(),
        leaked as *const Session as *mut Session,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(leaked),
        Err(winner) => {
            // Another thread won the race; use its session and let this one
            // leak (closing it could race the winner's first round trip).
            // SAFETY: the winner also leaked its session.
            Ok(unsafe { &*winner })
        }
    }
}

/// Submits one already-encoded request.
pub(crate) fn submit_encoded(
    request: &[u8],
    out: &mut [u8],
) -> Result<(ResponseMeta, usize), LinkError> {
    session()?.round_trip(request, out)
}

/// Drops the process-wide session — its mapping and all four handles —
/// so the next round trip opens a fresh one against whatever the registry
/// publishes *now*. This is the only way to re-target a running process
/// after a driver restart (every load randomizes the object names).
///
/// The swap frees the session immediately, so it is only safe while no
/// round trip is in flight and no other thread can enter [`session`]:
/// callers must gate every round-trip source first. ks-gui does exactly
/// that — its driver-lifecycle flag blocks `sync_ipc` on the GUI Lua
/// thread before any start/stop job that ends in this call.
pub fn close_session() {
    let stale = SESSION.swap(ptr::null_mut(), Ordering::AcqRel);
    if stale.is_null() {
        return;
    }
    // SAFETY: the swap removed the only published pointer and the gate
    // contract above guarantees no thread is inside `session` now; the
    // box was leaked by `session` and owns every handle it closes here.
    let session = unsafe { Box::from_raw(stale) };
    release_mapping(session.base, session.section);
    // SAFETY: handles owned by this session, closed exactly once.
    unsafe {
        let _ = CloseHandle(session.request_event);
        let _ = CloseHandle(session.response_event);
        let _ = CloseHandle(session.mutex);
    }
}

pub(crate) fn submit(
    request: &Request<'_>,
    buffer: &mut [u8],
    out: &mut [u8],
) -> Result<(ResponseMeta, usize), LinkError> {
    let len = encode_request(request, buffer).map_err(LinkError::Encode)?;
    submit_encoded(&buffer[..len], out)
}
