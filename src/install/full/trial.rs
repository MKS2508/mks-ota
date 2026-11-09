//! A full update on trial, and the way back (Linux / AppImage).
//!
//! [`super::linux::install`] renames the new AppImage over the old one and
//! keeps nothing. Before an app installs a full update on its own (without a
//! human watching), it must be able to undo it if the new version never
//! confirms it works. This module keeps the previous AppImage next to the
//! current one and tracks the trial in plain files, so that a shell launcher
//! (no `jq`, no Rust) can perform the same rollback when the new version
//! cannot even start.
//!
//! # File contract — `<app_path>.ota/`
//!
//! Shared with the kiosk launcher (`tpv-el-haido2`,
//! `nix/modules/tpv-kiosk.nix`). Every file holds a single line of text;
//! every write goes to a temporary name in the same directory and is renamed
//! into place, so a reader never sees a half-written file.
//!
//! ```text
//! <app_path>.ota/
//!   previous   the AppImage that was running before the trial (hardlink,
//!              or a full copy when hardlinking fails). Only ever appears
//!              complete: a copy is written as `previous.tmp`, fsynced, then
//!              renamed. Consumed (moved over <app_path>) by a revert.
//!   trial      version under trial. Present = a full update has not
//!              confirmed yet.
//!   boots      boots of the trial version that reached the app's own code
//!              (`on_boot`).
//!   unacked    launcher starts since the app last reached `on_boot`. The
//!              app resets it to 0; the launcher increments it before
//!              launching.
//!   reverted   last version reverted on this device. Not downloaded again.
//! ```
//!
//! # Who does what
//!
//! - App, before installing: [`begin`] → [`super::linux::install`] →
//!   [`super::linux::relaunch`]; [`abort`] if the install fails.
//! - App, first thing at startup: [`on_boot`]. [`FullBoot::MustRevert`] ⇒
//!   [`revert`] and relaunch the previous one. [`FullBoot::Trial`] ⇒ arm a
//!   grace timer ([`FULL_READY_GRACE`]); if the app does not report ready in
//!   time, [`revert`].
//! - App, once ready: [`mark_ready`] (keeps `previous` until the next
//!   [`begin`]).
//! - Launcher, before starting the app: with `trial` present, `unacked >= 1`
//!   and `previous` present ⇒ `mv previous <app_path>`, `mv trial reverted`,
//!   drop `boots`/`unacked`. Otherwise `unacked += 1`. This covers a new
//!   version that dies before `on_boot` (missing library, broken image): two
//!   failed starts and the launcher reverts.
//!
//! Every function takes the already-resolved `app_path`
//! ([`super::linux::current_app_bundle`]) and reads no environment variable:
//! a dev or deb build simply never calls into this module.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::OtaError;

/// Boots the trial version may consume without confirming; the next one
/// reverts.
pub const MAX_FULL_BOOTS: u32 = 2;

/// How long a trial version has, from startup, to report ready.
pub const FULL_READY_GRACE: Duration = Duration::from_secs(120);

const PREVIOUS: &str = "previous";
const TRIAL: &str = "trial";
const BOOTS: &str = "boots";
const UNACKED: &str = "unacked";
const REVERTED: &str = "reverted";

/// `"<app_path>.ota"`.
pub fn trial_dir(app_path: &Path) -> PathBuf {
    let mut name = app_path.as_os_str().to_owned();
    name.push(".ota");
    PathBuf::from(name)
}

/// What [`on_boot`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullBoot {
    /// No full update on trial.
    Normal,
    /// A full update is on trial; this is its `boots`-th boot.
    Trial { version: String, boots: u32 },
    /// The trial version exceeded its boots without confirming: revert.
    MustRevert { version: String },
}

/// Opens a trial for `version`: saves the running AppImage as `previous`
/// and records the trial. Call right before [`super::linux::install`].
pub fn begin(app_path: &Path, version: &str) -> Result<(), OtaError> {
    let dir = trial_dir(app_path);
    fs::create_dir_all(&dir)?;
    save_previous(app_path, &dir)?;
    write_line(&dir, BOOTS, "0")?;
    write_line(&dir, UNACKED, "0")?;
    // Last: `trial` is what the launcher keys on, and `previous` must
    // already be complete by the time it can see it.
    write_line(&dir, TRIAL, version)?;
    Ok(())
}

/// Undoes [`begin`] when the install itself failed (`app_path` untouched).
pub fn abort(app_path: &Path) {
    let dir = trial_dir(app_path);
    for name in [TRIAL, BOOTS, UNACKED] {
        let _ = remove(&dir, name);
    }
}

