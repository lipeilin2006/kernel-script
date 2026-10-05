//! In-process driver lifecycle: [`start`] and [`stop`].
//!
//! The KDU 1.5.0 map core is compiled into this crate by `build.rs`
//! (73 translation units from `KDU-1.5.0/Source` plus `kdu/ks_bridge.cpp`,
//! which exposes `ks_kdu_map`). Both images the mapper consumes are held
//! in memory: the target driver ([`DRIVER_IMAGE`]) and the packed
//! provider database (`assets/drv64.dll`). [`start`] maps the target
//! without ever writing it to disk — the only files this path creates are
//! the helper drivers KDU extracts for its provider and victim, into a
//! fresh temporary directory that becomes the process working directory
//! for the duration of the call (helper-driver extraction is CWD-relative)
//! and is removed again afterwards.
//!
//! Shellcode version 3 is mandatory: it builds a real `DRIVER_OBJECT` and
//! calls `DriverEntry(driverObject, &regPath)` synchronously, so the
//! reported status is the entry's own. Version 1 (KDU's default) instead
//! starts the entry as a bare system-thread routine with a `NULL` driver
//! object — the KernelScript driver rejects that, and KDU's status then
//! only reflects thread creation, not our entry.
//!
//! Teardown is the ring `shutdown` request (KDU never unloads a mapped
//! image): [`stop`] submits it and waits for the driver to release its
//! single-instance claim. A loaded driver that is still holding the claim
//! rejects every further [`start`] with [`Error::DriverEntry`]`(0xC0000035)`.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ks_link::{instance_claim_present, published_object_names_strict, shutdown, LinkError};

/// The packed KDU providers database: provider/victim driver blobs and
/// auxiliary payloads, consumed by the bridge in memory
/// (`kdu/ks_bridge.cpp` maps it section-by-section like
/// `LoadLibraryEx(..., DONT_RESOLVE_DLL_REFERENCES)`).
const KDU_DB_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/drv64.dll"));

/// The KernelScript driver image (`ks-driver.sys`), the bytes [`start`]
/// maps into the kernel. Embedded in the binary and only ever handed to
/// the mapper as memory — this SDK never writes the target image to disk.
pub const DRIVER_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ks-driver.sys"));

/// How long [`start`] waits for the driver to publish its object names
/// and [`stop`] waits for the single-instance claim to be released.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);

/// What [`bridge::ks_kdu_map`] returns when the attempt died before the
/// payload ran: the provider was rejected (blocklisted or unsupported
/// vulnerable driver, no V3 shellcode, victim-load failure), so another
/// provider may still succeed. Every other non-zero status is
/// `DriverEntry`'s own answer, which does not depend on the provider.
const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;

/// The ids [`start`] falls back to, in order, while attempts keep dying
/// before their payload runs. Cold hardware-vendor drivers that survive
/// where the notorious ones (Intel NAL, RTCore64, DBUtil, AsIO, ...) are
/// blocklisted by an anti-cheat or by Microsoft's vulnerable-driver
/// list. Every id must support shellcode V3 — KDU rejects others before
/// anything loads — and id 57 (Lenovo MSR I/O) is verified on the
/// development machine.
const FALLBACK_PROVIDERS: &[u32] = &[57, 56, 58, 60, 63, 67, 44, 32, 35, 47, 51, 6, 14];

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

/// Discards the raw C lines the bridge forwards (KDU's own step-log
/// output is kept quiet; the sink only ever carries the SDK's own
/// lines, such as the provider attempts in [`start`]). Replace this
/// with a function that splits the line and calls [`emit`] to bring
/// the raw mapper log back.
extern "C" fn kdu_log_quiet(_line: *const std::os::raw::c_char) {}

/// FFI surface of `kdu/ks_bridge.cpp`. `ks_kdu_map` returns
/// `DriverEntry`'s NTSTATUS; both image arguments are raw bytes.
mod bridge {
    pub type LogFn = extern "C" fn(*const std::os::raw::c_char);

