//! In-process driver lifecycle: [`start`] and [`stop`].
//!
//! The mapper is a pure-Rust port of the KDU 1.5.0 map core that used to
//! be compiled into this crate (see `ks-sdk/tools/shellcode_dump.cpp` for
//! the one C++ artifact that remains: the extracted shellcode V3 machine
//! code in `assets/shellcode_v3.bin`). Both inputs stay in memory: the
//! target driver ([`DRIVER_IMAGE`]) and the embedded loader-driver blobs
//! (`assets/loader_drivers`). [`start`] maps the target without ever
//! writing it to disk — the only files this path creates are the helper
//! drivers a provider extracts, into a fresh temporary directory that is
//! the process working directory for the duration of the call and is
//! removed again afterwards.
//!
//! Shellcode version 3 is mandatory: it builds a real `DRIVER_OBJECT`
//! and calls `DriverEntry(driverObject, &regPath)` synchronously, so the
//! reported status is the entry's own.
//!
//! Teardown is the ring `shutdown` request (the mapper never unloads a
//! mapped image): [`stop`] submits it and waits for the driver to
//! release its single-instance claim. A loaded driver that is still
//! holding the claim rejects every further [`start`] with
//! [`Error::DriverEntry`]`(0xC0000035)`.

mod dispatch;
mod drivers;
mod env;
mod loader;
mod nt;
mod payload;
mod pe;
mod primitives;
mod provider;
mod sc;
mod shellcode;
mod superfetch;
mod victim;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ks_link::{instance_claim_present, published_object_names_strict, shutdown, LinkError};

/// The KernelScript driver image (`ks-driver.sys`), the bytes [`start`]
/// maps into the kernel. Embedded in the binary and only ever handed to
/// the mapper as memory — this SDK never writes the target image to disk.
pub const DRIVER_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ks-driver.sys"));

/// How long [`start`] waits for the driver to publish its object names
/// and [`stop`] waits for the single-instance claim to be released.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);

/// The provider whose map last succeeded in this process, or
/// `u32::MAX` for none yet. [`provider_candidates`] tries it first, so
/// a repeated [`start`] (ks-test's duplicate-load and post-shutdown
/// re-map checks) does not re-fail the ids that were rejected earlier
/// in the same run. Only a `DriverEntry` result updates it; a
/// provider-level failure leaves it untouched.
static LAST_GOOD_PROVIDER: AtomicU32 = AtomicU32::new(u32::MAX);

/// Consumer for the log lines the SDK emits itself (one line at a
/// time). Defaults to a `kdu: ` stdout print; ks-test installs a sink
/// that routes the lines into its step log.
type LogSink = Box<dyn Fn(&str) + Send + Sync>;
static LOG_SINK: RwLock<Option<LogSink>> = RwLock::new(None);

/// Replaces the log sink. The sink runs on the calling thread inside
/// [`start`]; do not call this from inside a sink (deadlock) and do not
/// let it block on anything the map call could be waiting for.
pub fn set_log_sink(sink: impl Fn(&str) + Send + Sync + 'static) {
    *LOG_SINK.write().unwrap_or_else(|error| error.into_inner()) = Some(Box::new(sink));
}

/// Delivers one log line to the installed sink, or prints it.
fn emit(line: &str) {
    let guard = LOG_SINK.read().unwrap_or_else(|error| error.into_inner());
    match guard.as_ref() {
        Some(sink) => sink(line),
        None => println!("kdu: {line}"),
    }
}

/// A driver-load failure, as reported by [`start`] or [`stop`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// The driver's `DriverEntry` returned this NTSTATUS, so the map
    /// itself failed. `0xC0000035` (`STATUS_OBJECT_NAME_COLLISION`) is
    /// the single-instance guard rejecting a load while another instance
    /// owns the marker.
    DriverEntry(u32),

    /// The driver loaded but a follow-up signal never appeared within
    /// [`LIFECYCLE_TIMEOUT`].
    NotReady {
        what: &'static str,
        waited: Duration,
    },
    /// Every provider failed and the service-load fallback was
    /// rejected: the image never ran. The status is what the loader
    /// reported (signature, policy).
    ScLoad(u32),
    /// An internal step failed before the map ran (temporary directory,
    /// working-directory switch).
    Setup(String),
    /// A ring round trip behind [`stop`] failed.
    Link(LinkError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DriverEntry(status) => {
                write!(f, "DriverEntry returned NTSTATUS 0x{status:08X}")?;
                if *status == 0xC000_0035 {
                    f.write_str(" (single-instance guard: another instance is live)")?;
                }
                Ok(())
            }

            Self::NotReady { what, waited } => {
                write!(f, "{what} not observed after {waited:?}")
            }
            Self::ScLoad(status) => {
                write!(f, "service load rejected with NTSTATUS 0x{status:08X}")
            }
            Self::Setup(message) => f.write_str(message),
            Self::Link(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

/// A unique shellcode-V3 driver object name for one map attempt.
///
/// V3 creates a real, permanent kernel driver object under this name and
/// nothing deletes it, so every attempt must get a fresh name: a stale
/// name would make `ObCreateObject` fail with `STATUS_OBJECT_NAME_COLLISION`
/// before the driver's own single-instance marker could reject a genuine
/// duplicate load (both report the same code, so the marker's collision
/// must be the only one a caller ever sees).
fn driver_name() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "ksdrv-{:x}-{:x}-{:x}",
        std::process::id(),
        nanos & 0xFFFF_FFFF,
        n
    )
}

