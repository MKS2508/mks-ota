//! Installing a partial artifact: verify, decompress, activate (extracted
//! from `tpv-el-haido2`'s production updater, `ota/apply.rs`).
//!
//! The swap is deliberately two-step. `stage` leaves the artifact ready on
//! disk and may take a while; `activate` only moves pointers in the state
//! file and is instantaneous. The slow half happens while the app is busy,
//! the half that changes what the user sees runs in the quiet moment.

use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use crate::error::OtaError;
use crate::manifest::HubLatest;
use crate::verify;

use super::slots::{self, SlotState};

const CHUNK: usize = 64 * 1024;

/// Directory id derived from the artifact hash. Anything that is not hex is
/// rejected: the id comes from the manifest and ends up as a directory name.
pub fn slot_id(sha256_hex: &str) -> Result<String, OtaError> {
    let hex = sha256_hex.strip_prefix("sha256:").unwrap_or(sha256_hex);
    let clean: String = hex.chars().take(32).collect();
    if clean.len() < 16 || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(OtaError::BadArchive(format!(
            "hash unusable as a slot id: {sha256_hex}"
        )));
    }
    Ok(clean)
}

/// sha256 of a file on disk, constant RAM.
fn sha256_of_file(path: &Path) -> Result<String, OtaError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extracts the zip into `dest`, rejecting entries that escape it.
///
/// A zip can declare paths like `../../something`: `enclosed_name` already
/// drops those, and here it is additionally checked that there are no odd
/// components, because the content is remote and this is where it touches
/// the disk.
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), OtaError> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| OtaError::BadArchive(e.to_string()))?;

    for i in 0..archive.len() {
        let mut entry =
            archive.by_index(i).map_err(|e| OtaError::BadArchive(e.to_string()))?;

        let Some(relative) = entry.enclosed_name() else {
            return Err(OtaError::BadArchive(format!("entry with unsafe path: {}", entry.name())));
        };
        if relative.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(OtaError::BadArchive(format!("entry with unsafe path: {}", entry.name())));
        }

        let target = dest.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Verifies and leaves the artifact decompressed in its slot, without
