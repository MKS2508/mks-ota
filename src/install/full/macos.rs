// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Portions of this file are derived from `tauri-plugin-updater` 2.10.1
// (https://github.com/tauri-apps/plugins-workspace/tree/v2/plugins/updater),
// `src/updater.rs` lines 1288-1381 (the macOS `install_inner`): extract to a
// tmpdir skipping the archive's leading path component, then a two-step
// rename with an AppleScript privilege-escalation fallback. See
// `THIRD-PARTY.md` for the full attribution.
// Copyright (c) Tauri Programme within The Commons Conservancy.
//
// Adapted for this crate:
//   - the manifest/URL-templating half of the original file is dropped —
//     `crate::manifest` and `crate::download` own that here;
//   - a same-device check is added before the rename (upstream only does
//     this on Linux, `updater.rs:1064` — macOS did not, latent bug);
//   - relaunch is added (upstream does not relaunch on macOS,
//     `updater.rs:747-750` — "you need to relaunch the app").

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use tar::Archive;
use tempfile::TempDir;

use crate::error::OtaError;

/// Resolves the `.app` bundle root from the currently running executable —
/// a macOS executable always lives at `Some.app/Contents/MacOS/binary`, so
/// walking up from the `MacOS` ancestor twice lands on the bundle root
/// (same heuristic as `extract_path_from_executable`, `updater.rs:1424`).
pub fn current_app_bundle() -> Result<PathBuf, OtaError> {
    let exe = std::env::current_exe()?;
    exe.ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "MacOS"))
        .and_then(Path::parent) // Contents/
        .and_then(Path::parent) // *.app/
        .map(Path::to_path_buf)
        .ok_or(OtaError::AppBundleNotFound)
}

/// Extracts `archive` (a `.tar.gz`) into `into`, dropping the leading path
/// component — the archive's `AppName.app/` header — so `into` ends up
/// holding the bundle's contents directly (`updater.rs:1309`,
/// `entry.path()?.iter().skip(1)`).
pub fn extract_app_bundle(archive: &Path, into: &Path) -> Result<(), OtaError> {
    let file = fs::File::open(archive)?;
    let mut tar = Archive::new(GzDecoder::new(file));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let relative: PathBuf = entry.path()?.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue; // the archive's own top-level directory entry
        }
        let dest = into.join(&relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&dest)?;
    }
    Ok(())
}

/// Downloaded-and-verified install: extracts `archive` and swaps it into
/// `app_path` (the running `.app`), restoring the previous version if the
/// swap fails partway through.
pub fn install(archive: &Path, app_path: &Path) -> Result<(), OtaError> {
    let dest_parent = app_path.parent().ok_or(OtaError::AppBundleNotFound)?;

    let extract_dir = pick_tmp_dir(dest_parent)?;
    let new_app = extract_dir.path().join("new-app");
    fs::create_dir_all(&new_app)?;
    extract_app_bundle(archive, &new_app)?;

    let backup_dir = pick_tmp_dir(dest_parent)?;
    let backup_app = backup_dir.path().join("previous-app");

    swap_app_dirs(app_path, &new_app, &backup_app)
}

/// Relaunches the app at `app_path` via `open -n` — works uniformly right
/// after a fresh install, no need to parse `Info.plist` for the executable
/// name. Tauri's plugin does not do this on macOS (`updater.rs:747-750`) —
/// the caller is expected to exit the current process afterward through
/// Tauri's own shutdown (e.g. `app_handle.exit(0)`), not a raw
/// `std::process::exit`, so window/state cleanup still runs.
pub fn relaunch(app_path: &Path) -> Result<(), OtaError> {
    Command::new("open").arg("-n").arg(app_path).spawn()?;
    Ok(())
}

/// Moves `current` aside to `backup`, then moves `new_app` into `current`'s
/// place. If the second rename fails, restores `backup` back to `current`
/// before returning the original error — the app is left exactly as it was
/// found, never half-swapped.
fn swap_app_dirs(current: &Path, new_app: &Path, backup: &Path) -> Result<(), OtaError> {
    if let Err(e) = fs::rename(current, backup) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            escalate_rename(current, backup)?;
        } else {
            return Err(e.into());
        }
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

