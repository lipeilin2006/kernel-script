//! Shared-memory ring layout shared by the driver and every client.
//!
//! The driver creates a page-aligned section object named
//! `\BaseNamedObjects\KernelScriptSection` and maps it into system space;
//! user-mode clients open the same object as `Global\KernelScriptSection`
//! and map it into their own address space. Every field offset, size and
//! state value below is part of the wire contract: bump [`RING_VERSION`]
//! whenever any of them changes.
//!
//! The ring is a single request slot guarded by a four-state machine:
//!
//! ```text
//! IDLE 鈹€鈹€client鈹€鈹€> REQUEST 鈹€鈹€driver鈹€鈹€> PROCESSING 鈹€鈹€driver鈹€鈹€> RESPONSE
//!   ^                  鈹?                   鈹?                    鈹?//!   鈹斺攢鈹€鈹€鈹€client (5 s cancel, only while still REQUEST)鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹?//! ```
//!
//! Both sides publish transitions with sequentially consistent atomic
//! stores so the ordering of the request/response payload relative to the
//! state word is visible across processes.

use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

pub const RING_MAGIC: u32 = 0x4B53_5231;
pub const RING_VERSION: u32 = 3;

/// Bytes reserved for the header. Payload offsets are relative to the start
/// of the mapping, so the request region starts at [`HEADER_SPACE`].
pub const HEADER_SPACE: usize = 4096;
pub const REQUEST_OFFSET: usize = HEADER_SPACE;
pub const REQUEST_SIZE: usize = 512 * 1024;
pub const RESPONSE_OFFSET: usize = REQUEST_OFFSET + REQUEST_SIZE;
/// Postcard-encoded [`crate::protocol::ResponseMeta`], zero padded.
pub const RESPONSE_META_SIZE: usize = 64;
/// Largest response payload: `MAX_BATCH_ENTRIES * MAX_DRIVER_TRANSFER_SIZE`.
pub const RESPONSE_BULK_SIZE: usize = 1024 * 1024;
pub const RESPONSE_SIZE: usize = RESPONSE_META_SIZE + RESPONSE_BULK_SIZE;
pub const RING_TOTAL_SIZE: usize = RESPONSE_OFFSET + RESPONSE_SIZE;

pub const STATE_IDLE: u32 = 0;
pub const STATE_REQUEST: u32 = 1;
pub const STATE_PROCESSING: u32 = 2;
pub const STATE_RESPONSE: u32 = 3;

/// Kernel-side object names (`\BaseNamedObjects` is the kernel namespace
/// root; user mode reaches the same objects through the `Global` prefix).
/// The driver appends `-<16 hex chars>` (random token, drawn per startup)
/// before creating the objects and publishes the full names to
/// `HKLM\SOFTWARE\KernelScript`; these constants are the token-less
/// defaults used for documentation and client-side comparison.
pub const SECTION_KERNEL_NAME: &str = "\\BaseNamedObjects\\KernelScriptSection";
pub const REQUEST_EVENT_KERNEL_NAME: &str = "\\BaseNamedObjects\\KernelScriptRequest";
pub const RESPONSE_EVENT_KERNEL_NAME: &str = "\\BaseNamedObjects\\KernelScriptResponse";

/// Client-side default names for the driver's objects: the fixed-name era
/// fallback ks-link still uses when a registry value is unreadable (which
/// can no longer reach a randomized load) and the baseline the test
/// harness checks the published randomized names against.
pub const SECTION_CLIENT_NAME: &str = "Global\\KernelScriptSection";
pub const REQUEST_EVENT_CLIENT_NAME: &str = "Global\\KernelScriptRequest";
pub const RESPONSE_EVENT_CLIENT_NAME: &str = "Global\\KernelScriptResponse";
/// Serializes competing clients. The driver does not take it; the state
/// machine already prevents the driver from touching a payload it does not
/// own. Created by the first client with default (creator-default) security.
pub const RING_MUTEX_CLIENT_NAME: &str = "Global\\KernelScriptRingMutex";

