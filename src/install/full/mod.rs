//! Full-package install: extract the downloaded archive and swap it into
//! place. One implementation per platform.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

/// Previous AppImage kept on the side + trial bookkeeping, so a full update
/// that never confirms can be reverted (by the app or by a shell launcher).
#[cfg(target_os = "linux")]
pub mod trial;
