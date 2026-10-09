//! Legacy SCM lifecycle helpers: `sc.exe` invocation, service-state
//! queries and failure-output parsing (`ks-test sc [full]`).

use std::os::windows::process::CommandExt;
use std::process::Command;

use crate::CREATE_NO_WINDOW;

/// A service state as reported by `sc query`.
#[derive(Debug, Clone)]
pub(crate) enum ScState {
    Running,
    Stopped,
    /// Any other documented state (START_PENDING, STOP_PENDING, ...) as
    /// printed on the `STATE` line.
    Other(String),
    /// `ERROR_SERVICE_DOES_NOT_EXIST` (1060): the service is gone.
    Gone,
}

impl ScState {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Running => "RUNNING".into(),
            Self::Stopped => "STOPPED".into(),
            Self::Other(text) => text.clone(),
            Self::Gone => "deleted".into(),
        }
    }
}

/// Queries one service's state through `sc query`.
///
/// A missing service is detected by the error code 1060 in the output —
/// sc.exe exits 0 even on failure on recent builds, and the digits of the
/// code survive every code page (English `FAILED 1060:`, localized
/// `失败 1060:`). Healthy output for this harness's own kernel driver never
/// contains `1060` (no PID line; exit codes and hints are 0), so a plain
/// substring test is safe. Healthy output is then parsed by the English
/// keywords as a fast path and by the numeric `STATE` code (`1` =
/// STOPPED, `4` = RUNNING) as the locale-independent fallback.
pub(crate) fn sc_state(service: &str) -> Result<ScState, String> {
    let output = Command::new("sc.exe")
        .args(["query", service])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("sc.exe query: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stdout.contains("1060") || stderr.contains("1060") {
        return Ok(ScState::Gone);
    }
    if !output.status.success() || sc_reports_failure(&stdout) || sc_reports_failure(&stderr) {
        return Err(format!("sc query: {}", format!("{stdout}{stderr}").trim()));
    }
    if stdout.contains("STOPPED") {
        return Ok(ScState::Stopped);
    }
    if stdout.contains("RUNNING") {
        return Ok(ScState::Running);
    }
    if let Some(line) = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("STATE"))
    {
        let code = line
            .split(':')
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|token| token.parse::<u32>().ok());
        return match code {
            Some(1) => Ok(ScState::Stopped),
            Some(4) => Ok(ScState::Running),
            _ => Ok(ScState::Other(line.to_string())),
        };
    }
    Ok(ScState::Other(stdout.trim().to_string()))
}

/// Heuristic for a failed sc.exe command: failure messages are one-line
/// `[SC] ... <code>:` reports where the code is a decimal number directly
/// before a colon (English `FAILED 1060:` and localized `失败 1060:` both
/// end that way, and the digits survive any code page). Success lines
/// (`... SUCCESS`) and the empty output of `start`/`stop` carry no such
/// pattern.
pub(crate) fn sc_reports_failure(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("[SC]")
            && line
                .as_bytes()
                .windows(2)
                .any(|pair| pair[0].is_ascii_digit() && pair[1] == b':')
    })
}

/// The decimal error code from a `sc.exe` failure line (see
/// [`sc_reports_failure`]), or `None` when no code is recognizable. The
/// digits before the colon survive every locale, so this works on English
/// `FAILED 1060:` and localized `失败 1060:` alike; the `[SC]` marker keeps
/// the prefix of the `sc(...)` error string (service name, arguments) from
/// being mistaken for a code.
pub(crate) fn sc_failure_code(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        if !line.contains("[SC]") {
            return None;
        }
        let bytes = line.as_bytes();
        for (index, pair) in bytes.windows(2).enumerate() {
            if pair[0].is_ascii_digit() && pair[1] == b':' {
                let mut start = index;
                while start > 0 && bytes[start - 1].is_ascii_digit() {
                    start -= 1;
                }
                return core::str::from_utf8(&bytes[start..index + 1])
                    .ok()?
                    .parse()
                    .ok();
            }
        }
        None
    })
}

pub(crate) fn sc(args: &[&str]) -> Result<(), String> {
    let output = Command::new("sc.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("sc.exe: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // sc.exe on recent builds exits 0 even when the command fails, and it
    // prints errors to stdout (localized, non-UTF-8), so success must be
    // judged from the output text, not the exit code.
    if !output.status.success() || sc_reports_failure(&stdout) || sc_reports_failure(&stderr) {
        return Err(format!(
            "sc.exe {:?}: {}",
            args,
            format!("{stdout}{stderr}").trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{sc_failure_code, sc_reports_failure};

    #[test]
    fn sc_failure_detection() {
        // English: decimal error code directly before the colon.
        assert!(sc_reports_failure(
            "[SC] OpenService FAILED 1060:\r\n\r\nThe spec...\r\n"
        ));
        // Localized (GBK bytes decoded as UTF-8 to U+0269 etc.): the
        // digits still sit directly before the colon.
        assert!(sc_reports_failure(
            "[SC] StartService: OpenService \u{0269}\u{FFFD} 1060:\r\n"
        ));
        // Success lines, empty output and healthy `sc query` reports are
        // never failures — including a WIN32_EXIT_CODE of 1060, which is
        // not on an [SC] line.
        assert!(!sc_reports_failure("[SC] CreateService SUCCESS\r\n"));
        assert!(!sc_reports_failure(""));
        assert!(!sc_reports_failure(
            "SERVICE_NAME: kstdrv\r\n        TYPE               : 1  KERNEL_DRIVER\r\n\
             STATE              : 4  RUNNING\r\n        WIN32_EXIT_CODE    : 0  (0x0)\r\n"
        ));
        assert!(!sc_reports_failure(
            "        WIN32_EXIT_CODE    : 1060  (0x424)\r\n"
        ));
    }

    #[test]
    fn sc_failure_code_extraction() {
        assert_eq!(
            sc_failure_code("sc.exe: [SC] StartService FAILED 183: x"),
            Some(183)
        );
        // The real `sc(...)` error format: arguments prefix, code further on.
        assert_eq!(
            sc_failure_code(
                "sc.exe [\"start\", \"kstdrv3c339857\"]: [SC] StartService FAILED 183:\r\n\
                 The service cannot be started...\r\n"
            ),
            Some(183)
        );
        // Localized failure word, digits still before the colon.
        assert_eq!(
            sc_failure_code("[SC] StartService \u{0269}\u{FFFD} 183: y"),
            Some(183)
        );
        assert_eq!(sc_failure_code("[SC] CreateService SUCCESS"), None);
        assert_eq!(sc_failure_code(""), None);
        // Non-[SC] lines never contribute a code, even numeric ones.
        assert_eq!(sc_failure_code("183: not sc"), None);
    }
}