/// Header fields in wire order. All fields are atomic because both the
/// driver process (kernel system thread) and every user-mode client touch
/// them; the layout is `repr(C)` so offsets are stable:
///
/// | offset | field              |
/// |--------|--------------------|
/// | 0x00   | `magic`            |
/// | 0x04   | `version`          |
/// | 0x08   | `state`            |
/// | 0x0C   | `request_len`      |
/// | 0x10   | `response_len`     |
/// | 0x14   | `status`           |
/// | 0x18   | `sequence`         |
/// | 0x20   | `response_sequence`|
/// | 0x28   | `client_pid`       |
/// | 0x2C   | `generation`       |
#[repr(C)]
pub struct RingHeader {
    magic: AtomicU32,
    version: AtomicU32,
    state: AtomicU32,
    request_len: AtomicU32,
    response_len: AtomicU32,
    status: AtomicI32,
    sequence: AtomicU64,
    response_sequence: AtomicU64,
    client_pid: AtomicU32,
    generation: AtomicU32,
}

const _: () = {
    assert!(core::mem::offset_of!(RingHeader, sequence) == 0x18);
    assert!(core::mem::offset_of!(RingHeader, response_sequence) == 0x20);
    assert!(core::mem::size_of::<RingHeader>() == 0x30);
};

impl RingHeader {
    /// Header size in bytes; the rest of [`HEADER_SPACE`] is padding.
    pub const SIZE: usize = 0x30;

    /// Reset the header for a fresh driver load. Returns the new generation.
    /// Only the driver may call this; clients must never reinitialize a ring
    /// they did not create.
    pub fn init(&self, generation: u32) {
        self.magic.store(RING_MAGIC, Ordering::SeqCst);
        self.version.store(RING_VERSION, Ordering::SeqCst);
        self.state.store(STATE_IDLE, Ordering::SeqCst);
        self.request_len.store(0, Ordering::SeqCst);
        self.response_len.store(0, Ordering::SeqCst);
        self.status.store(0, Ordering::SeqCst);
        self.sequence.store(0, Ordering::SeqCst);
        self.response_sequence.store(0, Ordering::SeqCst);
        self.client_pid.store(0, Ordering::SeqCst);
        self.generation.store(generation, Ordering::SeqCst);
    }

    /// True when magic and version match this build of the protocol.
    pub fn is_compatible(&self) -> bool {
        self.magic.load(Ordering::SeqCst) == RING_MAGIC
            && self.version.load(Ordering::SeqCst) == RING_VERSION
    }

    pub fn state(&self) -> u32 {
        self.state.load(Ordering::SeqCst)
    }

    pub fn store_state(&self, state: u32) {
        self.state.store(state, Ordering::SeqCst);
    }

