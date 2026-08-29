//! End-to-end partial-install pipeline against a REAL signed fixture:
//! download -> verify (both gates) -> stage -> activate -> rollback ->
//! invalidate-on-native-change.
//!
//! The fixture under `tests/fixtures/partial/` was produced by the real
//! publisher toolchain, not by this code: the zip comes from `zip` over a
//! built `dist/`, and the signature from `tauri signer sign` (key id
//! `F26DF29653E189`, a throwaway key that signs nothing but this fixture —
//! `test-partial.key` is committed next to it on purpose, it guards no
//! secret). `manifest.json` carries the raw multi-line `.sig` the hub serves
//! after a `--component` publish, and the sha256 the hub computed.
//!
//! A disagreement between the publisher's format and the crate's parsing is
//! exactly what no self-generated test can catch.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;

use mks_ota::download;
use mks_ota::install::partial;
use mks_ota::manifest::HubLatest;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/partial")
        .join(name)
}

fn load_fixture() -> (HubLatest, Vec<u8>, String) {
    let manifest: HubLatest = serde_json::from_slice(
        &std::fs::read(fixture("manifest.json")).expect("fixture manifest.json"),
    )
    .expect("the publisher's manifest must deserialize as HubLatest");
    let zip = std::fs::read(fixture("frontend.zip")).expect("fixture frontend.zip");
    let pubkey = std::fs::read_to_string(fixture("pubkey.txt"))
        .expect("fixture pubkey.txt")
        .trim()
        .to_string();
    (manifest, zip, pubkey)
}

/// Minimal HTTP server on an ephemeral loopback port, serving `bytes` on
/// every GET — enough for `download`'s plain (non-resumed) path, which is
/// the path a fresh artifact always takes.
fn serve(bytes: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf); // request head — one segment
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&bytes).unwrap();
            let _ = stream.flush();
        }
    });
    format!("http://{addr}/frontend.zip")
}

fn tmpdir() -> PathBuf {
    let dir = std::env::temp_dir().join("mks-ota-pipeline");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn download_verify_stage_activate_rollback_invalidate() {
    let (manifest, zip, pubkey) = load_fixture();
    let url = serve(zip.clone());
    let latest = HubLatest { url: url.clone(), ..manifest };
    let dir = tmpdir();

    // ── download: sha256 gate over the streamed bytes ────────────────────
    let dest = dir.join("downloaded.zip");
    let outcome = download::download(&latest.url, &dest, latest.sha256_hex(), |_| {})
        .expect("download the fixture from the local server");
    assert_eq!(outcome.bytes, zip.len() as u64);
    assert_eq!(
        outcome.sha256_hex,
        latest.sha256_hex().expect("the fixture declares a real sha256")
    );

    // ── stage: both gates over the on-disk bytes, no activation ──────────
    let id = partial::stage(&dir, &latest, &dest, &pubkey)
        .expect("the real signed artifact passes both gates");
    let staged = partial::load_state(&dir);
    assert_eq!(staged.staged.as_deref(), Some(id.as_str()));
    assert!(staged.active.is_none(), "staging must not change what is served");
    assert!(partial::bundles_root(&dir).join(&id).join("index.html").is_file());

    // ── activate: pointer swap only, unverified until app-ready ─────────
    let activated = partial::activate_staged(&dir, "0.2.12").expect("activate the staged slot");
    assert_eq!(activated, id);
    let state = partial::load_state(&dir);
    assert_eq!(state.active.as_deref(), Some(id.as_str()));
    assert_eq!(state.active_version.as_deref(), Some("0.3.0"));
    assert!(!state.verified);

    // What the protocol would serve: the slot's index.html resolves and has
    // the fixture's content.
    let slot_dir = partial::active_dir(&dir, &state).expect("the active slot is on disk");
    let served = partial::resolve_within(&slot_dir, "/index.html").expect("index resolves");
    let body = std::fs::read(&served).unwrap();
    let index_marker = b"haido fixture v0.3.0";
    assert!(
        body.windows(index_marker.len()).any(|w| w == index_marker),
        "the served index.html comes from the fixture, not from somewhere else"
    );

    // Anti-downgrade: the same version is not offered again.
    assert!(!latest.is_newer_than("0.3.0").expect("semver compare"));

    // ── rollback: back to embedded, the failed slot is reported ─────────
    let failed = partial::rollback(&dir).expect("rollback").expect("the active slot is reported");
    assert_eq!(failed, id);
    assert!(partial::load_state(&dir).active.is_none());

    // ── native invalidation: re-stage, re-activate, bump the binary ──────
    let id2 = partial::stage(&dir, &latest, &dest, &pubkey).expect("re-stage the same artifact");
    assert_eq!(id2, id, "same content hashes to the same slot id");
    partial::activate_staged(&dir, "0.2.12").unwrap();
    assert!(partial::load_state(&dir).active.is_some());
    assert!(partial::invalidate_if_native_changed(&dir, "0.2.13"));
    let after = partial::load_state(&dir);
    assert!(after.active.is_none(), "a native update drops the partial slot");
    assert_eq!(after.previous.as_deref(), Some(id.as_str()));
}
