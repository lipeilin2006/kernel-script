//! The link error type and the Win32 helpers every module needs.

use core::fmt;

use ks_core::protocol::ProtocolError;
use windows_sys::Win32::Foundation::GetLastError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkError {
    /// The section or one of the events does not exist, so the driver is
    /// not loaded (or is still initializing) — or its DACL rejected us.
    /// `object` names what the client was opening; `code` is the failing
    /// `GetLastError` value.
    DriverNotReady {
        object: &'static str,
        code: u32,
    },
    WinApi {
        operation: &'static str,
        code: u32,
    },
    /// The mapping exists but its magic/version is not ours.
    RingIncompatible,
    Encode(ProtocolError),
    Decode(ProtocolError),
    /// The driver answered with a failure NTSTATUS.
    NtStatus(i32),
    /// The driver did not answer within the cancel window.
    TimedOut,
    ResponseTooSmall,
    TooManyEntries {
        limit: usize,
    },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DriverNotReady { object, code } => {
                write!(f, "driver not loaded ({object} open failed: {code})")?;
                // ERROR_ACCESS_DENIED (5) means the object exists but the
                // driver DACL rejected us: the caller is not elevated, or
                // the DACL does not actually match Administrators.
                if *code == 5 {
                    f.write_str(" — access denied: run elevated (the driver DACL only grants SYSTEM and Administrators)")?;
                }
                Ok(())
            }
            Self::WinApi { operation, code } => write!(f, "{operation} failed: {code}"),
            Self::RingIncompatible => f.write_str("ring header magic/version mismatch"),
            Self::Encode(error) => write!(f, "request encode failed: {error}"),
            Self::Decode(error) => write!(f, "response decode failed: {error}"),
            Self::NtStatus(status) => write!(f, "driver error {status:#x}"),
            Self::TimedOut => f.write_str("driver did not answer in time"),
            Self::ResponseTooSmall => f.write_str("caller buffer smaller than driver payload"),
            Self::TooManyEntries { limit } => write!(f, "more than {limit} entries in one request"),
        }
    }
}

impl std::error::Error for LinkError {}

pub(crate) fn last_error() -> u32 {
    unsafe { GetLastError() }
}

pub(crate) fn win_api(operation: &'static str) -> LinkError {
    LinkError::WinApi {
        operation,
        code: last_error(),
    }
}

/// Encodes `name` as a NUL-terminated UTF-16 buffer for the `*W` APIs.
pub(crate) fn wide(name: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let wide = wide("ab");
        assert_eq!(wide[0], u16::from(b'a'));
        assert_eq!(wide[1], u16::from(b'b'));
        assert_eq!(wide[2], 0);
    }

    #[test]
    fn error_display_mentions_operation() {
        let error = LinkError::WinApi {
            operation: "MapViewOfFile",
            code: 5,
        };
        assert_eq!(error.to_string(), "MapViewOfFile failed: 5");
    }
}
