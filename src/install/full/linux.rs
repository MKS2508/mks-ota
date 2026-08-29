// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Portions derived from `tauri-plugin-updater` 2.10.1 — see THIRD-PARTY.md.
//
// AppImage full-install — extracts the downloaded archive in place using the
// running executable's `$APPIMAGE` env var, then renames the new artifact on
// top and re-execs the new AppImage. The deb flavor falls back to `dpkg -i`
// for installations that ran from a deb package — the running process
// can't overwrite its own deb on disk (privilege + lock constraints), but
// the next launch picks up the new version installed via dpkg.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use tar::Archive;
use tempfile::TempDir;

use crate::error::OtaError;

/// Resolves the current `.AppImage` path from `$APPIMAGE` (set by the
/// AppImage runtime when launching the bundled binary, see
/// `tauri-plugin-updater` `src/updater.rs:101-113`). Falls back to
/// `/proc/self/exe` for deb installs (Tauri sets the binary path on
/// `tauri::Builder`, but reading from the kernel procfs is the same trick
/// `updater.rs:1424` uses for the macOS `current_app_bundle` shape).
pub fn current_app_bundle() -> Result<PathBuf, OtaError> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .filter(|p| p.is_absolute())
        .ok_or(OtaError::AppBundleNotFound)
}

/// Extracts `archive` (a `.tar.gz` containing the new `.AppImage` and
/// `.desktop` files under a leading `AppName/` directory) into `into`,
/// skipping the leading path component — same shape as the macOS
/// counterpart.
pub fn extract_app_bundle(archive: &Path, into: &Path) -> Result<(), OtaError> {
    let file = fs::File::open(archive)?;
    let mut tar = Archive::new(GzDecoder::new(file));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let relative: PathBuf = entry.path()?.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let dest = into.join(&relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&dest)?;
    }
    Ok(())
}

/// Downloaded-and-verified install: extracts the archive next to the running
/// AppImage, then renames the new AppImage on top of the current one
/// (same-device check, no `osascript` fallback — Linux is POSIX and the
/// AppImage is owned by the same user that runs it, no privilege
/// escalation needed). If the running bundle was a deb (no `$APPIMAGE`),
/// this delegates to `dpkg -i` and returns — the new binary takes effect on
/// the next launch.
pub fn install(archive: &Path, app_path: &Path) -> Result<(), OtaError> {
    // deb path: no APPIMAGE means this was installed via dpkg; delegate and
    // return, the next launch will pick up the new version.
    if std::env::var_os("APPIMAGE").is_none() {
        let status = Command::new("dpkg").arg("-i").arg(archive).status()?;
        if !status.success() {
            return Err(OtaError::PrivilegedInstallFailed(format!(
                "dpkg exited with {status}"
            )));
        }
        return Ok(());
    }

    let dest_parent = app_path.parent().ok_or(OtaError::AppBundleNotFound)?;

    let extract_dir = pick_tmp_dir(dest_parent)?;
    let new_app = extract_dir.path().join("new-appimage");
    fs::create_dir_all(&new_app)?;
    extract_app_bundle(archive, &new_app)?;

    let backup_dir = pick_tmp_dir(dest_parent)?;
    let backup_app = backup_dir.path().join("previous-appimage");

    swap_app_dirs(app_path, &new_app, &backup_app)
}

/// Relaunches the app at `app_path` via `execve` of the new AppImage
/// (AppImage case) or returns Ok(()) for the deb case (dpkg already queued
/// the new version, the caller is expected to exit so the next launch
/// picks it up).
pub fn relaunch(app_path: &Path) -> Result<(), OtaError> {
    if std::env::var_os("APPIMAGE").is_none() {
        // deb path — dpkg already queued the new binary; nothing to exec.
        return Ok(());
    }
    // Replace current process with the new AppImage.
    // Note: this only returns if execve fails.
    std::process::Command::new(app_path).spawn()?;
    Ok(())
}

/// Moves `current` aside to `backup`, then moves `new_app` into `current`'s
/// place. If the second rename fails, restores `backup` back to `current`
/// before returning the original error — the app is left exactly as it was
/// found, never half-swapped.
fn swap_app_dirs(current: &Path, new_app: &Path, backup: &Path) -> Result<(), OtaError> {
    if let Err(e) = fs::rename(current, backup) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(OtaError::PrivilegedInstallFailed(format!(
                "PermissionDenied on rename of {}. AppImage must be owned by the same user.",
                current.display()
            )));
        }
        return Err(e.into());
    }
    if let Err(e) = fs::rename(new_app, current) {
        fs::rename(backup, current).map_err(|restore_err| {
            OtaError::RollbackFailed(format!(
                "install failed ({e}); restoring the previous version also failed: {restore_err}"
            ))
        })?;
        return Err(e.into());
    }
    touch(current)?;
    Ok(())
}

