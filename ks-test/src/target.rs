//! The self-process check target plus the shared harness helpers every
//! suite step reports through.

use std::sync::atomic::AtomicU32;

use crate::step_log::say;

/// Image-backed target for the RVA tests. Lives in this module's data
/// section, so `image_base + rva` resolves to it and the page is writable.
pub(crate) static RVA_TARGET: AtomicU32 = AtomicU32::new(0);

pub(crate) fn image_base() -> Result<u64, String> {
    // SAFETY: a null module name asks for this executable's image base.
    let base =
        unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(core::ptr::null()) };
    if base.is_null() {
        return Err("GetModuleHandleW(NULL) failed".into());
    }
    Ok(base as usize as u64)
}

/// Everything the checks and benchmarks operate on. The buffer is this
/// process's own heap memory: the driver reads and writes it through the
/// kernel, so round trips verify real cross-context copies. Immutable
/// after [`Target::setup`], hence safely shareable across threads.
pub(crate) struct Target {
    pub(crate) pid: u64,
    /// Module base as the driver reports it (`GetProcessBase`).
    pub(crate) driver_base: u64,
    /// Module base as user mode sees it; must match `driver_base`.
    pub(crate) image_base: u64,
    pub(crate) buffer: Vec<u8>,
    pub(crate) buffer_addr: u64,
    /// RVA of [`RVA_TARGET`] inside this image.
    pub(crate) rva_target_rva: u64,
}

impl Target {
    pub(crate) fn setup() -> Result<Self, String> {
        let pid = std::process::id() as u64;
        say(&format!("target setup: get_process_base(pid={pid})"));
        let driver_base =
            ks_sdk::get_process_base(pid).map_err(|e| format!("get_process_base: {e}"))?;
        say(&format!(
            "target setup: driver_base=0x{driver_base:X} (get_process_base ok)"
        ));
        let image_base = image_base()?;
        let rva_target_rva = (core::ptr::addr_of!(RVA_TARGET) as usize as u64)
            .checked_sub(image_base)
            .ok_or("RVA_TARGET below image base")?;

        let mut buffer = vec![0u8; 4096];
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = (index * 31 + 7) as u8;
        }
        let buffer_addr = buffer.as_ptr() as u64;
        // buffer[0..4]: magic the concurrency readers keep verifying.
        buffer[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        // buffer[0x100..0x108]: pointer into the buffer for chain walks.
        buffer[0x100..0x108].copy_from_slice(&(buffer_addr + 0x200).to_le_bytes());
        // buffer[0x200..0x204]: chain walk target magic.
        buffer[0x200..0x204].copy_from_slice(&0x2468_ACE0u32.to_le_bytes());

        Ok(Self {
            pid,
            driver_base,
            image_base,
            buffer,
            buffer_addr,
            rva_target_rva,
        })
    }

    pub(crate) fn slot(&self, offset: u64) -> u64 {
        self.buffer_addr + offset
    }
}

/// Runs one check, recording and reporting the failure instead of
/// panicking (an aborted harness would leak the loaded driver).
pub(crate) fn check(
    failures: &mut Vec<(String, String)>,
    name: &str,
    op: impl FnOnce() -> Result<(), String>,
) {
    // The `RUN` marker is written BEFORE the operation so a bugcheck
    // mid-check names the culprit even without a trailing PASS.
    say(&format!("RUN  {name}"));
    match op() {
        Ok(()) => say(&format!("PASS {name}")),
        Err(error) => {
            say(&format!("FAIL {name}: {error}"));
            failures.push((name.to_owned(), error));
        }
    }
}

pub(crate) fn u32_at(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap())
}

pub(crate) fn read_is(t: &Target, address: u64, expected: &[u8]) -> Result<(), String> {
    let data = ks_sdk::read_bytes(t.pid, address, expected.len(), false, false)
        .map_err(|e| format!("read: {e}"))?;
    if data.as_slice() != expected {
        return Err(format!(
            "content mismatch at 0x{address:X}: got {:02X?}, want {:02X?}",
            &data[..data.len().min(16)],
            &expected[..expected.len().min(16)]
        ));
    }
    Ok(())
}
