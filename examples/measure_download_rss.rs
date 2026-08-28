//! Manual RSS measurement for M2's DoD ("pico de RSS del proceso muy por
//! debajo del tamaño del artefacto"). Run under `/usr/bin/time -l` and read
//! "maximum resident set size" from its output — that's the process-wide
//! high-water mark, exactly what the DoD asks for. Not part of `cargo
//! test`: this is a one-shot measurement to paste into the report, not a
//! pass/fail assertion.
//!
//!     /usr/bin/time -l cargo run --release --example measure_download_rss

use mks_ota::download::download;

const ARTIFACT_URL: &str =
    "https://wraith.releases.mks2508.systems/api/components/wraith-linux/download/0.1.0/darwin/aarch64/Wraith.app.tar.gz";
const EXPECTED_SHA256: &str = "26fb34894265c1b7abc35511a7793b50d481231787ab8987bb4122fd87957299";

fn main() {
    let dir = std::env::temp_dir().join("mks-ota-rss-measurement");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let dest = dir.join("Wraith.app.tar.gz");
    let _ = std::fs::remove_file(&dest); // start from a clean, non-resumed download

    let outcome = download(ARTIFACT_URL, &dest, Some(EXPECTED_SHA256), |_| {}).expect("download must succeed");
    println!("downloaded {} bytes, sha256 {}", outcome.bytes, outcome.sha256_hex);
}
