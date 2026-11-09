//! `download_controlled` against a loopback server that honours `Range`:
//! throughput cap, pause = cut + `Range` resume, and a sha256 mismatch that
//! leaves nothing behind for the next attempt to resume on top of.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use mks_ota::download::{download_controlled, DownloadControl};
use mks_ota::OtaError;
use sha2::{Digest, Sha256};

/// Every `Range` header the server saw, in order (`None` = plain GET).
type RangeLog = Arc<Mutex<Vec<Option<u64>>>>;

/// Serves `body` on every GET. `Range: bytes=N-` gets a 206 with the tail,
/// `N >= len` a 416, anything else a 200. Writes in 8 KiB pieces so the
/// client sees several chunks.
fn serve(body: Vec<u8>) -> (String, RangeLog) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().unwrap();
    let log: RangeLog = Arc::default();
    let seen = Arc::clone(&log);
    let body = Arc::new(body);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let body = Arc::clone(&body);
            let seen = Arc::clone(&seen);
            thread::spawn(move || handle(stream, &body, &seen));
        }
    });
    (format!("http://{addr}/artifact.bin"), log)
}

fn handle(mut stream: TcpStream, body: &[u8], seen: &RangeLog) {
    let head = read_head(&mut stream);
    let range = head.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("range") {
            return None;
        }
        value
            .trim()
            .strip_prefix("bytes=")?
            .strip_suffix('-')?
            .parse::<u64>()
            .ok()
    });
    seen.lock().unwrap().push(range);

    let len = body.len() as u64;
    let (status, from) = match range {
        Some(n) if n >= len => {
            let _ = stream.write_all(
                format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
            );
            return;
        }
        Some(n) => (
            format!(
                "206 Partial Content\r\nContent-Range: bytes {n}-{}/{len}",
                len - 1
            ),
            n as usize,
        ),
        None => ("200 OK".to_string(), 0),
    };
    let tail = &body[from..];
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        tail.len()
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    for piece in tail.chunks(8 * 1024) {
        if stream.write_all(piece).is_err() {
            return; // client cut the connection (pause)
        }
    }
    let _ = stream.flush();
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn dest(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    (dir, path)
}

#[test]
fn the_rate_limit_caps_the_average_throughput() {
    let body = payload(256 * 1024);
    let (url, _) = serve(body.clone());
    let (_dir, path) = dest("limited.bin");
    let never = || false;
    let ctl = DownloadControl {
        max_bytes_per_sec: Some(64 * 1024),
        should_pause: &never,
    };

    let started = Instant::now();
    let out = download_controlled(&url, &path, Some(&sha256_hex(&body)), ctl, |_| {}).unwrap();
    let took = started.elapsed();

    assert_eq!(out.bytes, body.len() as u64);
    assert!(
        took.as_secs_f64() >= 3.5,
        "256 KiB at 64 KiB/s must take ~4 s, took {took:?}"
    );
    assert!(
        took.as_secs_f64() < 8.0,
        "the limiter should not overshoot wildly, took {took:?}"
    );
}

#[test]
fn pausing_cuts_the_connection_and_the_next_call_resumes_with_range() {
    let body = payload(512 * 1024);
    let (url, ranges) = serve(body.clone());
    let (_dir, path) = dest("paused.bin");
    let expected = sha256_hex(&body);

    // Pause as soon as the first chunk has landed.
    let pause = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&pause);
    let should_pause = move || flag.load(Ordering::SeqCst);
    let ctl = DownloadControl {
        max_bytes_per_sec: None,
        should_pause: &should_pause,
    };
    let err = download_controlled(&url, &path, Some(&expected), ctl, |_| {
        pause.store(true, Ordering::SeqCst)
    })
    .unwrap_err();
    assert!(matches!(err, OtaError::Paused), "got {err:?}");
    let kept = std::fs::metadata(&path).unwrap().len();
    assert!(
        kept > 0 && kept < body.len() as u64,
        "a partial prefix is kept, got {kept} bytes"
    );

    // While paused, nothing connects at all.
    let always = || true;
    let err = download_controlled(
        &url,
        &path,
        Some(&expected),
        DownloadControl {
            max_bytes_per_sec: None,
            should_pause: &always,
        },
        |_| {},
    )
    .unwrap_err();
    assert!(matches!(err, OtaError::Paused));
    assert_eq!(
        ranges.lock().unwrap().len(),
        1,
        "a paused call must not open a connection"
    );

    // Resume.
    let first = Arc::new(AtomicUsize::new(usize::MAX));
    let seen_first = Arc::clone(&first);
    let out = download_controlled(
        &url,
        &path,
        Some(&expected),
        DownloadControl::unlimited(),
        move |p| {
            let _ = seen_first.compare_exchange(
                usize::MAX,
                p.downloaded as usize,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        },
    )
    .unwrap();
    assert_eq!(
        out.sha256_hex, expected,
        "sha256 covers the kept prefix plus the resumed tail"
    );
    assert_eq!(out.bytes, body.len() as u64);
    assert_eq!(std::fs::read(&path).unwrap(), body);
    assert!(
        first.load(Ordering::SeqCst) as u64 > kept,
        "progress continues from the prefix"
    );
    assert_eq!(
        ranges.lock().unwrap().as_slice(),
        &[None, Some(kept)],
        "the resume asked for the tail only"
    );
}

#[test]
fn a_checksum_mismatch_deletes_the_file_so_the_next_attempt_starts_clean() {
    let body = payload(96 * 1024);
    let (url, ranges) = serve(body.clone());
    let (_dir, path) = dest("bad.bin");

    let err = download_controlled(
        &url,
        &path,
        Some(&"0".repeat(64)),
        DownloadControl::unlimited(),
        |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(err, OtaError::ChecksumMismatch { .. }),
        "got {err:?}"
    );
    assert!(
        !path.exists(),
        "a mismatched file must not survive to poison the next Range resume"
    );

    let out = download_controlled(
        &url,
        &path,
        Some(&sha256_hex(&body)),
        DownloadControl::unlimited(),
        |_| {},
    )
    .unwrap();
    assert_eq!(out.bytes, body.len() as u64);
    assert_eq!(
        ranges.lock().unwrap().as_slice(),
        &[None, None],
        "the retry is a fresh GET, not a resume"
    );
}

#[test]
fn a_416_on_resume_restarts_from_zero_instead_of_failing_forever() {
    let body = payload(32 * 1024);
    let (url, ranges) = serve(body.clone());
    let (_dir, path) = dest("overlong.bin");
    // A prefix as long as the remote file: the server answers 416.
    std::fs::write(&path, vec![0u8; body.len()]).unwrap();

    let out = download_controlled(
        &url,
        &path,
        Some(&sha256_hex(&body)),
        DownloadControl::unlimited(),
        |_| {},
    )
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), body);
    assert_eq!(out.bytes, body.len() as u64);
    assert_eq!(
        ranges.lock().unwrap().as_slice(),
        &[Some(body.len() as u64), None]
    );
}