    extern "C" {
        pub fn ks_kdu_map(
            driver_image: *const u8,
            driver_image_size: usize,
            driver_object_name: *const u16,
            driver_registry_path: *const u16,
            provider_id: u32,
            shell_version: u32,
            db_image: *const u8,
            db_image_size: usize,
            log_fn: LogFn,
        ) -> u32;
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
    /// No provider in [`start`]'s chain could run the map: every
    /// attempt died before its payload ran (the target driver never
    /// executed). Distinct from [`Self::DriverEntry`] for that reason.
    NoProvider {
        /// The provider ids attempted, in order (the configured one
        /// first).
        tried: Vec<u32>,
    },
    /// The driver loaded but a follow-up signal never appeared within
    /// [`LIFECYCLE_TIMEOUT`].
    NotReady {
        what: &'static str,
        waited: Duration,
    },
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
            Self::NoProvider { tried } => {
                let ids: Vec<String> = tried.iter().map(|id| id.to_string()).collect();
                write!(
                    f,
                    "no provider could run the map (tried {}): the vulnerable driver never loaded",
                    ids.join(", ")
                )
            }
            Self::NotReady { what, waited } => {
                write!(f, "{what} not observed after {waited:?}")
            }
            Self::Setup(message) => f.write_str(message),
            Self::Link(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

/// A unique KDU shellcode-V3 driver object name for one map attempt.
///
/// V3 creates a real, permanent kernel driver object under this name and
/// KDU never deletes it, so every attempt must get a fresh name: a stale
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

/// Maps [`DRIVER_IMAGE`] into the kernel with the in-process KDU mapper
/// (shellcode V3) and waits for the driver to publish its object names.
///
/// The image itself never touches disk: KDU's helper drivers are extracted
/// into a fresh temporary directory which is the process working directory
/// for the duration of the call, and the directory is removed again
/// afterwards. The previous working directory is always restored first.
///
/// Readiness is the registry publication under `HKLM\SOFTWARE\KernelScript`
/// (section/request/response names); it proves the three ring objects
/// exist. Open a session through the usual link API afterwards to talk to
/// the driver — [`stop`] is its counterpart.
///
/// The chain starts at the configured provider id (KDU's default 0,
/// Intel NAL; override with `KS_SDK_KDU_PRV=<id>` — the retired
/// `KS_TEST_KDU_PRV` name is still honored) and falls back through
/// [`FALLBACK_PROVIDERS`] while an attempt dies before its payload ran
/// (the vulnerable driver was rejected, e.g. blocklisted by an
/// anti-cheat or by Microsoft's vulnerable-driver list). The provider
/// whose map last succeeded in this process is tried first, so a
/// repeated [`start`] does not re-run the ids that already failed in
/// the same run. Every attempt gets a fresh V3 driver-object name.
///
/// # Errors
///
/// [`Error::DriverEntry`] when the payload ran and `DriverEntry` itself
/// failed — including `0xC0000035` while another instance still owns
/// the marker, which [`stop`] (or a ring `shutdown`) releases. A second
/// attempt after a successful load is rejected the same way. The chain
/// stops at the first such status because the answer cannot change with
/// the provider. [`Error::NoProvider`] when every chain entry died
/// before its payload ran, [`Error::NotReady`] when the load succeeded
/// but the publication never appeared, [`Error::Setup`] for temporary
/// directory or working-directory failures.
pub fn start() -> Result<(), Error> {
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

    let mut tried = Vec::new();
    let mut mapped: Option<u32> = None;
    let mut payload_status: Option<u32> = None;
    for provider in provider_candidates() {
        tried.push(provider);
        emit(&format!("trying provider {provider}"));
        let name = driver_name();
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: every pointer is a live, NUL/slice-terminated buffer owned
        // by this function or the crate's static byte arrays.
        let status = unsafe {
            bridge::ks_kdu_map(
                DRIVER_IMAGE.as_ptr(),
                DRIVER_IMAGE.len(),
                name.as_ptr(),
                std::ptr::null(),
                provider,
                3,
                KDU_DB_BYTES.as_ptr(),
                KDU_DB_BYTES.len(),
                kdu_log_quiet,
            )
        };
        if status == 0 {
            LAST_GOOD_PROVIDER.store(provider, Ordering::Relaxed);
            mapped = Some(provider);
            break;
        }
        if status != STATUS_UNSUCCESSFUL {
            // The payload ran: DriverEntry answered, and the answer does
            // not depend on the provider (0xC0000035 = the marker's
            // duplicate-load rejection).
            payload_status = Some(status);
            break;
        }
    }

    let restore = std::env::set_current_dir(&previous);
    let _ = std::fs::remove_dir_all(&root);

    match (mapped, payload_status) {
        (Some(_), _) => {
            restore.map_err(|error| Error::Setup(format!("restore cwd: {error}")))?;
            wait_for_publication()
        }
        (None, Some(status)) => Err(Error::DriverEntry(status)),
        (None, None) => Err(Error::NoProvider { tried }),
    }
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
    loop {
        if !instance_claim_present().map_err(Error::Link)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::NotReady {
                what: "single-instance claim release",
                waited: LIFECYCLE_TIMEOUT,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
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

/// A fresh per-call temporary root for KDU's helper-driver extraction.
fn temp_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("kernel-script-{nanos}"))
}

/// Resolves the KDU provider id: `KS_SDK_KDU_PRV`, then the retired
/// `KS_TEST_KDU_PRV`, then KDU's default (0, Intel NAL).
fn provider_id() -> u32 {
    for key in ["KS_SDK_KDU_PRV", "KS_TEST_KDU_PRV"] {
        if let Ok(value) = std::env::var(key) {
            if let Ok(id) = value.parse() {
                return id;
            }
        }
    }
    0
}

/// The provider chain for one [`start`] call: the last provider that
/// succeeded in this process first (if any), then the configured id
/// ([`provider_id`]), then [`FALLBACK_PROVIDERS`], each id at most
/// once.
fn provider_candidates() -> Vec<u32> {
    let mut ids = Vec::with_capacity(2 + FALLBACK_PROVIDERS.len());
    let good = LAST_GOOD_PROVIDER.load(Ordering::Relaxed);
    if good != u32::MAX {
        ids.push(good);
    }
    for id in std::iter::once(provider_id()).chain(FALLBACK_PROVIDERS.iter().copied()) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}
