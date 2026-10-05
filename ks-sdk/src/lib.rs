//! ks-sdk — the Kernel Script SDK.
//!
//! Two halves in one crate:
//!
//! * [`start`] / [`stop`] — the driver lifecycle: `start` maps the
//!   embedded image ([`DRIVER_IMAGE`]) into the kernel in-process through
//!   KDU's map core (compiled from `KDU-1.5.0/Source` by this crate's
//!   `build.rs`, entry `kdu/ks_bridge.cpp`) without writing the target
//!   image to disk; `stop` shuts the driver down again. Both live in
//!   [`kdu`].
//! * The whole [`ks_link`] API re-exported at the crate root — session,
//!   ring round trips, process enumeration, memory reads/writes, batch
//!   operations, pointer walks and memory locks:
//!
//! ```text
//! ks_sdk::start()?;                          // load the driver
//! ks_sdk::published_object_names_strict();    // wait for readiness
//! let pid = ks_sdk::find_pid("game.exe")?;   // use the link API
//! let value = ks_sdk::read_bytes(pid, addr, 4, false, false)?;
//! ks_sdk::stop()?;                           // shut the driver down
//! ```
//!
//! The C++/MSVC toolchain requirement introduced by the KDU build applies
//! to every crate that depends on ks-sdk (see AGENTS.md).

pub use ks_link::*;

/// In-process driver loading (`start`, `stop`, `set_log_sink`, the
/// [`DRIVER_IMAGE`] bytes and the [`Error`] type).
pub mod kdu;

pub use kdu::{set_log_sink, start, stop, Error, DRIVER_IMAGE};
