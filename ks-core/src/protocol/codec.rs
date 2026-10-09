use super::{ProtocolError, Request, ResponseMeta};
use crate::ring::RESPONSE_META_SIZE;

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
