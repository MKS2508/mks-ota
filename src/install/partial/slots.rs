//! On-disk state of the A/B slots holding partial artifacts (extracted from
//! `tpv-el-haido2`'s production updater, `ota/slots.rs`).
//!
//! Layout under `{app_data_dir}/bundles/`:
//!
//! ```text
//! bundles/
//!   state.json        <- which slot is active, previous, staged, verified
//!   <slot-id>/        <- decompressed artifact
//!   <slot-id>/
//! ```
//!
//! The pointer to the active slot is a state FILE, not a symlink: on Windows
//! creating symlinks requires privileges the point-of-sale user does not
//! have.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the state file inside `bundles/`.
const STATE_FILE: &str = "state.json";

/// Persistent slot state. `serde(default)` keeps old `state.json` files
/// readable as fields are added, and unknown fields (e.g. the retired
/// hub-report ids) are ignored on load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotState {
    /// Slot being served. `None` = the embedded frontend/assets are served.
    pub active: Option<String>,
    /// Previous slot, kept so a rollback has somewhere to land.
    pub previous: Option<String>,
    /// Downloaded and verified slot, waiting to be activated.
    pub staged: Option<String>,
    /// Semver of the staged artifact — feeds the no-downgrade gate.
    pub staged_version: Option<String>,
    /// Semver of the active artifact.
    pub active_version: Option<String>,
    /// `false` until the active slot has confirmed `app-ready`.
    pub verified: bool,
    /// Boots consumed by the active slot without confirming.
    pub boot_attempts: u32,
    /// Native binary version at the time the slot was activated.
    ///
    /// After a full update swaps the native binary, the embedded frontend is
    /// newer than any installed slot: keep serving the old slot and an old UI
    /// ends up talking to new commands. The native channel invalidates the
    /// partial one — [`invalidate_if_native_changed`].
    pub native_version_at_swap: Option<String>,
}

/// Root of the slots inside the app data dir.
pub fn bundles_root(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("bundles")
}

/// Reads the state. A missing or corrupt state reads as "no slot": the
/// embedded assets are served, and they are always available.
pub fn load_state(app_data_dir: &Path) -> SlotState {
    let path = bundles_root(app_data_dir).join(STATE_FILE);
    let Ok(raw) = fs::read_to_string(&path) else {
        return SlotState::default();
    };
    serde_json::from_str(&raw).unwrap_or_else(|err| {
        eprintln!("[ota] state.json unreadable ({err}); serving the embedded frontend");
        SlotState::default()
    })
}

/// Writes the state with tmp + rename, so a crash mid-write cannot leave a
/// truncated `state.json` that would leave the app with nothing to serve.
pub fn save_state(app_data_dir: &Path, state: &SlotState) -> Result<(), String> {
    let root = bundles_root(app_data_dir);
    fs::create_dir_all(&root).map_err(|e| format!("could not create {}: {e}", root.display()))?;

    let final_path = root.join(STATE_FILE);
    let tmp_path = root.join(format!("{STATE_FILE}.tmp"));
    let body = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;

    fs::write(&tmp_path, body).map_err(|e| format!("could not write the state: {e}"))?;
    fs::rename(&tmp_path, &final_path).map_err(|e| format!("could not commit the state: {e}"))
}

/// Directory of the active slot, if there is one and it exists on disk.
pub fn active_dir(app_data_dir: &Path, state: &SlotState) -> Option<PathBuf> {
    let id = state.active.as_ref()?;
    let dir = bundles_root(app_data_dir).join(id);
    dir.is_dir().then_some(dir)
}

/// Deactivates the active slot if a different native binary installed it.
///
/// Runs at startup, before serving anything. Contract invariant L6: the
/// native channel invalidates the partial one.
///
/// @returns `true` if something was deactivated (the state is persisted).
pub fn invalidate_if_native_changed(app_data_dir: &Path, native_version: &str) -> bool {
    let mut state = load_state(app_data_dir);
    if state.active.is_none() {
        return false;
    }

    let matches = state
        .native_version_at_swap
        .as_deref()
        .is_some_and(|v| v == native_version);
    if matches {
        return false;
    }

    eprintln!(
        "[ota] native binary changed ({:?} -> {native_version}); dropping the active slot",
        state.native_version_at_swap
    );
    state.previous = state.active.take();
    state.active_version = None;
    state.verified = false;
    state.boot_attempts = 0;
    state.native_version_at_swap = None;
    if let Err(err) = save_state(app_data_dir, &state) {
        eprintln!("[ota] could not persist the invalidation: {err}");
    }
    true
}

