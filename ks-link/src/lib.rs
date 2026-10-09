//! User-mode client for the KernelScript driver's shared-memory ring.
//!
//! A `session::Session` opens the section, the two events and the client
//! mutex the driver publishes in `\BaseNamedObjects`, maps the section into
//! the process, and serializes every request through its round trip. The
//! module-level free functions are the stable surface consumed by the GUI
//! and the test harness: each one connects on demand, submits one request
//! and copies the answer out of the ring before returning.

mod error;
mod lock;
mod names;
mod process;
mod session;

use heapless::Vec as SmallVec;
use ks_core::protocol::{
    BatchWriteItem, Request, MAX_BATCH_ENTRIES, MAX_BATCH_WRITE_ENTRIES, MAX_CHAIN_OFFSETS,
};

pub use error::LinkError;
pub use lock::{lock, lock_rva, unlock, unlock_all, unlock_rva};
pub use lock::{MAX_MEMORY_LOCKS, MAX_MEMORY_LOCK_SIZE};
pub use names::{instance_claim_present, published_object_names, published_object_names_strict};
pub use process::find_pid;
pub use session::close_session;

use session::submit;

/// Stack headroom for the largest fixed-size request: a batch read with
/// [`MAX_BATCH_ENTRIES`] addresses.
const SMALL_REQUEST_BUFFER: usize = 4096;

pub fn ping() -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Ping, &mut buffer, &mut []).map(|_| ())
}

/// Asks the driver's worker thread to stop serving after it publishes this
/// response. Every later request times out until the driver is reloaded;
/// stopping the service afterwards takes the usual unload path (the worker
/// has already exited, so `DriverUnload` just releases the objects).
pub fn shutdown() -> Result<(), LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    submit(&Request::Shutdown, &mut buffer, &mut []).map(|_| ())
}

pub fn get_process_base(pid: u64) -> Result<u64, LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, _) = submit(&Request::GetProcessBase { pid }, &mut buffer, &mut [])?;
    Ok(meta.value)
}

pub fn read_bytes(
    pid: u64,
    address: u64,
    size: usize,
    rva: bool,
    mdl: bool,
) -> Result<Vec<u8>, LinkError> {
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let mut out = vec![0u8; size];
    let (_, copied) = submit(
        &Request::Read {
            pid,
            address,
            size: size as u32,
            rva,
            mdl,
        },
        &mut buffer,
        &mut out,
    )?;
    out.truncate(copied);
    Ok(out)
}

pub fn write_bytes(
    pid: u64,
    address: u64,
    data: &[u8],
    rva: bool,
    mdl: bool,
) -> Result<(), LinkError> {
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + data.len()];
    submit(
        &Request::Write {
            pid,
            address,
            data,
            rva,
            mdl,
        },
        &mut buffer,
        &mut [],
    )?;
    Ok(())
}

pub fn batch_read(pid: u64, size: u32, addresses: &[u64]) -> Result<Vec<u8>, LinkError> {
    let addresses = addresses_small_vec(addresses, MAX_BATCH_ENTRIES)?;
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let mut out = vec![0u8; addresses.len() * size as usize];
    let (_, copied) = submit(
        &Request::BatchRead {
            pid,
            size,
            addresses,
        },
        &mut buffer,
        &mut out,
    )?;
    out.truncate(copied);
    Ok(out)
}

/// One NTSTATUS per entry, in request order.
pub fn batch_write(pid: u64, entries: &[(u64, Vec<u8>)]) -> Result<Vec<i32>, LinkError> {
    if entries.len() > MAX_BATCH_WRITE_ENTRIES {
        return Err(LinkError::TooManyEntries {
            limit: MAX_BATCH_WRITE_ENTRIES,
        });
    }
    let mut writes = SmallVec::<BatchWriteItem<'_>, MAX_BATCH_WRITE_ENTRIES>::new();
    for (address, data) in entries {
        writes
            .push(BatchWriteItem {
                pid,
                address: *address,
                data,
            })
            .map_err(|_| LinkError::TooManyEntries {
                limit: MAX_BATCH_WRITE_ENTRIES,
            })?;
    }
    let data_len: usize = entries.iter().map(|(_, data)| data.len()).sum();
    let mut buffer = vec![0u8; SMALL_REQUEST_BUFFER + data_len];
    let mut out = [0u8; MAX_BATCH_WRITE_ENTRIES * 4];
    let (_, copied) = submit(&Request::BatchWrite { writes }, &mut buffer, &mut out)?;
    Ok(out[..copied]
        .chunks_exact(4)
        .map(|status| i32::from_le_bytes(status.try_into().unwrap()))
        .collect())
}

pub fn traverse_pointer_chain(pid: u64, base: u64, offsets: &[u64]) -> Result<u64, LinkError> {
    let offsets = addresses_small_vec(offsets, MAX_CHAIN_OFFSETS)?;
    let mut buffer = [0u8; SMALL_REQUEST_BUFFER];
    let (meta, _) = submit(
        &Request::TraverseChain { pid, base, offsets },
        &mut buffer,
        &mut [],
    )?;
    Ok(meta.value)
}

fn addresses_small_vec<const N: usize>(
    items: &[u64],
    limit: usize,
) -> Result<SmallVec<u64, N>, LinkError> {
    if items.len() > limit {
        return Err(LinkError::TooManyEntries { limit });
    }
    let mut vec = SmallVec::new();
    for item in items {
        vec.push(*item)
            .map_err(|_| LinkError::TooManyEntries { limit })?;
    }
    Ok(vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_vectors_reject_overflows() {
        assert_eq!(
            addresses_small_vec::<1>(&[1, 2], 1),
            Err(LinkError::TooManyEntries { limit: 1 })
        );
        assert!(addresses_small_vec::<4>(&[1, 2], 4).is_ok());
    }
}
