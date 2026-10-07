//! ks-test: correctness harness and benchmark for the KernelScript ring.
//!
//! Default lifecycle: the pure-Rust mapper in `ks-sdk/src/kdu/` loads
//! the driver through `ks_sdk::start()` — the signed image goes through
//! a normal service load first, and only a rejected service load falls
//! back to manual mapping (shellcode V3, `DriverEntry` runs in place,
//! nothing registers with the SCM). Modes: `full`/`minimal` correctness
//! suites, `benchmark` performance only, `load` the provider × victim
//! matrix, `shutdown` recovery. Readiness is the registry publication:
//! poll `HKLM\SOFTWARE\KernelScript` until the driver publishes its object
//! names, connect through `ks-sdk` (the `ks-link` re-export) and exercise
//! every operation against
//! this process's own memory: plain, RVA-resolved and MDL-remap
//! reads/writes, batch operations, pointer-chain walks, the driver-side
//! memory locks and concurrent ring access from several threads. The
//! read/write paths additionally run performance benchmarks (all sizes and
//! transports); pointer walks and friends only need to work.
//!
//! KDU never unloads a mapped image, so teardown inverts: the ring
//! `shutdown` request is what makes the driver release its ring objects,
//! registry claim and single-instance marker, and a `ks-test shutdown`
//! child process (link sessions never reconnect) verifies nothing live
//! remains and that the driver maps again cleanly. The legacy SCM lifecycle
//! (`ks-test sc [full]`) still exists for regression runs: `sc
//! create`/`sc start`, then after `shutdown` the checked `sc stop`/`sc
//! query`/`sc delete` sequence.
//!
//! Every correctness failure is collected and reported; benchmarks only run
//! when the whole suite passes. The process exits non-zero on failure, so
//! it can drive automated smoke runs. RVA/MDL-RVA checks target a static in
//! this image: RVA addressing is relative to the module base and the heap
//! typically sits below it, so heap addresses have no usable RVA. The
//! workspace profiles use `panic = "abort"`, so the harness reports
//! failures instead of unwinding — an aborted run would leak the loaded
//! driver instance.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long the harness waits for the driver ring to appear after the load
/// command reported success.
const DRIVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Settle time around lock checks: the driver's merged worker rewrites one
/// lock entry per loop pass between requests, at full speed, so a fraction
/// of a second is always enough for sweeps to apply or stop.
const LOCK_SETTLE: Duration = Duration::from_millis(200);

/// Image-backed target for the RVA tests. Lives in this module's data
/// section, so `image_base + rva` resolves to it and the page is writable.
static RVA_TARGET: AtomicU32 = AtomicU32::new(0);

/// Step logs: every harness milestone is flushed to every file that opened
/// successfully so a bugcheck (which takes the console with it) still
/// leaves the exact crashing step on disk. Includes fixed absolute paths so
/// the log is findable no matter where the executable was copied.
static STEP_LOG: Mutex<Vec<std::fs::File>> = Mutex::new(Vec::new());

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prints to the console and appends+flushes to the step logs.
fn say(line: &str) {
    println!("{line}");
    if let Ok(mut files) = STEP_LOG.lock() {
        for file in files.iter_mut() {
            let _ = writeln!(file, "[{:>6}] {line}", now_secs());
            let _ = file.flush();
            let _ = file.sync_all();
        }
    }
}

fn open_step_log() {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ks-test-run.log"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ks-test-run.log"));
    }
    candidates.push(PathBuf::from(r"D:\kernel-script\ks-test-run.log"));
    candidates.push(PathBuf::from(r"C:\ks-test-run.log"));

    let mut files = Vec::new();
    let mut opened = Vec::new();
    for path in &candidates {
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
            opened.push(format!("{}", path.display()));
            files.push(file);
        }
    }
    *STEP_LOG.lock().expect("step log poisoned") = files;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    say(&format!("harness start exe={exe} logs={opened:?}"));
}

/// How the embedded driver image reaches the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoadMode {
    /// KDU manual mapping (the default): the in-process KDU bridge runs
    /// `DriverEntry` inside a mapped image (shellcode V3). No service, no
    /// signature, no unload — teardown is the ring `shutdown` request,
    /// which makes the driver release its single-instance claim and erase
    /// its registry publication itself.
    Kdu,
    /// Legacy SCM load (`ks-test sc ...`): `sc create`/`sc start` a kernel
    /// service; teardown is `sc stop`/`sc delete` after the ring
    /// `shutdown`.
    Sc { service: String },
}

struct EmbeddedDriver {
    root: PathBuf,
    mode: LoadMode,
    /// Set once the checked teardown ran so `Drop` does not repeat a
    /// (best-effort) cleanup.
    finished: bool,
}

impl EmbeddedDriver {
    /// Loads the embedded driver: the legacy SCM service when
    /// `legacy_sc` is set, otherwise `ks_sdk::start()` (the in-process
    /// KDU mapper). Both paths first clear a leftover live instance
    /// (see [`cleanup_leftover`]). Only the SCM path puts the image on
    /// disk — `sc create` needs a `binPath` — and it writes it into the
    /// process-local temp root below.
    fn start(legacy_sc: bool) -> Result<Self, String> {
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        );
        let root = std::env::temp_dir().join(format!("kernel-script-{token}"));

        // A previous run that died before teardown can leave a live mapped
        // instance holding the single-instance marker. ks-link sessions
        // never reconnect, so that leftover must be stopped in its own
        // process before this one touches the ring.
        cleanup_leftover()?;

