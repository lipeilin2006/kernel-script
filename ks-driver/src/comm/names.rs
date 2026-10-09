//! Randomized ring object names: NUL-terminated UTF-16 buffers and the
//! per-load token shared by all three names.

use super::{OBJECT_NAME_CAPACITY, REQUEST_NAME_PREFIX, RESPONSE_NAME_PREFIX, SECTION_NAME_PREFIX};
use crate::wdm::*;

/// A NUL-terminated UTF-16 object name kept alive for the `UNICODE_STRING`
/// view handed to the object manager.
pub(super) struct ObjectName {
    buffer: [u16; OBJECT_NAME_CAPACITY],
    len: usize,
}

impl ObjectName {
    pub(super) fn new(name: &str) -> Self {
        let mut buffer = [0u16; OBJECT_NAME_CAPACITY];
        let mut len = 0;
        for unit in name.encode_utf16() {
            if len >= OBJECT_NAME_CAPACITY - 1 {
                break;
            }
            buffer[len] = unit;
            len += 1;
        }
        Self { buffer, len }
    }

    pub(super) fn as_unistring(&self) -> UNICODE_STRING {
        UNICODE_STRING {
            Length: (self.len * 2) as u16,
            MaximumLength: ((self.len + 1) * 2) as u16,
            Buffer: self.buffer.as_ptr() as *mut u16,
        }
    }

    /// A name built from a fixed `prefix` plus the shared random token —
    /// the randomized counterpart of [`ObjectName::new`] for ring objects.
    fn with_token(prefix: &str, token: &[u32; 2]) -> Self {
        let mut buffer = [0u16; OBJECT_NAME_CAPACITY];
        let mut len = 0;
        for unit in prefix.encode_utf16() {
            if len >= OBJECT_NAME_CAPACITY - 1 {
                break;
            }
            buffer[len] = unit;
            len += 1;
        }
        push_token(&mut buffer, &mut len, token);
        Self { buffer, len }
    }
}

/// Appends the token as 16 lowercase hex UTF-16 units (two `u32`
/// draws), stopping at the buffer bound (never reached: prefix + token
/// stays well under [`OBJECT_NAME_CAPACITY`](super::OBJECT_NAME_CAPACITY)).
pub(super) fn push_token(
    buffer: &mut [u16; OBJECT_NAME_CAPACITY],
    len: &mut usize,
    token: &[u32; 2],
) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for value in token {
        for shift in (0..32).step_by(4).rev() {
            let digit = ((value >> shift) & 0xF) as usize;
            if *len >= OBJECT_NAME_CAPACITY - 1 {
                return;
            }
            buffer[*len] = HEX[digit] as u16;
            *len += 1;
        }
    }
}

/// The three startup-randomized ring object names: one token shared by all
/// of them, so a client learns the complete set from the single registry
/// publication. Kept on `start`'s stack for `create_ring` and
/// `publish_object_names`; only the created handles outlive the call.
pub(super) struct RingNames {
    pub(super) section: ObjectName,
    pub(super) request: ObjectName,
    pub(super) response: ObjectName,
    pub(super) token: [u32; 2],
}

impl RingNames {
    /// Fresh token for this load: interrupt time and a stack address seed
    /// the kernel PRNG, then two draws make 64 bits of name entropy. The
    /// single-instance guard ensures only one driver generates at a time.
    pub(super) fn generate() -> Self {
        let mut last: u64 = 0;
        // SAFETY: out parameter is a valid local; no aliasing.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut last) };
        let entropy = now ^ (now >> 32) ^ (&now as *const u64 as u64);
        // SAFETY: seed is a valid local; `RtlRandomEx` only reads and
        // rewrites it. The `| 1` keeps the classic zero-seed degeneracy out.
        let mut seed = entropy as u32 | 1;
        // SAFETY: same local seed, sequential draws.
        let token = unsafe { [RtlRandomEx(&mut seed), RtlRandomEx(&mut seed)] };
        Self {
            section: ObjectName::with_token(SECTION_NAME_PREFIX, &token),
            request: ObjectName::with_token(REQUEST_NAME_PREFIX, &token),
            response: ObjectName::with_token(RESPONSE_NAME_PREFIX, &token),
            token,
        }
    }
}