/// activating it.
///
/// Two gates run BEFORE anything is decompressed, both over the exact bytes
/// of the zip on disk: the sha256 the manifest declared, and the minisign
/// signature. Decompressing is already executing the decision to trust the
/// content — it must never be the thing that discovers a bad artifact.
///
/// @returns The id of the staged slot.
pub fn stage(
    app_data_dir: &Path,
    latest: &HubLatest,
    zip_path: &Path,
    pubkey: &str,
) -> Result<String, OtaError> {
    let actual_sha = sha256_of_file(zip_path)?;
    if let Some(expected) = latest.sha256_hex() {
        if !actual_sha.eq_ignore_ascii_case(expected) {
            return Err(OtaError::ChecksumMismatch {
                expected: expected.to_string(),
                actual: actual_sha,
            });
        }
    }
    verify::verify_stream_from_file(zip_path, &latest.signature, pubkey)?;

    let id = slot_id(&actual_sha)?;
    let root = slots::bundles_root(app_data_dir);
    let staging = root.join(format!(".staging-{id}"));
    let final_dir = root.join(&id);

    // Decompress into a separate directory and rename: if the process dies
    // mid-extraction, no half-written slot with a definitive name is left
    // behind for the protocol to start serving.
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    if let Err(err) = extract_zip(zip_path, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    if !staging.join("index.html").is_file() {
        let _ = fs::remove_dir_all(&staging);
        return Err(OtaError::BadArchive(
            "the artifact does not carry index.html at its root".into(),
        ));
    }

    let _ = fs::remove_dir_all(&final_dir);
    fs::rename(&staging, &final_dir)?;

    let mut state = slots::load_state(app_data_dir);
    state.staged = Some(id.clone());
    state.staged_version = Some(latest.version.clone());
    slots::save_state(app_data_dir, &state).map_err(OtaError::StateIo)?;
    Ok(id)
}

/// Activates the staged slot. Only moves pointers: this is the instant half.
///
/// The slot is marked unverified — until the new frontend confirms
/// `app-ready` it is not considered good.
pub fn activate_staged(app_data_dir: &Path, native_version: &str) -> Result<String, OtaError> {
    let mut state = slots::load_state(app_data_dir);
    let Some(staged) = state.staged.take() else {
        return Err(OtaError::NothingStaged);
    };

    state.previous = state.active.take();
    state.active_version = state.staged_version.take();
    state.active = Some(staged.clone());
    state.verified = false;
    state.boot_attempts = 0;
    state.native_version_at_swap = Some(native_version.to_string());
    slots::save_state(app_data_dir, &state).map_err(OtaError::StateIo)?;
    Ok(staged)
}

/// Goes back to the previous slot. If there is none, the artifact channel
/// ends up empty and the protocol serves the embedded frontend, which always
/// works.
pub fn rollback(app_data_dir: &Path) -> Result<Option<String>, OtaError> {
    let mut state = slots::load_state(app_data_dir);
    let failed = state.active.take();
    state.active_version = None;
    state.active = state.previous.take();
    state.verified = state.active.is_some();
    state.boot_attempts = 0;
    if state.active.is_none() {
        state.native_version_at_swap = None;
    }
    slots::save_state(app_data_dir, &state).map_err(OtaError::StateIo)?;
    Ok(failed)
}

/// Deletes slots that are no longer the active, previous, or staged one.
pub fn prune(app_data_dir: &Path) -> Result<usize, OtaError> {
    let state: SlotState = slots::load_state(app_data_dir);
    let keep: Vec<&str> = [&state.active, &state.previous, &state.staged]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();

    let root = slots::bundles_root(app_data_dir);
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(0);
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if keep.contains(&name.as_str()) {
            continue;
        }
        if fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::partial::testkit::{hex_of, signed_latest, tmpdir, write_file, zip_with, TestKey};

    #[test]
    fn slot_id_rejects_a_hash_it_cannot_use() {
        assert!(slot_id("sha256:zzz").is_err());
        assert!(slot_id("too-short").is_err());
        assert_eq!(slot_id("sha256:0123456789abcdef0123456789abcdef9999").unwrap(),
                   "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn stage_leaves_the_artifact_ready_but_not_active() {
        let dir = tmpdir("stage");
        let zip = zip_with(&[("index.html", b"<html>new</html>"), ("assets/a.js", b"1")]);
        let key = TestKey::generate();
        let latest = signed_latest(&zip, &key, "0.3.0");
        let zip_path = write_file(&dir, "frontend.zip", &zip);

        let id = stage(&dir, &latest, &zip_path, &key.pubkey_payload()).unwrap();
        let state = slots::load_state(&dir);
        assert_eq!(state.staged.as_deref(), Some(id.as_str()));
        assert_eq!(state.staged_version.as_deref(), Some("0.3.0"));
        assert!(state.active.is_none(), "stage must not activate anything");
        assert!(slots::bundles_root(&dir).join(&id).join("index.html").is_file());
    }

    #[test]
    fn a_mismatched_sha256_is_rejected_before_touching_disk() {
        let dir = tmpdir("bad-hash");
        let good = zip_with(&[("index.html", b"<html>")]);
        let key = TestKey::generate();
        let latest = signed_latest(&good, &key, "0.3.0");
        let other = zip_with(&[("index.html", b"<html>different content")]);
        let zip_path = write_file(&dir, "frontend.zip", &other);

        let err = stage(&dir, &latest, &zip_path, &key.pubkey_payload()).unwrap_err();
        assert!(matches!(err, OtaError::ChecksumMismatch { .. }), "got {err:?}");
        // Nothing on disk: verification happens before the destination is
        // touched.
        let root = slots::bundles_root(&dir);
        let has_slots = fs::read_dir(&root)
            .map(|d| d.flatten().any(|e| e.path().is_dir()))
            .unwrap_or(false);
        assert!(!has_slots);
    }

    #[test]
    fn a_signature_from_a_different_key_is_rejected() {
        let dir = tmpdir("wrong-key");
        let zip = zip_with(&[("index.html", b"<html>")]);
        let key = TestKey::generate();
        let other_key = TestKey::generate();
        let latest = signed_latest(&zip, &key, "0.3.0");
        let zip_path = write_file(&dir, "frontend.zip", &zip);

        let err = stage(&dir, &latest, &zip_path, &other_key.pubkey_payload()).unwrap_err();
        assert!(matches!(err, OtaError::UnexpectedKeyId), "got {err:?}");
    }

    #[test]
    fn a_tampered_zip_is_rejected_by_the_signature() {
        let dir = tmpdir("tampered");
        let zip = zip_with(&[("index.html", b"<html>")]);
        let key = TestKey::generate();
        // The manifest declares the correct sha256 (the hub computed it over
        // the original bytes), but the downloaded file was modified in
        // flight — so the sha256 gate passes only if the manifest was also
        // rewritten. Re-signing is what a compromised hub would do; here the
        // bytes differ from the signed ones and the signature must fail.
        let latest = signed_latest(&zip, &key, "0.3.0");
        let mut tampered = zip.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        let mut latest = latest;
        latest.sha256 = format!("sha256:{}", hex_of(&tampered));
        let zip_path = write_file(&dir, "frontend.zip", &tampered);

        let err = stage(&dir, &latest, &zip_path, &key.pubkey_payload()).unwrap_err();
        assert!(matches!(err, OtaError::InvalidSignature), "got {err:?}");
    }

    #[test]
    fn a_zip_that_tries_to_escape_is_rejected() {
        let dir = tmpdir("escape");
        let zip = zip_with(&[("index.html", b"<html>"), ("../outside.txt", b"x")]);
        let key = TestKey::generate();
        let latest = signed_latest(&zip, &key, "0.3.0");
        let zip_path = write_file(&dir, "frontend.zip", &zip);

        let err = stage(&dir, &latest, &zip_path, &key.pubkey_payload()).unwrap_err();
        assert!(matches!(err, OtaError::BadArchive(_)), "got {err:?}");
        assert!(!dir.join("outside.txt").exists());
    }

    #[test]
    fn an_artifact_without_index_html_does_not_serve() {
        let dir = tmpdir("no-index");
        let zip = zip_with(&[("assets/a.js", b"1")]);
        let key = TestKey::generate();
        let latest = signed_latest(&zip, &key, "0.3.0");
        let zip_path = write_file(&dir, "frontend.zip", &zip);

        assert!(matches!(
            stage(&dir, &latest, &zip_path, &key.pubkey_payload()).unwrap_err(),
            OtaError::BadArchive(_)
        ));
    }

    #[test]
    fn activate_moves_pointers_and_leaves_it_unverified() {
        let dir = tmpdir("activate");
        let zip = zip_with(&[("index.html", b"<html>")]);
        let key = TestKey::generate();
        let latest = signed_latest(&zip, &key, "0.3.0");
        let zip_path = write_file(&dir, "frontend.zip", &zip);
        let id = stage(&dir, &latest, &zip_path, &key.pubkey_payload()).unwrap();

        let activated = activate_staged(&dir, "0.2.12").unwrap();
        assert_eq!(activated, id);
        let s = slots::load_state(&dir);
        assert_eq!(s.active.as_deref(), Some(id.as_str()));
        assert_eq!(s.active_version.as_deref(), Some("0.3.0"));
        assert!(s.staged.is_none());
        assert!(s.staged_version.is_none());
        assert!(!s.verified, "a freshly activated slot is not confirmed yet");
        assert_eq!(s.native_version_at_swap.as_deref(), Some("0.2.12"));
    }

    #[test]
    fn activating_with_nothing_staged_is_an_error() {
        let dir = tmpdir("nothing");
        assert!(matches!(
            activate_staged(&dir, "0.2.12").unwrap_err(),
            OtaError::NothingStaged
        ));
    }

    #[test]
    fn rollback_returns_to_the_previous_slot() {
        let dir = tmpdir("rollback");
        slots::save_state(&dir, &SlotState {
            active: Some("new".into()),
            active_version: Some("0.3.0".into()),
            previous: Some("old".into()),
            verified: false,
            ..Default::default()
        })
        .unwrap();

        let failed = rollback(&dir).unwrap();
        assert_eq!(failed.as_deref(), Some("new"));
        let s = slots::load_state(&dir);
        assert_eq!(s.active.as_deref(), Some("old"));
        assert!(s.verified, "the slot we fell back to had already worked");
        assert!(s.active_version.is_none());
    }

    #[test]
    fn rollback_with_no_previous_falls_back_to_embedded() {
        let dir = tmpdir("rollback-empty");
        slots::save_state(&dir, &SlotState {
            active: Some("only".into()),
            native_version_at_swap: Some("0.1.0".into()),
            ..Default::default()
        })
        .unwrap();

        rollback(&dir).unwrap();
        let s = slots::load_state(&dir);
        assert!(s.active.is_none());
        assert!(s.native_version_at_swap.is_none());
    }

    #[test]
    fn prune_keeps_active_previous_and_staged() {
        let dir = tmpdir("prune");
        let root = slots::bundles_root(&dir);
        for name in ["act", "prev", "stag", "garbage1", "garbage2"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        slots::save_state(&dir, &SlotState {
            active: Some("act".into()),
            previous: Some("prev".into()),
            staged: Some("stag".into()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(prune(&dir).unwrap(), 2);
        assert!(root.join("act").is_dir());
        assert!(root.join("prev").is_dir());
        assert!(root.join("stag").is_dir());
        assert!(!root.join("garbage1").exists());
    }
}
