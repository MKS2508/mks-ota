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

/// Assembles the `latest` URL for a platform-agnostic partial artifact
/// (component convention L3: published AND queried with `target=any&arch=any`).
/// The hub's SQL match is exact equality — publish `any` against a query for
/// `all` and the answer is a silent 404.
pub fn partial_latest_url(hub_base: &str, component: &str) -> String {
    format!("{hub_base}/api/components/{component}/latest?target=any&arch=any")
}

/// Interprets `max`, which the haido contract defines as an INCLUSIVE upper
/// bound that also admits ranges (extracted from `tpv-el-haido2`
/// `ota/manifest.rs::parse_max`).
///
/// A bare version (`"1.6.0"`) reads as `<=1.6.0`, which is what the field
/// name says: everything below enters. Anything else (`"1.5.x"`, `"^1.5.0"`,
/// `">=1 <2"`) reads as a range verbatim.
///
/// Treating a bare version as a range — which is what the hub did with
/// `semver.satisfies` before commit `bcda900` — turns `[1.4.0, 1.6.0]` into
/// "only exactly 1.6.0"; see `a_bare_upper_bound_is_inclusive`.
fn parse_max(raw: &str) -> Result<semver::VersionReq, OtaError> {
    let cleaned = raw.trim().replace(".x", ".*").replace(".X", ".*");

    // The hub validates with node-semver, which separates a range's
    // comparators with spaces (">=1.0.0 <2.0.0"); Rust's crate wants commas.
    // A range the hub accepts must parse here too.
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(", ");

    // node-semver admits `||` alternatives, which this crate does not
    // support. Rejected explicitly instead of letting the parse fail with a
    // message that says nothing.
    if cleaned.contains("||") {
        return Err(OtaError::BadVersion(format!(
            "{raw}: ranges with `||` are not supported on the client"
        )));
    }

    let is_plain_version = semver::Version::parse(&cleaned).is_ok();
    let expr = if is_plain_version { format!("<={cleaned}") } else { cleaned };
    semver::VersionReq::parse(&expr).map_err(|_| OtaError::BadVersion(raw.to_string()))
}

