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
    round_trip(&Request::Alloc {
        pid: 9,
        size: 4096,
        protect: ALLOC_PROTECT_READWRITE,
    });
    round_trip(&Request::Alloc {
        pid: 9,
        size: MAX_ALLOC_SIZE as u32,
        protect: ALLOC_PROTECT_EXECUTE_READWRITE,
    });
    round_trip(&Request::Free {
        pid: 9,
        address: 0x1A2B_0000,
    });
    round_trip(&Request::LockList);
    round_trip(&Request::Suspend { pid: 1234 });
    round_trip(&Request::Resume { pid: 1234 });
    round_trip(&Request::Hook {
        op: HookOp::Create {
            pid: 0,
            target: 0xFFFF_F800_1234_5678,
            trampoline: 0xFFFF_F800_9ABC_DEF0,
        },
    });
    round_trip(&Request::Hook {
        op: HookOp::Enable { pid: 0, id: 3 },
    });
    round_trip(&Request::Hook {
        op: HookOp::Disable { pid: 0, id: 3 },
    });
    round_trip(&Request::Hook {
        op: HookOp::Remove { pid: 0, id: 3 },
    });
    round_trip(&Request::Hook {
        op: HookOp::EnableAll,
    });
    round_trip(&Request::Hook {
        op: HookOp::DisableAll,
    });
    round_trip(&Request::Hook {
        op: HookOp::RemoveAll,
    });
    round_trip(&Request::Hook {
        op: HookOp::SelfTest,
    });
    round_trip(&Request::Protect {
        pid: 9,
        address: 0x0000_7FF6_1234_5000,
        size: 0x2000,
        protect: PROT_RX,
        cfg_no_update: false,
    });
    round_trip(&Request::Protect {
        pid: 9,
        address: 0x0000_7FF6_1234_5000,
        size: MAX_PROTECT_SIZE as u32,
        protect: PROT_RWX,
        cfg_no_update: true,
    });
    round_trip(&Request::QueryRegion {
        pid: 9,
        address: 0x0000_7FF6_1234_5000,
    });
}

#[test]
fn protect_and_query_region_validation() {
    let mut buffer = [0u8; 4096];
    let mut protect = |pid, address, size, protect| {
        encode_request(
            &Request::Protect {
                pid,
                address,
                size,
                protect,
                cfg_no_update: false,
            },
            &mut buffer,
        )
    };
    // The happy path: every whitelisted protection, any non-zero span.
    for protection in [PROT_READ, PROT_RW, PROT_RX, PROT_RWX] {
        assert!(protect(7, 0x1000, 0x1000, protection).is_ok());
    }
    // Guard pages and PAGE_NOACCESS are never handed out: the target
    // would fault on its own memory.
    for protection in [0x01u32, 0x100, 0x08 | 0x200, 0x80] {
        assert_eq!(
            protect(7, 0x1000, 0x1000, protection),
            Err(ProtocolError::InvalidPayload),
            "protection {protection:#x} must be refused"
        );
    }
    assert_eq!(
        protect(7, 0x1000, 0, PROT_RW),
        Err(ProtocolError::InvalidPayload)
    );
    assert_eq!(
        protect(7, 0, 0x1000, PROT_RW),
        Err(ProtocolError::InvalidPayload)
    );
    assert_eq!(
        protect(0, 0x1000, 0x1000, PROT_RW),
        Err(ProtocolError::InvalidPayload)
    );
    assert_eq!(
        protect(7, 0x1000, MAX_PROTECT_SIZE as u32 + 1, PROT_RW),
        Err(ProtocolError::TooLarge)
    );
    // The exact cap still fits.
    assert!(protect(7, 0x1000, MAX_PROTECT_SIZE as u32, PROT_RW).is_ok());

    let mut query =
        |pid, address| encode_request(&Request::QueryRegion { pid, address }, &mut buffer);
    assert!(query(7, 0x1000).is_ok());
    assert_eq!(query(7, 0), Err(ProtocolError::InvalidPayload));
    assert_eq!(query(0, 0x1000), Err(ProtocolError::InvalidPayload));
}

#[test]
fn region_info_round_trips() {
    let info = RegionInfo {
        base: 0x0000_7FF6_1234_5000,
        allocation_base: 0x0000_7FF6_1200_0000,
        region_size: 0x2_0000,
        state: MEM_COMMIT,
        protect: PROT_RX,
        allocation_protect: PROT_READ,
        typ: MEM_IMAGE,
    };
    let encoded = info.encode();
    assert_eq!(encoded.len(), REGION_INFO_SIZE);
    assert_eq!(RegionInfo::decode(&encoded), Some(info));
    // The wire order is fixed: base, allocation base, size, then the four
    // u32 fields.
    assert_eq!(&encoded[0..8], &info.base.to_le_bytes());
    assert_eq!(&encoded[36..40], &info.typ.to_le_bytes());
    // A truncated answer must not be decoded by guessing the tail.
    assert_eq!(RegionInfo::decode(&encoded[..REGION_INFO_SIZE - 1]), None);
}

