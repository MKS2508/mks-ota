//! `mks-ota` — hub-agnostic OTA updater, reusable across Tauri apps.
//!
//! Full install (whole app package + relaunch) and partial install (a
//! single artifact inside `appDataDir`, no reinstall) are separate paths,
//! not a flag (ADR-0045 D8-C). F1 implements full, macOS-only; the module
//! tree below leaves the declared holes for Linux full and for partial.
//!
//! ```text
//! manifest              -> deserializes GET /api/components/{c}/latest (our shape, not Tauri's)
//! verify                -> minisign-verify: content + trusted-comment signature, streaming
//! download              -> stream to a temp file, sha256 while streaming, Range/resume
//! install::full::macos  -> extract + swap the .app, relaunch
//! install::full::linux  -> hole (F1 does not implement it)
//! install::partial      -> hole (F2, extracted from tpv-el-haido2, not written from scratch)
//! ```

pub mod download;
pub mod error;
pub mod install;
pub mod manifest;
pub mod verify;

pub use error::OtaError;
pub use manifest::HubLatest;
