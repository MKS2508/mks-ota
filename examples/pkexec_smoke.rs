//! Manual end-to-end smoke for the deb branch of `install::full::linux::install` —
//! not part of `cargo test` (needs a real polkit authority + a real `.deb` on
//! disk, neither of which exist on the CI/dev machine by default). Run as a
//! non-root user with pkexec/polkit configured:
//!
//!     cargo run --release --example pkexec_smoke -- /path/to/real.deb
//!
//! Prints the `Result` from `install()` and exits non-zero on `Err`.
use std::path::PathBuf;

fn main() {
    let archive = std::env::args().nth(1).expect("usage: pkexec_smoke <path-to-deb>");
    let archive = PathBuf::from(archive);
    // Irrelevant for the deb branch (only used by the AppImage branch), but
    // `install()` requires a value.
    let app_path = PathBuf::from("/tmp/unused-app-path");

    match mks_ota::install::full::linux::install(&archive, &app_path) {
        Ok(()) => println!("install() returned Ok(())"),
        Err(e) => {
            eprintln!("install() returned Err: {e}");
            std::process::exit(1);
        }
    }
}
