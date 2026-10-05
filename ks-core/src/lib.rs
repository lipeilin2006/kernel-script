#![cfg_attr(not(test), no_std)]

pub mod protocol;
pub mod ring;

pub use protocol::{
    decode_request, decode_response_meta, encode_request, encode_response_meta, BatchWriteItem,
    ProtocolError, Request, ResponseMeta, MAX_BATCH_ENTRIES, MAX_BATCH_WRITE_ENTRIES,
    MAX_CHAIN_OFFSETS, MAX_DRIVER_TRANSFER_SIZE, MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE,
    MAX_WRITE_SIZE,
};
pub use ring::{
    header, request_bytes, response_bytes, RingHeader, REQUEST_EVENT_CLIENT_NAME,
    REQUEST_EVENT_KERNEL_NAME, REQUEST_SIZE, RESPONSE_BULK_SIZE, RESPONSE_EVENT_CLIENT_NAME,
    RESPONSE_EVENT_KERNEL_NAME, RESPONSE_META_SIZE, RESPONSE_SIZE, RING_MAGIC,
    RING_MUTEX_CLIENT_NAME, RING_TOTAL_SIZE, RING_VERSION, SECTION_CLIENT_NAME,
    SECTION_KERNEL_NAME, STATE_IDLE, STATE_PROCESSING, STATE_REQUEST, STATE_RESPONSE,
};