/// Startup bookkeeping. Resets `unacked` (the app did start) and counts one
/// more boot of the trial version.
pub fn on_boot(app_path: &Path, max_boots: u32) -> FullBoot {
    let dir = trial_dir(app_path);
    let Some(version) = read_line(&dir, TRIAL) else {
        return FullBoot::Normal;
    };
    let boots = read_u32(&dir, BOOTS).saturating_add(1);
    if let Err(err) =
        write_line(&dir, UNACKED, "0").and_then(|_| write_line(&dir, BOOTS, &boots.to_string()))
    {
        eprintln!("[ota] could not record the trial boot: {err}");
    }
    if boots > max_boots {
        FullBoot::MustRevert { version }
    } else {
        FullBoot::Trial { version, boots }
    }
}

/// The trial version confirmed: drops `trial`, `boots` and `unacked`.
/// `previous` stays until the next [`begin`].
pub fn mark_ready(app_path: &Path) -> Result<(), OtaError> {
    let dir = trial_dir(app_path);
    for name in [TRIAL, BOOTS, UNACKED] {
        remove(&dir, name)?;
    }
    Ok(())
}

/// Puts `previous` back over `app_path` (consuming it) and records the trial
/// version as `reverted`. Returns the reverted version. The caller relaunches.
pub fn revert(app_path: &Path) -> Result<String, OtaError> {
    let dir = trial_dir(app_path);
    let version =
        read_line(&dir, TRIAL).ok_or(OtaError::NothingToRevert("no full update is on trial"))?;
    let previous = dir.join(PREVIOUS);
    if !previous.is_file() {
        return Err(OtaError::NothingToRevert("the previous AppImage is gone"));
    }
    // `reverted` first: if the process dies right after the rename, the
    // version is already known as bad and is not downloaded again.
    write_line(&dir, REVERTED, &version)?;
    fs::rename(&previous, app_path)?;
    for name in [TRIAL, BOOTS, UNACKED] {
        remove(&dir, name)?;
    }
    Ok(version)
}

/// Version under trial, if any.
pub fn trial_version(app_path: &Path) -> Option<String> {
    read_line(&trial_dir(app_path), TRIAL)
}

/// Last version reverted on this device (by the app or the launcher).
pub fn reverted_version(app_path: &Path) -> Option<String> {
    read_line(&trial_dir(app_path), REVERTED)
}

/// `previous` = the current AppImage: a hardlink when possible (same
/// filesystem, instant, no extra space), else a full copy written under a
/// temporary name so a cut copy is never called `previous`.
fn save_previous(app_path: &Path, dir: &Path) -> Result<(), OtaError> {
    let tmp = dir.join(format!("{PREVIOUS}.tmp"));
    let _ = fs::remove_file(&tmp);
    if fs::hard_link(app_path, &tmp).is_err() {
        fs::copy(app_path, &tmp)?;
        fs::File::open(&tmp)?.sync_all()?;
    }
    fs::rename(&tmp, dir.join(PREVIOUS))?;
    Ok(())
}

fn write_line(dir: &Path, name: &str, value: &str) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, format!("{value}\n"))?;
    fs::rename(&tmp, dir.join(name))
}