/// Maps [`DRIVER_IMAGE`] into the kernel and waits for the driver to
/// publish its object names.
///
/// Loading order: the **normal service load first** — the image is
/// signed, and the loader path does not overwrite live kernel code.
/// Only when the service load is rejected (signature, policy) does
/// [`start`] fall back to manual mapping: it walks the
/// [`provider::PROVIDERS`] table in order — the last provider that
/// succeeded in this process first, then the rest — until one maps the
/// driver. `KS_SDK_MAP=1` reverses the order (manual mapping only, no
/// service attempt); `KS_SDK_KDU_PRV=<id>` (the retired
/// `KS_TEST_KDU_PRV` is still honored) pins the provider in `None`
/// mode. A `Some(id)` argument runs that provider alone — but only
/// after the service load was tried first, unless `KS_SDK_MAP=1`.
///
/// The image reaches disk only on the service path
/// (`%SystemRoot%\Temp\KernelScriptSc.sys`, removed again by [`stop`]).
/// The manual path keeps everything in memory except the helper
/// drivers, which land in a fresh temporary directory that is the
/// process working directory for the duration of the call and is
/// removed afterwards.
///
/// Readiness is the registry publication under `HKLM\SOFTWARE\KernelScript`
/// (section/request/response names); it proves the three ring objects
/// exist. Open a session through the usual link API afterwards to talk to
/// the driver — [`stop`] is its counterpart.
///
/// Every attempt gets a fresh shellcode-V3 driver-object name.
///
/// # Errors
///
/// [`Error::DriverEntry`] when the driver ran and `DriverEntry` itself
/// failed — including `0xC0000035` while another instance still owns
/// the marker, which [`stop`] (or a ring `shutdown`) releases. A second
/// attempt after a successful load is rejected the same way.
/// [`Error::ScLoad`] when the service load was rejected and no manual
/// provider could run the map either, [`Error::NotReady`] when the load
/// succeeded but the publication never appeared, [`Error::Setup`] for
/// temporary directory or working-directory failures.
pub fn start(provider: Option<u32>) -> Result<(), Error> {
    let root = temp_root();
    if let Err(error) = std::fs::create_dir_all(&root) {
        return Err(Error::Setup(format!("create {}: {error}", root.display())));
    }
    let previous =
        std::env::current_dir().map_err(|error| Error::Setup(format!("current dir: {error}")))?;

    if let Err(error) = std::env::set_current_dir(&root) {
        let _ = std::fs::remove_dir_all(&root);
        return Err(Error::Setup(format!("enter {}: {error}", root.display())));
    }

    let outcome = run_provider_chain(provider);

    let restore = std::env::set_current_dir(&previous);
    let _ = std::fs::remove_dir_all(&root);

    match outcome {
        Ok(()) => {
            restore.map_err(|error| Error::Setup(format!("restore cwd: {error}")))?;
            wait_for_publication()
        }
        Err(error) => Err(error),
    }
}

