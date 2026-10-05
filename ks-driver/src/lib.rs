#![no_std]
// The trace! diagnostic macro intentionally expands request arguments
// inside the DbgPrint unsafe block; remove this once tracing is no longer
// needed.
#![allow(clippy::macro_metavars_in_unsafe)]

pub mod comm;
pub mod lock;
pub mod memory;
pub mod request;
mod wdm;

pub use ks_core::protocol;
pub use wdm::{DRIVER_OBJECT, NTSTATUS, UNICODE_STRING};

/// Kernel debug tracing (`DbgPrint`), visible in DebugView with kernel
/// capture or a kernel debugger. C-style `%lu`/`%llu`/`%p` specifiers; the
/// args must match their widths exactly (u32/u64/pointer).
#[macro_export]
macro_rules! trace {
    ($fmt:expr) => {
        #[allow(unused_unsafe)]
        unsafe { $crate::wdm::DbgPrint(concat!("[ksdrv] ", $fmt, "\n\0").as_ptr()) }
    };
    ($fmt:expr, $($arg:expr),+ $(,)?) => {
        #[allow(unused_unsafe)]
        unsafe { $crate::wdm::DbgPrint(concat!("[ksdrv] ", $fmt, "\n\0").as_ptr(), $($arg),+) }
    };
}

/// There is no device object: `DriverEntry` only stands up the shared ring
/// and the worker thread. The security boundary is the section/event DACL
/// combined with the ring state machine, not an IOCTL dispatch table.
///
/// # Safety
///
/// `driver` must be the valid `DRIVER_OBJECT` the kernel passes to
/// `DriverEntry`; the I/O manager calls this exactly once per driver load.
pub unsafe extern "system" fn driver_entry(
    driver: *mut DRIVER_OBJECT,
    _registry_path: *mut UNICODE_STRING,
) -> NTSTATUS {
    if driver.is_null() {
        return wdm::STATUS_INVALID_PARAMETER;
    }
    let status = comm::start();
    if !wdm::nt_success(status) {
        return status;
    }
    (*driver).DriverUnload = Some(KsDriverUnload);
    wdm::STATUS_SUCCESS
}

/// Kept as a separate symbol so the WDK loader can find the unload routine.
///
/// # Safety
///
/// The kernel invokes this through `DRIVER_OBJECT::DriverUnload` while no
/// worker thread is dispatching ring requests; `driver` must be the object
/// `driver_entry` received.
#[no_mangle]
pub unsafe extern "system" fn KsDriverUnload(_driver: *const DRIVER_OBJECT) {
    comm::stop();
}
