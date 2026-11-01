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
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use tar::Archive;

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

/// Downloaded-and-verified install. AppImage case: `downloaded` is either
/// the `.AppImage` itself (what `tauri build` emits and signs) or a
/// `.tar.gz` carrying one; the new file is staged in the AppImage's own
/// directory and renamed over it. `rename(2)` replaces the file atomically:
/// the running process keeps the old inode mounted, and a crash leaves
/// either the old or the new AppImage at `app_path`, never a mix. If the
/// running bundle was a deb (no `$APPIMAGE`), this resolves `downloaded` to
/// an installable `.deb` and runs `pkexec dpkg -i` on it — the new binary
/// takes effect on the next launch.
pub fn install(downloaded: &Path, app_path: &Path) -> Result<(), OtaError> {
    // deb path: no APPIMAGE means this was installed via dpkg.
    if std::env::var_os("APPIMAGE").is_none() {
        let deb = resolve_deb_path(downloaded)?;
        return install_deb_with_pkexec(&deb);
    }

    let dest_parent = app_path.parent().ok_or(OtaError::AppBundleNotFound)?;
    let staging = tempfile::Builder::new()
        .prefix(".mks-ota-")
        .tempdir_in(dest_parent)
        .map_err(|e| permission_hint(e, dest_parent))?;
    let new_appimage = staging.path().join("new.AppImage");

    let magic = read_magic(downloaded)?;
    if magic.starts_with(ELF_MAGIC) {
        fs::copy(downloaded, &new_appimage)?;
    } else if magic.starts_with(&GZIP_MAGIC) {
        extract_appimage_from_tar_gz(downloaded, &new_appimage)?;
    } else {
        return Err(OtaError::BadFullArtifact(format!(
            "{} is neither an AppImage nor a .tar.gz",
            downloaded.display()
        )));
    }

    fs::set_permissions(&new_appimage, fs::metadata(app_path)?.permissions())?;
    fs::File::open(&new_appimage)?.sync_all()?;
    fs::rename(&new_appimage, app_path).map_err(|e| permission_hint(e, app_path))
}

/// Every AppImage is an ELF executable (type 2 runtime + squashfs payload).
const ELF_MAGIC: &[u8] = b"\x7fELF";
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
    let magic = read_magic(downloaded)?;
    if magic.starts_with(DEB_MAGIC) {
        return Ok(downloaded.to_path_buf());
    }
    if magic.starts_with(&GZIP_MAGIC) {
        return extract_deb_from_tar_gz(downloaded);
    }
    Err(OtaError::PrivilegedInstallFailed(format!(
        "{} is neither a .deb nor a .tar.gz — cannot install",
        downloaded.display()
    )))
}

/// The first bytes of `path`, enough to tell the artifact kinds apart
/// (shorter if the file is).
fn read_magic(path: &Path) -> Result<Vec<u8>, OtaError> {
    let mut header = Vec::with_capacity(8);
    fs::File::open(path)?.take(8).read_to_end(&mut header)?;
    Ok(header)
}

/// Unpacks the single `.AppImage` entry of `archive` (a gzip tarball) to
/// `dest`, wherever it sits inside the tarball.
fn extract_appimage_from_tar_gz(archive: &Path, dest: &Path) -> Result<(), OtaError> {
    let mut tar = Archive::new(GzDecoder::new(fs::File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let is_appimage = entry
            .path()?
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("appimage"));
        if is_appimage && entry.header().entry_type().is_file() {
            entry.unpack(dest)?;
            return Ok(());
        }
    }
    Err(OtaError::BadFullArtifact(format!(
        "{} is a tar.gz with no .AppImage entry inside",
        archive.display()
    )))
}

/// Maps `PermissionDenied` to the actionable error: an AppImage can only
/// replace itself from a directory its user can write to.
fn permission_hint(e: std::io::Error, path: &Path) -> OtaError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return OtaError::PrivilegedInstallFailed(format!(
            "no write access to {}; the AppImage must live in a directory owned by the user running it",
            path.display()
        ));
    }
    e.into()
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

