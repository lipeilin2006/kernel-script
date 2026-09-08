#![no_std]
#![no_main]

use ks_driver::{driver_entry, DRIVER_OBJECT, UNICODE_STRING};

#[no_mangle]
pub unsafe extern "system" fn DriverEntry(
    driver: *mut DRIVER_OBJECT,
    registry_path: *mut UNICODE_STRING,
) -> windows_sys::Win32::Foundation::NTSTATUS {
    driver_entry(driver, registry_path)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
