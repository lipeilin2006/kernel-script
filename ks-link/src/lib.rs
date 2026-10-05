//! User-mode client for the KernelScript driver's shared-memory ring.
//!
//! A [`Session`] opens the section, the two events and the client mutex the
//! driver publishes in `\BaseNamedObjects`, maps the section into the
//! process, and serializes every request through [`Session::round_trip`].
//! The module-level free functions are the stable surface consumed by the
//! GUI and the test harness: each one connects on demand, submits one
//! request and copies the answer out of the ring before returning.

mod lock;
mod process;

use core::ffi::c_void;
use core::fmt;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::{ptr, slice};

use heapless::Vec as SmallVec;
use ks_core::protocol::{
    decode_response_meta, encode_request, BatchWriteItem, ProtocolError, Request, ResponseMeta,
    MAX_BATCH_ENTRIES, MAX_BATCH_WRITE_ENTRIES, MAX_CHAIN_OFFSETS,
};
use ks_core::ring::{
    header, REQUEST_EVENT_CLIENT_NAME, REQUEST_SIZE, RESPONSE_BULK_SIZE,
    RESPONSE_EVENT_CLIENT_NAME, RESPONSE_META_SIZE, RING_MUTEX_CLIENT_NAME, SECTION_CLIENT_NAME,
    STATE_IDLE, STATE_REQUEST, STATE_RESPONSE,
};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcessId, OpenEventW, ReleaseMutex, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, INFINITE,
};

pub use lock::{lock, lock_rva, unlock, unlock_all, unlock_rva};
pub use lock::{MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE};
pub use process::find_pid;

/// How long a client waits for the driver to answer before it cancels a
/// request the driver never picked up (`STATE_REQUEST`). The cancel window
/// is deliberately generous: target-process reads can stall on paged-out
/// memory.
const RESPONSE_TIMEOUT_MS: u32 = 5_000;

/// Stack headroom for the largest fixed-size request: a batch read with
/// [`MAX_BATCH_ENTRIES`] addresses.
const SMALL_REQUEST_BUFFER: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkError {
    /// The section or one of the events does not exist, so the driver is
    /// not loaded (or is still initializing) — or its DACL rejected us.
    /// `object` names what the client was opening; `code` is the failing
    /// `GetLastError` value.
    DriverNotReady {
        object: &'static str,
        code: u32,
    },
    WinApi {
        operation: &'static str,
        code: u32,
    },
    /// The mapping exists but its magic/version is not ours.
    RingIncompatible,
    Encode(ProtocolError),
    Decode(ProtocolError),
    /// The driver answered with a failure NTSTATUS.
    NtStatus(i32),
    /// The driver did not answer within the cancel window.
    TimedOut,
    ResponseTooSmall,
    TooManyEntries {
        limit: usize,
    },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DriverNotReady { object, code } => {
                write!(f, "driver not loaded ({object} open failed: {code})")?;
                // ERROR_ACCESS_DENIED (5) means the object exists but the
                // driver DACL rejected us: the caller is not elevated, or
                // the DACL does not actually match Administrators.
                if *code == 5 {
                    f.write_str(" — access denied: run elevated (the driver DACL only grants SYSTEM and Administrators)")?;
                }
                Ok(())
            }
            Self::WinApi { operation, code } => write!(f, "{operation} failed: {code}"),
            Self::RingIncompatible => f.write_str("ring header magic/version mismatch"),
            Self::Encode(error) => write!(f, "request encode failed: {error}"),
            Self::Decode(error) => write!(f, "response decode failed: {error}"),
            Self::NtStatus(status) => write!(f, "driver error {status:#x}"),
            Self::TimedOut => f.write_str("driver did not answer in time"),
            Self::ResponseTooSmall => f.write_str("caller buffer smaller than driver payload"),
            Self::TooManyEntries { limit } => write!(f, "more than {limit} entries in one request"),
        }
    }
}

impl std::error::Error for LinkError {}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

fn win_api(operation: &'static str) -> LinkError {
    LinkError::WinApi {
        operation,
        code: last_error(),
    }
}

/// Encodes `name` as a NUL-terminated UTF-16 buffer for the `*W` APIs.
fn wide(name: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

/// Subkey under `HKLM` where the driver publishes the object names.
const NAMES_KEY: &str = r"SOFTWARE\KernelScript";

/// Value below [`NAMES_KEY`] marking the live single-instance claim
/// (`REG_DWORD` 1, written at load, deleted at teardown).
const CLAIM_VALUE: &str = "Instance";

/// Reads one `REG_SZ` value below `HKLM\SOFTWARE\KernelScript`, returning
/// its UTF-16 units (with the terminating NUL, if any) or `None` when the
/// key/value is missing or holds another type.
fn registry_sz(value: &str) -> Option<Vec<u16>> {
    let key = wide(NAMES_KEY);
    let name = wide(value);
    let mut data = [0u16; 96];
    let mut size = (data.len() * 2) as u32;
    let mut kind: u32 = 0;
    // SAFETY: buffers are live for the call and `size` bounds the write.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            data.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if status != 0 || size < 2 {
        return None;
    }
    Some(data[..(size as usize / 2)].to_vec())
}