/// `updater.rs:1341-1366`'s AppleScript escalation, ported to shell out to
/// `osascript` directly instead of pulling in the `osakit` crate the
/// plugin uses for it — same effect, one less dependency.
fn escalate_rename(from: &Path, to: &Path) -> Result<(), OtaError> {
    let script =
        format!("do shell script \"mv -f {} {}\" with administrator privileges", shell_quote(from), shell_quote(to));
    let status = Command::new("osascript").arg("-e").arg(&script).status()?;
    if !status.success() {
        return Err(OtaError::PrivilegedInstallFailed(format!("osascript exited with {status}")));
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Refreshes the bundle's mtime so LaunchServices/Finder notice the change
/// (`updater.rs:1376-1378`).
fn touch(path: &Path) -> Result<(), OtaError> {
    let status = Command::new("touch").arg(path).status()?;
    if !status.success() {
        return Err(OtaError::PrivilegedInstallFailed(format!("touch exited with {status}")));
    }
    Ok(())
}

/// Picks a tmpdir on the same device as `dest_parent` — the check
/// `tauri-plugin-updater` has on Linux (`updater.rs:1064`,
/// `TempDirNotOnSameMountPoint`) but not on macOS, where a `tempfile`
/// landing on a different volume than `/Applications` would make the final
/// rename fail. Prefers the system temp dir; falls back to `dest_parent`
/// itself, which is on its own device by construction.
fn pick_tmp_dir(dest_parent: &Path) -> Result<TempDir, OtaError> {
    let dest_dev = fs::metadata(dest_parent)?.dev();
    for candidate in [std::env::temp_dir(), dest_parent.to_path_buf()] {
        if fs::metadata(&candidate).ok().map(|m| m.dev()) == Some(dest_dev) {
            return tempfile::Builder::new().prefix("mks-ota-").tempdir_in(&candidate).map_err(OtaError::from);
        }
    }
    Err(OtaError::CrossDeviceRename { tmp_dev: fs::metadata(std::env::temp_dir())?.dev(), dest_dev })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT_URL: &str =
        "https://wraith.releases.mks2508.systems/api/components/wraith-linux/download/0.1.0/darwin/aarch64/Wraith.app.tar.gz";
    const EXPECTED_BYTES: u64 = 31_871_987;

    /// Cached under the system temp dir across test runs — re-downloading
    /// 30 MB per test in this module would be wasteful; the artifact is
    /// immutable at this pinned version.
    fn real_artifact_path() -> PathBuf {
        static ARTIFACT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ARTIFACT
            .get_or_init(|| {
                let dir = std::env::temp_dir().join("mks-ota-test-fixtures");
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("Wraith.app.tar.gz");
                if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) != EXPECTED_BYTES {
                    let mut resp = reqwest::blocking::get(ARTIFACT_URL).unwrap();
                    let mut file = fs::File::create(&path).unwrap();
                    std::io::copy(&mut resp, &mut file).unwrap();
                }
                path
            })
            .clone()
    }

    #[test]
    fn extracts_dropping_the_leading_app_component() {
        let into = tempfile::tempdir().unwrap();
        extract_app_bundle(&real_artifact_path(), into.path()).unwrap();
        assert!(
            into.path().join("Contents/MacOS").exists(),
            "expected Contents/MacOS directly under the extract root, not nested under Wraith.app/"
        );
        assert!(into.path().join("Contents/Info.plist").is_file());
    }

    #[test]
    fn install_swaps_the_bundle_in_place() {
        let apps_dir = tempfile::tempdir().unwrap(); // stands in for /Applications
        let app_path = apps_dir.path().join("Wraith.app");
        fs::create_dir_all(app_path.join("Contents/MacOS")).unwrap();
        fs::write(app_path.join("Contents/MacOS/old-marker"), b"0.1.0").unwrap();

        install(&real_artifact_path(), &app_path).unwrap();

        assert!(app_path.join("Contents/Info.plist").is_file(), "new bundle should be in place");
        assert!(
            !app_path.join("Contents/MacOS/old-marker").exists(),
            "old bundle content must be gone, not merged with the new one"
        );
        assert!(app_path.join("Contents/MacOS/wraithd").is_file());
    }

    #[test]
    fn relaunch_issues_open_without_erroring_synchronously() {
        // `spawn()` only fails if `open` itself can't start; whether the
        // fabricated bundle below actually launches is decided by a
        // separate `open` helper process, observed asynchronously — not
        // something this test can assert without a real signed bundle.
        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = apps_dir.path().join("Wraith.app");
        fs::create_dir_all(app_path.join("Contents/MacOS")).unwrap();
        relaunch(&app_path).unwrap();
    }

    #[test]
    fn a_failed_swap_restores_the_previous_bundle() {
        let apps_dir = tempfile::tempdir().unwrap();
        let app_path = apps_dir.path().join("Wraith.app");
        fs::create_dir_all(app_path.join("Contents/MacOS")).unwrap();
        fs::write(app_path.join("Contents/MacOS/marker"), b"still the old one").unwrap();

        let backup_dir = tempfile::tempdir().unwrap();
        // A `new_app` source that does not exist forces the second rename
        // to fail deterministically, without needing to break permissions.
        let missing_new_app = backup_dir.path().join("this-does-not-exist");

        let err = swap_app_dirs(&app_path, &missing_new_app, &backup_dir.path().join("backup")).unwrap_err();
        assert!(matches!(err, OtaError::Io(_)), "got {err:?}");

        assert!(app_path.join("Contents/MacOS/marker").is_file(), "the original bundle must be restored");
        let content = fs::read_to_string(app_path.join("Contents/MacOS/marker")).unwrap();
        assert_eq!(content, "still the old one");
    }

    #[test]
    fn pick_tmp_dir_lands_on_the_same_device_as_the_destination() {
        let dest = tempfile::tempdir().unwrap();
        let picked = pick_tmp_dir(dest.path()).unwrap();
        assert_eq!(fs::metadata(dest.path()).unwrap().dev(), fs::metadata(picked.path()).unwrap().dev());
    }
}
