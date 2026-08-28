//! Full-package install: extract the downloaded archive and swap it into
//! place. One implementation per platform.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;
