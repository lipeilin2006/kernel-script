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