/// Resolves a webview-requested path against a root directory, rejecting
/// anything that escapes it.
///
/// A partial artifact is remote content: without this check, a request to
/// `../../app.db` would serve the database over the custom scheme.
pub fn resolve_within(root: &Path, request_path: &str) -> Option<PathBuf> {
    let relative = request_path.trim_start_matches('/');
    let candidate = Path::new(relative);

    // Reject by path components BEFORE touching the disk: `canonicalize`
    // only works on files that exist, and paths that point outside must be
    // stopped whether or not they exist.
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            _ => return None,
        }
    }

    let joined = root.join(candidate);

    // Second barrier for symlinks inside the artifact, which can point
    // outside even when the requested path looks innocent.
    let (Ok(real_root), Ok(real_target)) = (root.canonicalize(), joined.canonicalize()) else {
        return None;
    };
    real_target.starts_with(&real_root).then_some(real_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mks-ota-slots-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_state_reads_as_embedded_frontend() {
        let dir = tmpdir("missing");
        let state = load_state(&dir);
        assert!(state.active.is_none());
        assert!(active_dir(&dir, &state).is_none());
    }

    #[test]
    fn corrupt_state_does_not_break_startup() {
        let dir = tmpdir("corrupt");
        fs::create_dir_all(bundles_root(&dir)).unwrap();
        fs::write(bundles_root(&dir).join(STATE_FILE), "{ not json").unwrap();
        assert!(load_state(&dir).active.is_none());
    }

    #[test]
    fn state_round_trips() {
        let dir = tmpdir("roundtrip");
        let state = SlotState {
            active: Some("abc123".into()),
            active_version: Some("0.2.0".into()),
            verified: true,
            native_version_at_swap: Some("0.1.0".into()),
            ..Default::default()
        };
        save_state(&dir, &state).unwrap();
        let back = load_state(&dir);
        assert_eq!(back.active.as_deref(), Some("abc123"));
        assert_eq!(back.active_version.as_deref(), Some("0.2.0"));
        assert!(back.verified);
    }

    #[test]
    fn old_state_files_with_retired_fields_still_load() {
        // state.json written by tpv-el-haido2 before the migration carries
        // staged_hub_id/active_hub_id; serde must ignore them, not fail.
        let dir = tmpdir("legacy");
        fs::create_dir_all(bundles_root(&dir)).unwrap();
        fs::write(
            bundles_root(&dir).join(STATE_FILE),
            r#"{"active":"old","previous":null,"staged":null,"staged_hub_id":null,
                "active_hub_id":"some-uuid","verified":true,"boot_attempts":0,
                "native_version_at_swap":"0.2.11"}"#,
        )
        .unwrap();
        let state = load_state(&dir);
        assert_eq!(state.active.as_deref(), Some("old"));
        assert!(state.verified);
    }

    #[test]
    fn active_slot_only_when_it_exists_on_disk() {
        let dir = tmpdir("exists");
        let state = SlotState { active: Some("ghost".into()), ..Default::default() };
        assert!(active_dir(&dir, &state).is_none(), "a slot that is not on disk is not served");

        fs::create_dir_all(bundles_root(&dir).join("ghost")).unwrap();
        assert!(active_dir(&dir, &state).is_some());
    }

    #[test]
    fn a_native_binary_change_drops_the_slot() {
        let dir = tmpdir("native-change");
        save_state(&dir, &SlotState {
            active: Some("old-bundle".into()),
            active_version: Some("0.1.0".into()),
            verified: true,
            native_version_at_swap: Some("0.1.0".into()),
            ..Default::default()
        })
        .unwrap();

        assert!(invalidate_if_native_changed(&dir, "0.2.0"));
        let state = load_state(&dir);
        assert!(state.active.is_none());
        assert!(state.active_version.is_none());
        assert_eq!(state.previous.as_deref(), Some("old-bundle"));

        // Idempotent: with no active slot there is nothing to invalidate.
        assert!(!invalidate_if_native_changed(&dir, "0.2.0"));
    }

    #[test]
    fn same_native_binary_keeps_the_slot() {
        let dir = tmpdir("native-same");
        save_state(&dir, &SlotState {
            active: Some("bundle".into()),
            native_version_at_swap: Some("0.1.0".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(!invalidate_if_native_changed(&dir, "0.1.0"));
        assert_eq!(load_state(&dir).active.as_deref(), Some("bundle"));
    }

    #[test]
    fn escaping_the_slot_directory_is_rejected() {
        let dir = tmpdir("traversal");
        let root = dir.join("slot");
        fs::create_dir_all(&root).unwrap();
        fs::write(dir.join("secret.db"), b"data").unwrap();
        fs::write(root.join("index.html"), b"<html>").unwrap();

        assert!(resolve_within(&root, "/index.html").is_some());
        assert!(resolve_within(&root, "/../secret.db").is_none());
        assert!(resolve_within(&root, "../secret.db").is_none());
        assert!(resolve_within(&root, "/assets/../../secret.db").is_none());
        assert!(resolve_within(&root, "/does-not-exist.js").is_none());
    }
}