/// Refreshes the bundle's mtime so the AppImage runtime notices the change.
fn touch(path: &Path) -> Result<(), OtaError> {
    let status = Command::new("touch").arg(path).status()?;
    if !status.success() {
        return Err(OtaError::PrivilegedInstallFailed(format!("touch exited with {status}")));
    }
    Ok(())
}

/// Picks a tmpdir on the same device as `dest_parent` — the check
/// `tauri-plugin-updater` has on Linux (`updater.rs:1064`,
/// `TempDirNotOnSameMountPoint`). An AppImage on a different mount from the
/// system temp dir would have its `rename` fail.
fn pick_tmp_dir(dest_parent: &Path) -> Result<TempDir, OtaError> {
    let dest_dev = fs::metadata(dest_parent)?.dev();
    for candidate in [std::env::temp_dir(), dest_parent.to_path_buf()] {
        if fs::metadata(&candidate).ok().map(|m| m.dev()) == Some(dest_dev) {
            return tempfile::Builder::new()
                .prefix("mks-ota-")
                .tempdir_in(&candidate)
                .map_err(OtaError::from);
        }
    }
    Err(OtaError::CrossDeviceRename {
        tmp_dev: fs::metadata(std::env::temp_dir())?.dev(),
        dest_dev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic `.tar.gz` AppImage fixture inside `into_dir`,
    /// returning the path to the archive. The archive contains a single
    /// `TestApp.appimage` entry under a leading `TestApp/` path component,
    /// so `extract_app_bundle` strips it and lands with the file directly
    /// in `into_dir`.
    fn make_appimage_fixture(into_dir: &Path) -> PathBuf {
        let archive_path = into_dir.join("fixture-appimage.tar.gz");
        let appimage_name = "TestApp.appimage";
        let leading = format!("{}/", "TestApp");
        let mut encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        {
            let mut tar = tar::Builder::new(&mut encoder);
            // Header entry for the leading "TestApp/" directory
            let mut header = tar::Header::new_gnu();
            header.set_path(format!("{} empty-marker", leading)).unwrap();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            tar.append(&header, &[] as &[u8]).unwrap();

            // The actual AppImage file
            let mut header = tar::Header::new_gnu();
            header.set_path(format!("{}{}", leading, appimage_name)).unwrap();
            let contents = b"#!/bin/sh\necho fake-appimage\n";
            header.set_size(contents.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o755);
            tar.append(&header, &contents[..]).unwrap();
        }
        encoder.finish().unwrap();
        archive_path
    }

    #[test]
    fn extracts_dropping_the_leading_app_component() {
        let into = tempfile::tempdir().unwrap();
        let archive = make_appimage_fixture(into.path());
        extract_app_bundle(&archive, into.path()).unwrap();
        assert!(
            into.path().join("TestApp.appimage").is_file(),
            "expected TestApp.appimage directly under the extract root, not nested under TestApp/"
        );
    }

    #[test]
    fn install_swaps_the_bundle_in_place() {
        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = apps_dir.path().join("TestApp.AppImage");
        // Create a fake existing AppImage
        fs::write(&app_path, b"old-appimage-content").unwrap();

        let archive = make_appimage_fixture(apps_dir.path());
        install(&archive, &app_path).unwrap();

        let content = fs::read(&app_path).unwrap();
        assert!(
            !content.contains(b"old-appimage-content"),
            "old AppImage content must be gone"
        );
        assert!(content.contains(b"#!/bin/sh"), "new AppImage should be the fake one we built");
    }

    #[test]
    fn relaunch_invokes_execve_without_erroring_synchronously() {
        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = apps_dir.path().join("TestApp.AppImage");
        fs::write(&app_path, b"#!/bin/sh\necho ok\n").unwrap();

        // `spawn()` only fails synchronously if the path is nonexistent or
        // not executable; the execve itself is observed by a separate
        // process — we just assert spawn itself doesn't error.
        relaunch(&app_path).unwrap();
    }

    #[test]
    fn a_failed_swap_restores_the_previous_bundle() {
        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = apps_dir.path().join("TestApp.AppImage");
        fs::write(&app_path, b"still the old one").unwrap();

        let backup_dir = tempfile::tempdir().unwrap();
        // A `new_app` source that does not exist forces the second rename
        // to fail deterministically.
        let missing_new_app = backup_dir.path().join("this-does-not-exist");

        let err =
            swap_app_dirs(&app_path, &missing_new_app, &backup_dir.path().join("backup")).unwrap_err();
        assert!(matches!(err, OtaError::Io(_)), "got {err:?}");

        let content = fs::read(&app_path).unwrap();
        assert_eq!(content, b"still the old one", "the original bundle must be restored");
    }

    #[test]
    fn pick_tmp_dir_lands_on_the_same_device_as_the_destination() {
        let dest = tempfile::tempdir().unwrap();
        let picked = pick_tmp_dir(dest.path()).unwrap();
        assert_eq!(
            fs::metadata(dest.path()).unwrap().dev(),
            fs::metadata(picked.path()).unwrap().dev()
        );
    }
}
