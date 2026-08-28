//! The hub's own manifest shape — NOT `tauri-plugin-updater`'s
//! `RemoteRelease`. ADR-0045 D8-B: the hub stays agnostic of artifacts, the
//! client owns the shape.

use serde::Deserialize;

use crate::error::OtaError;

/// `GET /api/components/{component}/latest?target=<t>&arch=<a>` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubLatest {
    pub component: String,
    /// Strict semver — the hub validates this at publish time.
    pub version: String,
    /// Absolute, already resolved against the tenant.
    pub url: String,
    /// `"sha256:<hex>"`. Can be literally `"sha256:unknown"` on backfilled
    /// rows — see [`HubLatest::sha256_hex`].
    pub sha256: String,
    /// Raw minisign `.sig` text (4 lines) on fresh publishes; some legacy
    /// rows are base64-wrapped. `crate::verify` handles both.
    pub signature: String,
    /// ISO-8601.
    pub pub_date: String,
}

impl HubLatest {
    /// The hex digest, or `None` when the hub hasn't computed one
    /// (`"unknown"` on backfilled rows). sha256 is integrity/dedupe only —
    /// minisign is what authenticates (ADR-0045 D8, design §6.2).
    pub fn sha256_hex(&self) -> Option<&str> {
        let hex = self.sha256.strip_prefix("sha256:").unwrap_or(&self.sha256);
        (!hex.is_empty() && !hex.eq_ignore_ascii_case("unknown")).then_some(hex)
    }

    /// Whether this manifest is a strict semver upgrade over `current` —
    /// never a downgrade, never a no-op. Fixes the `!=` bug at
    /// `wraith-linux/src-tauri/src/lib.rs:496` (ADR-0045 D8, design §6).
    pub fn is_newer_than(&self, current: &str) -> Result<bool, OtaError> {
        let remote = semver::Version::parse(&self.version)?;
        let current = semver::Version::parse(current)?;
        Ok(remote > current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sha256: &str) -> HubLatest {
        HubLatest {
            component: "wraith-linux".into(),
            version: "0.1.0".into(),
            url: "https://example.invalid/artifact".into(),
            sha256: sha256.into(),
            signature: String::new(),
            pub_date: "2026-08-28T00:00:00Z".into(),
        }
    }

    #[test]
    fn sha256_hex_strips_the_prefix() {
        let m = sample("sha256:26fb34894265c1b7abc35511a7793b50d481231787ab8987bb4122fd87957299");
        assert_eq!(
            m.sha256_hex(),
            Some("26fb34894265c1b7abc35511a7793b50d481231787ab8987bb4122fd87957299")
        );
    }

    #[test]
    fn sha256_hex_unknown_is_none() {
        assert_eq!(sample("sha256:unknown").sha256_hex(), None);
        assert_eq!(sample("unknown").sha256_hex(), None);
    }

    #[test]
    fn deserializes_the_real_hub_shape() {
        // Byte-for-byte the response of
        // GET wraith.releases.mks2508.systems/api/components/wraith-linux/latest?target=darwin&arch=aarch64
        let json = r#"{
            "component": "wraith-linux",
            "version": "0.1.0",
            "url": "https://wraith.releases.mks2508.systems/api/components/wraith-linux/download/0.1.0/darwin/aarch64/Wraith.app.tar.gz",
            "sha256": "sha256:26fb34894265c1b7abc35511a7793b50d481231787ab8987bb4122fd87957299",
            "signature": "untrusted comment: signature from tauri secret key\nRUT3w/5VWLzVgLjAEojeq6EV5794fQW+Bh/kd1OOhd+Hca/EF4FSu2ztwTTjB66yEmaSph+ny0KV5cPCRfCPchShOzK30zBFSAE=\ntrusted comment: timestamp:1787944633\tfile:Wraith.app.tar.gz\nEiTE7elRm8kyBhuCWuMuvGyl5KLFB+dXr2Kzkyzuhr/Y5QoW7GCb8IQ3F2GWS3xVxSFIrxKQ04y5mHxlsRgBDw==",
            "pubDate": "2026-08-28T19:21:50.730Z"
        }"#;
        let manifest: HubLatest = serde_json::from_str(json).expect("valid manifest JSON");
        assert_eq!(manifest.component, "wraith-linux");
        assert_eq!(manifest.pub_date, "2026-08-28T19:21:50.730Z");
        assert_eq!(manifest.sha256_hex(), Some("26fb34894265c1b7abc35511a7793b50d481231787ab8987bb4122fd87957299"));
    }

    #[test]
    fn up_to_date_never_offers_a_downgrade() {
        // The exact demo sequence from the design doc §6: 0.1.1 installed
        // by hand, hub still serving 0.1.0.
        let hub_serving_0_1_0 = HubLatest { version: "0.1.0".into(), ..sample("sha256:unknown") };
        assert!(!hub_serving_0_1_0.is_newer_than("0.1.1").unwrap());
    }

    #[test]
    fn newer_hub_version_offers_an_update() {
        let hub_serving_0_2_0 = HubLatest { version: "0.2.0".into(), ..sample("sha256:unknown") };
        assert!(hub_serving_0_2_0.is_newer_than("0.1.1").unwrap());
    }

    #[test]
    fn malformed_version_is_a_typed_error_not_a_panic() {
        let bad = HubLatest { version: "not-a-version".into(), ..sample("sha256:unknown") };
        assert!(bad.is_newer_than("0.1.1").is_err());
    }
}
