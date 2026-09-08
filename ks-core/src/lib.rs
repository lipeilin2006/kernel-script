#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod memory;
pub mod protocol;

#[cfg(feature = "alloc")]
pub use protocol::FrameDecoder;
pub use protocol::{Frame, MessageType, ProtocolError, WireDecode, WireEncode};