/// Maps a published kernel name (`\BaseNamedObjects\...`) onto the client
/// namespace (`Global\...`).
fn kernel_name_to_client(units: &[u16]) -> Option<String> {
    let text = String::from_utf16_lossy(units);
    let text = text.trim_end_matches('\0');
    let tail = text.strip_prefix(r"\BaseNamedObjects\")?;
    if tail.is_empty() || tail.contains('\\') {
        return None;
    }
    Some(format!(r"Global\{tail}"))
}

fn published_name_opt(value: &str) -> Option<String> {
    registry_sz(value).and_then(|units| kernel_name_to_client(&units))
}

fn published_name(value: &str, fallback: &str) -> String {
    published_name_opt(value).unwrap_or_else(|| fallback.to_string())
}

/// The client-facing object names for this driver load, resolved from the
/// driver's registry publication with the compiled-in defaults as fallback
/// — a fallback that cannot reach a randomized load, so an unreadable key
/// effectively fails the session open.
pub fn published_object_names() -> [String; 3] {
    [
        published_name("SectionName", SECTION_CLIENT_NAME),
        published_name("RequestEventName", REQUEST_EVENT_CLIENT_NAME),
        published_name("ResponseEventName", RESPONSE_EVENT_CLIENT_NAME),
    ]
}

/// Like [`published_object_names`] but returns `None` when any value is
/// missing: callers that must prove the driver really wrote
/// `HKLM\SOFTWARE\KernelScript` (the test harness polls it as its
/// readiness signal) use this instead of the fallback variant.
pub fn published_object_names_strict() -> Option<[String; 3]> {
    Some([
        published_name_opt("SectionName")?,
        published_name_opt("RequestEventName")?,
        published_name_opt("ResponseEventName")?,
    ])
}

/// Whether the driver's [`CLAIM_VALUE`] claim value is still present.
///
/// The driver writes it at load and deletes it at teardown (which removes
/// the whole key as well), so `true` means a loaded instance owns the
/// single-instance marker, while `false` means either nothing ever loaded
/// or `shutdown` (or `sc stop`) finished teardown. A missing key — before
/// the first load, or after teardown — is `false`; any other registry
/// failure is returned.
pub fn instance_claim_present() -> Result<bool, LinkError> {
    let key = wide(NAMES_KEY);
    let name = wide(CLAIM_VALUE);
    let mut data = [0u8; 4];
    let mut kind: u32 = 0;
    let mut size = data.len() as u32;
    // SAFETY: buffers are live for the call and `size` bounds the write.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut kind,
            data.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    match status {
        0 => Ok(true),
        // Missing key or missing value: no claim either way.
        ERROR_FILE_NOT_FOUND => Ok(false),
        code => Err(LinkError::WinApi {
            operation: "read the Instance claim",
            code,
        }),
    }
}

/// An open connection to the driver: section mapping, both events and the
/// client mutex. Names are randomized per driver load and resolved once at
/// open, so the session belongs to the load it opened against: a driver
/// reload creates fresh objects under fresh names, orphaning this session
/// (its round trips time out) — the client must drop it with
/// [`close_session`] (or start a new process) to open one against the
/// current load. A session never reconnects on its own.
struct Session {
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
fn submit_encoded(request: &[u8], out: &mut [u8]) -> Result<(ResponseMeta, usize), LinkError> {
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

fn submit(
    request: &Request<'_>,
    buffer: &mut [u8],
    out: &mut [u8],
) -> Result<(ResponseMeta, usize), LinkError> {
    let len = encode_request(request, buffer).map_err(LinkError::Encode)?;
    submit_encoded(&buffer[..len], out)
}

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
    fn wide_strings_are_nul_terminated() {
        let wide = wide("ab");
        assert_eq!(wide[0], u16::from(b'a'));
        assert_eq!(wide[1], u16::from(b'b'));
        assert_eq!(wide[2], 0);
    }

    #[test]
    fn error_display_mentions_operation() {
        let error = LinkError::WinApi {
            operation: "MapViewOfFile",
            code: 5,
        };
        assert_eq!(error.to_string(), "MapViewOfFile failed: 5");
    }

    #[test]
    fn address_vectors_reject_overflows() {
        assert_eq!(
            addresses_small_vec::<1>(&[1, 2], 1),
            Err(LinkError::TooManyEntries { limit: 1 })
        );
        assert!(addresses_small_vec::<4>(&[1, 2], 4).is_ok());
    }
}
