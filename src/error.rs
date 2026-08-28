//! Error type shared by every stage of the update pipeline.

/// Errors surfaced by signature verification, download, and full-package
/// install.
#[derive(Debug, thiserror::Error)]
pub enum OtaError {
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid semver: {0}")]
    InvalidVersion(#[from] semver::Error),

    #[error("signature is malformed: {0}")]
    SignatureMalformed(String),

    #[error("signature key id does not match the trust anchor")]
    UnexpectedKeyId,

    #[error("signature does not verify")]
    InvalidSignature,

    #[error("expected sha256 {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error(
        "temp dir (dev {tmp_dev}) and destination (dev {dest_dev}) are on different filesystems; rename would fail"
    )]
    CrossDeviceRename { tmp_dev: u64, dest_dev: u64 },

    #[error("could not locate the .app bundle from the running executable path")]
    AppBundleNotFound,

    #[error("install failed and restoring the previous version also failed: {0}")]
    RollbackFailed(String),

    #[error("privileged install step failed: {0}")]
    PrivilegedInstallFailed(String),
}