#[test]
fn hook_validation_checks_pid_and_target() {
    let mut buffer = [0u8; 8192];
    // pid=0 requires kernel addresses.
    for (target, trampoline) in [
        (0x1000_u64, 0xFFFF_F800_0000_1000_u64),
        (0xFFFF_F800_0000_1000_u64, 0x2000_u64),
        (0, 0),
    ] {
        assert_eq!(
            encode_request(
                &Request::Hook {
                    op: HookOp::Create {
                        pid: 0,
                        target,
                        trampoline
                    }
                },
                &mut buffer
            ),
            Err(ProtocolError::InvalidPayload)
        );
    }
    // A user pid requires user addresses.
    assert!(encode_request(
        &Request::Hook {
            op: HookOp::Create {
                pid: 0x1234,
                target: 0x0000_0000_0040_1000,
                trampoline: 0x0000_0000_0040_2000,
            }
        },
        &mut buffer
    )
    .is_ok());
    assert_eq!(
        encode_request(
            &Request::Hook {
                op: HookOp::Create {
                    pid: 0x1234,
                    target: 0xFFFF_F800_0000_1000,
                    trampoline: 0x0000_0000_0040_2000,
                }
            },
            &mut buffer
        ),
        Err(ProtocolError::InvalidPayload)
    );
    assert!(encode_request(
        &Request::Hook {
            op: HookOp::Create {
                pid: 0,
                target: 0xFFFF_F800_0000_1000,
                trampoline: 0xFFFF_F800_0000_2000,
            }
        },
        &mut buffer
    )
    .is_ok());
    assert_eq!(
        encode_request(
            &Request::Hook {
                op: HookOp::Enable { pid: 0, id: 0 }
            },
            &mut buffer
        ),
        Err(ProtocolError::InvalidPayload)
    );
    assert!(is_kernel_address(0xFFFF_8000_0000_0000));
    assert!(!is_kernel_address(0x7FFF_FFFF_FFFF));
    assert!(check_hook_address(0, 0xFFFF_F800_0000_1000));
    assert!(!check_hook_address(0, 0x1000));
    assert!(check_hook_address(0x1234, 0x0000_0000_0040_1000));
    assert!(!check_hook_address(0x1234, 0xFFFF_F800_0000_1000));
}

#[test]
fn suspend_resume_validation() {
    let mut buffer = [0u8; 4096];
    for pid in [0u64, 4] {
        assert_eq!(
            encode_request(&Request::Suspend { pid }, &mut buffer),
            Err(ProtocolError::InvalidPayload),
            "pid {pid} must be refused"
        );
        assert_eq!(
            encode_request(&Request::Resume { pid }, &mut buffer),
            Err(ProtocolError::InvalidPayload),
            "pid {pid} must be refused"
        );
    }
    assert!(encode_request(&Request::Suspend { pid: 1234 }, &mut buffer).is_ok());
    assert!(encode_request(&Request::Resume { pid: 1234 }, &mut buffer).is_ok());
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
fn alloc_and_free_validation() {
    let mut buffer = [0u8; 8192];
    let zero_size = Request::Alloc {
        pid: 1,
        size: 0,
        protect: ALLOC_PROTECT_READWRITE,
    };
    assert_eq!(
        encode_request(&zero_size, &mut buffer),
        Err(ProtocolError::InvalidPayload)
    );
    let oversized = Request::Alloc {
        pid: 1,
        size: (MAX_ALLOC_SIZE + 1) as u32,
        protect: ALLOC_PROTECT_READWRITE,
    };
    assert_eq!(
        encode_request(&oversized, &mut buffer),
        Err(ProtocolError::TooLarge)
    );
    let bad_protect = Request::Alloc {
        pid: 1,
        size: 4096,
        protect: 0x80,
    };
    assert_eq!(
        encode_request(&bad_protect, &mut buffer),
        Err(ProtocolError::InvalidPayload)
    );
    let zero_pid = Request::Alloc {
        pid: 0,
        size: 4096,
        protect: ALLOC_PROTECT_READWRITE,
    };
    assert_eq!(
        encode_request(&zero_pid, &mut buffer),
        Err(ProtocolError::InvalidPayload)
    );
    let zero_address = Request::Free { pid: 1, address: 0 };
    assert_eq!(
        encode_request(&zero_address, &mut buffer),
        Err(ProtocolError::InvalidPayload)
    );
    assert!(valid_alloc_protect(ALLOC_PROTECT_READWRITE));
    assert!(valid_alloc_protect(ALLOC_PROTECT_EXECUTE_READWRITE));
    // The alloc whitelist is three of the four protect values: PROT_READ
    // stays valid for `protect` but is refused here (a read-only
    // allocation cannot be written through the plain write path), while
    // PROT_RX saves a later protect round trip.
    for protection in [PROT_RW, PROT_RX, PROT_RWX] {
        assert!(
            valid_alloc_protect(protection),
            "{protection:#x} must be allocable"
        );
    }
    assert!(!valid_alloc_protect(PROT_READ));
    assert!(valid_protect(PROT_READ));
    assert!(!valid_alloc_protect(0x01)); // PAGE_NOACCESS
    assert!(!valid_alloc_protect(0x100)); // PAGE_GUARD
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