/// One provider attempt: load the vulnerable driver, open its device,
/// run the map, then always unload it again (the mapped target keeps
/// running; only the loader driver is released, exactly like
/// `KDUProviderRelease` did).
fn try_provider(
    def: &'static provider::ProviderDef,
    kernel_image: usize,
    kernel_base: usize,
    target_image: &pe::MappedImage,
    memory_tag: u32,
    hvci: bool,
    build: u32,
) -> Result<u32, String> {
    if build < def.min_build {
        return Err(format!("build {build} is older than the provider requires"));
    }
    if def.max_build != provider::KDU_MAX_NTBUILDNUMBER && build > def.max_build {
        emit("[!] Warning: selected provider may not work on this Windows NT version");
    }
    emit(&format!(
        "[+] Provider: \"{}\", Name \"{}\"",
        def.description, def.driver_name
    ));
    if hvci && def.flags & provider::FLAG_SUPPORT_HVCI == 0 {
        return Err("provider does not support HVCI".to_string());
    }
    if let Some(validate) = def.ops.validate_prerequisites {
        if !validate() {
            return Err("provider prerequisites are not met".to_string());
        }
    }

    let already_loaded = loader::is_device_object_exists("\\Device", def.device_name);
    let mut loaded = false;
    if !already_loaded {
        let path = loader::temp_driver_path(&format!("{}.sys", def.driver_name));
        loader::write_file(&path, def.blob)
            .map_err(|error| format!("extracting vulnerable driver: {error}"))?;
        loader::load_driver(def.driver_name, &path, false)
            .map_err(|status| format!("NtLoadDriver failed with {status:#010x}"))?;
        loaded = true;
        emit(&format!(
            "[+] Vulnerable driver \"{}\" loaded",
            def.driver_name
        ));
    }

    let device = match loader::open_device(
        def.device_name,
        nt::SYNCHRONIZE | nt::WRITE_DAC | nt::GENERIC_WRITE | nt::GENERIC_READ,
    ) {
        Ok(handle) => handle,
        Err(status) => {
            if loaded {
                let _ = loader::unload_driver(def.driver_name, true);
            }
            return Err(format!(
                "cannot open device \"{}\": {status:#010x}",
                def.device_name
            ));
        }
    };

    emit(&format!(
        "[+] Driver device \"{}\" has been opened successfully",
        def.device_name
    ));

    if let Some(register) = def.ops.register_driver {
        if !register(device) {
            nt_close(device);
            if loaded {
                let _ = loader::unload_driver(def.driver_name, true);
            }
            return Err("cannot register provider driver".to_string());
        }
    }

    let object_name = driver_name();
    let registry_name = object_name.clone();
    let result = dispatch::map_driver(dispatch::MapParams {
        device_handle: device,
        def,
        target_image,
        kernel_base,
        kernel_image,
        memory_tag,
        object_name: object_name.clone(),
        registry_name,
    });

    nt_close(device);
    if loaded {
        let _ = loader::unload_driver(def.driver_name, true);
        loader::delete_file_with_wait(
            &loader::temp_driver_path(&format!("{}.sys", def.driver_name)),
            1000,
            5,
        );
    }
    // The victim reloads per attempt and may land at a different base;
    // a cached Superfetch map would translate stale pages (the C++
    // drops the cache on provider release too).
    superfetch::free_cache();

    result.map_err(|error| format!("provider {}: {error}", def.id))
}

/// The [`start`] provider chain: probe the environment once, enable the
/// loader privileges (`KDUProviderCreate` did this before every map),
/// then run every candidate until one payload reports a status.
fn run_provider_chain(provider: Option<u32>) -> Result<(), Error> {
    let (hvci, build) = env::probe().map_err(Error::Setup)?;
    loader::enable_privilege(nt::SE_DEBUG_PRIVILEGE)
        .map_err(|error| Error::Setup(format!("SeDebugPrivilege: {error}")))?;
    loader::enable_privilege(nt::SE_LOAD_DRIVER_PRIVILEGE)
        .map_err(|error| Error::Setup(format!("SeLoadDriverPrivilege: {error}")))?;

    let mut mapped: Option<u32> = None;
    let mut payload_status: Option<u32> = None;

    // Service load FIRST: the signed image goes through the normal
    // loader, which is deterministic and does not overwrite live kernel
    // code (the manual map crashed the machine twice in the field when
    // a CPU touched the patched dispatch inside the write→execute
    // window). `KS_SDK_MAP=1` reverses the order for testing the
    // exploit path; the service load failed is the only time manual
    // mapping runs by default.
    let force_map = std::env::var("KS_SDK_MAP").as_deref() == Ok("1");
    let mut sc_failed: Option<u32> = None;

    if !force_map {
        match sc::load() {
            Ok(()) => return Ok(()),
            Err(sc::ScLoadError::InstanceLive) => {
                return Err(Error::DriverEntry(0xC000_0035));
            }
            Err(sc::ScLoadError::Failed(status)) => {
                sc_failed = Some(status);
                emit(&format!(
                    "[-] service load rejected with 0x{status:08X}; falling back to manual mapping"
                ));
            }
        }
    }

    let Some((kernel_image, kernel_base)) = pe::load_ntoskrnl() else {
        return Err(Error::Setup("cannot load ntoskrnl.exe".to_string()));
    };
    let Some(target_image) = pe::MappedImage::load(DRIVER_IMAGE) else {
        return Err(Error::Setup(
            "cannot map the target driver image".to_string(),
        ));
    };
    let memory_tag = env::select_nonpaged_pool_tag();

    for id in provider_candidates(provider) {
        let Some(def) = provider::by_id(id) else {
            // Not ported (e.g. 32: dbghelp symbol resolution) — the
            // chain moves on.
            continue;
        };
        emit(&format!("trying provider {id}"));
        match try_provider(
            def,
            kernel_image,
            kernel_base,
            &target_image,
            memory_tag,
            hvci,
            build,
        ) {
            Ok(status) => {
                LAST_GOOD_PROVIDER.store(id, Ordering::Relaxed);
                mapped = Some(id);
                payload_status = Some(status);
                break;
            }
            Err(error) => {
                emit(&format!("[-] provider {id} failed: {error}"));
            }
        }
    }
    superfetch::free_cache();

    match (mapped, payload_status) {
        (Some(_), Some(0)) => Ok(()),
        (Some(_), Some(status)) => Err(Error::DriverEntry(status)),
        // The service load already failed (that is why the chain ran)
        // and manual mapping could not run the payload either.
        (_, _) => match sc_failed {
            Some(status) => Err(Error::ScLoad(status)),
            None => Err(Error::Setup(
                "no provider could run the map and KS_SDK_MAP=1 skipped the service load"
                    .to_string(),
            )),
        },
    }
}

