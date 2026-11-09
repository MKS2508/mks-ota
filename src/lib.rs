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
//! download              -> stream to a temp file, sha256 while streaming, Range/resume,
//!                          rate limit + pause (download_controlled)
//! install::full::macos  -> extract + swap the .app, relaunch
//! install::full::linux  -> AppImage (raw or .tar.gz, atomic file swap) + deb (pkexec dpkg)
//! install::full::trial  -> previous AppImage kept aside + trial/boots/unacked files, revert
//! install::partial      -> A/B slots in appDataDir: two gates, stage/activate, rollback (F2)
//! push                  -> optional SSE subscription to the hub's /api/events: a hint to
//!                          query /latest now, never an update (reconnect, backoff, cancel)
//! policy                -> decide(urgency, kind): ask or apply-when-idle; IdleHook (app-defined,
//!                          NeverIdle by default) and when_idle(hook, every, apply)
//! protocol  [tauri]     -> custom URI scheme: active slot with embedded fallback (F2)
//! watchdog              -> boot counter + hot-apply grace, auto-rollback (F2)
//! ```
//!
//! The `tauri` feature (off by default) gates everything that needs a Tauri
//! runtime handle; the rest of the crate is plain Rust + reqwest.
//!
//! Two packages build this same source: `mks-ota` binds `tauri` to Tauri 2,
//! `mks-ota-tauri3` (in `tauri3/`) binds it to Tauri 3 and also turns on the
//! `tauri-v3` marker. Code that must differ between majors uses
//! `#[cfg(feature = "tauri-v3")]`; everything else is shared as is. They
//! cannot be combined: both majors declare `links = "Tauri"`.

pub mod download;
pub mod error;
pub mod install;
pub mod manifest;
pub mod policy;
#[cfg(feature = "tauri")]
pub mod protocol;
pub mod push;
pub mod verify;
pub mod watchdog;

pub use error::OtaError;

#[cfg(all(test, feature = "tauri"))]
mod tauri_major {
    /// Each package compiles against the Tauri major it promises. Catches a
    /// manifest edit that points one package at the other's major, which
    /// would otherwise build green (the shared code compiles on both).
    #[test]
    fn the_package_links_the_tauri_major_it_promises() {
        let expected = if cfg!(feature = "tauri-v3") { "3." } else { "2." };
        assert!(
            tauri::VERSION.starts_with(expected),
            "{} is built against tauri {}, expected {expected}x",
            env!("CARGO_PKG_NAME"),
            tauri::VERSION
        );
    }
}
pub use manifest::{HubLatest, Urgency};