fn read_line(dir: &Path, name: &str) -> Option<String> {
    let raw = fs::read_to_string(dir.join(name)).ok()?;
    let line = raw.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

fn read_u32(dir: &Path, name: &str) -> u32 {
    read_line(dir, name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn remove(dir: &Path, name: &str) -> io::Result<()> {
    match fs::remove_file(dir.join(name)) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// A fake `<dir>/App.AppImage` holding `content`.
    fn app(content: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("App.AppImage");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// What `linux::install` does to `app_path`: an atomic rename of a new
    /// file over it (a new inode — the old one lives on through `previous`).
    fn swap_in(app_path: &Path, content: &[u8]) {
        let tmp = app_path.with_extension("new");
        fs::write(&tmp, content).unwrap();
        fs::rename(&tmp, app_path).unwrap();
    }

    #[test]
    fn trial_dir_sits_next_to_the_appimage() {
        assert_eq!(
            trial_dir(Path::new("/var/lib/tpv/TPV.AppImage")),
            PathBuf::from("/var/lib/tpv/TPV.AppImage.ota")
        );
    }

    #[test]
    fn begin_saves_the_running_appimage_as_previous() {
        let (_d, app_path) = app(b"old");
        begin(&app_path, "0.3.5").unwrap();

        let dir = trial_dir(&app_path);
        let previous = dir.join(PREVIOUS);
        assert_eq!(fs::read(&previous).unwrap(), b"old");
        assert_eq!(
            fs::metadata(&previous).unwrap().ino(),
            fs::metadata(&app_path).unwrap().ino(),
            "hardlink on the same fs"
        );
        assert_eq!(trial_version(&app_path).as_deref(), Some("0.3.5"));
        assert_eq!(fs::read_to_string(dir.join(BOOTS)).unwrap(), "0\n");
        assert_eq!(fs::read_to_string(dir.join(UNACKED)).unwrap(), "0\n");
        assert!(!dir.join(format!("{PREVIOUS}.tmp")).exists());
    }

    #[test]
    fn install_then_revert_restores_the_original_and_records_the_version() {
        let (_d, app_path) = app(b"old");
        begin(&app_path, "0.3.5").unwrap();
        swap_in(&app_path, b"new");
        assert_eq!(fs::read(&app_path).unwrap(), b"new");

        assert_eq!(revert(&app_path).unwrap(), "0.3.5");
        assert_eq!(fs::read(&app_path).unwrap(), b"old");
        assert_eq!(reverted_version(&app_path).as_deref(), Some("0.3.5"));
        assert_eq!(trial_version(&app_path), None);
        let dir = trial_dir(&app_path);
        assert!(!dir.join(PREVIOUS).exists(), "revert consumes previous");
        assert!(!dir.join(BOOTS).exists() && !dir.join(UNACKED).exists());
        assert_eq!(on_boot(&app_path, MAX_FULL_BOOTS), FullBoot::Normal);
    }

    #[test]
    fn the_boot_after_max_boots_must_revert() {
        let (_d, app_path) = app(b"old");
        begin(&app_path, "0.3.5").unwrap();
        swap_in(&app_path, b"new");

        // The launcher counted a start; the app reaching on_boot resets it.
        write_line(&trial_dir(&app_path), UNACKED, "1").unwrap();
        assert_eq!(
            on_boot(&app_path, MAX_FULL_BOOTS),
            FullBoot::Trial {
                version: "0.3.5".into(),
                boots: 1
            }
        );
        assert_eq!(read_u32(&trial_dir(&app_path), UNACKED), 0);
        assert_eq!(
            on_boot(&app_path, MAX_FULL_BOOTS),
            FullBoot::Trial {
                version: "0.3.5".into(),
                boots: 2
            }
        );
        assert_eq!(
            on_boot(&app_path, MAX_FULL_BOOTS),
            FullBoot::MustRevert {
                version: "0.3.5".into()
            }
        );
    }

    #[test]
    fn mark_ready_ends_the_trial_and_keeps_previous() {
        let (_d, app_path) = app(b"old");
        begin(&app_path, "0.3.5").unwrap();
        swap_in(&app_path, b"new");
        on_boot(&app_path, MAX_FULL_BOOTS);

        mark_ready(&app_path).unwrap();
        let dir = trial_dir(&app_path);
        assert_eq!(trial_version(&app_path), None);
        assert!(!dir.join(BOOTS).exists() && !dir.join(UNACKED).exists());
        assert_eq!(
            fs::read(dir.join(PREVIOUS)).unwrap(),
            b"old",
            "previous stays until the next begin"
        );
        assert_eq!(on_boot(&app_path, MAX_FULL_BOOTS), FullBoot::Normal);
        assert!(
            matches!(revert(&app_path), Err(OtaError::NothingToRevert(_))),
            "no trial, nothing to revert"
        );
        mark_ready(&app_path).unwrap(); // idempotent
    }

    #[test]
    fn abort_clears_the_trial_without_touching_the_appimage() {
        let (_d, app_path) = app(b"old");
        begin(&app_path, "0.3.5").unwrap();
        abort(&app_path);
        assert_eq!(fs::read(&app_path).unwrap(), b"old");
        assert_eq!(trial_version(&app_path), None);
        assert_eq!(on_boot(&app_path, MAX_FULL_BOOTS), FullBoot::Normal);
    }

    #[test]
    fn a_second_begin_replaces_previous_with_the_current_appimage() {
        let (_d, app_path) = app(b"v1");
        begin(&app_path, "2").unwrap();
        swap_in(&app_path, b"v2");
        mark_ready(&app_path).unwrap();

        begin(&app_path, "3").unwrap();
        assert_eq!(
            fs::read(trial_dir(&app_path).join(PREVIOUS)).unwrap(),
            b"v2"
        );
        assert_eq!(trial_version(&app_path).as_deref(), Some("3"));
    }

    #[test]
    fn revert_without_previous_fails_and_leaves_the_appimage() {
        let (_d, app_path) = app(b"old");
        begin(&app_path, "0.3.5").unwrap();
        swap_in(&app_path, b"new");
        fs::remove_file(trial_dir(&app_path).join(PREVIOUS)).unwrap();

        assert!(matches!(
            revert(&app_path),
            Err(OtaError::NothingToRevert(_))
        ));
        assert_eq!(fs::read(&app_path).unwrap(), b"new");
        assert_eq!(reverted_version(&app_path), None);
    }
}
