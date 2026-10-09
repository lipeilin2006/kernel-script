//! Driver lifecycle for the harness itself: how the embedded image is
//! loaded (legacy SCM service or the SDK mapper), how teardown inverts it,
//! and the readiness wait for the published ring objects.

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::elevate::{cleanup_leftover, is_elevated};
use crate::sc::{sc, sc_state, ScState};
use crate::step_log::say;
use crate::CREATE_NO_WINDOW;

/// How long the harness waits for the driver ring to appear after the load
/// command reported success.
const DRIVER_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How the embedded driver image reaches the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoadMode {
    /// SDK path (the default): `ks_sdk::start()` runs the manual-map
    /// chain first (shellcode V3, `DriverEntry` inside a mapped image,
    /// no signature check) and falls back to a service load when the
    /// chain cannot run the driver. No unload either way — teardown is
    /// the ring `shutdown` request,
    /// which makes the driver release its single-instance claim and erase
    /// its registry publication itself.
    Kdu,
    /// Legacy SCM load (`ks-test sc ...`): `sc create`/`sc start` a kernel
    /// service; teardown is `sc stop`/`sc delete` after the ring
    /// `shutdown`.
    Sc { service: String },
}

pub(crate) struct EmbeddedDriver {
    pub(crate) root: PathBuf,
    pub(crate) mode: LoadMode,
    /// Set once the checked teardown ran so `Drop` does not repeat a
    /// (best-effort) cleanup.
    pub(crate) finished: bool,
}

impl EmbeddedDriver {
    /// Loads the embedded driver: the legacy SCM service when
    /// `legacy_sc` is set, otherwise `ks_sdk::start()` (the in-process
    /// KDU mapper). Both paths first clear a leftover live instance
    /// (see [`cleanup_leftover`](crate::elevate::cleanup_leftover)). Only
    /// the SCM path puts the image on
    /// disk — `sc create` needs a `binPath` — and it writes it into the
    /// process-local temp root below.
    pub(crate) fn start(legacy_sc: bool) -> Result<Self, String> {
        let token = start_token();
        let root = std::env::temp_dir().join(format!("kernel-script-{token}"));

        // A previous run that died before teardown can leave a live mapped
        // instance holding the single-instance marker. ks-link sessions
        // never reconnect, so that leftover must be stopped in its own
        // process before this one touches the ring.
        cleanup_leftover()?;

        let mode = if legacy_sc {
            start_sc_service(&root, &token)?
        } else {
            start_mapped_driver(&root)?
        };
        Ok(Self {
            root,
            mode,
            finished: false,
        })
    }

    /// The SCM service name (legacy [`LoadMode::Sc`] only).
    pub(crate) fn service(&self) -> Result<&str, String> {
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
    pub(crate) fn teardown(&mut self) -> (Result<(), String>, Result<(), String>) {
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

/// The per-run token embedded in the temp-root name (and, in legacy mode,
/// hashed into the service name), so parallel runs never collide.
fn start_token() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    )
}

/// Legacy SCM path: write the embedded image into `root` (the service
/// needs a `binPath` on disk), then `sc create`/`sc start` it. A failed
/// command removes the root (and the half-created service) again before
/// propagating.
fn start_sc_service(root: &Path, token: &str) -> Result<LoadMode, String> {
    fs::create_dir_all(root).map_err(|e| format!("create temp dir: {e}"))?;
    let driver_path = root.join("ks-driver.sys");
    fs::write(&driver_path, ks_sdk::DRIVER_IMAGE).map_err(|e| format!("write driver: {e}"))?;
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
        let _ = fs::remove_dir_all(root);
        return Err(error);
    }
    if let Err(error) = sc(&["start", &service]) {
        let _ = sc(&["delete", &service]);
        let _ = fs::remove_dir_all(root);
        return Err(error);
    }
    Ok(LoadMode::Sc { service })
}

/// SDK path: the mapper runs in-process against the SDK's embedded image
/// bytes; only KDU's extracted helper drivers need files (in their own
/// temporary root, created and removed by `start`). The harness's own
/// temp root is dropped again when the map fails.
fn start_mapped_driver(root: &Path) -> Result<LoadMode, String> {
    if let Err(error) = ks_sdk::start() {
        let _ = fs::remove_dir_all(root);
        return Err(format!("ks_sdk::start: {error}"));
    }
    Ok(LoadMode::Kdu)
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
pub(crate) fn shutdown_standalone() -> i32 {
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
        Ok(()) => shutdown_stop(),
    }
}

/// A live instance answered the ping: send the ring `shutdown` request,
/// then verify the worker really wound down.
fn shutdown_stop() -> i32 {
    if let Err(error) = ks_sdk::stop() {
        eprintln!("shutdown: shutdown request failed: {error}");
        return 1;
    }
    shutdown_verify_stopped()
}

/// After a successful `shutdown` request: the driver must stop answering,
/// release the single-instance claim and erase its published object names.
fn shutdown_verify_stopped() -> i32 {
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
                eprintln!("shutdown: driver stopped but the object names are still published");
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

/// Waits for the driver to publish its object names, then for the ring to
/// answer.
///
/// Readiness is the registry publication itself: `sc start` returning only
/// means the driver entry ran, while `HKLM\SOFTWARE\KernelScript` holding
/// all three `REG_SZ` values proves section, request event and response
/// event exist. The ping afterwards proves the worker thread serves.
///
/// Every state transition is written through [`say`](crate::step_log::say)
/// (not just stderr) so a
/// bugcheck pinpoints whether it happened inside the first blocked ping or
/// after a recorded failure.
pub(crate) fn wait_for_driver() -> Result<(), String> {
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
