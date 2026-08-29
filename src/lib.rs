//! `mks-ota` — hub-agnostic OTA updater, reusable across Tauri apps.
//!
//! Full install (whole app package + relaunch) and partial install (a
//! single artifact inside `appDataDir`, no reinstall) are separate paths,
//! not a flag (ADR-0045 D8-C). F1 implemented full, macOS-only; F2 extracted
//! the partial path from `tpv-el-haido2`'s production A/B slot updater.
//!
//! ```text
//! manifest              -> deserializes GET /api/components/{c}/latest (our shape, not Tauri's)
//! verify                -> minisign-verify: content + trusted-comment signature, streaming
//! download              -> stream to a temp file, sha256 while streaming, Range/resume
//! install::full::macos  -> extract + swap the .app, relaunch
//! install::full::linux  -> hole (not implemented)
//! install::partial      -> A/B slots in appDataDir: two gates, stage/activate, rollback (F2)
//! protocol  [tauri]     -> custom URI scheme: active slot with embedded fallback (F2)
//! watchdog              -> boot counter + hot-apply grace, auto-rollback (F2)
//! ```
//!
//! The `tauri` feature (off by default) gates everything that needs a Tauri
//! runtime handle; the rest of the crate is plain Rust + reqwest.

pub mod download;
pub mod error;
pub mod install;
pub mod manifest;
#[cfg(feature = "tauri")]
pub mod protocol;
pub mod verify;
pub mod watchdog;

pub use error::OtaError;
pub use manifest::HubLatest;