#![no_std]

pub mod dispatch;
pub mod memory;
mod wdm;

pub use ks_core::protocol;
pub use wdm::{DRIVER_OBJECT, NTSTATUS, UNICODE_STRING};

pub unsafe extern "system" fn driver_entry(
    driver: *mut DRIVER_OBJECT,
    registry_path: *mut UNICODE_STRING,
) -> NTSTATUS {
    dispatch::driver_entry(driver, registry_path)
}

/// Kept as a separate symbol so the WDK loader can find the unload routine.
#[no_mangle]
pub unsafe extern "system" fn KsDriverUnload(driver: *const DRIVER_OBJECT) {
    dispatch::driver_unload(driver as *mut DRIVER_OBJECT);
}
