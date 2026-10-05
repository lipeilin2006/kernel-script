//! Request/response protocol carried by the shared-memory ring.
//!
//! Payloads are [`postcard`] encoded: a `no_std`, `no_alloc`, explicitly
//! little-endian (LEB128 varints + little-endian fixed fields) format whose
//! deserializer borrows directly out of the ring, so the driver never copies
//! a request payload onto the stack. Variants are append-only; adding,
//! removing or reordering fields requires bumping
//! [`crate::ring::RING_VERSION`].
//!
//! The response side is a fixed [`RESPONSE_META_SIZE`]-byte postcard slot
//! followed by a raw bulk region, so the driver never needs a staging
//! buffer: read results are written straight into the bulk area.

use core::fmt;

use heapless::Vec as SmallVec;
use serde::{Deserialize, Serialize};

use crate::ring::RESPONSE_META_SIZE;

/// Largest single memory operation accepted by the driver.
pub const MAX_DRIVER_TRANSFER_SIZE: usize = 4096;
/// Entries in one batch read.
pub const MAX_BATCH_ENTRIES: usize = 256;
/// Entries in one batch write.
pub const MAX_BATCH_WRITE_ENTRIES: usize = 64;
/// Offsets in one pointer-chain walk.
pub const MAX_CHAIN_OFFSETS: usize = 32;
/// Slots in the service-side memory lock table.
pub const MAX_MEMORY_LOCKS: usize = 64;
/// Largest payload held by one memory lock.
pub const MAX_MEMORY_LOCK_SIZE: usize = 4096;
/// Convenience alias used by the Lua layer for write sizes.
pub const MAX_WRITE_SIZE: usize = MAX_DRIVER_TRANSFER_SIZE;

/// One entry of a batch write. `data` borrows straight out of the ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatchWriteItem<'a> {
    pub pid: u64,
    pub address: u64,
    #[serde(borrow)]
    pub data: &'a [u8],
}

/// Every request the driver services.
///
/// `rva` resolves `address` against the target module base and `mdl` routes
/// the operation through the MDL-remap path; the two flags are orthogonal
/// and replace the eight former `IOCTL_*` variants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Request<'a> {
    Ping,
    GetProcessBase {
        pid: u64,
    },
    Read {
        pid: u64,
        address: u64,
        size: u32,
        rva: bool,
        mdl: bool,
    },
    Write {
        pid: u64,
        address: u64,
        #[serde(borrow)]
        data: &'a [u8],
        rva: bool,
        mdl: bool,
    },
    BatchRead {
        pid: u64,
        size: u32,
        addresses: SmallVec<u64, MAX_BATCH_ENTRIES>,
    },
    BatchWrite {
        writes: SmallVec<BatchWriteItem<'a>, MAX_BATCH_WRITE_ENTRIES>,
    },
    TraverseChain {
        pid: u64,
        base: u64,
        offsets: SmallVec<u64, MAX_CHAIN_OFFSETS>,
    },
    /// Wind down the driver's worker thread. The response is published
    /// before the worker exits, so this is an ordinary round trip; every
    /// later request times out until the driver is reloaded. It also stops
    /// the driver-side lock rewrite thread and drops the whole lock table.
    Shutdown,
    /// Install or replace one entry in the driver's lock table. The
    /// driver's dedicated rewrite thread replays the payload with plain
    /// writes until the entry is removed by [`Request::Unlock`] or
    /// [`Request::UnlockAll`]. `rva` resolves `address` against the target
    /// module base once, at insert time.
    Lock {
        id: u64,
        pid: u64,
        address: u64,
        #[serde(borrow)]
        data: &'a [u8],
        rva: bool,
    },
    /// Remove one lock entry. Removing an unknown id succeeds, so unlock
    /// stays idempotent.
    Unlock {
        id: u64,
    },
    /// Remove every lock entry held against `pid`.
    UnlockAll {
        pid: u64,
    },
}