/// Checks that the native binary falls inside the declared window
/// `[min, max]` — the guard that stops a new partial artifact from landing
/// on a binary lacking the commands that artifact calls. Validated on the
/// client too, not only on the hub: the client does not trust the server.
///
/// `min` is a concrete version compared with `>=`; `max` is an inclusive
/// upper bound that also admits ranges (see [`parse_max`]). In the
/// `components` channel the hub does not carry this window — the caller
/// declares its own constants per release.
pub fn native_within_window(native: &str, min: &str, max: &str) -> Result<(), OtaError> {
    let native = semver::Version::parse(native)
        .map_err(|_| OtaError::BadVersion(native.to_string()))?;

    let min = semver::Version::parse(min.trim())
        .map_err(|_| OtaError::BadVersion(min.to_string()))?;
    let bound = parse_max(max)?;

    if native >= min && bound.matches(&native) {
        Ok(())
    } else {
        Err(OtaError::Incompatible {
            native: native.to_string(),
            min: min.to_string(),
            max: max.to_string(),
        })
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

    // ── Partial-artifact component URL (L3) ──────────────────────────────

    #[test]
    fn partial_latest_url_pins_target_any_arch_any() {
        assert_eq!(
            partial_latest_url("https://haido.releases.mks2508.systems", "haido-frontend"),
            "https://haido.releases.mks2508.systems/api/components/haido-frontend/latest?target=any&arch=any"
        );
    }

    // ── Native compatibility window (ported from tpv-el-haido2) ──────────

    #[test]
    fn the_native_window_is_inclusive_on_both_ends() {
        assert!(native_within_window("1.4.0", "1.4.0", "1.6.0").is_ok(), "the minimum enters");
        assert!(native_within_window("1.6.0", "1.4.0", "1.6.0").is_ok(), "the maximum enters");
        assert!(native_within_window("1.5.9", "1.4.0", "1.6.0").is_ok());
        assert!(native_within_window("1.3.9", "1.4.0", "1.6.0").is_err(), "below does not");
        assert!(native_within_window("1.6.1", "1.4.0", "1.6.0").is_err(), "above does not");
    }

    #[test]
    fn a_bare_upper_bound_is_inclusive() {
        // The field name says "max native version", so a bare "1.6.0" means
        // <=1.6.0. Reading it as a range (semver.satisfies) would make
        // [1.4.0, 1.6.0] reach only exactly-1.6.0 binaries and silently skip
        // every 1.5.x. The hub had this bug until bcda900; this is the
        // client-side half of that fix.
        assert!(native_within_window("1.4.0", "1.4.0", "1.6.0").is_ok(), "the minimum enters");
        assert!(native_within_window("1.5.9", "1.4.0", "1.6.0").is_ok(), "the middle enters");
        assert!(native_within_window("1.6.0", "1.4.0", "1.6.0").is_ok(), "the maximum enters");
        assert!(native_within_window("1.3.9", "1.4.0", "1.6.0").is_err());
        assert!(native_within_window("1.6.1", "1.4.0", "1.6.0").is_err());
    }

    #[test]
    fn the_ranges_the_hub_accepts_are_accepted() {
        // The hub validates max with semver.validRange, which accepts any
        // range. The client must understand the same ones, or it would
        // reject valid artifacts.
        assert!(native_within_window("1.5.3", "1.0.0", "^1.5.0").is_ok());
        assert!(native_within_window("2.0.0", "1.0.0", "^1.5.0").is_err());

        assert!(native_within_window("1.9.9", "1.0.0", ">=1.0.0 <2.0.0").is_ok());
        assert!(native_within_window("2.0.1", "1.0.0", ">=1.0.0 <2.0.0").is_err());
    }

    #[test]
    fn the_contract_patch_wildcard_is_understood() {
        // "1.5.x" is not valid semver: it must be normalized before parsing.
        assert!(native_within_window("1.5.0", "1.4.0", "1.5.x").is_ok());
        assert!(native_within_window("1.5.12", "1.4.0", "1.5.x").is_ok());
        assert!(native_within_window("1.6.0", "1.4.0", "1.5.x").is_err());
    }

    #[test]
    fn matches_the_hub_window() {
        // Table generated by running the hub's real implementation
        // (BundleService.withinWindow, desktop-release-hub bcda900, after the
        // bare-upper-bound fix) over these same fourteen cases. If either
        // side changes criteria, this test catches it before the hub starts
        // serving artifacts the client silently discards.
        let cases: &[(&str, &str, &str, bool)] = &[
            ("0.2.0", "0.2.0", "0.2.x", true),
            ("0.2.3", "0.2.0", "0.2.x", true),
            ("0.3.0", "0.2.0", "0.2.x", false),
            ("1.5.9", "1.4.0", "1.6.0", true),
            ("1.4.0", "1.4.0", "1.6.0", true),
            ("1.6.0", "1.4.0", "1.6.0", true),
            ("1.6.1", "1.4.0", "1.6.0", false),
            ("1.3.9", "1.4.0", "1.6.0", false),
            ("1.5.3", "1.0.0", "^1.5.0", true),
            ("2.0.0", "1.0.0", "^1.5.0", false),
            ("1.9.9", "1.0.0", ">=1.0.0 <2.0.0", true),
            ("2.0.1", "1.0.0", ">=1.0.0 <2.0.0", false),
            ("0.1.0", "0.1.0", "0.9.0", true),
            ("0.5.0", "0.1.0", "0.9.0", true),
        ];

        for (native, min, max, expected) in cases {
            assert_eq!(
                native_within_window(native, min, max).is_ok(),
                *expected,
                "native {native} with window [{min}, {max}]: the hub says {expected}"
            );
        }
    }

    #[test]
    fn an_unparseable_version_is_not_assumed_compatible() {
        assert!(matches!(
            native_within_window("not-semver", "0.1.0", "0.9.0").unwrap_err(),
            OtaError::BadVersion(_)
        ));
    }
}
