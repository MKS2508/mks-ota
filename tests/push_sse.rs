//! `mks_ota::push` against a scripted loopback SSE server: events, heartbeats,
//! cuts and reconnection with `Last-Event-ID`, `retry:`, malformed events, a
//! stalled stream, `stop()` latency, and a hub that answers errors.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mks_ota::push::{self, Published, PushConfig, PushSignal};
use mks_ota::Urgency;

/// One accepted connection as the server saw it.
#[derive(Debug, Clone)]
struct Conn {
    at: Instant,
    head: String,
}

type Script = dyn Fn(usize, &mut TcpStream) + Send + Sync;

/// Accepts connections forever; connection `i` is handed to `script(i, …)`
/// after its request head is read and logged. The script owns the socket:
/// returning closes it.
fn server(
    script: impl Fn(usize, &mut TcpStream) + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Conn>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Arc<Mutex<Vec<Conn>>> = Arc::default();
    let seen = Arc::clone(&log);
    let script: Arc<Script> = Arc::new(script);
    thread::spawn(move || {
        for (i, stream) in listener.incoming().flatten().enumerate() {
            let seen = Arc::clone(&seen);
            let script = Arc::clone(&script);
            thread::spawn(move || {
                let mut stream = stream;
                let head = read_head(&mut stream);
                seen.lock().unwrap().push(Conn {
                    at: Instant::now(),
                    head,
                });
                script(i, &mut stream);
            });
        }
    });
    (format!("http://{addr}/api/events?components=c"), log)
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
    String::from_utf8_lossy(&head).to_ascii_lowercase()
}

/// `200 text/event-stream`, no length: the body runs until the socket closes.
fn open_stream(stream: &mut TcpStream) {
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")
        .unwrap();
}