/// Description of a successful response payload. Failures never reach this
/// struct: they are reported through [`crate::ring::RingHeader::status`] with
/// `bulk_len == 0`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseMeta {
    /// Bytes of payload following the meta slot.
    pub bulk_len: u32,
    /// Element count for structured payloads (batch entries). Unused for
    /// plain byte payloads, where it repeats `bulk_len`.
    pub count: u32,
    /// Scalar result: process base or pointer-chain address.
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    BufferTooSmall,
    EncodeFailed,
    DecodeFailed,
    /// The decoded message did not consume the whole input. A request slot
    /// always holds exactly one message, so leftovers mean corruption or a
    /// version mismatch.
    TrailingBytes,
    InvalidPayload,
    TooLarge,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::BufferTooSmall => "buffer too small",
            Self::EncodeFailed => "encode failed",
            Self::DecodeFailed => "decode failed",
            Self::TrailingBytes => "trailing bytes after message",
            Self::InvalidPayload => "invalid payload",
            Self::TooLarge => "payload too large",
        };
        f.write_str(text)
    }
}

fn check_len(len: usize) -> Result<(), ProtocolError> {
    if len == 0 {
        Err(ProtocolError::InvalidPayload)
    } else if len > MAX_DRIVER_TRANSFER_SIZE {
        Err(ProtocolError::TooLarge)
    } else {
        Ok(())
    }
}

fn check_pid(pid: u64) -> Result<(), ProtocolError> {
    if pid == 0 {
        Err(ProtocolError::InvalidPayload)
    } else {
        Ok(())
    }
}

impl<'a> Request<'a> {
    /// Validate everything the driver is allowed to assume about a decoded
    /// request. The driver re-checks the same rules at the memory layer, but
    /// rejecting bad requests here keeps them out of the operation code.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Ping => Ok(()),
            Self::GetProcessBase { pid } => check_pid(*pid),
            Self::Read {
                pid,
                address,
                size,
                rva,
                ..
            } => {
                check_pid(*pid)?;
                if *address == 0 && !*rva {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_len(*size as usize)
            }
            Self::Write {
                pid,
                address,
                data,
                rva,
                ..
            } => {
                check_pid(*pid)?;
                if *address == 0 && !*rva {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_len(data.len())
            }
            Self::BatchRead {
                pid,
                size,
                addresses,
            } => {
                check_pid(*pid)?;
                check_len(*size as usize)?;
                if addresses.is_empty() {
                    return Err(ProtocolError::InvalidPayload);
                }
                let total = (*size as usize)
                    .checked_mul(addresses.len())
                    .ok_or(ProtocolError::TooLarge)?;
                if total > crate::ring::RESPONSE_BULK_SIZE {
                    return Err(ProtocolError::TooLarge);
                }
                Ok(())
            }
            Self::BatchWrite { writes } => {
                if writes.is_empty() {
                    return Err(ProtocolError::InvalidPayload);
                }
                for item in writes {
                    check_pid(item.pid)?;
                    if item.address == 0 {
                        return Err(ProtocolError::InvalidPayload);
                    }
                    check_len(item.data.len())?;
                }
                Ok(())
            }
            Self::TraverseChain { pid, base, offsets } => {
                check_pid(*pid)?;
                if *base == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                if offsets.len() > MAX_CHAIN_OFFSETS {
                    return Err(ProtocolError::TooLarge);
                }
                Ok(())
            }
            Self::Shutdown => Ok(()),
            Self::Lock {
                id,
                pid,
                address,
                data,
                rva,
            } => {
                if *id == 0 {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_pid(*pid)?;
                if *address == 0 && !*rva {
                    return Err(ProtocolError::InvalidPayload);
                }
                check_len(data.len())?;
                if data.len() > MAX_MEMORY_LOCK_SIZE {
                    return Err(ProtocolError::TooLarge);
                }
                Ok(())
            }
            Self::Unlock { id } => {
                if *id == 0 {
                    Err(ProtocolError::InvalidPayload)
                } else {
                    Ok(())
                }
            }
            Self::UnlockAll { pid } => check_pid(*pid),
        }
    }
}

/// Serialize `request` into `out` and return the number of bytes used.
///
/// Encoding fails when validation fails or when the message does not fit,
/// which also bounds every request by `out.len()` (the request region).
pub fn encode_request(request: &Request<'_>, out: &mut [u8]) -> Result<usize, ProtocolError> {
    request.validate()?;
    postcard::to_slice(request, out)
        .map(|used| used.len())
        .map_err(|_| ProtocolError::EncodeFailed)
}

/// Decode exactly one request from the request region.
///
/// `input` is the first `request_len` bytes of the region. Any trailing
/// byte is rejected so a longer stale message can never be reinterpreted as
/// a newer, shorter one.
pub fn decode_request(input: &[u8]) -> Result<Request<'_>, ProtocolError> {
    let (request, rest) =
        postcard::take_from_bytes::<Request<'_>>(input).map_err(|_| ProtocolError::DecodeFailed)?;
    if !rest.is_empty() {
        return Err(ProtocolError::TrailingBytes);
    }
    request.validate()?;
    Ok(request)
}

