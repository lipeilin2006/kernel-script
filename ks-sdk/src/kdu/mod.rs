//! Kernel Script driver lifecycle on top of the `dt-loader` crate.
//!
//! The mapper itself — the KDU 1.5.0 map core, the loader-driver
//! provider table and the shellcode V3 machine code — lives in the
//! standalone `dt-loader` crate (the private sibling repo
//! <https://github.com/lipeilin2006/driver-loader>, checked out at
//! `../driver-toolkit/dt-loader`), which embeds its own assets
//! (`assets/drivers`, `assets/shellcode_v3.bin`) and takes the
//! target image as a parameter. This module keeps the Kernel Script
//! half of the lifecycle:
//!
//! * the embedded target image [`DRIVER_IMAGE`] (`ks-driver.sys`,
//!   never written to disk by the manual path);
//! * readiness: [`start`] waits for the driver's registry publication
//!   under `HKLM\SOFTWARE\KernelScript`;
//! * teardown: [`stop`] submits the ring `shutdown` request, waits for
//!   the single-instance claim to be released and unloads a leftover
//!   service load;
//! * the [`Error`] type the SDK has always reported, into which
//!   [`dt_loader::Error`] is converted.
//!
//! Shellcode version 3 is mandatory (dt-loader enforces it): it builds
//! a real `DRIVER_OBJECT` and calls `DriverEntry` synchronously, so the
//! reported status is the entry's own. A loaded driver that is still
//! holding the claim rejects every further [`start`] with
//! [`Error::DriverEntry`]`(0xC0000035)`.

use std::fmt;
use std::time::{Duration, Instant};

use ks_link::{instance_claim_present, published_object_names_strict, shutdown, LinkError};

pub use dt_loader::{cleanup_service_load, provider_ids, set_log_sink, victim_builds};

/// The KernelScript driver image (`ks-driver.sys`), the bytes [`start`]
/// maps into the kernel. Embedded in the binary and only ever handed to
/// the mapper as memory — this SDK never writes the target image to disk.
pub const DRIVER_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ks-driver.sys"));

/// How long [`start`] waits for the driver to publish its object names
/// and [`stop`] waits for the single-instance claim to be released.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The manual-map chain failed and the service-load fallback was
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

impl From<dt_loader::Error> for Error {
    fn from(error: dt_loader::Error) -> Self {
        match error {
            dt_loader::Error::DriverEntry(status) => Self::DriverEntry(status),
            dt_loader::Error::ScLoad(status) => Self::ScLoad(status),
            dt_loader::Error::Setup(message) => Self::Setup(message),
        }
    }
}

/// Loads the driver with no explicit provider or victim: the manual-map
/// provider chain runs first (every provider × victim combination in
/// order until one maps the driver), and a chain that cannot run the
/// driver falls back to the normal service load. Equivalent to
/// [`start_with`]`(None, None)`.
pub fn start() -> Result<(), Error> {
    start_with(None, None)
}

/// Loads the driver with an explicit provider and/or victim.
///
/// `provider` selects the vulnerable-driver provider; `Some(id)` runs
/// that provider alone, `None` walks the whole table in order (the last
/// provider that succeeded in this process first, then the rest).
/// `victim` is a PROCEXP152 build number (1627/1702/1712); `Some(v)`
/// pins that build, `None` walks newest-first per attempt.
/// `KS_SDK_KDU_PRV=<id>` is tried first in the `None` provider mode;
/// `KS_SDK_MAP=1` skips the service-load fallback entirely (manual
/// mapping only).
///
/// The mapping itself runs through `dt_loader::start_with_image`:
/// **manual-map provider chain first**, and only when it did not run
/// the driver does the normal service load run as the fallback —
/// details in the `dt-loader` crate docs. The image reaches disk only
/// on that fallback path (`%SystemRoot%\Temp\KernelScriptSc.sys`,
/// removed again by [`stop`] or [`cleanup_service_load`]); the manual
/// path keeps everything in memory except the helper drivers, which
/// land in a fresh temporary directory that is the process working
/// directory for the duration of the call and is removed afterwards.
///
/// Readiness is the registry publication under `HKLM\SOFTWARE\KernelScript`
/// (section/request/response names); it proves the three ring objects
/// exist. Open a session through the usual link API afterwards to talk to
/// the driver — [`stop`] is its counterpart.
///
/// # Errors
///
/// [`Error::DriverEntry`] when the driver ran and `DriverEntry` itself
/// failed — including `0xC0000035` while another instance still owns
/// the marker, which [`stop`] (or a ring `shutdown`) releases. A second
/// attempt after a successful load is rejected the same way.
/// [`Error::ScLoad`] when manual mapping did not run the driver and
/// the service-load fallback was rejected too, [`Error::NotReady`] when
/// the load succeeded but the publication never appeared,
/// [`Error::Setup`] for temporary directory or working-directory
/// failures.
pub fn start_with(provider: Option<u32>, victim: Option<u32>) -> Result<(), Error> {
    // The service-load fallback distinguishes a still-live instance
    // from a torn-down one through the driver's claim; the claim is
    // ring-side knowledge, so it is injected here.
    dt_loader::set_instance_probe(|| instance_claim_present().unwrap_or(true));
    dt_loader::start_with_image(DRIVER_IMAGE, provider, victim).map_err(Error::from)?;
    wait_for_publication()
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
    cleanup_service_load();
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
