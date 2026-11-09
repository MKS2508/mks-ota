//! Streaming download to a temp file — never a `Vec<u8>` in RAM (that's
//! where `tauri-plugin-updater` is worse than us: it buffers the whole
//! artifact in memory before verifying, `/tmp/ota-crates-report.md` §2).
//! sha256 runs while streaming; sha256 is integrity/dedupe, not
//! authenticity — [`crate::verify`] is what authenticates.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::header::RANGE;
use reqwest::StatusCode;
use sha2::{Digest, Sha256};

use crate::error::OtaError;

const CHUNK: usize = 64 * 1024;

/// Longest single sleep of the rate limiter: a pause request is noticed at
/// most this late while throttled.
const THROTTLE_SLICE: Duration = Duration::from_millis(250);

/// Knobs for [`download_controlled`].
///
/// `should_pause` is polled before connecting, after every chunk and between
/// throttle sleeps. Pausing **closes the connection** (an idle socket would be
/// cut by the server's idle timeout anyway) and returns [`OtaError::Paused`]
/// with `dest` holding what was fetched; the next call resumes it with
/// `Range`.
#[derive(Clone, Copy)]
pub struct DownloadControl<'a> {
    /// Average throughput cap for this call, in bytes per second. `None` =
    /// unlimited.
    pub max_bytes_per_sec: Option<u64>,
    pub should_pause: &'a (dyn Fn() -> bool + Sync),
}

fn never_pause() -> bool {
    false
}

impl DownloadControl<'static> {
    /// No rate limit, never pauses — what [`download`] uses.
    pub fn unlimited() -> Self {
        DownloadControl { max_bytes_per_sec: None, should_pause: &never_pause }
    }
}

impl std::fmt::Debug for DownloadControl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadControl").field("max_bytes_per_sec", &self.max_bytes_per_sec).finish_non_exhaustive()
    }
}

/// One step of download progress. `total` is `None` when the server didn't
/// send `Content-Length` — callers must tolerate that; the hub's manifest
/// doesn't carry a size either (design §4.1: "No hay `size` en ningún
/// manifest").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    pub sha256_hex: String,
    pub bytes: u64,
}

/// Downloads `url` into `dest`, resuming from `dest`'s current length via a
/// `Range` request when it already exists and is non-empty. Verifies
/// `expected_sha256_hex` at the end when given — pass `None` for
/// `HubLatest::sha256_hex() == None` ("unknown" rows). sha256 is never the
/// sole trust anchor: only [`crate::verify`] authenticates.
///
/// Same as [`download_controlled`] with [`DownloadControl::unlimited`].
pub fn download(
    url: &str,
    dest: &Path,
    expected_sha256_hex: Option<&str>,
    on_progress: impl FnMut(DownloadProgress),
) -> Result<DownloadOutcome, OtaError> {
    download_controlled(url, dest, expected_sha256_hex, DownloadControl::unlimited(), on_progress)
}

/// [`download`] with a throughput cap and a pause switch (see
/// [`DownloadControl`]).
///
/// Failure never poisons the next attempt:
/// - a sha256 mismatch **deletes `dest`** before returning
///   [`OtaError::ChecksumMismatch`] — a bad prefix left on disk would be
///   resumed with `Range` and fail forever;
/// - a `416 Range Not Satisfiable` on a resume (the prefix is as long as or
///   longer than the remote file) drops `dest` and restarts from zero once;
/// - a network error mid-stream keeps `dest`, which is a valid prefix.
pub fn download_controlled(
    url: &str,
    dest: &Path,
    expected_sha256_hex: Option<&str>,
    ctl: DownloadControl<'_>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<DownloadOutcome, OtaError> {
    if (ctl.should_pause)() {
        return Err(OtaError::Paused);
    }
    let client = Client::builder().user_agent(concat!("mks-ota/", env!("CARGO_PKG_VERSION"))).build()?;

    let mut existing_len = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    let mut response = loop {
        let mut request = client.get(url);
        if existing_len > 0 {
            request = request.header(RANGE, format!("bytes={existing_len}-"));
        }
        let response = request.send()?;
        if existing_len > 0 && response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            drop(response);
            fs::remove_file(dest)?;
            existing_len = 0;
            continue;
        }
        break response.error_for_status()?;
    };
    let resumed = existing_len > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
    let start = if resumed { existing_len } else { 0 };

    let mut hasher = Sha256::new();
    let mut file = if resumed {
        rehash_existing(dest, &mut hasher)?;
        OpenOptions::new().append(true).open(dest)?
    } else {
        File::create(dest)?
    };

    let total = response.content_length().map(|remaining| start + remaining);
    let mut downloaded = start;
    let mut session_bytes: u64 = 0;
    let session_start = Instant::now();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = response.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        downloaded += n as u64;
        session_bytes += n as u64;
        on_progress(DownloadProgress { downloaded, total });

        let paused = (ctl.should_pause)()
            || match ctl.max_bytes_per_sec {
                Some(rate) => throttle(session_bytes, rate, session_start, ctl.should_pause),
                None => false,
            };
        if paused {
            // Closing the connection is the pause: what is on disk is a
            // valid prefix for the next `Range` request.
            drop(response);
            file.flush()?;
            return Err(OtaError::Paused);
        }
    }
    file.flush()?;

    let actual = hex(&hasher.finalize());
    if let Some(expected) = expected_sha256_hex {
        if !actual.eq_ignore_ascii_case(expected) {
            drop(file);
            fs::remove_file(dest)?;
            return Err(OtaError::ChecksumMismatch { expected: expected.to_string(), actual });
        }
    }
    Ok(DownloadOutcome { path: dest.to_path_buf(), sha256_hex: actual, bytes: downloaded })
}

