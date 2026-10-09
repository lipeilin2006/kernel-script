//! The driver's registry publication: object names and the instance claim.

use core::ffi::c_void;

use ks_core::ring::{REQUEST_EVENT_CLIENT_NAME, RESPONSE_EVENT_CLIENT_NAME, SECTION_CLIENT_NAME};
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

use super::error::{wide, LinkError};

/// Subkey under `HKLM` where the driver publishes the object names.
const NAMES_KEY: &str = r"SOFTWARE\KernelScript";

/// Value below [`NAMES_KEY`] marking the live single-instance claim
/// (`REG_DWORD` 1, written at load, deleted at teardown).
const CLAIM_VALUE: &str = "Instance";

/// Reads one `REG_SZ` value below `HKLM\SOFTWARE\KernelScript`, returning
/// its UTF-16 units (with the terminating NUL, if any) or `None` when the
/// key/value is missing or holds another type.
fn registry_sz(value: &str) -> Option<Vec<u16>> {
    let key = wide(NAMES_KEY);
    let name = wide(value);
    let mut data = [0u16; 96];
    let mut size = (data.len() * 2) as u32;
    let mut kind: u32 = 0;
    // SAFETY: buffers are live for the call and `size` bounds the write.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            data.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if status != 0 || size < 2 {
        return None;
    }
    Some(data[..(size as usize / 2)].to_vec())
}

/// Maps a published kernel name (`\BaseNamedObjects\...`) onto the client
/// namespace (`Global\...`).
fn kernel_name_to_client(units: &[u16]) -> Option<String> {
    let text = String::from_utf16_lossy(units);
    let text = text.trim_end_matches('\0');
    let tail = text.strip_prefix(r"\BaseNamedObjects\")?;
    if tail.is_empty() || tail.contains('\\') {
        return None;
    }
    Some(format!(r"Global\{tail}"))
}

fn published_name_opt(value: &str) -> Option<String> {
    registry_sz(value).and_then(|units| kernel_name_to_client(&units))
}

fn published_name(value: &str, fallback: &str) -> String {
    published_name_opt(value).unwrap_or_else(|| fallback.to_string())
}

/// The client-facing object names for this driver load, resolved from the
/// driver's registry publication with the compiled-in defaults as fallback
/// — a fallback that cannot reach a randomized load, so an unreadable key
/// effectively fails the session open.
pub fn published_object_names() -> [String; 3] {
    [
        published_name("SectionName", SECTION_CLIENT_NAME),
        published_name("RequestEventName", REQUEST_EVENT_CLIENT_NAME),
        published_name("ResponseEventName", RESPONSE_EVENT_CLIENT_NAME),
    ]
}

/// Like [`published_object_names`] but returns `None` when any value is
/// missing: callers that must prove the driver really wrote
/// `HKLM\SOFTWARE\KernelScript` (the test harness polls it as its
/// readiness signal) use this instead of the fallback variant.
pub fn published_object_names_strict() -> Option<[String; 3]> {
    Some([
        published_name_opt("SectionName")?,
        published_name_opt("RequestEventName")?,
        published_name_opt("ResponseEventName")?,
    ])
}

/// Whether the driver's [`CLAIM_VALUE`] claim value is still present.
///
/// The driver writes it at load and deletes it at teardown (which removes
/// the whole key as well), so `true` means a loaded instance owns the
/// single-instance marker, while `false` means either nothing ever loaded
/// or `shutdown` (or `sc stop`) finished teardown. A missing key — before
/// the first load, or after teardown — is `false`; any other registry
/// failure is returned.
pub fn instance_claim_present() -> Result<bool, LinkError> {
    let key = wide(NAMES_KEY);
    let name = wide(CLAIM_VALUE);
    let mut data = [0u8; 4];
    let mut kind: u32 = 0;
    let mut size = data.len() as u32;
    // SAFETY: buffers are live for the call and `size` bounds the write.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut kind,
            data.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    match status {
        0 => Ok(true),
        // Missing key or missing value: no claim either way.
        ERROR_FILE_NOT_FOUND => Ok(false),
        code => Err(LinkError::WinApi {
            operation: "read the Instance claim",
            code,
        }),
    }
}
