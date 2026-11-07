//! Shared user-mode infrastructure for the Kernel Script desktop apps.
//!
//! `ks-gui` (overlay + Lua) and `ks-inspector` (standalone memory tool) are
//! separate executables, but both need the same driver lifecycle, the same
//! synchronous round-trip facade, the same persisted UI settings and the
//! same bilingual string table. Keeping them here prevents the two apps
//! from drifting apart:
//!
//! - [`driver`] owns the silent probe/start/stop lifecycle, the startup log
//!   and the `sync_ipc` gate that keeps round trips away from the
//!   process-wide session while a lifecycle job may replace it.
//! - [`sync_ipc`] is the blocking facade every UI caller goes through (the
//!   Lua thread in `ks-gui`, the worker thread in `ks-inspector`).
//! - [`config_store`] persists typed script settings in `config.json`
//!   beside the executable, including the selected UI language.
//! - [`lang`] is the two-table bilingual UI string set.
//!
//! The crate deliberately contains no window or renderer code: each app
//! owns its GLFW/egui host (`ks-gui` through `egui_overlay`, `ks-inspector`
//! through its own decorated window).

pub mod config_store;
pub mod driver;
mod font;
pub mod lang;
pub mod sync_ipc;

pub use font::install_chinese_font;