fn send(stream: &mut TcpStream, text: &str) {
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

fn published(component: &str, version: &str, urgency: &str, id: u32) -> String {
    format!(
        "event: component-published\nid: {id}\ndata: {{\"component\":\"{component}\",\"version\":\"{version}\",\"target\":\"any\",\"arch\":\"any\",\"kind\":\"binary\",\"urgency\":\"{urgency}\",\"pubDate\":\"2026-11-09T10:00:00.000Z\"}}\n\n"
    )
}

/// Holds the socket open (the client decides when it ends) for `d`.
fn hold(d: Duration) {
    thread::sleep(d);
}

fn cfg(url: String) -> PushConfig {
    PushConfig {
        url,
        read_timeout: Duration::from_secs(5),
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(100),
        stable_after: Duration::from_secs(60),
    }
}

fn start(cfg: PushConfig) -> (push::PushHandle, Receiver<PushSignal>) {
    let (tx, rx) = mpsc::channel();
    let handle = push::subscribe(cfg, move |s| {
        let _ = tx.send(s);
    })
    .unwrap();
    (handle, rx)
}

fn next(rx: &Receiver<PushSignal>) -> PushSignal {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("a signal within 5 s")
}

fn wait_for(cond: impl Fn() -> bool, within: Duration) -> bool {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    cond()
}

// (a)
#[test]
fn a_published_event_arrives_with_its_urgency() {
    let (url, log) = server(|_, s| {
        open_stream(s);
        send(s, "retry: 5000\n: connected\n\n");
        send(
            s,
            &published("haido-frontend-staging", "2026.11.9-1", "urgent", 1),
        );
        hold(Duration::from_secs(3));
    });
    let (handle, rx) = start(cfg(url));

    assert_eq!(next(&rx), PushSignal::Connected);
    assert_eq!(
        next(&rx),
        PushSignal::Published(Published {
            component: "haido-frontend-staging".into(),
            version: "2026.11.9-1".into(),
            kind: Some("binary".into()),
            urgency: Urgency::Urgent,
        })
    );
    let head = log.lock().unwrap()[0].head.clone();
    assert!(head.starts_with("get /api/events?components=c "), "{head}");
    assert!(head.contains("accept: text/event-stream"), "{head}");
    assert!(
        !head.contains("last-event-id"),
        "nothing to resume on the first connection"
    );
    handle.stop();
}

// (b)
#[test]
fn heartbeats_alone_produce_no_signals() {
    let (url, _) = server(|_, s| {
        open_stream(s);
        for _ in 0..10 {
            send(s, ": hb\n\n");
            thread::sleep(Duration::from_millis(50));
        }
        hold(Duration::from_secs(2));
    });
    let (handle, rx) = start(cfg(url));
    assert_eq!(next(&rx), PushSignal::Connected);
    assert!(
        rx.recv_timeout(Duration::from_millis(800)).is_err(),
        "a heartbeat is not an event"
    );
    handle.stop();
}

// (c)
#[test]
fn a_cut_reconnects_signals_connected_again_and_sends_last_event_id() {
    let (url, log) = server(|i, s| {
        open_stream(s);
        if i == 0 {
            send(s, &published("c", "1.0.0", "normal", 41));
            send(s, &published("c", "1.0.1", "normal", 42));
            // return ⇒ the server cuts the stream
        } else {
            send(s, &published("c", "1.0.2", "normal", 43));
            hold(Duration::from_secs(3));
        }
    });
    let (handle, rx) = start(cfg(url));

    assert_eq!(next(&rx), PushSignal::Connected);
    assert!(matches!(next(&rx), PushSignal::Published(p) if p.version == "1.0.0"));
    assert!(matches!(next(&rx), PushSignal::Published(p) if p.version == "1.0.1"));
    assert_eq!(
        next(&rx),
        PushSignal::Connected,
        "the caller learns it must re-query /latest"
    );
    assert!(matches!(next(&rx), PushSignal::Published(p) if p.version == "1.0.2"));

    let conns = log.lock().unwrap().clone();
    assert!(
        conns[1].head.contains("last-event-id: 42"),
        "{}",
        conns[1].head
    );
    handle.stop();
}

// (d)
#[test]
fn retry_from_the_server_is_the_minimum_reconnection_delay() {
    let (url, log) = server(|i, s| {
        open_stream(s);
        if i == 0 {
            send(s, "retry: 400\n\n");
        } else {
            hold(Duration::from_secs(3));
        }
    });
    let (handle, rx) = start(PushConfig {
        backoff_min: Duration::from_millis(1),
        backoff_max: Duration::from_secs(2),
        ..cfg(url)
    });
    assert_eq!(next(&rx), PushSignal::Connected);
    assert_eq!(next(&rx), PushSignal::Connected);
    let conns = log.lock().unwrap().clone();
    let gap = conns[1].at - conns[0].at;
    assert!(
        gap >= Duration::from_millis(400),
        "reconnected after {gap:?}, below retry: 400"
    );
    handle.stop();
}

// (e)
#[test]
fn malformed_events_are_skipped_and_the_stream_goes_on() {
    let (url, _) = server(|_, s| {
        open_stream(s);
        send(s, "event: component-published\ndata: {not json\n\n");
        send(
            s,
            "event: component-published\ndata: {\"component\":\"c\"}\n\n",
        );
        send(
            s,
            "event: something-else\ndata: {\"component\":\"c\",\"version\":\"9.9.9\"}\n\n",
        );
        send(s, "garbage line without colon\n\n");
        send(s, &published("c", "2.0.0", "normal", 7));
        hold(Duration::from_secs(3));
    });
    let (handle, rx) = start(cfg(url));
    assert_eq!(next(&rx), PushSignal::Connected);
    assert_eq!(
        next(&rx),
        PushSignal::Published(Published {
            component: "c".into(),
            version: "2.0.0".into(),
            kind: Some("binary".into()),
            urgency: Urgency::Normal
        })
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "only the good event is a signal"
    );
    handle.stop();
}

// (f)
#[test]
fn a_silent_server_is_dropped_after_the_read_timeout() {
    let read_timeout = Duration::from_millis(300);
    let (url, log) = server(|i, s| match i {
        0 => hold(Duration::from_secs(5)), // accepts, never answers
        1 => {
            open_stream(s);
            hold(Duration::from_secs(5)); // answers, then a stalled stream
        }
        _ => {
            open_stream(s);
            hold(Duration::from_secs(5));
        }
    });
    let (handle, rx) = start(PushConfig {
        read_timeout,
        ..cfg(url)
    });

    assert_eq!(
        next(&rx),
        PushSignal::Connected,
        "after giving up on the head-less connection"
    );
    assert_eq!(
        next(&rx),
        PushSignal::Connected,
        "after giving up on the stalled stream"
    );
    let conns = log.lock().unwrap().clone();
    assert!(conns.len() >= 3);
    assert!(
        conns[1].at - conns[0].at >= read_timeout,
        "no head: waited the read timeout"
    );
    assert!(
        conns[2].at - conns[1].at >= read_timeout,
        "no bytes: waited the read timeout"
    );
    assert!(
        conns[2].at - conns[1].at < Duration::from_secs(2),
        "but not much longer"
    );
    handle.stop();
}

// (g)
#[test]
fn stop_returns_within_read_timeout_plus_a_slice() {
    let read_timeout = Duration::from_secs(1);
    let (url, _) = server(|_, s| {
        open_stream(s);
        hold(Duration::from_secs(10));
    });
    let (handle, rx) = start(PushConfig {
        read_timeout,
        ..cfg(url)
    });
    assert_eq!(next(&rx), PushSignal::Connected);

    let started = Instant::now();
    handle.stop();
    let took = started.elapsed();
    assert!(
        took < read_timeout + Duration::from_millis(250),
        "stop() took {took:?}"
    );
}

// (h)
#[test]
fn errors_back_off_and_never_signal_connected() {
    let (url, log) = server(|i, s| {
        let response: &[u8] = if i % 2 == 0 {
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        } else {
            // 200 but not an event stream (a captive portal, a wrong route).
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi"
        };
        let _ = s.write_all(response);
    });
    let (handle, rx) = start(PushConfig {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_secs(1),
        ..cfg(url)
    });

    assert!(
        wait_for(|| log.lock().unwrap().len() >= 3, Duration::from_secs(5)),
        "it keeps retrying"
    );
    thread::sleep(Duration::from_millis(1500));
    let attempts = log.lock().unwrap().len();
    assert!(
        rx.try_recv().is_err(),
        "an error response is not a connection"
    );
    // Without backoff a loopback loop would make thousands of attempts.
    assert!(
        attempts <= 25,
        "{attempts} attempts in ~1.5 s: the backoff is not growing"
    );
    handle.stop();
}

#[test]
fn dropping_the_handle_stops_the_subscription() {
    let (url, log) = server(|_, s| {
        open_stream(s);
        // return ⇒ cut; a live subscription would reconnect every ~10-100 ms
    });
    let (handle, rx) = start(PushConfig {
        read_timeout: Duration::from_millis(300),
        ..cfg(url)
    });
    assert_eq!(next(&rx), PushSignal::Connected);
    drop(handle);
    thread::sleep(Duration::from_millis(600));
    let settled = log.lock().unwrap().len();
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        log.lock().unwrap().len(),
        settled,
        "no new connections after drop"
    );
}
