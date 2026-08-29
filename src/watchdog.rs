//! Safety net of the partial channel: an artifact that never boots gets
//! reverted on its own (extracted from `tpv-el-haido2`'s production updater,
//! `ota/watchdog.rs`).
//!
//! The watchdog counts BOOTS, not seconds. A timer inside the process only
//! covers "the app hangs"; if the new artifact kills the webview or the whole
//! process, that timer never fires. By counting boots consumed without
//! confirmation, an artifact that dies on load is reverted on the next boot —
//! which is exactly the bad scenario at a point of sale.

use std::path::Path;
use std::time::Duration;

#[cfg(feature = "tauri")]
use tauri::Manager;

use crate::install::partial::{apply, slots};

/// Boots granted to an unconfirmed artifact before reverting it.
///
/// Two, not one: a first boot can die for reasons unrelated to the artifact
/// (power cut mid-way, a hard kill), and reverting for that would be a false
/// positive.
pub const MAX_BOOT_ATTEMPTS: u32 = 2;

/// Grace given to a freshly hot-applied artifact to confirm.
///
/// Counting boots covers the artifact killing the process, but not the
/// hot path: after activating and reloading the webview there is NO restart,
/// and if the new artifact dies in JS nobody consumes a boot. Without this
/// timer the machine is stuck with a broken UI until someone reboots by hand
/// — and then it would take three reboots to revert. The two mechanisms
/// complement each other: a counter for the process, a timer for the reload.
pub const HOT_APPLY_GRACE: Duration = Duration::from_secs(90);

/// Event notifying the frontend that a hot revert happened. Requires the
/// `tauri` feature to be emitted (see [`arm_hot_apply`]); declared here so
/// consumers on any feature set share one name.
pub const BUNDLE_REVERTED_EVENT: &str = "ota://bundle-reverted";

/// Boot reconciliation outcome, for logging.
#[derive(Debug, PartialEq, Eq)]
pub enum BootOutcome {
    /// No active slot: the embedded frontend is served.
    NoBundle,
    /// The active slot was already confirmed.
    Verified,
    /// Unconfirmed slot that still has attempts left.
    Pending { attempt: u32 },
    /// Attempts exhausted, fell back to the previous slot.
    RolledBack { failed: Option<String> },
}

/// Runs at startup, BEFORE the window is created: if it returns
/// `RolledBack`, the protocol must already serve the previous slot.
pub fn reconcile_boot(app_data_dir: &Path) -> BootOutcome {
    let mut state = slots::load_state(app_data_dir);

    if state.active.is_none() {
        return BootOutcome::NoBundle;
    }
    if state.verified {
        return BootOutcome::Verified;
    }

    state.boot_attempts += 1;

    if state.boot_attempts > MAX_BOOT_ATTEMPTS {
        let failed = apply::rollback(app_data_dir).ok().flatten();
        eprintln!(
            "[ota] the slot {failed:?} did not confirm in {MAX_BOOT_ATTEMPTS} boots; reverted"
        );
        return BootOutcome::RolledBack { failed };
    }

    let attempt = state.boot_attempts;
    if let Err(err) = slots::save_state(app_data_dir, &state) {
        eprintln!("[ota] could not record the boot attempt: {err}");
    }
    BootOutcome::Pending { attempt }
}

/// Marks the active slot as good. Called by the frontend once mounted.
pub fn mark_ready(app_data_dir: &Path) -> Result<(), String> {
    let mut state = slots::load_state(app_data_dir);
    if state.active.is_none() || state.verified {
        return Ok(());
    }
    state.verified = true;
    state.boot_attempts = 0;
    slots::save_state(app_data_dir, &state)
}

/// Watches a freshly hot-applied artifact and reverts it if it does not
/// confirm. Requires the `tauri` feature.
///
/// Armed right after `activate_staged`. When the grace expires, if the
/// artifact is still active and unconfirmed, it is reverted and the frontend
/// is notified to reload: from that moment the webview serves the previous
/// slot or the embedded frontend.
#[cfg(feature = "tauri")]
pub fn arm_hot_apply<R: tauri::Runtime>(app: tauri::AppHandle<R>, slot_id: String) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(HOT_APPLY_GRACE).await;

        let Ok(data_dir) = app.path().app_data_dir() else {
            return;
        };
        let state = slots::load_state(&data_dir);

        // Already confirmed, or another slot was activated meanwhile:
        // nothing to do.
        if state.verified || state.active.as_deref() != Some(slot_id.as_str()) {
            return;
        }

        eprintln!("[ota] slot {slot_id} did not confirm after a hot apply; reverting");
        match apply::rollback(&data_dir) {
            Ok(_) => {
                use tauri::Emitter as _;
                let _ = app.emit(BUNDLE_REVERTED_EVENT, slot_id);
            }
            Err(err) => eprintln!("[ota] could not revert: {err}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::partial::slots::SlotState;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mks-ota-watchdog-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn without_a_slot_there_is_nothing_to_watch() {
        assert_eq!(reconcile_boot(&tmpdir("none")), BootOutcome::NoBundle);
    }

    #[test]
    fn a_confirmed_slot_spends_no_attempts() {
        let dir = tmpdir("confirmed");
        slots::save_state(&dir, &SlotState {
            active: Some("b".into()),
            verified: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(reconcile_boot(&dir), BootOutcome::Verified);
        assert_eq!(slots::load_state(&dir).boot_attempts, 0);
    }

    #[test]
    fn a_slot_that_keeps_crashing_reverts_when_boots_run_out() {
        let dir = tmpdir("crashloop");
        slots::save_state(&dir, &SlotState {
            active: Some("bad".into()),
            previous: Some("good".into()),
            verified: false,
            ..Default::default()
        })
        .unwrap();

        // Each unconfirmed boot consumes one attempt.
        assert_eq!(reconcile_boot(&dir), BootOutcome::Pending { attempt: 1 });
        assert_eq!(reconcile_boot(&dir), BootOutcome::Pending { attempt: 2 });

        // Third one is over: back to the previous slot.
        assert_eq!(
            reconcile_boot(&dir),
            BootOutcome::RolledBack { failed: Some("bad".into()) }
        );
        assert_eq!(slots::load_state(&dir).active.as_deref(), Some("good"));
    }

    #[test]
    fn confirming_cuts_the_attempt_count() {
        let dir = tmpdir("confirm");
        slots::save_state(&dir, &SlotState {
            active: Some("b".into()),
            previous: Some("old".into()),
            verified: false,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(reconcile_boot(&dir), BootOutcome::Pending { attempt: 1 });
        mark_ready(&dir).unwrap();

        let s = slots::load_state(&dir);
        assert!(s.verified);
        assert_eq!(s.boot_attempts, 0);

        // And from there on boots no longer count.
        assert_eq!(reconcile_boot(&dir), BootOutcome::Verified);
        assert_eq!(slots::load_state(&dir).active.as_deref(), Some("b"));
    }

    #[test]
    fn confirming_without_an_active_slot_does_nothing() {
        let dir = tmpdir("confirm-empty");
        mark_ready(&dir).unwrap();
        assert!(!slots::load_state(&dir).verified);
    }

    #[test]
    fn reverting_with_no_previous_leaves_the_embedded_frontend() {
        let dir = tmpdir("revert-empty");
        slots::save_state(&dir, &SlotState {
            active: Some("only".into()),
            verified: false,
            boot_attempts: MAX_BOOT_ATTEMPTS,
            ..Default::default()
        })
        .unwrap();

        assert!(matches!(reconcile_boot(&dir), BootOutcome::RolledBack { .. }));
        assert!(slots::load_state(&dir).active.is_none());
    }
}
