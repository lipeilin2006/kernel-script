#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod memory;
pub mod protocol;

pub mod crypto {
    pub const KEY_LEN: usize = 32;
    pub const FALLBACK_KEY: [u8; KEY_LEN] = *b"KS-XOR-obfuscation-key-2026!!!!!";

    pub const PID: usize = 0;
    pub const ADDRESS: usize = 1;
    pub const RVA: usize = 2;
    pub const BASE: usize = 3;
    pub const OFFSET: usize = 4;
    pub const RESULT: usize = 5;

    #[inline]
    pub fn xor_u64(value: u64, key: &[u8; KEY_LEN], field: usize) -> u64 {
        let mut bytes = value.to_le_bytes();
        let start = (field * 8) % KEY_LEN;
        let mut index = 0;
        while index < bytes.len() {
            bytes[index] ^= key[(start + index) % KEY_LEN];
            index += 1;
        }
        u64::from_le_bytes(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn xor_is_reversible() {
            let value = 0x1234_5678_9abc_def0;
            assert_eq!(
                xor_u64(xor_u64(value, &FALLBACK_KEY, 7), &FALLBACK_KEY, 7),
                value
            );
        }
    }
}

#[cfg(feature = "alloc")]
pub use protocol::FrameDecoder;
pub use protocol::{Frame, MessageType, ProtocolError, WireDecode, WireEncode};