        let mode = if legacy_sc {
            fs::create_dir_all(&root).map_err(|e| format!("create temp dir: {e}"))?;
            let driver_path = root.join("ks-driver.sys");
            fs::write(&driver_path, ks_sdk::DRIVER_IMAGE)
                .map_err(|e| format!("write driver: {e}"))?;
            let suffix = format!(
                "{:x}",
                token.bytes().fold(0u64, |acc, byte| acc
                    .wrapping_mul(131)
                    .wrapping_add(byte as u64))
            );
            let service = format!("kstdrv{}", &suffix[..suffix.len().min(8)]);
            let driver_path_arg = driver_path.to_string_lossy().into_owned();
            if let Err(error) = sc(&[
                "create",
                &service,
                "type=",
                "kernel",
                "start=",
                "demand",
                "binPath=",
                &driver_path_arg,
            ]) {
                let _ = fs::remove_dir_all(&root);
                return Err(error);
            }
            if let Err(error) = sc(&["start", &service]) {
                let _ = sc(&["delete", &service]);
                let _ = fs::remove_dir_all(&root);
                return Err(error);
            }
            LoadMode::Sc { service }
        } else {
            // The mapper runs in-process against the SDK's embedded image
            // bytes; only KDU's extracted helper drivers need files (in
            // their own temporary root, created and removed by `start`).
            if let Err(error) = ks_sdk::start() {
                let _ = fs::remove_dir_all(&root);
                return Err(format!("ks_sdk::start: {error}"));
            }
            LoadMode::Kdu
        };
        Ok(Self {
            root,
            mode,
            finished: false,
        })
    }

    /// The SCM service name (legacy [`LoadMode::Sc`] only).
    fn service(&self) -> Result<&str, String> {
        match &self.mode {
            LoadMode::Sc { service } => Ok(service),
            LoadMode::Kdu => Err("no service in KDU mode".into()),
        }
    }

    /// The checked end of the legacy SCM lifecycle: stop the service,
    /// verify `sc query` reports STOPPED (after `shutdown` the worker is
    /// already gone and `sc stop` only completes the unload), then delete
    /// the service and verify it is gone. Both outcomes are returned so
    /// the harness can record them as checks.
    fn teardown(&mut self) -> (Result<(), String>, Result<(), String>) {
        self.finished = true;
        let stop = self.stop_and_verify();
        let delete = self.delete_and_verify();
        (stop, delete)
    }

    /// `sc stop`, then poll `sc query` until the service reports STOPPED
    /// (or no longer exists). The stop command itself is only the trigger;
    /// reaching the state is what counts.
    fn stop_and_verify(&self) -> Result<(), String> {
        let service = self.service()?;
        let issued = sc(&["stop", service]).err();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = match issued {
            Some(error) => format!("sc stop: {error}"),
            None => "sc stop issued".to_string(),
        };
        loop {
            if Instant::now() >= deadline {
                return Err(format!("service not STOPPED within 10 s (last: {last})"));
            }
            match sc_state(service) {
                Ok(ScState::Stopped) | Ok(ScState::Gone) => return Ok(()),
                Ok(state) => last = state.describe().to_string(),
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// `sc delete`, then poll `sc query` until the service no longer
    /// exists (`ERROR_SERVICE_DOES_NOT_EXIST`).
    fn delete_and_verify(&self) -> Result<(), String> {
        let service = self.service()?;
        let issued = sc(&["delete", service]).err();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = match issued {
            Some(error) => format!("sc delete: {error}"),
            None => "sc delete issued".to_string(),
        };
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "service still present 10 s after sc delete (last: {last})"
                ));
            }
            match sc_state(service) {
                Ok(ScState::Gone) => return Ok(()),
                Ok(state) => last = state.describe().to_string(),
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A service state as reported by `sc query`.
#[derive(Debug, Clone)]
enum ScState {
    Running,
    Stopped,
    /// Any other documented state (START_PENDING, STOP_PENDING, ...) as
    /// printed on the `STATE` line.
    Other(String),
    /// `ERROR_SERVICE_DOES_NOT_EXIST` (1060): the service is gone.
    Gone,
}

impl ScState {
    fn describe(&self) -> String {
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
fn sc_state(service: &str) -> Result<ScState, String> {
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

impl Drop for EmbeddedDriver {
    fn drop(&mut self) {
        // Early-return paths (start-up failure, minimal mode, a
        // failed readiness wait) never ran the checked teardown. Legacy
        // SCM mode stops and deletes the service best-effort; KDU mode
        // stops a still-mapped instance from a child process, because
        // this process's ks-link session (if any) never reconnects and
        // may already point at a dead ring. After a checked teardown only
        // the temp directory is removed.
        if !self.finished {
            match &self.mode {
                LoadMode::Sc { service } => {
                    let _ = sc(&["stop", service]);
                    let _ = sc(&["delete", service]);
                }
                LoadMode::Kdu => {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = Command::new(exe)
                            .arg("shutdown")
                            .creation_flags(CREATE_NO_WINDOW)
                            .output();
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Heuristic for a failed sc.exe command: failure messages are one-line
/// `[SC] ... <code>:` reports where the code is a decimal number directly
/// before a colon (English `FAILED 1060:` and localized `失败 1060:` both
/// end that way, and the digits survive any code page). Success lines
/// (`... SUCCESS`) and the empty output of `start`/`stop` carry no such
/// pattern.
fn sc_reports_failure(text: &str) -> bool {
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
fn sc_failure_code(text: &str) -> Option<u32> {
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

fn sc(args: &[&str]) -> Result<(), String> {
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

/// True when this process runs with a full (elevated) token. The driver's
/// section DACL only grants SYSTEM and Administrators, so an unelevated
/// harness is denied up front with ERROR_ACCESS_DENIED.
fn is_elevated() -> bool {
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
fn run_shutdown_child() -> Result<(), String> {
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
fn cleanup_leftover() -> Result<(), String> {
    if ks_sdk::published_object_names_strict().is_none() {
        return Ok(());
    }
    say("cleanup: published names present; checking for a leftover live instance");
    run_shutdown_child()
}

/// `ks-test shutdown`: stop whatever driver instance the registry currently
/// points at, from this process. Exit 0 when nothing live remains (names
/// absent or stale, or a live instance was stopped and its registry
/// publication removed), 1 when a live instance could not be stopped.
///
/// This is both the harness's leftover-cleanup/re-map-verification tool
/// and the manual recovery command for an instance left behind by a killed
/// run — ks-link sessions never reconnect, so cleanup always needs a fresh
/// process.
fn shutdown_standalone() -> i32 {
    let Some(names) = ks_sdk::published_object_names_strict() else {
        say("shutdown: no published object names; cleaning any leftover service load");
        ks_sdk::cleanup_service_load();
        return 0;
    };
    say(&format!(
        "shutdown: published names: {} | {} | {}",
        names[0], names[1], names[2]
    ));
    match ks_sdk::ping() {
        Err(error) => {
            say(&format!(
                "shutdown: driver not answering ({error}); stale names, nothing to do"
            ));
            0
        }
        Ok(()) => match ks_sdk::stop() {
            Err(error) => {
                eprintln!("shutdown: shutdown request failed: {error}");
                1
            }
            Ok(()) => {
                if ks_sdk::ping().is_ok() {
                    eprintln!("shutdown: driver still answers after shutdown");
                    return 1;
                }
                match ks_sdk::instance_claim_present() {
                    Ok(true) => {
                        eprintln!("shutdown: driver stopped but the Instance claim is present");
                        1
                    }
                    Ok(false) => match ks_sdk::published_object_names_strict() {
                        Some(_) => {
                            eprintln!(
                                "shutdown: driver stopped but the object names are still published"
                            );
                            1
                        }
                        None => {
                            say("shutdown: driver stopped, registry publication removed");
                            0
                        }
                    },
                    Err(error) => {
                        eprintln!("shutdown: {error}");
                        1
                    }
                }
            }
        },
    }
}

/// Waits for the driver to publish its object names, then for the ring to
/// answer.
///
/// Readiness is the registry publication itself: `sc start` returning only
/// means the driver entry ran, while `HKLM\SOFTWARE\KernelScript` holding
/// all three `REG_SZ` values proves section, request event and response
/// event exist. The ping afterwards proves the worker thread serves.
///
/// Every state transition is written through [`say`] (not just stderr) so a
/// bugcheck pinpoints whether it happened inside the first blocked ping or
/// after a recorded failure.
fn wait_for_driver() -> Result<(), String> {
    if !is_elevated() {
        let error = "this process is NOT elevated; the driver DACL only grants SYSTEM and \
             Administrators, so the ring objects deny access (run the release build \
             or an elevated console)"
            .to_string();
        say(&format!("wait_for_driver: {error}"));
        return Err(error);
    }
    say("wait_for_driver: elevated; polling HKLM\\SOFTWARE\\KernelScript for object names");
    let deadline = Instant::now() + DRIVER_READY_TIMEOUT;
    let mut attempt = 0u32;
    let mut last = String::from("registry names not published yet");
    loop {
        attempt += 1;
        if Instant::now() >= deadline {
            let error = format!("driver not ready after {attempt} attempt(s): {last}");
            say(&format!("wait_for_driver: {error}"));
            return Err(error);
        }
        match ks_sdk::published_object_names_strict() {
            None => {
                last = "registry: section/event names not published yet".to_string();
            }
            Some(names) => {
                say(&format!(
                    "wait_for_driver: registry names present (attempt {attempt}): {} | {} | {}",
                    names[0], names[1], names[2]
                ));
                match ks_sdk::ping() {
                    Ok(()) => {
                        say(&format!("wait_for_driver: ping ok (attempt {attempt})"));
                        return Ok(());
                    }
                    Err(error) => {
                        last = format!("ping: {error}");
                        say(&format!(
                            "wait_for_driver: ping attempt {attempt} failed: {last}; retrying"
                        ));
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn image_base() -> Result<u64, String> {
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
struct Target {
    pid: u64,
    /// Module base as the driver reports it (`GetProcessBase`).
    driver_base: u64,
    /// Module base as user mode sees it; must match `driver_base`.
    image_base: u64,
    buffer: Vec<u8>,
    buffer_addr: u64,
    /// RVA of [`RVA_TARGET`] inside this image.
    rva_target_rva: u64,
}

impl Target {
    fn setup() -> Result<Self, String> {
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

    fn slot(&self, offset: u64) -> u64 {
        self.buffer_addr + offset
    }
}

/// Runs one check, recording and reporting the failure instead of
/// panicking (an aborted harness would leak the loaded driver).
fn check(
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

fn u32_at(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap())
}

fn read_is(t: &Target, address: u64, expected: &[u8]) -> Result<(), String> {
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

fn run_checks(target: &Arc<Target>, failures: &mut Vec<(String, String)>) {
    let t: &Target = target;
    say("--- Correctness ---");

    check(failures, "ping", || {
        ks_sdk::ping().map_err(|e| e.to_string())
    });

    check(failures, "get_pid(self)", || {
        let exe = std::env::current_exe()
            .map_err(|e| format!("current_exe: {e}"))?
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or("current_exe has no file name")?;
        let pid = ks_sdk::find_pid(&exe).map_err(|e| format!("{exe}: {e}"))?;
        if pid == t.pid {
            Ok(())
        } else {
            Err(format!("found pid {pid}, want {}", t.pid))
        }
    });

    check(failures, "get_pid(negative)", || {
        if ks_sdk::find_pid("definitely-not-a-real-process.exe").is_err() {
            Ok(())
        } else {
            Err("enumeration found a process that cannot exist".into())
        }
    });

    check(failures, "get_process_base == image base", || {
        if t.driver_base == t.image_base {
            Ok(())
        } else {
            Err(format!(
                "driver reports 0x{:X}, user mode sees 0x{:X}",
                t.driver_base, t.image_base
            ))
        }
    });

    check(failures, "get_process_base(invalid pid) errors", || {
        // 0xFFFF_FFFE is virtually certain not to exist; the driver must
        // fail the lookup instead of reporting a base.
        match ks_sdk::get_process_base(0xFFFF_FFFE) {
            Ok(base) => Err(format!("nonexistent pid reported base 0x{base:X}")),
            Err(_) => Ok(()),
        }
    });

    check(failures, "find_pid locates this process", || {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let name = exe
            .file_name()
            .ok_or_else(|| "exe has no file name".to_string())?
            .to_string_lossy()
            .into_owned();
        let found = ks_sdk::find_pid(&name).map_err(|e| format!("find_pid({name}): {e}"))?;
        if found == t.pid {
            Ok(())
        } else {
            Err(format!("find_pid returned {found}, want {}", t.pid))
        }
    });

    check(
        failures,
        "registry publishes randomized object names",
        || {
            // Strict read: no fallback to the compiled-in names, so a driver
            // that never wrote the key fails this check.
            let names = ks_sdk::published_object_names_strict()
                .ok_or_else(|| "HKLM\\SOFTWARE\\KernelScript values missing".to_string())?;
            // Each published name must be the client-side default plus the
            // driver's `-<16 hex>` startup token: prefix intact, token well
            // formed, and the plain default (token-less) never published.
            let defaults = [
                ks_core::ring::SECTION_CLIENT_NAME,
                ks_core::ring::REQUEST_EVENT_CLIENT_NAME,
                ks_core::ring::RESPONSE_EVENT_CLIENT_NAME,
            ];
            let prefixes = [
                "Global\\KernelScriptSection-",
                "Global\\KernelScriptRequest-",
                "Global\\KernelScriptResponse-",
            ];
            for (index, name) in names.iter().enumerate() {
                let prefix = prefixes[index];
                let Some(token) = name.strip_prefix(prefix) else {
                    return Err(format!(
                        "registry value {index} is {name:?}, want {prefix}<16 hex>"
                    ));
                };
                if token.len() != 16 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(format!(
                        "registry value {index} token {token:?} is not 16 hex chars"
                    ));
                }
                if name.as_str() == defaults[index] {
                    return Err(format!(
                        "registry value {index} published the fixed default"
                    ));
                }
            }
            Ok(())
        },
    );

    check(failures, "write_i32/read_i32 roundtrip", || {
        let address = t.slot(0x400);
        ks_sdk::write_bytes(t.pid, address, &0x1234_5678i32.to_le_bytes(), false, false)
            .map_err(|e| format!("write: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read: {e}"))?;
        if u32_at(&data, 0) == 0x1234_5678 {
            Ok(())
        } else {
            Err(format!("read back 0x{:X}", u32_at(&data, 0)))
        }
    });

    check(failures, "read_bytes(64) content", || {
        read_is(t, t.slot(0x40), &t.buffer[0x40..0x80])
    });

    check(failures, "write_bytes/read_bytes roundtrip (2 KiB)", || {
        // 2 KiB at 0x800: exactly to the end of the buffer, clear of the
        // bookkeeping fields at the front.
        let address = t.slot(0x800);
        let pattern: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
        ks_sdk::write_bytes(t.pid, address, &pattern, false, false)
            .map_err(|e| format!("write: {e}"))?;
        read_is(t, address, &pattern)
    });

    check(failures, "read size > 4096 rejected", || {
        // The protocol caps single reads/writes at 4096 bytes; 4097 must
        // fail client-side validation before anything reaches the driver.
        match ks_sdk::read_bytes(t.pid, t.slot(0), 4097, false, false) {
            Ok(_) => Err("4097-byte read was accepted".into()),
            Err(_) => Ok(()),
        }
    });

    check(failures, "read_rva(0) == MZ", || {
        let data =
            ks_sdk::read_bytes(t.pid, 0, 2, true, false).map_err(|e| format!("read: {e}"))?;
        if data.as_slice() == b"MZ" {
            Ok(())
        } else {
            Err(format!("image header reads {data:02X?}"))
        }
    });

    check(failures, "write_rva/read_rva roundtrip (image)", || {
        let value = 0xABCD_1234u32;
        ks_sdk::write_bytes(t.pid, t.rva_target_rva, &value.to_le_bytes(), true, false)
            .map_err(|e| format!("write_rva: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, t.rva_target_rva, 4, true, false)
            .map_err(|e| format!("read_rva: {e}"))?;
        if u32_at(&data, 0) != value {
            return Err(format!("rva read back 0x{:X}", u32_at(&data, 0)));
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen == value {
            Ok(())
        } else {
            Err(format!("user view is 0x{seen:X}, want 0x{value:X}"))
        }
    });

    check(
        failures,
        "write_mdl_rva(0) identity roundtrip (image header)",
        || {
            // address == 0 with the RVA flag is legal (RVA 0 = module base).
            // The DOS header page is read-only, so the identity write goes
            // through the MDL-remap path, which must bypass page protection.
            let data = ks_sdk::read_bytes(t.pid, 0, 2, true, false)
                .map_err(|e| format!("read_rva: {e}"))?;
            if data.as_slice() != b"MZ" {
                return Err(format!("header reads {data:02X?}"));
            }
            ks_sdk::write_bytes(t.pid, 0, &data, true, true)
                .map_err(|e| format!("write_mdl_rva(0): {e}"))?;
            let again = ks_sdk::read_bytes(t.pid, 0, 2, true, false)
                .map_err(|e| format!("re-read: {e}"))?;
            if again == data {
                Ok(())
            } else {
                Err(format!("header now reads {again:02X?}"))
            }
        },
    );

    check(failures, "read_mdl roundtrip", || {
        // Seed through the plain path, then read the same bytes back
        // through the MDL-remap path.
        let address = t.slot(0x40);
        let pattern: Vec<u8> = (0..=255u8).cycle().take(512).collect();
        ks_sdk::write_bytes(t.pid, address, &pattern, false, false)
            .map_err(|e| format!("seed write: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, address, pattern.len(), false, true)
            .map_err(|e| format!("mdl read: {e}"))?;
        if data.as_slice() == pattern.as_slice() {
            Ok(())
        } else {
            Err("mdl read content mismatch".into())
        }
    });

    check(failures, "write_mdl/read_mdl roundtrip", || {
        let address = t.slot(0x40);
        let pattern = vec![0x5Au8; 256];
        ks_sdk::write_bytes(t.pid, address, &pattern, false, true)
            .map_err(|e| format!("mdl write: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, address, pattern.len(), false, false)
            .map_err(|e| format!("plain read: {e}"))?;
        if data.as_slice() == pattern.as_slice() {
            Ok(())
        } else {
            Err("plain read after mdl write mismatch".into())
        }
    });

    check(
        failures,
        "write_mdl_rva/read_mdl_rva roundtrip (image)",
        || {
            let value = 0x7777_7777u32;
            ks_sdk::write_bytes(t.pid, t.rva_target_rva, &value.to_le_bytes(), true, true)
                .map_err(|e| format!("write_mdl_rva: {e}"))?;
            let data = ks_sdk::read_bytes(t.pid, t.rva_target_rva, 4, true, true)
                .map_err(|e| format!("read_mdl_rva: {e}"))?;
            if u32_at(&data, 0) != value {
                return Err(format!("mdl_rva read back 0x{:X}", u32_at(&data, 0)));
            }
            let seen = RVA_TARGET.load(Ordering::SeqCst);
            if seen == value {
                Ok(())
            } else {
                Err(format!("user view is 0x{seen:X}, want 0x{value:X}"))
            }
        },
    );

    check(failures, "batch_read content", || {
        let addresses: Vec<u64> = (0..8).map(|i| t.slot(0x800 + i * 4)).collect();
        let seeds: Vec<u32> = (0..8u32).map(|i| 0x1000_0000 + i).collect();
        for (index, seed) in seeds.iter().enumerate() {
            ks_sdk::write_bytes(t.pid, addresses[index], &seed.to_le_bytes(), false, false)
                .map_err(|e| format!("seed write {index}: {e}"))?;
        }
        let data =
            ks_sdk::batch_read(t.pid, 4, &addresses).map_err(|e| format!("batch_read: {e}"))?;
        if data.len() != addresses.len() * 4 {
            return Err(format!("returned {} bytes", data.len()));
        }
        for (index, seed) in seeds.iter().enumerate() {
            if u32_at(&data, index * 4) != *seed {
                return Err(format!("slot {index} mismatch"));
            }
        }
        Ok(())
    });

    check(failures, "batch_write statuses + readback", || {
        let addresses: Vec<u64> = (0..8).map(|i| t.slot(0x880 + i * 4)).collect();
        let entries: Vec<(u64, Vec<u8>)> = (0..8u32)
            .map(|i| {
                (
                    addresses[i as usize],
                    (0x2000_0000u32 + i).to_le_bytes().to_vec(),
                )
            })
            .collect();
        let statuses =
            ks_sdk::batch_write(t.pid, &entries).map_err(|e| format!("batch_write: {e}"))?;
        if statuses.len() != entries.len() {
            return Err(format!(
                "{} statuses for {} entries",
                statuses.len(),
                entries.len()
            ));
        }
        if let Some(bad) = statuses.iter().position(|&status| status != 0) {
            return Err(format!("entry {bad} status {:#x}", statuses[bad]));
        }
        for (index, entry) in entries.iter().enumerate() {
            let data = ks_sdk::read_bytes(t.pid, entry.0, 4, false, false)
                .map_err(|e| format!("readback {index}: {e}"))?;
            if u32_at(&data, 0) != 0x2000_0000 + index as u32 {
                return Err(format!("slot {index} mismatch after batch write"));
            }
        }
        Ok(())
    });

    check(failures, "batch_write rejects 65 entries", || {
        let entries: Vec<(u64, Vec<u8>)> = (0..65u64)
            .map(|i| (t.slot(0xC00 + i * 4), vec![0u8; 4]))
            .collect();
        match ks_sdk::batch_write(t.pid, &entries) {
            Err(_) => Ok(()),
            Ok(_) => Err("65-entry batch was accepted".into()),
        }
    });

    check(failures, "traverse_pointer_chain", || {
        let hop1 = t.buffer_addr + 0x300;
        let hop2 = t.buffer_addr + 0x200;
        ks_sdk::write_bytes(t.pid, t.slot(0x100), &hop1.to_le_bytes(), false, false)
            .map_err(|e| format!("seed hop1: {e}"))?;
        ks_sdk::write_bytes(t.pid, t.slot(0x300), &hop2.to_le_bytes(), false, false)
            .map_err(|e| format!("seed hop2: {e}"))?;
        let final_address = ks_sdk::traverse_pointer_chain(t.pid, t.slot(0x0), &[0x100, 0x0])
            .map_err(|e| format!("traverse: {e}"))?;
        if final_address == t.slot(0x200) {
            Ok(())
        } else {
            Err(format!(
                "walked to 0x{final_address:X}, want 0x{:X}",
                t.slot(0x200)
            ))
        }
    });

    check(failures, "traverse chain limit enforced", || {
        // 33 offsets exceed MAX_CHAIN_OFFSETS; the protocol must reject the
        // request before it reaches the driver.
        let offsets: Vec<u64> = (0..33).map(|i| 8 * i as u64).collect();
        match ks_sdk::traverse_pointer_chain(t.pid, t.slot(0x0), &offsets) {
            Err(_) => Ok(()),
            Ok(_) => Err("33-offset chain was accepted".into()),
        }
    });

    check(failures, "lock lifecycle (driver rewrite + unlock)", || {
        let locked = 0x1111_1111u32;
        let foreign = 0x2222_2222u32;
        let freed = 0x3333_3333u32;
        let address = t.slot(0x700);
        let rva_locked = 0x5555_5555u32;

        ks_sdk::lock(1, t.pid, address, &locked.to_le_bytes()).map_err(|e| format!("lock: {e}"))?;
        // The RVA lock exercises lock_rva/unlock_rva on the image static.
        ks_sdk::lock_rva(2, t.pid, t.rva_target_rva, &rva_locked.to_le_bytes())
            .map_err(|e| format!("lock_rva: {e}"))?;
        thread::sleep(LOCK_SETTLE);

        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read locked: {e}"))?;
        if u32_at(&data, 0) != locked {
            return Err(format!("locked slot reads 0x{:X}", u32_at(&data, 0)));
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen != rva_locked {
            return Err(format!("rva locked static reads 0x{seen:X}"));
        }

        // A plain user-mode write must lose against the driver rewrite.
        ks_sdk::write_bytes(t.pid, address, &foreign.to_le_bytes(), false, false)
            .map_err(|e| format!("foreign write: {e}"))?;
        RVA_TARGET.store(0x6666_6666, Ordering::SeqCst);
        thread::sleep(LOCK_SETTLE);
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read after foreign write: {e}"))?;
        if u32_at(&data, 0) != locked {
            return Err("driver rewrite did not restore the locked value".into());
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen != rva_locked {
            return Err(format!(
                "rva driver rewrite did not restore 0x{rva_locked:X}"
            ));
        }

        ks_sdk::unlock(1).map_err(|e| format!("unlock: {e}"))?;
        ks_sdk::unlock_rva(2).map_err(|e| format!("unlock_rva: {e}"))?;
        // Sweep-clear for this pid; harmless when the table is already
        // empty.
        ks_sdk::unlock_all(t.pid).map_err(|e| format!("unlock_all: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        ks_sdk::write_bytes(t.pid, address, &freed.to_le_bytes(), false, false)
            .map_err(|e| format!("write after unlock: {e}"))?;
        RVA_TARGET.store(0x8888_8888, Ordering::SeqCst);
        thread::sleep(LOCK_SETTLE);
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read after unlock: {e}"))?;
        if u32_at(&data, 0) != freed {
            return Err("driver rewrite kept running after unlock".into());
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen != 0x8888_8888 {
            return Err("rva driver rewrite kept running after unlock_rva".into());
        }
        Ok(())
    });

    check(failures, "lock table limit enforced", || {
        const SLOTS: u64 = ks_sdk::MAX_MEMORY_LOCKS as u64;
        // Fill every driver-side lock slot with a distinct 4-byte entry.
        for index in 0..SLOTS {
            ks_sdk::lock(
                index + 1,
                t.pid,
                t.slot(0x800 + index * 4),
                &0xA5A5_5A5Au32.to_le_bytes(),
            )
            .map_err(|e| format!("lock {}/{}: {e}", index + 1, SLOTS))?;
        }
        // A 65th distinct id must be rejected by the driver's quota; the
        // client maps STATUS_QUOTA_EXCEEDED back to TooManyEntries.
        match ks_sdk::lock(SLOTS + 1, t.pid, t.slot(0xF00), b"full") {
            Err(ks_sdk::LinkError::TooManyEntries { limit })
                if limit == ks_sdk::MAX_MEMORY_LOCKS => {}
            Err(error) => return Err(format!("65th lock failed as: {error}")),
            Ok(()) => return Err("65th lock was accepted".into()),
        }
        thread::sleep(LOCK_SETTLE);
        // All 64 entries are held: a foreign write must lose to the sweep.
        ks_sdk::write_bytes(t.pid, t.slot(0x800), &0u32.to_le_bytes(), false, false)
            .map_err(|e| format!("foreign write: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        let held = ks_sdk::read_bytes(t.pid, t.slot(0x800), 4, false, false)
            .map_err(|e| format!("read after foreign write: {e}"))?;
        if u32_at(&held, 0) != 0xA5A5_5A5A {
            return Err(format!("slot 0 not held: reads 0x{:X}", u32_at(&held, 0)));
        }
        // unlock_all clears the whole table for this pid: afterwards a
        // write must stick with no rewrite racing it.
        ks_sdk::unlock_all(t.pid).map_err(|e| format!("unlock_all: {e}"))?;
        ks_sdk::write_bytes(
            t.pid,
            t.slot(0x800),
            &0xDEAD_BEEFu32.to_le_bytes(),
            false,
            false,
        )
        .map_err(|e| format!("write after unlock_all: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        let cleared = ks_sdk::read_bytes(t.pid, t.slot(0x800), 4, false, false)
            .map_err(|e| format!("read after unlock_all: {e}"))?;
        if u32_at(&cleared, 0) != 0xDEAD_BEEF {
            return Err("a lock kept rewriting after unlock_all".into());
        }
        Ok(())
    });

    check(failures, "concurrent ring access", || {
        run_concurrency_smoke(target)
    });
}

/// Several threads hammer the single-slot ring through the client mutex:
/// readers keep verifying the magic word while writers churn dedicated
/// slots. Fails if any operation errors, a reader observes corruption, or
/// a writer slot ends up holding another writer's value.
fn run_concurrency_smoke(target: &Arc<Target>) -> Result<(), String> {
    const READS_PER_READER: usize = 150;
    const WRITES_PER_WRITER: usize = 100;
    const MAGIC: u32 = 0x1234_5678;

    let mut handles = Vec::new();

    for reader in 0..2usize {
        let target = Arc::clone(target);
        handles.push(thread::spawn(move || -> Result<(), String> {
            for _ in 0..READS_PER_READER {
                let data = ks_sdk::read_bytes(target.pid, target.slot(0x0), 4, false, false)
                    .map_err(|e| format!("reader {reader}: {e}"))?;
                if u32_at(&data, 0) != MAGIC {
                    return Err(format!("reader {reader}: magic slot corrupted"));
                }
            }
            Ok(())
        }));
    }

    for writer in 0..2usize {
        let target = Arc::clone(target);
        handles.push(thread::spawn(move || -> Result<(), String> {
            let address = target.slot(0x600 + writer as u64 * 4);
            for iteration in 0..WRITES_PER_WRITER as u32 {
                let value = ((writer as u32 + 1) << 16) | iteration;
                ks_sdk::write_bytes(target.pid, address, &value.to_le_bytes(), false, false)
                    .map_err(|e| format!("writer {writer}: {e}"))?;
            }
            Ok(())
        }));
    }

    // The main thread joins the contention with its own reads.
    for _ in 0..50 {
        ks_sdk::read_bytes(target.pid, target.slot(0x40), 64, false, false)
            .map_err(|e| format!("main thread read: {e}"))?;
    }

    let mut error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => error = Some(err),
            Err(_) => error = Some("worker thread panicked".into()),
        }
    }
    if let Some(error) = error {
        return Err(error);
    }

    for writer in 0..2usize {
        let address = target.slot(0x600 + writer as u64 * 4);
        let data = ks_sdk::read_bytes(target.pid, address, 4, false, false)
            .map_err(|e| format!("final read slot {writer}: {e}"))?;
        let value = u32_at(&data, 0);
        if (value >> 16) as usize != writer + 1 {
            return Err(format!("writer slot {writer} holds 0x{value:X}"));
        }
    }
    Ok(())
}

struct Stats {
    label: String,
    times_us: Vec<u64>,
    errors: usize,
}

impl Stats {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            times_us: Vec::new(),
            errors: 0,
        }
    }

    fn report(&self) {
        if self.times_us.is_empty() {
            say(&format!(
                "  {:<24} no samples ({} error(s))",
                self.label, self.errors
            ));
            return;
        }
        let mut sorted = self.times_us.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let sum: u64 = sorted.iter().sum();
        let avg = sum / n as u64;
        let p50 = sorted[n / 2];
        let p99 = sorted[((n as f64 * 0.99) as usize).min(n - 1)];
        let min = sorted[0];
        let max = sorted[n - 1];
        let ops_per_sec = if avg > 0 {
            1_000_000.0 / avg as f64
        } else {
            0.0
        };
        let errors = if self.errors > 0 {
            format!("  [{} errors]", self.errors)
        } else {
            String::new()
        };
        say(&format!(
            "  {:<24} n={:<5} avg={:>7} us  p50={:>7} us  p99={:>7} us  min={:>7} us  max={:>7} us  {:.0} ops/s{}",
            self.label, n, avg, p50, p99, min, max, ops_per_sec, errors
        ));
    }
}

/// Times `iters` executions of `op`; op errors are counted, printed once
/// and never abort the benchmark (correctness is the suite's job).
fn bench(label: &str, iters: usize, mut op: impl FnMut() -> Result<(), String>) -> Stats {
    say(&format!("RUN  bench {label}"));
    let mut stats = Stats::new(label);
    for _ in 0..iters {
        let t0 = Instant::now();
        match op() {
            Ok(()) => stats.times_us.push(t0.elapsed().as_micros() as u64),
            Err(error) => {
                stats.errors += 1;
                if stats.errors == 1 {
                    say(&format!("    [{label}] {error}"));
                }
            }
        }
    }
    stats
}

fn run_benchmarks(t: &Target, iters: usize) {
    say("--- Single Read Benchmarks ---");
    bench("ReadMemory(4)", iters, || {
        let data =
            ks_sdk::read_bytes(t.pid, t.slot(0x0), 4, false, false).map_err(|e| e.to_string())?;
        if data.len() == 4 {
            Ok(())
        } else {
            Err("short read".into())
        }
    })
    .report();

    bench("ReadMemory(64)", iters, || {
        let data =
            ks_sdk::read_bytes(t.pid, t.slot(0x40), 64, false, false).map_err(|e| e.to_string())?;
        if data == t.buffer[0x40..0x80] {
            Ok(())
        } else {
            Err("content mismatch".into())
        }
    })
    .report();

    bench("ReadMemory(4096)", iters, || {
        let data = ks_sdk::read_bytes(t.pid, t.slot(0x0), 4096, false, false)
            .map_err(|e| e.to_string())?;
        if data == t.buffer {
            Ok(())
        } else {
            Err("content mismatch".into())
        }
    })
    .report();
    say("");

    say("--- Alternate Path Benchmarks ---");
    bench("ReadMdl(4)", iters, || {
        ks_sdk::read_bytes(t.pid, t.slot(0x0), 4, false, true)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();

    bench("ReadMdl(4096)", iters, || {
        ks_sdk::read_bytes(t.pid, t.slot(0x0), 4096, false, true)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();

    bench("ReadRva(4)", iters, || {
        ks_sdk::read_bytes(t.pid, t.rva_target_rva, 4, true, false)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();

    bench("WriteMemory(4)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0x400), &0u32.to_le_bytes(), false, false)
            .map_err(|e| e.to_string())
    })
    .report();

    bench("WriteMdl(4)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0x400), &0u32.to_le_bytes(), false, true)
            .map_err(|e| e.to_string())
    })
    .report();

    bench("WriteRva(4)", iters, || {
        ks_sdk::write_bytes(t.pid, t.rva_target_rva, &0u32.to_le_bytes(), true, false)
            .map_err(|e| e.to_string())
    })
    .report();

    // Full-size writes: identity pushes of the whole target buffer, so the
    // pointer-chain fields at 0x100/0x200 are restored rather than
    // disturbed and the later benchmarks still see a walkable chain.
    bench("WriteMemory(4096)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0), &t.buffer, false, false).map_err(|e| e.to_string())
    })
    .report();

    bench("WriteMdl(4096)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0), &t.buffer, false, true).map_err(|e| e.to_string())
    })
    .report();
    say("");

    say("--- Batch Benchmarks ---");
    let batch_50: Vec<u64> = (0..50).map(|i| t.slot(0x900 + i * 4)).collect();
    bench("BatchRead(50x4)", iters, || {
        let data = ks_sdk::batch_read(t.pid, 4, &batch_50).map_err(|e| e.to_string())?;
        if data.len() == batch_50.len() * 4 {
            Ok(())
        } else {
            Err("short batch".into())
        }
    })
    .report();

    let batch_200: Vec<u64> = (0..200).map(|i| t.slot(i * 2)).collect();
    bench("BatchRead(200x12)", iters, || {
        let data = ks_sdk::batch_read(t.pid, 12, &batch_200).map_err(|e| e.to_string())?;
        if data.len() == batch_200.len() * 12 {
            Ok(())
        } else {
            Err("short batch".into())
        }
    })
    .report();

    let write_entries: Vec<(u64, Vec<u8>)> = (0..64u64)
        .map(|i| (t.slot(0xA00 + i * 4), vec![0xAA; 4]))
        .collect();
    bench("BatchWrite(64x4)", iters, || {
        let statuses = ks_sdk::batch_write(t.pid, &write_entries).map_err(|e| e.to_string())?;
        if statuses.iter().all(|&status| status == 0) {
            Ok(())
        } else {
            Err("non-zero entry status".into())
        }
    })
    .report();
    say("");

    say("--- Pointer Walk Benchmarks ---");
    bench("PtrWalk(1 offs)", iters, || {
        ks_sdk::traverse_pointer_chain(t.pid, t.slot(0x0), &[0x100])
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();
    say("");

    say("--- Sequential Chain (3 x ReadI32) ---");
    bench("Seq3_ReadI32", iters, || {
        for offset in [0x0u64, 0x100, 0x200] {
            ks_sdk::read_bytes(t.pid, t.slot(offset), 4, false, false)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .report();
}

/// `ks-test loadone <provider> <victim>`: one pinned provider × victim
/// attempt in a FRESH process. The matrix parent spawns this per
/// combination because a ks-link session binds to one driver
/// generation and never reconnects — a second combo in the same
/// process would talk to a dead ring.
fn load_one_standalone(provider: u32, victim: u32) -> i32 {
    std::env::set_var("KS_SDK_MAP", "1");
    std::env::remove_var("KS_SDK_KDU_PRV");
    std::env::remove_var("KS_TEST_KDU_PRV");
    std::env::remove_var("KS_SDK_VICTIM");
    if let Err(error) = ks_sdk::start_with(Some(provider), Some(victim)) {
        eprintln!("loadone: start failed: {error}");
        return 1;
    }
    if let Err(error) = ks_sdk::ping() {
        eprintln!("loadone: loaded but ping failed: {error}");
        let _ = ks_sdk::stop();
        return 1;
    }
    println!("loadone: provider {provider} × victim {victim}: loaded, ring answers");
    let _ = ks_sdk::stop();
    0
}

/// `ks-test load`: sweep every retained provider × victim build through
/// `ks_sdk::start_with` on the manual-map path and report the matrix.
///
/// Each combination runs in a fresh `loadone` child process (one
/// ks-link session per driver generation) and gets a full [`ks_sdk::stop`].
/// Exit 0 only when every combination passed.
fn run_load_matrix() -> i32 {
    std::env::set_var("KS_SDK_MAP", "1");
    std::env::remove_var("KS_SDK_KDU_PRV");
    std::env::remove_var("KS_TEST_KDU_PRV");
    std::env::remove_var("KS_SDK_VICTIM");

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("load matrix: current exe: {error}");
            return 2;
        }
    };
    let providers = ks_sdk::provider_ids();
    let victims = ks_sdk::victim_builds();
    say(&format!(
        "load matrix: {} providers × {} victims, manual mapping only",
        providers.len(),
        victims.len()
    ));

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut results: Vec<String> = Vec::new();

    for provider in &providers {
        for victim in &victims {
            say(&format!("=== provider {provider} × victim {victim} ==="));
            let output = Command::new(&exe)
                .args(["loadone", &provider.to_string(), &victim.to_string()])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            let outcome = match output {
                Ok(output) if output.status.success() => {
                    pass += 1;
                    "PASS".to_string()
                }
                Ok(output) => {
                    fail += 1;
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let last = text.lines().last().unwrap_or("no output").to_string();
                    format!("FAIL ({last})")
                }
                Err(error) => {
                    fail += 1;
                    format!("FAIL (spawn: {error})")
                }
            };
            say(&format!("    {outcome}"));
            results.push(format!(
                "  provider {provider} × victim {victim}: {outcome}"
            ));
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    say("");
    say("--- load matrix results ---");
    for line in &results {
        say(line);
    }
    say(&format!(
        "load matrix: {pass} pass, {fail} fail (of {})",
        pass + fail
    ));
    if fail == 0 {
        0
    } else {
        1
    }
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let full = args.iter().any(|a| a == "full");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    // Default load path is the service load with manual-map fallback;
    // `ks-test sc ...` selects the legacy SCM lifecycle for regression
    // runs, `benchmark` runs the performance suite only, and `load`
    // sweeps every provider × victim combination through start_with.
    let legacy_sc = args.iter().any(|a| a == "sc");

    if args.iter().any(|a| a == "load") {
        return run_load_matrix();
    }

    // Held (not dropped) until the end of `run`, so the driver stays loaded
    // while the harness executes. Full mode ends with the checked teardown
    // (per mode: `sc stop`/`sc query`/`sc delete`, or the instance-claim
    // checks); other paths fall back to Drop.
    let mut embedded = match EmbeddedDriver::start(legacy_sc) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start embedded driver: {error}");
            return 2;
        }
    };
    match &embedded.mode {
        LoadMode::Kdu => say("driver load mode: sdk (service load first, manual-map fallback)"),
        LoadMode::Sc { service } => {
            say(&format!("driver load mode: sc (legacy service {service})"));
        }
    }

    say("ks-test: self-process correctness + benchmark harness");
    if let Err(error) = wait_for_driver() {
        eprintln!("{error}");
        return 2;
    }

    let target = match Target::setup() {
        Ok(target) => Arc::new(target),
        Err(error) => {
            say(&format!("target setup failed: {error}"));
            return 2;
        }
    };
    say(&format!(
        "target pid: {}  image base: 0x{:X}  buffer: 0x{:X}  iters: {iters}",
        target.pid, target.image_base, target.buffer_addr
    ));
    say("");

    let mut failures = Vec::new();

    if args.iter().any(|a| a == "benchmark") {
        say("mode: benchmark (performance suite only; `ks-test full` adds correctness)");
        say("");
        run_benchmarks(&target, iters);
        return 0;
    }

    if !full {
        // Minimal mode: transport (ping/getbase) plus normal non-MDL reads
        // only. A bugcheck here implicates the ring/request path; passing
        // means the transport is sound and later suspects (mdl/write/batch)
        // can be reintroduced one at a time. `ks-test full` runs the suite.
        say("mode: minimal (normal reads only; `ks-test full` = full suite)");
        check(&mut failures, "ping", || {
            ks_sdk::ping().map_err(|e| e.to_string())
        });
        check(&mut failures, "get_process_base == image base", || {
            if target.driver_base == target.image_base {
                Ok(())
            } else {
                Err(format!(
                    "driver reports 0x{:X}, user mode sees 0x{:X}",
                    target.driver_base, target.image_base
                ))
            }
        });
        check(&mut failures, "read_bytes(4) content", || {
            read_is(&target, target.slot(0x400), &target.buffer[0x400..0x404])
        });
        check(&mut failures, "read_bytes(64) content", || {
            read_is(&target, target.slot(0x40), &target.buffer[0x40..0x80])
        });
        check(&mut failures, "read_bytes(4096) content", || {
            read_is(&target, target.slot(0), &target.buffer)
        });
        say("");
        if failures.is_empty() {
            say("=== ALL CHECKS PASSED (minimal read-only) ===");
        } else {
            say(&format!("=== {} CHECK(S) FAILED ===", failures.len()));
            for (name, error) in &failures {
                say(&format!("  {name}: {error}"));
            }
        }
        return if failures.is_empty() { 0 } else { 1 };
    }

    // Single-instance guard: a second driver load while this one is live
    // must fail inside DriverEntry (the marker object is openable ->
    // STATUS_OBJECT_NAME_COLLISION), and the live instance must keep
    // answering afterwards. KDU mode re-runs the mapper (the second image
    // never reaches a service); the legacy path uses a duplicate service.
    check(&mut failures, "second driver instance rejected", || {
        match &embedded.mode {
            LoadMode::Kdu => {
                match ks_sdk::start() {
                    Ok(()) => return Err("second instance mapped (guard missing)".into()),
                    // STATUS_OBJECT_NAME_COLLISION, exactly what the
                    // marker probe returns.
                    Err(ks_sdk::Error::DriverEntry(0xC000_0035)) => {}
                    Err(ks_sdk::Error::DriverEntry(status)) => {
                        return Err(format!(
                            "second map failed for the wrong reason (0x{status:08X}, \
                             expected 0xC0000035)"
                        ));
                    }
                    Err(error) => return Err(format!("second map failed: {error}")),
                }
                ks_sdk::ping().map_err(|e| format!("ping after rejected load: {e}"))
            }
            LoadMode::Sc { service } => {
                let duplicate_service = format!("{service}dup");
                // A separate copy at a distinct path: same bytes,
                // independent load, so the guard (not loader path
                // aliasing) is what rejects it.
                let duplicate_image = embedded.root.join("ks-driver-dup.sys");
                fs::copy(embedded.root.join("ks-driver.sys"), &duplicate_image)
                    .map_err(|e| format!("copy image: {e}"))?;
                let image_arg = duplicate_image.to_string_lossy().into_owned();
                if let Err(error) = sc(&[
                    "create",
                    &duplicate_service,
                    "type=",
                    "kernel",
                    "start=",
                    "demand",
                    "binPath=",
                    &image_arg,
                ]) {
                    let _ = fs::remove_file(&duplicate_image);
                    return Err(format!("sc create: {error}"));
                }
                let started = sc(&["start", &duplicate_service]);
                let _ = sc(&["delete", &duplicate_service]);
                let _ = fs::remove_file(&duplicate_image);
                let error = match started {
                    Err(error) => error,
                    Ok(()) => return Err("second instance started (guard missing)".into()),
                };
                match sc_failure_code(&error) {
                    Some(183) => {}
                    other => {
                        return Err(format!(
                            "second start failed for the wrong reason (code {other:?}): \
                             {error}"
                        ));
                    }
                }
                ks_sdk::ping().map_err(|e| format!("ping after rejected load: {e}"))
            }
        }
    });

    run_checks(&target, &mut failures);

    if failures.is_empty() {
        say("");
        run_benchmarks(&target, iters);
    } else {
        say("");
        say("benchmarks skipped: correctness failures present");
    }

    // The shutdown command is destructive (it stops the worker), so it runs
    // last, after the benchmarks. In KDU mode it is also what releases the
    // single-instance claim, because no unload path exists.
    let mut shutdown_ok = false;
    if failures.is_empty() {
        say("");
        check(&mut failures, "shutdown command stops the worker", || {
            ks_sdk::shutdown().map_err(|e| e.to_string())
        });
        shutdown_ok = failures.is_empty();
        check(
            &mut failures,
            "requests time out after shutdown",
            || match ks_sdk::ping() {
                Err(_) => Ok(()),
                Ok(()) => Err("ping succeeded after shutdown".into()),
            },
        );
    }

    // Mode-specific teardown. Both run even when earlier checks failed, so
    // no instance is ever left behind.
    say("");
    match embedded.mode.clone() {
        // Legacy SCM: after shutdown the worker is gone, so `sc stop` must
        // complete the unload (service reports STOPPED) and `sc delete`
        // must remove the service. With a live worker this exercises the
        // normal unload path instead.
        LoadMode::Sc { .. } => {
            let (stop_outcome, delete_outcome) = embedded.teardown();
            check(&mut failures, "sc stop: service reports STOPPED", || {
                stop_outcome
            });
            check(&mut failures, "sc delete: service removed", || {
                delete_outcome
            });
            // DriverUnload runs inside `sc stop`, so an unloaded driver has
            // already erased its registry publication.
            check(
                &mut failures,
                "unload removed the published object names",
                || match ks_sdk::published_object_names_strict() {
                    None => Ok(()),
                    Some(_) => Err("object names still published".into()),
                },
            );
        }
        // KDU: no service exists. The invariants are that shutdown
        // released the single-instance claim and the registry publication,
        // that the driver maps again afterwards (the marker and ring are
        // really gone, no reboot needed), and that nothing live remains
        // behind.
        LoadMode::Kdu => {
            if shutdown_ok {
                check(
                    &mut failures,
                    "shutdown released the single-instance claim",
                    || match ks_sdk::instance_claim_present() {
                        Ok(false) => Ok(()),
                        Ok(true) => Err("Instance claim still present".into()),
                        Err(error) => Err(error.to_string()),
                    },
                );
                check(
                    &mut failures,
                    "shutdown removed the published object names",
                    || match ks_sdk::published_object_names_strict() {
                        None => Ok(()),
                        Some(_) => Err("object names still published".into()),
                    },
                );
            }
            if failures.is_empty() {
                check(&mut failures, "driver re-maps after shutdown", || {
                    ks_sdk::start().map_err(|e| format!("ks_sdk::start: {e}"))?;
                    // This process's session still points at the dead ring,
                    // so the fresh instance is stopped through a child.
                    run_shutdown_child()
                });
            }
            check(&mut failures, "no live driver instance remains", || {
                run_shutdown_child()
            });
            embedded.finished = true;
        }
    }

    say("");
    if failures.is_empty() {
        say("=== ALL CHECKS PASSED ===");
    } else {
        say(&format!("=== {} CHECK(S) FAILED ===", failures.len()));
        for (name, error) in &failures {
            say(&format!("  {name}: {error}"));
        }
    }
    // The instance is already torn down and verified by the mode-specific
    // block above; `embedded` drops here and only removes the temp
    // directory before main decides about the pause prompt.
    if failures.is_empty() {
        0
    } else {
        1
    }
}

fn main() {
    open_step_log();
    // Route the KDU mapper's step-log lines into the step log with the
    // historic `kdu: ` prefix.
    ks_sdk::kdu::set_log_sink(|line| say(&format!("kdu: {line}")));
    // `ks-test shutdown` is the standalone recovery/cleanup channel (see
    // `shutdown_standalone`): it must never pause for input, so it exits
    // before run()'s interactive paths.
    if std::env::args().nth(1).as_deref() == Some("shutdown") {
        std::process::exit(shutdown_standalone());
    }
    // `ks-test loadone <provider> <victim>`: one pinned matrix cell,
    // driven by `run_load_matrix` (fresh process per driver generation).
    if std::env::args().nth(1).as_deref() == Some("loadone") {
        let parsed = (|| {
            let provider: u32 = std::env::args().nth(2)?.parse().ok()?;
            let victim: u32 = std::env::args().nth(3)?.parse().ok()?;
            Some((provider, victim))
        })();
        match parsed {
            Some((provider, victim)) => std::process::exit(load_one_standalone(provider, victim)),
            None => {
                eprintln!("usage: ks-test loadone <provider-id> <victim-build>");
                std::process::exit(2);
            }
        }
    }
    let failures = run();
    if failures != 0 {
        println!("Press Enter to exit.");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }
    std::process::exit(failures);
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