    /// Compare-and-swap the state word. Returns `true` when the transition
    /// was performed.
    pub fn cas_state(&self, current: u32, new: u32) -> bool {
        self.state
            .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Allocate the next request sequence number. The client bumps this
    /// before publishing `REQUEST`; the driver echoes it as
    /// `response_sequence` so a stale response can never be mistaken for the
    /// answer to the current request.
    pub fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The sequence of the request the driver is answering. The driver
    /// echoes it as [`RingHeader::set_response_sequence`].
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    pub fn response_sequence(&self) -> u64 {
        self.response_sequence.load(Ordering::SeqCst)
    }

    pub fn set_response_sequence(&self, sequence: u64) {
        self.response_sequence.store(sequence, Ordering::SeqCst);
    }

    pub fn request_len(&self) -> u32 {
        self.request_len.load(Ordering::SeqCst)
    }

    pub fn set_request_len(&self, len: u32) {
        self.request_len.store(len, Ordering::SeqCst);
    }

    /// Total bytes written to the response region, always at least
    /// [`RESPONSE_META_SIZE`] once the driver has answered.
    pub fn response_len(&self) -> u32 {
        self.response_len.load(Ordering::SeqCst)
    }

    pub fn set_response_len(&self, len: u32) {
        self.response_len.store(len, Ordering::SeqCst);
    }

    /// NTSTATUS of the last request. `0` (`STATUS_SUCCESS`) on success.
    /// This is the single source of truth for transport-level failure; the
    /// response meta only describes a successful payload.
    pub fn status(&self) -> i32 {
        self.status.load(Ordering::SeqCst)
    }

    pub fn set_status(&self, status: i32) {
        self.status.store(status, Ordering::SeqCst);
    }

    pub fn client_pid(&self) -> u32 {
        self.client_pid.load(Ordering::SeqCst)
    }

    pub fn set_client_pid(&self, pid: u32) {
        self.client_pid.store(pid, Ordering::SeqCst);
    }

    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// Reinterpret a mapped view of at least [`RING_TOTAL_SIZE`] bytes as the
/// ring header, or `None` when `base` is null or misaligned.
///
/// # Safety
///
/// `base` must point at a live mapping of at least [`RING_TOTAL_SIZE`]
/// bytes for the lifetime `'a`, and must not be mutated through any other
/// pointer while `'a` is alive.
pub unsafe fn header<'a>(base: *mut u8) -> Option<&'a RingHeader> {
    if base.is_null() || (base as usize) % core::mem::align_of::<RingHeader>() != 0 {
        return None;
    }
    Some(&*(base as *const RingHeader))
}

/// Mutable view of the request region.
///
/// # Safety
///
/// Same requirements as [`header`]. The caller must guarantee that no other
/// reference into the request region is alive for `'a`.
pub unsafe fn request_bytes<'a>(base: *mut u8) -> &'a mut [u8] {
    core::slice::from_raw_parts_mut(base.add(REQUEST_OFFSET), REQUEST_SIZE)
}

/// Mutable view of the response region (meta slot first, then bulk).
///
/// # Safety
///
/// Same requirements as [`header`]. The caller must guarantee that no other
/// reference into the response region is alive for `'a`.
pub unsafe fn response_bytes<'a>(base: *mut u8) -> &'a mut [u8] {
    core::slice::from_raw_parts_mut(base.add(RESPONSE_OFFSET), RESPONSE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_documented_offsets() {
        assert_eq!(REQUEST_OFFSET, 0x1000);
        assert_eq!(RESPONSE_OFFSET, 0x1000 + 0x80000);
        assert_eq!(RESPONSE_META_SIZE, 64);
        assert_eq!(RESPONSE_BULK_SIZE, 0x10_0000);
        assert_eq!(RING_TOTAL_SIZE, 0x1000 + 0x80000 + 64 + 0x10_0000);
    }

    #[test]
    fn header_state_machine_round_trips() {
        let mut backing = [0u8; 128];
        let head = unsafe { header(backing.as_mut_ptr()) }.unwrap();
        head.init(3);
        assert!(head.is_compatible());
        assert_eq!(head.state(), STATE_IDLE);
        assert_eq!(head.generation(), 3);

        assert!(head.cas_state(STATE_IDLE, STATE_REQUEST));
        assert!(!head.cas_state(STATE_IDLE, STATE_REQUEST));
        head.set_request_len(12);
        head.set_status(-1);
        assert_eq!(head.next_sequence(), 1);
        assert_eq!(head.next_sequence(), 2);
        head.set_response_sequence(2);
        assert_eq!(head.response_sequence(), 2);
        assert_eq!(head.request_len(), 12);
        assert_eq!(head.status(), -1);
    }

    #[test]
    fn null_or_misaligned_header_is_rejected() {
        assert!(unsafe { header(core::ptr::null_mut()) }.is_none());
        let mut backing = [0u8; 128];
        let odd = unsafe { backing.as_mut_ptr().add(1) };
        assert!(unsafe { header(odd) }.is_none());
    }
}
