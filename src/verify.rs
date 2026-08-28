//! minisign signature verification — content signature AND the trusted
//! comment's global signature (ADR-0045 D8 L5: "verificación completa").
//! The hub verifies only the content signature and explicitly delegates
//! the rest to the client (`hub/lib/minisign.ts:40-42`); `minisign-verify`
//! does both for free (`rust-minisign-verify` `lib.rs:334-346`, per
//! `/tmp/ota-crates-report.md` §1).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};

use crate::error::OtaError;

const CHUNK: usize = 64 * 1024;

/// Verifies `artifact` against `sig` (minisign `.sig` text) with `pubkey`
/// (bare base64 public-key payload, e.g. what `HubLatest` does not carry —
/// the pubkey is app-specific, baked in by the caller). Whole artifact in
/// memory — use [`verify_stream_from_file`] for large artifacts already on
/// disk.
pub fn verify_bytes(artifact: &[u8], sig: &str, pubkey: &str) -> Result<(), OtaError> {
    let pk = parse_pubkey(pubkey)?;
    let signature = decode_signature(sig)?;
    pk.verify(artifact, &signature, false).map_err(map_minisign_err)
}

/// Verifies a file on disk with constant RAM via
/// `PublicKey::verify_stream` — only prehashed (`ED`) signatures support
/// this, and `tauri signer sign` always produces those
/// (`/tmp/ota-crates-report.md` §1, "Streaming: sólo prehashed"). A legacy
/// (`Ed`) signature fails closed with `UnsupportedLegacyMode` — it does not
/// silently fall back to loading the file into memory.
pub fn verify_stream_from_file(path: &Path, sig: &str, pubkey: &str) -> Result<(), OtaError> {
    let pk = parse_pubkey(pubkey)?;
    let signature = decode_signature(sig)?;
    let mut verifier = pk.verify_stream(&signature).map_err(map_minisign_err)?;
    let mut file = File::open(path)?;
    let mut buf = [0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier.finalize().map_err(map_minisign_err)
}

fn parse_pubkey(pubkey: &str) -> Result<PublicKey, OtaError> {
    PublicKey::from_base64(pubkey.trim()).map_err(|e| OtaError::SignatureMalformed(e.to_string()))
}

/// Decodes a minisign `.sig`, tolerating the base64-wrapped form some
/// legacy rows carry (`tpv-el-haido2`'s backfilled releases). Tries the
/// raw multi-line format first; on failure, unwraps base64 **once** and
/// retries — never loops (handoff M1: "Una vez, no en bucle").
fn decode_signature(sig: &str) -> Result<Signature, OtaError> {
    if let Ok(signature) = Signature::decode(sig) {
        return Ok(signature);
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(sig.trim()).map_err(|_| {
        OtaError::SignatureMalformed("not a minisign signature, and not valid base64 either".into())
    })?;
    let text = String::from_utf8(decoded)
        .map_err(|_| OtaError::SignatureMalformed("base64-decoded signature is not valid UTF-8".into()))?;
    Signature::decode(&text).map_err(map_minisign_err)
}

fn map_minisign_err(e: minisign_verify::Error) -> OtaError {
    use minisign_verify::Error as E;
    match e {
        E::UnexpectedKeyId => OtaError::UnexpectedKeyId,
        E::InvalidSignature => OtaError::InvalidSignature,
        other => OtaError::SignatureMalformed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Trust anchor: `.local-secrets/tauri-dev.pub` in mks-agentics, key id
    /// `80D5BC5855FEC3F7`. Same key `wraith-linux` 0.1.0 was published and
    /// signed with.
    const PUBKEY: &str = "RWT3w/5VWLzVgBmU7JlYbIMdm5LScd8c3Z5dIeMyFUYD13hLzwkPWjFE";
    const ARTIFACT_URL: &str =
        "https://wraith.releases.mks2508.systems/api/components/wraith-linux/download/0.1.0/darwin/aarch64/Wraith.app.tar.gz";
    /// `signature` field, byte-identical, from
    /// `GET .../api/components/wraith-linux/latest?target=darwin&arch=aarch64`.
    const RAW_SIGNATURE: &str = "untrusted comment: signature from tauri secret key\nRUT3w/5VWLzVgLjAEojeq6EV5794fQW+Bh/kd1OOhd+Hca/EF4FSu2ztwTTjB66yEmaSph+ny0KV5cPCRfCPchShOzK30zBFSAE=\ntrusted comment: timestamp:1787944633\tfile:Wraith.app.tar.gz\nEiTE7elRm8kyBhuCWuMuvGyl5KLFB+dXr2Kzkyzuhr/Y5QoW7GCb8IQ3F2GWS3xVxSFIrxKQ04y5mHxlsRgBDw==";
    /// A different, freshly generated minisign keypair (`minisign -G`) with
    /// no relation whatsoever to the trust anchor above — used only to
    /// prove key-id mismatch is reported precisely, not as a generic
    /// failure.
    const OTHER_PUBKEY: &str = "RWQXHi88IfrwHcBxeCTmnOyH7GsCFP++eVdbIB8Gg8L4INMbr4/1Pf8W";

    fn artifact() -> &'static [u8] {
        static ARTIFACT: OnceLock<Vec<u8>> = OnceLock::new();
        ARTIFACT.get_or_init(|| {
            reqwest::blocking::get(ARTIFACT_URL)
                .expect("fetch the real published artifact")
                .bytes()
                .expect("read artifact body")
                .to_vec()
        })
    }

    #[test]
    fn verifies_the_real_artifact_and_signature() {
        verify_bytes(artifact(), RAW_SIGNATURE, PUBKEY).expect("real artifact + real signature must verify");
    }

    #[test]
    fn one_byte_tampering_fails() {
        let mut tampered = artifact().to_vec();
        tampered[0] ^= 0xFF;
        let err = verify_bytes(&tampered, RAW_SIGNATURE, PUBKEY).unwrap_err();
        assert!(matches!(err, OtaError::InvalidSignature), "got {err:?}");
    }

    #[test]
    fn wrong_key_gives_unexpected_key_id_not_a_generic_error() {
        let err = verify_bytes(artifact(), RAW_SIGNATURE, OTHER_PUBKEY).unwrap_err();
        assert!(matches!(err, OtaError::UnexpectedKeyId), "got {err:?}");
    }

    #[test]
    fn accepts_raw_and_base64_wrapped_signature_forms() {
        verify_bytes(artifact(), RAW_SIGNATURE, PUBKEY).expect("raw 4-line form");
        let wrapped = base64::engine::general_purpose::STANDARD.encode(RAW_SIGNATURE);
        verify_bytes(artifact(), &wrapped, PUBKEY).expect("base64-wrapped Tauri form");
    }

    #[test]
    fn streams_the_real_artifact_from_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Wraith.app.tar.gz");
        std::fs::write(&path, artifact()).unwrap();
        verify_stream_from_file(&path, RAW_SIGNATURE, PUBKEY).expect("streaming verify of the real file");
    }

    #[test]
    fn streaming_also_rejects_a_tampered_file() {
        let mut tampered = artifact().to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Wraith.app.tar.gz");
        std::fs::write(&path, &tampered).unwrap();
        let err = verify_stream_from_file(&path, RAW_SIGNATURE, PUBKEY).unwrap_err();
        assert!(matches!(err, OtaError::InvalidSignature), "got {err:?}");
    }
}
