//! Client-side memory lock API.
//!
//! The table and the rewrite loop live in the driver: every call here is
//! one synchronous ring round trip that mutates the driver-side table
//! (`Request::Lock` / `Request::Unlock` / `Request::UnlockAll`), and a
//! dedicated driver system thread replays the entries with plain writes.
//! Argument validation stays in user mode so bad input fails before any
//! round trip; the driver re-validates everything at its own trust
//! boundary.

use ks_core::protocol::{ProtocolError, Request};
use windows_sys::Win32::Foundation::STATUS_QUOTA_EXCEEDED;

use crate::{submit, LinkError};

/// Slots in the driver-side lock table.
pub const MAX_MEMORY_LOCKS: usize = ks_core::protocol::MAX_MEMORY_LOCKS;
/// Largest payload held by one lock.
pub const MAX_MEMORY_LOCK_SIZE: usize = ks_core::protocol::MAX_MEMORY_LOCK_SIZE;

fn invalid() -> LinkError {
    LinkError::Encode(ProtocolError::InvalidPayload)
}

/// Validation shared by `lock` and `lock_rva`. Id 0, pid 0, empty and
/// oversized payloads are rejected without touching the driver, matching
/// the old local-table errors.
fn validate(id: u64, pid: u64, data: &[u8]) -> Result<(), LinkError> {
    if id == 0 || pid == 0 || data.is_empty() || data.len() > MAX_MEMORY_LOCK_SIZE {
        return Err(invalid());
    }
    Ok(())
}

/// The driver reports a full table with `STATUS_QUOTA_EXCEEDED`; translate
/// it into the historical client-side error so callers keep matching on
/// `TooManyEntries`.
fn map_table_full(error: LinkError) -> LinkError {
    match error {
        LinkError::NtStatus(status) if status == STATUS_QUOTA_EXCEEDED => {
            LinkError::TooManyEntries {
                limit: MAX_MEMORY_LOCKS,
            }
        }
        other => other,
    }
}

fn submit_lock(data: &[u8], request: Request<'_>) -> Result<(), LinkError> {
    // The lock payload can be 4096 bytes, so the request buffer grows with
    // it (same pattern as `write_bytes`).
    let mut buffer = vec![0u8; crate::SMALL_REQUEST_BUFFER + data.len()];
    submit(&request, &mut buffer, &mut [])
        .map(|_| ())
        .map_err(map_table_full)
}

/// Continuously rewrites one locked byte pattern (absolute address).
pub fn lock(id: u64, pid: u64, address: u64, data: &[u8]) -> Result<(), LinkError> {
    validate(id, pid, data)?;
    if address == 0 {
        return Err(invalid());
    }
    submit_lock(
        data,
        Request::Lock {
            id,
            pid,
            address,
            data,
            rva: false,
        },
    )
}

/// Locks a module-relative address; the driver resolves the base once at
/// insert time, so relative offset zero (the image base) is valid.
pub fn lock_rva(id: u64, pid: u64, relative_address: u64, data: &[u8]) -> Result<(), LinkError> {
    validate(id, pid, data)?;
    submit_lock(
        data,
        Request::Lock {
            id,
            pid,
            address: relative_address,
            data,
            rva: true,
        },
    )
}

/// Removes one lock. Unknown ids succeed (idempotent); the round trip only
/// fails when the transport or the driver itself fails.
pub fn unlock(id: u64) -> Result<(), LinkError> {
    if id == 0 {
        return Err(invalid());
    }
    let mut buffer = [0u8; crate::SMALL_REQUEST_BUFFER];
    submit(&Request::Unlock { id }, &mut buffer, &mut [])
        .map(|_| ())
        .map_err(map_table_full)
}

/// Alias of [`unlock`]; RVA locks and absolute locks share one table.
pub fn unlock_rva(id: u64) -> Result<(), LinkError> {
    unlock(id)
}

/// Removes every lock held against `pid`.
pub fn unlock_all(pid: u64) -> Result<(), LinkError> {
    if pid == 0 {
        return Err(invalid());
    }
    let mut buffer = [0u8; crate::SMALL_REQUEST_BUFFER];
    submit(&Request::UnlockAll { pid }, &mut buffer, &mut [])
        .map(|_| ())
        .map_err(map_table_full)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Argument validation must fail before any round trip, so these hold
    /// without a driver. The table itself (replace, pid-scoped clear, the
    /// 64-entry quota and the rewrite loop) is covered end to end by
    /// ks-test against a live driver.
    #[test]
    fn bad_lock_arguments_fail_before_any_round_trip() {
        let oversized = [0u8; MAX_MEMORY_LOCK_SIZE + 1];

        for error in [
            lock(0, 7, 0x1000, b"a"),
            lock(3, 0, 0x1000, b"a"),
            lock(3, 7, 0, b"a"),
            lock(3, 7, 0x1000, b""),
            lock(3, 7, 0x1000, &oversized),
            lock_rva(0, 7, 0x10, b"a"),
            lock_rva(3, 0, 0x10, b"a"),
            lock_rva(3, 7, 0x10, b""),
            lock_rva(3, 7, 0x10, &oversized),
            unlock(0),
            unlock_rva(0),
            unlock_all(0),
        ] {
            assert!(
                matches!(error, Err(LinkError::Encode(ProtocolError::InvalidPayload))),
                "expected local validation failure, got {error:?}"
            );
        }
    }

    #[test]
    fn quota_status_maps_to_too_many_entries() {
        let mapped = map_table_full(LinkError::NtStatus(STATUS_QUOTA_EXCEEDED));
        assert_eq!(
            mapped,
            LinkError::TooManyEntries {
                limit: MAX_MEMORY_LOCKS
            }
        );
        // Other driver failures pass through untouched.
        let other = LinkError::NtStatus(0xC000_0001u32 as i32);
        assert_eq!(map_table_full(other), other);
    }
}
