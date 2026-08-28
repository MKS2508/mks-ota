//! AppImage full-install — F2 scope, not implemented in F1.
//!
//! `docs/handoffs/lane-ota-crate-f1-2026-08-28.md`: "Linux/AppImage: sólo
//! el hueco de módulo, cero implementación." `wraith-linux`'s Linux
//! artifact is an AppImage, whose running instance overwrites itself in
//! place — Tauri reads the real on-disk path from the `APPIMAGE` env var
//! (`tauri-plugin-updater` `lib.rs:101-113`, `updater.rs:379-383`) rather
//! than deriving it from the executable path the way macOS does. That
//! same-device `rename` check upstream already has on this platform
//! (`updater.rs:1064`) is the one macOS was missing and
//! [`super::macos`] adds.