/// Encode `meta` into the first [`RESPONSE_META_SIZE`] bytes of `out`,
/// zero-filling the rest of the slot so the client always decodes a clean
/// record.
pub fn encode_response_meta(meta: ResponseMeta, out: &mut [u8]) -> Result<(), ProtocolError> {
    if out.len() < RESPONSE_META_SIZE {
        return Err(ProtocolError::BufferTooSmall);
    }
    let (slot, _) = out.split_at_mut(RESPONSE_META_SIZE);
    let used = postcard::to_slice(&meta, slot)
        .map(|used| used.len())
        .map_err(|_| ProtocolError::EncodeFailed)?;
    slot[used..].fill(0);
    Ok(())
}

/// Decode the response meta from the first [`RESPONSE_META_SIZE`] bytes of
/// the response region. Zero padding after the record is ignored.
pub fn decode_response_meta(slot: &[u8]) -> Result<ResponseMeta, ProtocolError> {
    if slot.len() < RESPONSE_META_SIZE {
        return Err(ProtocolError::BufferTooSmall);
    }
    let (meta, _) = postcard::take_from_bytes::<ResponseMeta>(&slot[..RESPONSE_META_SIZE])
        .map_err(|_| ProtocolError::DecodeFailed)?;
    Ok(meta)
}

/// The largest legal batch read must fit the response bulk region exactly
/// or below it, otherwise a valid request could not be answered.
const _: () =
    assert!(MAX_BATCH_ENTRIES * MAX_DRIVER_TRANSFER_SIZE <= crate::ring::RESPONSE_BULK_SIZE);

