//! Synchronous memory operations for the GUI Lua thread, backed by the
//! shared-memory link re-exported from [`ks_sdk`].
//!
//! Every call performs exactly one ring round trip and blocks the calling
//! thread for its duration (~60-100 us); the public signatures are the Lua
//! API contract and must not grow blocking alternatives.
//!
//! [`gate`] additionally refuses every call while a driver lifecycle job
//! (probe/start) is in flight: that job may close and replace the
//! process-wide session, and the check runs on the same GUI Lua thread
//! that issues the round trips — so no round trip can be in flight when
//! the session goes away (see `ks-gui/src/driver.rs`).

fn reason<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

/// Refuses the call while a driver lifecycle job owns the session.
fn gate() -> Result<(), String> {
    if crate::driver::lifecycle_busy() {
        Err("the driver is starting".to_string())
    } else {
        Ok(())
    }
}

pub fn get_pid(name: &str) -> Result<u64, String> {
    gate()?;
    ks_sdk::find_pid(name).map_err(reason)
}

pub fn get_process_base(pid: u64) -> Result<u64, String> {
    gate()?;
    ks_sdk::get_process_base(pid).map_err(reason)
}

pub fn read_i32(pid: u64, address: u64) -> Result<i32, String> {
    gate()?;
    let data = ks_sdk::read_bytes(pid, address, 4, false, false).map_err(reason)?;
    if data.len() >= 4 {
        Ok(i32::from_le_bytes(data[..4].try_into().unwrap()))
    } else {
        Err("short read".into())
    }
}

pub fn read_bytes(pid: u64, address: u64, size: u64) -> Result<Vec<u8>, String> {
    gate()?;
    ks_sdk::read_bytes(pid, address, size as usize, false, false).map_err(reason)
}

pub fn write_i32(pid: u64, address: u64, value: i32) -> Result<(), String> {
    gate()?;
    ks_sdk::write_bytes(pid, address, &value.to_le_bytes(), false, false).map_err(reason)
}

pub fn write_bytes(pid: u64, address: u64, data: &[u8]) -> Result<(), String> {
    gate()?;
    ks_sdk::write_bytes(pid, address, data, false, false).map_err(reason)
}

pub fn read_rva(pid: u64, relative_address: u64, size: u64) -> Result<Vec<u8>, String> {
    gate()?;
    ks_sdk::read_bytes(pid, relative_address, size as usize, true, false).map_err(reason)
}

pub fn write_rva(pid: u64, relative_address: u64, data: &[u8]) -> Result<(), String> {
    gate()?;
    ks_sdk::write_bytes(pid, relative_address, data, true, false).map_err(reason)
}

pub fn read_mdl(pid: u64, address: u64, size: u64) -> Result<Vec<u8>, String> {
    gate()?;
    ks_sdk::read_bytes(pid, address, size as usize, false, true).map_err(reason)
}

pub fn write_mdl(pid: u64, address: u64, data: &[u8]) -> Result<(), String> {
    gate()?;
    ks_sdk::write_bytes(pid, address, data, false, true).map_err(reason)
}

pub fn read_mdl_rva(pid: u64, relative_address: u64, size: u64) -> Result<Vec<u8>, String> {
    gate()?;
    ks_sdk::read_bytes(pid, relative_address, size as usize, true, true).map_err(reason)
}

pub fn write_mdl_rva(pid: u64, relative_address: u64, data: &[u8]) -> Result<(), String> {
    gate()?;
    ks_sdk::write_bytes(pid, relative_address, data, true, true).map_err(reason)
}

pub fn batch_read(pid: u64, size: u32, addresses: &[u64]) -> Result<Vec<u8>, String> {
    gate()?;
    ks_sdk::batch_read(pid, size, addresses).map_err(reason)
}

/// Writes every (address, data) entry for `pid` in one ring round trip.
/// Returns one flag per entry: true when the driver reported success.
pub fn batch_write(pid: u64, entries: &[(u64, Vec<u8>)]) -> Result<Vec<bool>, String> {
    gate()?;
    ks_sdk::batch_write(pid, entries)
        .map(|statuses| statuses.iter().map(|&status| status == 0).collect())
        .map_err(reason)
}

pub fn traverse_pointer_chain(pid: u64, base: u64, offsets: &[u64]) -> Result<u64, String> {
    gate()?;
    ks_sdk::traverse_pointer_chain(pid, base, offsets).map_err(reason)
}

pub fn lock(id: u64, pid: u64, address: u64, data: &[u8]) -> Result<(), String> {
    gate()?;
    ks_sdk::lock(id, pid, address, data).map_err(reason)
}

pub fn unlock(id: u64) -> Result<(), String> {
    gate()?;
    ks_sdk::unlock(id).map_err(reason)
}

pub fn unlock_all(pid: u64) -> Result<(), String> {
    gate()?;
    ks_sdk::unlock_all(pid).map_err(reason)
}

pub fn lock_rva(id: u64, pid: u64, relative_address: u64, data: &[u8]) -> Result<(), String> {
    gate()?;
    ks_sdk::lock_rva(id, pid, relative_address, data).map_err(reason)
}

pub fn unlock_rva(id: u64) -> Result<(), String> {
    gate()?;
    ks_sdk::unlock_rva(id).map_err(reason)
}