/// AppImage case: replaces the running process with the new AppImage at
/// `app_path` (`execve`, same PID and arguments) and only returns if that
/// fails. A spawned child would briefly run alongside the old instance, and
/// launchers that sandbox the AppImage with `--die-with-parent` (NixOS
/// `appimage-run`) kill it as soon as the old instance exits. Deb case:
/// returns Ok(()) — dpkg already queued the new version, the caller is
/// expected to exit so the next launch picks it up.
pub fn relaunch(app_path: &Path) -> Result<(), OtaError> {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("APPIMAGE").is_none() {
        return Ok(());
    }
    let err = std::process::Command::new(app_path)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// An existing executable AppImage at `apps_dir/TestApp.AppImage`.
    fn make_current_appimage(apps_dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let app_path = apps_dir.join("TestApp.AppImage");
        fs::write(&app_path, b"old-appimage-content").unwrap();
        fs::set_permissions(&app_path, fs::Permissions::from_mode(0o755)).unwrap();
        app_path
    }

    /// `app_path` is a regular executable file holding exactly `expected`,
    /// and the staging directory left nothing behind next to it.
    fn assert_swapped(app_path: &Path, expected: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(app_path).unwrap();
        assert!(meta.is_file(), "the AppImage must stay a file");
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read(app_path).unwrap(), expected);
        let leftovers: Vec<_> = fs::read_dir(app_path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".mks-ota-"))
            .collect();
        assert!(leftovers.is_empty(), "staging dir left behind: {leftovers:?}");
    }

    #[test]
    fn install_replaces_the_appimage_with_a_raw_appimage() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _appimage = AppImageEnvGuard::set("/nonexistent-appimage-marker");

        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = make_current_appimage(apps_dir.path());
        let downloads = tempfile::tempdir().unwrap();
        let downloaded = downloads.path().join("TestApp_0.2.0_amd64.AppImage");
        let new_contents = b"\x7fELF\x02\x01\x01\x00AI\x02-new-appimage";
        fs::write(&downloaded, new_contents).unwrap();

        install(&downloaded, &app_path).unwrap();

        assert_swapped(&app_path, new_contents);
    }

    #[test]
    fn install_replaces_the_appimage_with_one_from_a_tar_gz() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _appimage = AppImageEnvGuard::set("/nonexistent-appimage-marker");

        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = make_current_appimage(apps_dir.path());
        let downloads = tempfile::tempdir().unwrap();
        let archive = make_appimage_fixture(downloads.path());

        install(&archive, &app_path).unwrap();

        assert_swapped(&app_path, b"#!/bin/sh\necho fake-appimage\n");
    }

    #[test]
    fn install_rejects_an_artifact_that_is_not_an_appimage_and_keeps_the_current_one() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _appimage = AppImageEnvGuard::set("/nonexistent-appimage-marker");

        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = make_current_appimage(apps_dir.path());
        let downloads = tempfile::tempdir().unwrap();
        let bogus = downloads.path().join("TestApp.AppImage");
        fs::write(&bogus, b"<html>404</html>").unwrap();

        let err = install(&bogus, &app_path).unwrap_err();
        assert!(matches!(err, OtaError::BadFullArtifact(_)), "got {err:?}");
        assert_swapped(&app_path, b"old-appimage-content");
    }

    #[test]
    fn relaunch_returns_the_exec_error_and_keeps_the_process_when_exec_fails() {
        // A successful exec would replace the test runner, so only the
        // failure path is observable in-process.
        let _lock = ENV_LOCK.lock().unwrap();
        let _appimage = AppImageEnvGuard::set("/nonexistent-appimage-marker");
        let apps_dir = tempfile::tempdir().unwrap();

        let err = relaunch(&apps_dir.path().join("missing.AppImage")).unwrap_err();
        assert!(matches!(err, OtaError::Io(_)), "got {err:?}");
    }

    #[test]
    fn relaunch_is_a_no_op_for_deb_installs() {
        let _lock = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("APPIMAGE");
        unsafe { std::env::remove_var("APPIMAGE") };
        let result = relaunch(Path::new("/nonexistent"));
        if let Some(v) = previous {
            unsafe { std::env::set_var("APPIMAGE", v) };
        }
        result.unwrap();
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
