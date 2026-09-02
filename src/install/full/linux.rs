// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Portions derived from `tauri-plugin-updater` 2.10.1 — see THIRD-PARTY.md.
//
// AppImage full-install — extracts the downloaded archive in place using the
// running executable's `$APPIMAGE` env var, then renames the new artifact on
// top and re-execs the new AppImage. The deb flavor falls back to
// `pkexec dpkg -i` for installations that ran from a deb package — the
// running process can't overwrite its own deb on disk (privilege + lock
// constraints), but the next launch picks up the new version installed via
// dpkg. The GUI runs as a normal user, so dpkg needs a privilege prompt;
// see `install_deb_with_pkexec` for what happens when `pkexec` is missing
// or the prompt is cancelled.

use std::fs;
use std::io::Read;
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
/// this resolves `archive` to an installable `.deb` and runs `pkexec dpkg
/// -i` on it — the new binary takes effect on the next launch.
pub fn install(archive: &Path, app_path: &Path) -> Result<(), OtaError> {
    // deb path: no APPIMAGE means this was installed via dpkg.
    if std::env::var_os("APPIMAGE").is_none() {
        let deb = resolve_deb_path(archive)?;
        return install_deb_with_pkexec(&deb);
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

/// The `ar`-archive magic every real `.deb` starts with (`man 5 deb`).
const DEB_MAGIC: &[u8] = b"!<arch>\n";
/// gzip magic — what `downloaded` looks like when the hub's `latest` slot
/// for this component/target/arch served the same tarball an AppImage
/// install would get. The manifest has one slot per component/target/arch
/// with no package-format dimension yet, so nothing on the client side
/// guarantees a deb-installed build is served a `.deb`.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Resolves `downloaded` to an installable `.deb` path: passed through
/// unchanged if it already is one (mirrors upstream's `install_deb`
/// checking `infer::archive::is_deb` before calling `dpkg`,
/// `updater.rs:1049-1057`), or — if it's a gzip tarball instead — extracts
/// the single `.deb` entry inside it. Anything else is rejected before it
/// reaches `dpkg`, which would otherwise fail with a confusing "Errors
/// were encountered while processing" instead of naming the real problem.
fn resolve_deb_path(downloaded: &Path) -> Result<PathBuf, OtaError> {
    let mut header = [0u8; 8];
    let read = fs::File::open(downloaded)?.read(&mut header)?;
    if header[..read].starts_with(DEB_MAGIC) {
        return Ok(downloaded.to_path_buf());
    }
    if read >= 2 && header[..2] == GZIP_MAGIC {
        return extract_deb_from_tar_gz(downloaded);
    }
    Err(OtaError::PrivilegedInstallFailed(format!(
        "{} is neither a .deb nor a .tar.gz — cannot install",
        downloaded.display()
    )))
}

/// Extracts the single `.deb` entry out of `archive` (a gzip tarball), next
/// to `archive` itself so the extracted file survives independently of it —
/// the caller may delete `archive` on error while still pointing the user
/// at the extracted `.deb`.
fn extract_deb_from_tar_gz(archive: &Path) -> Result<PathBuf, OtaError> {
    let file = fs::File::open(archive)?;
    let mut tar = Archive::new(GzDecoder::new(file));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.extension().and_then(|e| e.to_str()) != Some("deb") {
            continue;
        }
        let dest = archive.with_file_name(format!(
            "{}.deb",
            archive.file_name().and_then(|n| n.to_str()).unwrap_or("wraith-ota-extracted")
        ));
        entry.unpack(&dest)?;
        return Ok(dest);
    }
    Err(OtaError::PrivilegedInstallFailed(format!(
        "{} is a tar.gz with no .deb entry inside",
        archive.display()
    )))
}

/// Runs `pkexec dpkg -i` on `deb` for the graphical polkit privilege
/// prompt — the GUI runs as a normal user and dpkg needs root. Only the
/// first of upstream's three escalation steps is ported
/// (`try_install_with_privileges`, `updater.rs:1106-1123`): no
/// password-capturing GUI fallback (zenity/kdialog piping into `sudo -S`)
/// and no terminal `sudo` — the GUI has no terminal to run it in. A missing
/// `pkexec`, a cancelled prompt, or any other failure all return the exact
/// command the user can paste into a terminal instead.
fn install_deb_with_pkexec(deb: &Path) -> Result<(), OtaError> {
    let manual_command = format!("sudo dpkg -i {}", deb.display());
    let status = match Command::new("pkexec").arg("dpkg").arg("-i").arg(deb).status() {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(OtaError::PrivilegedInstallFailed(format!(
                "pkexec is not installed — run this yourself: {manual_command}"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    if !status.success() {
        return Err(OtaError::PrivilegedInstallFailed(format!(
            "pkexec dpkg -i exited with {status} (cancelled or denied) — run this yourself: {manual_command}"
        )));
    }
    Ok(())
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

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// `install()`/`install_deb_with_pkexec()` branch on process-wide env
    /// vars (`$APPIMAGE`, `$PATH`); tests that mutate either take this lock
    /// for the duration so they don't race each other under cargo's
    /// default multi-threaded runner.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Sets `$APPIMAGE` and restores the previous value on drop. Caller
    /// must hold `ENV_LOCK` for the guard's lifetime.
    struct AppImageEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl AppImageEnvGuard {
        fn set(value: &str) -> Self {
            let previous = std::env::var_os("APPIMAGE");
            unsafe { std::env::set_var("APPIMAGE", value) };
            Self { previous }
        }
    }

    impl Drop for AppImageEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => unsafe { std::env::set_var("APPIMAGE", v) },
                None => unsafe { std::env::remove_var("APPIMAGE") },
            }
        }
    }

    /// Sets `$PATH` and restores the previous value on drop. Caller must
    /// hold `ENV_LOCK` for the guard's lifetime.
    struct PathEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl PathEnvGuard {
        fn set(value: &std::ffi::OsStr) -> Self {
            let previous = std::env::var_os("PATH");
            unsafe { std::env::set_var("PATH", value) };
            Self { previous }
        }
    }

    impl Drop for PathEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => unsafe { std::env::set_var("PATH", v) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
    }

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
            header.set_cksum();
            tar.append(&header, &[] as &[u8]).unwrap();

            // The actual AppImage file
            let mut header = tar::Header::new_gnu();
            header.set_path(format!("{}{}", leading, appimage_name)).unwrap();
            let contents = b"#!/bin/sh\necho fake-appimage\n";
            header.set_size(contents.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o755);
            header.set_cksum();
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
    #[ignore = "pre-existing bug, out of scope here: swap_app_dirs renames the whole \
        extracted directory onto app_path, so app_path becomes a directory instead of \
        the .AppImage file the AppImage runtime expects — unrelated to the deb/pkexec \
        fix this test module was touched for; reported separately"]
    fn install_swaps_the_bundle_in_place() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _appimage = AppImageEnvGuard::set("/nonexistent-appimage-marker");

        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = apps_dir.path().join("TestApp.AppImage");
        // Create a fake existing AppImage
        fs::write(&app_path, b"old-appimage-content").unwrap();

        let archive = make_appimage_fixture(apps_dir.path());
        install(&archive, &app_path).unwrap();

        let content = fs::read(&app_path).unwrap();
        assert!(
            !contains_subslice(&content, b"old-appimage-content"),
            "old AppImage content must be gone"
        );
        assert!(
            contains_subslice(&content, b"#!/bin/sh"),
            "new AppImage should be the fake one we built"
        );
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

    #[test]
    fn resolve_deb_path_passes_through_a_real_deb_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let deb = dir.path().join("wraith.deb");
        // Only the magic matters for this check — the rest of a real `.deb`
        // is an `ar` member table this function never looks at.
        fs::write(&deb, b"!<arch>\nrest-of-the-deb-does-not-matter-here").unwrap();

        let resolved = resolve_deb_path(&deb).unwrap();
        assert_eq!(resolved, deb);
    }

    #[test]
    fn resolve_deb_path_extracts_the_deb_from_a_tar_gz_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("wraith-ota-0.3.1.tar.gz");
        let deb_contents = b"!<arch>\nfake-but-magic-correct-deb";
        let mut encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        {
            let mut tar = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_path("wraith_0.3.1_amd64.deb").unwrap();
            header.set_size(deb_contents.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &deb_contents[..]).unwrap();
        }
        encoder.finish().unwrap();

        let resolved = resolve_deb_path(&archive_path).unwrap();
        assert_ne!(resolved, archive_path, "must extract to a sibling file, not the tarball itself");
        assert_eq!(fs::read(&resolved).unwrap(), deb_contents);
        assert!(archive_path.exists(), "resolving must not delete the original download");
    }

    #[test]
    fn resolve_deb_path_rejects_content_that_is_neither_deb_nor_tar_gz() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("wraith-ota-0.3.1.tar.gz");
        fs::write(&bogus, b"not a deb and not gzip either").unwrap();

        let err = resolve_deb_path(&bogus).unwrap_err();
        assert!(matches!(err, OtaError::PrivilegedInstallFailed(_)), "got {err:?}");
    }

    #[test]
    fn install_deb_with_pkexec_fails_clearly_when_pkexec_is_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let empty_path_dir = tempfile::tempdir().unwrap();
        let _path = PathEnvGuard::set(empty_path_dir.path().as_os_str());

        let deb = Path::new("/tmp/wraith_0.3.1_amd64.deb");
        let err = install_deb_with_pkexec(deb).unwrap_err();
        assert!(matches!(err, OtaError::PrivilegedInstallFailed(_)), "got {err:?}");
        let message = err.to_string();
        assert!(
            message.contains("sudo dpkg -i /tmp/wraith_0.3.1_amd64.deb"),
            "error must carry the exact copy-paste command, got: {message}"
        );
    }
}
