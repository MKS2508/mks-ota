//! Partial install — a single artifact (frontend bundle, sidecar, assets,
//! hooks/skills) swapped without reinstalling the app, via A/B slots under
//! `appDataDir` (extracted from `tpv-el-haido2`'s production updater, not
//! written from scratch — design `docs/jarvis/ota-crate-design-2026-08-28.md`
//! §2).
//!
//! - the artifact lives in `appDataDir`, outside the signed bundle (L1 —
//!   writing inside a signed `.app`/AppImage breaks the seal);
//! - slots A/B with a pointer file, not a symlink (Windows can't create
//!   symlinks without privileges);
//! - stage (decompress, slow) and activate (swap the pointer, instant) are
//!   two separate steps;
//! - the two trust gates — declared sha256 and minisign signature — run over
//!   the exact zip bytes on disk BEFORE any decompression;
//! - [`slots::invalidate_if_native_changed`] — after a full install swaps the
//!   native binary, a stale slot must not keep serving an old frontend
//!   against new native commands (L6, in the crate contract since F1).

pub mod apply;
pub mod slots;

pub use apply::{activate_staged, prune, rollback, slot_id, stage};
pub use slots::{
    active_dir, bundles_root, invalidate_if_native_changed, load_state, resolve_within, save_state,
    SlotState,
};

#[cfg(test)]
pub(crate) mod testkit {
    //! Shared helpers for the partial-install tests: throwaway dirs,
    //! in-memory zips, and a real minisign keypair generated per test so any
    //! zip (including deliberately malicious ones) can be signed the way the
    //! publisher would sign it.

    use std::io::{Cursor, Write};
    use std::path::PathBuf;

    use sha2::{Digest, Sha256};

    use crate::manifest::HubLatest;

    pub fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mks-ota-partial-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for (name, body) in files {
                w.start_file(*name, opts).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    /// A fresh, disposable minisign keypair. Random per call — the tests only
    /// need the signature and the pubkey to come from the same key.
    pub struct TestKey {
        public: minisign::PublicKey,
        sk: minisign::SecretKey,
    }

    impl TestKey {
        pub fn generate() -> Self {
            let minisign::KeyPair { pk, sk } =
                minisign::KeyPair::generate_unencrypted_keypair().expect("generate a test keypair");
            TestKey { public: pk.clone(), sk }
        }

        /// The bare base64 pubkey payload `verify` expects (line 2 of the
        /// `.pub` box).
        pub fn pubkey_payload(&self) -> String {
            self.public
                .to_box()
                .expect("box the public key")
                .into_string()
                .lines()
                .nth(1)
                .expect("pub box has a payload line")
                .trim()
                .to_string()
        }

        /// Signs `bytes` and returns the raw multi-line `.sig` text, the
        /// same shape the hub serves.
        pub fn sign(&self, bytes: &[u8]) -> String {
            let sig_box = minisign::sign(
                Some(&self.public),
                &self.sk,
                Cursor::new(bytes),
                Some("test:partial"),
                None,
            )
            .expect("sign the test artifact");
            sig_box.into_string()
        }
    }

    /// A well-formed `HubLatest` for `bytes`: correct sha256, correct
    /// signature, deterministic metadata.
    pub fn signed_latest(bytes: &[u8], key: &TestKey, version: &str) -> HubLatest {
        HubLatest {
            component: "test-frontend".into(),
            version: version.into(),
            url: "http://127.0.0.1:9/frontend.zip".into(),
            sha256: format!("sha256:{}", hex_of(bytes)),
            signature: key.sign(bytes),
            pub_date: "2026-08-29T00:00:00Z".into(),
        }
    }

    pub fn hex_of(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Writes `bytes` to `<dir>/<name>` and returns the path — stage works on
    /// files on disk, not in-memory buffers.
    pub fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}