/// Best-effort removal of a leftover service load: recreates the fixed
/// fallback service entry, calls `NtUnloadDriver` (running the driver's
/// `DriverUnload`) and deletes the service key and image file again.
///
/// This is the recovery path for a run that died between a successful
/// service load and its [`stop`] — the mapped image keeps the image
/// file locked until it runs. Safe to call at any time: with nothing
/// loaded, `NtUnloadDriver` fails harmlessly.
pub fn cleanup_service_load() {
    sc::unload();
}

/// Shuts the loaded driver down: submits the ring `shutdown` request and
/// waits for the driver to release its single-instance claim.
///
/// The request winds the worker down and publishes its response first, so
/// the round trip itself is an ordinary one; the claim release follows as
/// the worker finishes tearing down. Afterwards no driver instance is live
/// and [`start`] can map a fresh one. Requests through a session opened
/// before this call time out from here on — a session never reconnects, so
/// any further work needs a new process (or at least a fresh session).
///
/// # Errors
///
/// [`Error::Link`] when the `shutdown` round trip (or the claim poll)
/// failed, [`Error::NotReady`] when a live instance still owns the claim
/// after [`LIFECYCLE_TIMEOUT`].
pub fn stop() -> Result<(), Error> {
    shutdown().map_err(Error::Link)?;
    let deadline = Instant::now() + LIFECYCLE_TIMEOUT;
    let released = loop {
        if !instance_claim_present().map_err(Error::Link)? {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // A service-loaded instance also leaves the SCM service behind:
    // with the worker gone, `NtUnloadDriver` runs `DriverUnload` and
    // the service key and image file are removed again.
    sc::unload();
    if released {
        Ok(())
    } else {
        Err(Error::NotReady {
            what: "single-instance claim release",
            waited: LIFECYCLE_TIMEOUT,
        })
    }
}

/// Polls `HKLM\SOFTWARE\KernelScript` until the driver publishes all
/// three object names (or the deadline passes). A clean teardown of an
/// earlier load deleted its publication with the key, so names present
/// now are this load's (the `DriverEntry` status returned by the map is
/// what proves the load ran) — only a run that died before teardown can
/// leave leftovers that make this return early.
fn wait_for_publication() -> Result<(), Error> {
    let deadline = Instant::now() + LIFECYCLE_TIMEOUT;
    loop {
        if published_object_names_strict().is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::NotReady {
                what: "object-name publication",
                waited: LIFECYCLE_TIMEOUT,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A fresh per-call temporary root for helper-driver extraction.
fn temp_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("kernel-script-{nanos}"))
}

/// Resolves the provider override: `KS_SDK_KDU_PRV`, then the retired
/// `KS_TEST_KDU_PRV`.
fn provider_env_override() -> Option<u32> {
    for key in ["KS_SDK_KDU_PRV", "KS_TEST_KDU_PRV"] {
        if let Ok(value) = std::env::var(key) {
            if let Ok(id) = value.parse() {
                return Some(id);
            }
        }
    }
    None
}

/// The provider chain for one [`start`] call: an explicit id runs alone;
/// otherwise the environment override first, then the last provider that
/// succeeded in this process (if any), then the whole table in order —
/// each id at most once.
fn provider_candidates(explicit: Option<u32>) -> Vec<u32> {
    if let Some(id) = explicit {
        return vec![id];
    }
    let mut ids = Vec::with_capacity(provider::PROVIDERS.len() + 1);
    if let Some(id) = provider_env_override() {
        ids.push(id);
    }
    let good = LAST_GOOD_PROVIDER.load(Ordering::Relaxed);
    if good != u32::MAX {
        ids.push(good);
    }
    for def in provider::PROVIDERS {
        if !ids.contains(&def.id) {
            ids.push(def.id);
        }
    }
    ids
}

/// Closes a handle, ignoring the status.
fn nt_close(handle: nt::HANDLE) {
    unsafe {
        let _ = nt::NtClose(handle);
    }
}