/// Sleeps until `session_bytes` fits under `rate` bytes/s since `since`, in
/// slices of at most [`THROTTLE_SLICE`]. Returns `true` if a pause was
/// requested while waiting.
fn throttle(session_bytes: u64, rate: u64, since: Instant, should_pause: &(dyn Fn() -> bool + Sync)) -> bool {
    let rate = rate.max(1);
    let due = Duration::from_secs_f64(session_bytes as f64 / rate as f64);
    loop {
        let elapsed = since.elapsed();
        if elapsed >= due {
            return false;
        }
        thread::sleep((due - elapsed).min(THROTTLE_SLICE));
        if should_pause() {
            return true;
        }
    }
}

/// Feeds an already-downloaded prefix into `hasher` before resuming — the
/// sha256 covers the whole file, not just the bytes fetched in this call.
fn rehash_existing(path: &Path, hasher: &mut Sha256) -> Result<(), OtaError> {
    let mut existing = File::open(path)?;
    let mut buf = [0u8; CHUNK];
    loop {
        let n = existing.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT_URL: &str =
        "https://wraith.releases.mks2508.systems/api/components/wraith-linux/download/0.1.0/darwin/aarch64/Wraith.app.tar.gz";
    const EXPECTED_SHA256: &str = "26fb34894265c1b7abc35511a7793b50d481231787ab8987bb4122fd87957299";
    const EXPECTED_BYTES: u64 = 31_871_987;

    #[test]
    fn downloads_the_real_artifact_and_matches_the_published_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Wraith.app.tar.gz");
        let outcome = download(ARTIFACT_URL, &dest, Some(EXPECTED_SHA256), |_| {}).unwrap();
        assert_eq!(outcome.bytes, EXPECTED_BYTES);
        assert_eq!(outcome.sha256_hex, EXPECTED_SHA256);
        assert_eq!(fs::metadata(&dest).unwrap().len(), EXPECTED_BYTES);
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Wraith.app.tar.gz");
        let bogus = "0".repeat(64);
        let err = download(ARTIFACT_URL, &dest, Some(&bogus), |_| {}).unwrap_err();
        assert!(matches!(err, OtaError::ChecksumMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn unknown_sha256_skips_the_checksum_check() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Wraith.app.tar.gz");
        // None mirrors `HubLatest::sha256_hex()` on a backfilled
        // "sha256:unknown" row.
        let outcome = download(ARTIFACT_URL, &dest, None, |_| {}).unwrap();
        assert_eq!(outcome.bytes, EXPECTED_BYTES);
    }

    #[test]
    fn resumes_a_partial_download_instead_of_restarting() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Wraith.app.tar.gz");

        // Fabricate a genuinely partial file with a raw ranged GET — half
        // the artifact, not the whole thing.
        let client = Client::new();
        let half = EXPECTED_BYTES / 2;
        let partial = client
            .get(ARTIFACT_URL)
            .header(RANGE, format!("bytes=0-{}", half - 1))
            .send()
            .unwrap()
            .bytes()
            .unwrap();
        assert_eq!(partial.len() as u64, half);
        fs::write(&dest, &partial).unwrap();

        let mut saw_progress_above_half = false;
        let outcome = download(ARTIFACT_URL, &dest, Some(EXPECTED_SHA256), |p| {
            // If the download had restarted instead of resuming, the first
            // progress tick would report a count below `half`.
            assert!(p.downloaded >= half, "progress went backwards, download restarted instead of resuming: {p:?}");
            if p.downloaded > half {
                saw_progress_above_half = true;
            }
        })
        .unwrap();

        assert!(saw_progress_above_half);
        assert_eq!(outcome.bytes, EXPECTED_BYTES);
        assert_eq!(outcome.sha256_hex, EXPECTED_SHA256);
    }
}