/// A lock payload travels in one request and is re-checked with the same
/// single-transfer rule in the driver, so the two limits must not drift.
const _: () = assert!(MAX_MEMORY_LOCK_SIZE <= MAX_DRIVER_TRANSFER_SIZE);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::RESPONSE_META_SIZE;

    fn round_trip(request: &Request<'_>) -> usize {
        let mut buffer = [0u8; 8192];
        let len = encode_request(request, &mut buffer).expect("encode");
        let decoded = decode_request(&buffer[..len]).expect("decode");
        assert_eq!(&decoded, request);
        len
    }

    #[test]
    fn every_variant_round_trips() {
        round_trip(&Request::Ping);
        round_trip(&Request::Shutdown);
        round_trip(&Request::GetProcessBase { pid: 4242 });
        round_trip(&Request::Lock {
            id: 3,
            pid: 7,
            address: 0x7FF6_0000_1234,
            data: b"locked",
            rva: false,
        });
        // The largest lock payload still fits the request region.
        round_trip(&Request::Lock {
            id: 4,
            pid: 7,
            address: 0x1234,
            data: &[0xABu8; MAX_MEMORY_LOCK_SIZE],
            rva: true,
        });
        round_trip(&Request::Unlock { id: 9 });
        round_trip(&Request::UnlockAll { pid: 9 });
        for mdl in [false, true] {
            for rva in [false, true] {
                round_trip(&Request::Read {
                    pid: 7,
                    address: 0x7FF6_0000_1234,
                    size: 4096,
                    rva,
                    mdl,
                });
                round_trip(&Request::Write {
                    pid: 7,
                    address: 0x7FF6_0000_1234,
                    data: b"payload",
                    rva,
                    mdl,
                });
            }
        }
        let mut chain = SmallVec::<u64, MAX_CHAIN_OFFSETS>::new();
        for offset in [0x10u64, 0x20, 0x30] {
            chain.push(offset).unwrap();
        }
        round_trip(&Request::TraverseChain {
            pid: 9,
            base: 0x1A2B_0000,
            offsets: chain,
        });
    }

    #[test]
    fn batch_write_round_trips_at_every_supported_count() {
        for count in [1usize, 2, 7, MAX_BATCH_WRITE_ENTRIES] {
            let mut writes = SmallVec::<BatchWriteItem<'_>, MAX_BATCH_WRITE_ENTRIES>::new();
            for index in 0..count {
                writes
                    .push(BatchWriteItem {
                        pid: (index as u64 % 3) + 1,
                        address: 0x1000 + index as u64 * 4,
                        data: &[0xABu8; 4],
                    })
                    .unwrap();
            }
            let request = Request::BatchWrite { writes };
            let len = round_trip(&request);
            assert!(len < crate::ring::REQUEST_SIZE);
        }
    }

    #[test]
    fn batch_read_round_trips_at_max_entry_count() {
        let mut addresses = SmallVec::<u64, MAX_BATCH_ENTRIES>::new();
        for index in 0..MAX_BATCH_ENTRIES {
            addresses
                .push(0x7FFF_0000_0000 + index as u64 * 4096)
                .unwrap();
        }
        let request = Request::BatchRead {
            pid: 31,
            size: 4096,
            addresses,
        };
        let len = round_trip(&request);
        assert!(len < crate::ring::REQUEST_SIZE);
    }

    #[test]
    fn truncated_request_is_rejected() {
        let request = Request::Write {
            pid: 5,
            address: 0x1000,
            data: &[1, 2, 3, 4],
            rva: false,
            mdl: false,
        };
        let mut buffer = [0u8; 256];
        let len = encode_request(&request, &mut buffer).unwrap();
        for cut in 0..len {
            assert!(
                decode_request(&buffer[..cut]).is_err(),
                "truncation at {cut} of {len} must fail"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut buffer = [0u8; 64];
        let len = encode_request(&Request::Ping, &mut buffer).unwrap();
        let mut extended = [0u8; 65];
        extended[..len].copy_from_slice(&buffer[..len]);
        assert_eq!(
            decode_request(&extended[..len + 1]),
            Err(ProtocolError::TrailingBytes)
        );
    }

    #[test]
    fn validation_rejects_out_of_range_requests() {
        let mut buffer = [0u8; 8192];

        let read = Request::Read {
            pid: 1,
            address: 0x1000,
            size: (MAX_DRIVER_TRANSFER_SIZE + 1) as u32,
            rva: false,
            mdl: false,
        };
        assert_eq!(
            encode_request(&read, &mut buffer),
            Err(ProtocolError::TooLarge)
        );

        let zero_pid = Request::GetProcessBase { pid: 0 };
        assert_eq!(
            encode_request(&zero_pid, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let empty_batch = Request::BatchRead {
            pid: 1,
            size: 4,
            addresses: SmallVec::new(),
        };
        assert_eq!(
            encode_request(&empty_batch, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let empty_writes = Request::BatchWrite {
            writes: SmallVec::new(),
        };
        assert_eq!(
            encode_request(&empty_writes, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let mut writes = SmallVec::<BatchWriteItem<'_>, MAX_BATCH_WRITE_ENTRIES>::new();
        writes
            .push(BatchWriteItem {
                pid: 1,
                address: 0,
                data: b"x",
            })
            .unwrap();
        let bad_address = Request::BatchWrite { writes };
        assert_eq!(
            encode_request(&bad_address, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let base_zero = Request::TraverseChain {
            pid: 1,
            base: 0,
            offsets: SmallVec::new(),
        };
        assert_eq!(
            encode_request(&base_zero, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let zero_size = Request::Read {
            pid: 1,
            address: 0x1000,
            size: 0,
            rva: false,
            mdl: false,
        };
        assert_eq!(
            encode_request(&zero_size, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let lock_zero_id = Request::Lock {
            id: 0,
            pid: 1,
            address: 0x1000,
            data: b"x",
            rva: false,
        };
        assert_eq!(
            encode_request(&lock_zero_id, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let lock_zero_pid = Request::Lock {
            id: 1,
            pid: 0,
            address: 0x1000,
            data: b"x",
            rva: false,
        };
        assert_eq!(
            encode_request(&lock_zero_pid, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let lock_zero_address = Request::Lock {
            id: 1,
            pid: 1,
            address: 0,
            data: b"x",
            rva: false,
        };
        assert_eq!(
            encode_request(&lock_zero_address, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let lock_empty = Request::Lock {
            id: 1,
            pid: 1,
            address: 0x1000,
            data: b"",
            rva: false,
        };
        assert_eq!(
            encode_request(&lock_empty, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let lock_oversized = Request::Lock {
            id: 1,
            pid: 1,
            address: 0x1000,
            data: &[0u8; MAX_MEMORY_LOCK_SIZE + 1],
            rva: false,
        };
        assert_eq!(
            encode_request(&lock_oversized, &mut buffer),
            Err(ProtocolError::TooLarge)
        );

        // An RVA lock may address offset zero: it resolves against the
        // module base before it ever reaches the table.
        let lock_rva_zero = Request::Lock {
            id: 1,
            pid: 1,
            address: 0,
            data: b"MZ",
            rva: true,
        };
        let len = encode_request(&lock_rva_zero, &mut buffer).expect("rva lock encodes");
        assert_eq!(
            decode_request(&buffer[..len]).expect("decode"),
            lock_rva_zero
        );

        let unlock_zero = Request::Unlock { id: 0 };
        assert_eq!(
            encode_request(&unlock_zero, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let unlock_all_zero = Request::UnlockAll { pid: 0 };
        assert_eq!(
            encode_request(&unlock_all_zero, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );
    }

    #[test]
    fn zero_address_is_rejected_unless_rva() {
        let mut buffer = [0u8; 256];

        let read_rva = Request::Read {
            pid: 1,
            address: 0,
            size: 2,
            rva: true,
            mdl: false,
        };
        let len = encode_request(&read_rva, &mut buffer).expect("rva zero address encodes");
        assert_eq!(decode_request(&buffer[..len]).expect("decode"), read_rva);

        let read_abs = Request::Read {
            pid: 1,
            address: 0,
            size: 2,
            rva: false,
            mdl: false,
        };
        assert_eq!(
            encode_request(&read_abs, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );

        let write_rva = Request::Write {
            pid: 1,
            address: 0,
            data: b"MZ",
            rva: true,
            mdl: false,
        };
        encode_request(&write_rva, &mut buffer).expect("rva zero-address write encodes");

        let write_abs = Request::Write {
            pid: 1,
            address: 0,
            data: b"MZ",
            rva: false,
            mdl: false,
        };
        assert_eq!(
            encode_request(&write_abs, &mut buffer),
            Err(ProtocolError::InvalidPayload)
        );
    }

    #[test]
    fn response_meta_round_trips_through_zero_padding() {
        let meta = ResponseMeta {
            bulk_len: 4096,
            count: 1,
            value: 0x1234_5678_9ABC_DEF0,
        };
        let mut slot = [0u8; RESPONSE_META_SIZE];
        slot.fill(0xFF);
        encode_response_meta(meta, &mut slot).unwrap();
        assert_eq!(decode_response_meta(&slot), Ok(meta));

        // A meta encoded into a larger buffer still decodes from its slot.
        let mut region = [0u8; RESPONSE_META_SIZE + 16];
        encode_response_meta(ResponseMeta::default(), &mut region).unwrap();
        assert_eq!(
            decode_response_meta(&region[..RESPONSE_META_SIZE]),
            Ok(ResponseMeta::default())
        );
    }

    #[test]
    fn response_meta_rejects_short_slots() {
        let mut short = [0u8; RESPONSE_META_SIZE - 1];
        assert_eq!(
            encode_response_meta(ResponseMeta::default(), &mut short),
            Err(ProtocolError::BufferTooSmall)
        );
        assert_eq!(
            decode_response_meta(&short),
            Err(ProtocolError::BufferTooSmall)
        );
    }
}
