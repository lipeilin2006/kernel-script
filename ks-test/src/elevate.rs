//! Elevation probe and the child-process shutdown channel: once this
//! process's ks-link session has died with the driver, a fresh
//! `ks-test shutdown` process is the only cleanup channel left.

use std::os::windows::process::CommandExt;
use std::process::Command;

use crate::step_log::say;
use crate::CREATE_NO_WINDOW;

/// True when this process runs with a full (elevated) token. The driver's
/// section DACL only grants SYSTEM and Administrators, so an unelevated
/// harness is denied up front with ERROR_ACCESS_DENIED.
pub(crate) fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = core::ptr::null_mut();
        // SAFETY: pseudo process handle + out parameter; token is closed below.
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        // SAFETY: buffer is sized for exactly one TOKEN_ELEVATION.
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Runs `ks-test shutdown` as a child process and requires it to report
/// that no live driver instance remains. The child is the only channel
/// left once this process's ks-link session has died with the driver —
/// every other process opens its own session against the current registry
/// names.
pub(crate) fn run_shutdown_child() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let output = Command::new(exe)
        .arg("shutdown")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("spawn `ks-test shutdown`: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for line in text.lines() {
        say(&format!("shutdown-child: {line}"));
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`ks-test shutdown` exit {:?}: {}",
            output.status.code(),
            text.trim()
        ))
    }
}

/// Stops a leftover driver instance in a child process before this one
/// opens a session: only a run that died before teardown leaves the
/// published names behind (a clean exit removes them with the key), so the
/// child (and its ping) is only spawned when the values exist at all, and
/// it distinguishes "stale" from "live" itself.
pub(crate) fn cleanup_leftover() -> Result<(), String> {
    if ks_sdk::published_object_names_strict().is_none() {
        return Ok(());
    }
    say("cleanup: published names present; checking for a leftover live instance");
    run_shutdown_child()
}
