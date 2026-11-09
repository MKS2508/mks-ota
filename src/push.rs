//! Optional push channel: a long-lived Server-Sent Events subscription to
//! the hub's `GET /api/events?components=…` that says "component C has a new
//! version V with urgency U".
//!
//! **A push event is a hint, never an update.** The only thing it is good for
//! is triggering the caller's normal `/latest` query sooner than its poll
//! timer would. That is why the output type is [`Published`] (three strings'
//! worth of information) and never a [`crate::HubLatest`]: download, sha256,
//! minisign and slots keep running off `/latest`, and a forged or repeated
//! event costs at most one extra query. For the same reason the caller should
//! query `/latest` on every [`PushSignal::Connected`]: the hub does not replay
//! what was published while the connection was down.
//!
//! The subscription never replaces polling. If the hub is down or a proxy
//! keeps cutting the stream, this module just keeps reconnecting in the
//! background (exponential backoff with full jitter, honouring the server's
//! `retry:`), and the caller's poll timer is unaffected.
//!
//! Design constraints: no global state, no async runtime, no new
//! dependencies. One `std` thread per subscription, a blocking reqwest client
//! built *inside* that thread (building or dropping a blocking client on a
//! tokio worker panics), and cancellation through an `AtomicBool` checked
//! after every read and every backoff slice.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::manifest::Urgency;

/// The only event name this module turns into a signal.
pub const EVENT_COMPONENT_PUBLISHED: &str = "component-published";

/// Longest uninterrupted sleep while backing off: `stop()` is noticed at most
/// this late between connection attempts.
const STOP_SLICE: Duration = Duration::from_millis(250);

/// Read buffer per `read` call on the stream.
const READ_BUF: usize = 8 * 1024;

/// A line longer than this is discarded whole (a hostile or broken server
/// cannot make the client buffer without bound).
const MAX_LINE: usize = 64 * 1024;

/// Cap on the accumulated `data` of one event.
const MAX_EVENT_DATA: usize = 256 * 1024;

/// Subscription parameters. [`PushConfig::for_hub`] fills in the defaults of
/// the wire contract.
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// Full events URL.
    pub url: String,
    /// No byte at all for this long ⇒ the connection is considered dead and
    /// is reopened. Must comfortably exceed the server's heartbeat (the hub
    /// sends one every 15 s). Also bounds connect and how long `stop()` can
    /// block.
    pub read_timeout: Duration,
    /// First reconnection delay (before jitter).
    pub backoff_min: Duration,
    /// Ceiling of the reconnection delay.
    pub backoff_max: Duration,
    /// A connection that lived at least this long resets the backoff; one
    /// that is cut sooner keeps growing it (a proxy cutting every few seconds
    /// must not turn into a reconnect storm).
    pub stable_after: Duration,
}

impl PushConfig {
    /// `{hub_base}/api/events?components=a,b` with the contract defaults:
    /// read timeout 40 s, backoff 1 s → 300 s, reset after 60 s alive.
    /// `components` empty ⇒ no filter (every component of the tenant).
    pub fn for_hub(hub_base: &str, components: &[&str]) -> PushConfig {
        let base = hub_base.trim_end_matches('/');
        let url = if components.is_empty() {
            format!("{base}/api/events")
        } else {
            format!("{base}/api/events?components={}", components.join(","))
        };
        PushConfig {
            url,
            read_timeout: Duration::from_secs(40),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// "Component `component` now has `version`, published with `urgency`."
/// Informational: the authoritative values come from `/latest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub component: String,
    pub version: String,
    pub urgency: Urgency,
}

/// What the subscription reports to the callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushSignal {
    /// A connection was (re)established. Anything published while
    /// disconnected was not replayed: query `/latest` now.
    Connected,
    Published(Published),
}

/// Running subscription. [`PushHandle::stop`] ends it and waits for the
/// thread; dropping the handle also asks it to stop, without waiting.
#[must_use = "dropping the handle stops the subscription"]
#[derive(Debug)]
pub struct PushHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PushHandle {
    /// Stops the subscription and joins its thread. Blocks at most about
    /// `read_timeout` (a read in flight is not interruptible) plus one
    /// backoff slice.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PushHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts a subscription on its own thread (`mks-ota-push`). `on_signal` runs
/// on that thread; keep it short (send on a channel). The only error is
/// failing to spawn the thread.
pub fn subscribe<F>(cfg: PushConfig, on_signal: F) -> std::io::Result<PushHandle>
where
    F: FnMut(PushSignal) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = thread::Builder::new()
        .name("mks-ota-push".into())
        .spawn(move || run(cfg, flag, on_signal))?;
    Ok(PushHandle {
        stop,
        thread: Some(thread),
    })
}

/// Reconnect loop. Owns the blocking client for its whole life.
fn run<F: FnMut(PushSignal)>(cfg: PushConfig, stop: Arc<AtomicBool>, mut on_signal: F) {
    let client = match Client::builder()
        .user_agent(concat!("mks-ota/", env!("CARGO_PKG_VERSION")))
        // In reqwest's blocking client this bounds the wait for the response
        // head AND every single `read` of the body: a read timeout, not a
        // total one — exactly what a long-lived stream needs.
        .timeout(cfg.read_timeout)
        .connect_timeout(cfg.read_timeout)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            eprintln!("[mks-ota push] could not build the HTTP client, push disabled: {err}");
            return;
        }
    };

    let mut seed = initial_seed();
    let mut stream = StreamState::default();
    let mut attempt: u32 = 0;
    while !stop.load(Ordering::SeqCst) {
        let opened = Instant::now();
        let connected = connect_once(&client, &cfg, &stop, &mut stream, &mut on_signal);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if connected && opened.elapsed() >= cfg.stable_after {
            attempt = 0;
        }
        seed = xorshift(seed);
        let delay = backoff(
            attempt,
            cfg.backoff_min,
            cfg.backoff_max,
            stream.retry_hint,
            seed,
        );
        attempt = attempt.saturating_add(1);
        sleep_unless_stopped(delay, &stop);
    }
}

/// What survives across connections: the `Last-Event-ID` to send and the
/// server's last `retry:` hint.
#[derive(Debug, Default)]
struct StreamState {
    last_event_id: Option<String>,
    retry_hint: Option<Duration>,
}

/// One connection, read until it ends. Returns whether it got as far as a
/// valid `200 text/event-stream` (i.e. whether `Connected` was emitted).
fn connect_once<F: FnMut(PushSignal)>(
    client: &Client,
    cfg: &PushConfig,
    stop: &AtomicBool,
    stream: &mut StreamState,
    on_signal: &mut F,
) -> bool {
    let mut request = client
        .get(&cfg.url)
        .header(ACCEPT, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache");
    if let Some(id) = &stream.last_event_id {
        request = request.header("Last-Event-ID", id.as_str());
    }
    let mut response = match request.send() {
        Ok(response) => response,
        Err(_) => return false,
    };
    let is_stream = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        });
    if response.status() != StatusCode::OK || !is_stream {
        return false;
    }

    on_signal(PushSignal::Connected);
    let mut parser = SseParser::default();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        // EOF (server closed), timeout (no heartbeat within read_timeout) or
        // a reset: all mean "reconnect".
        let n = match response.read(&mut buf) {
            Ok(0) | Err(_) => return true,
            Ok(n) => n,
        };
        for item in parser.feed(&buf[..n]) {
            match item {
                SseItem::Retry(ms) => stream.retry_hint = Some(Duration::from_millis(ms)),
                SseItem::Event(event) => {
                    if event.id.is_some() {
                        stream.last_event_id = event.id.clone();
                    }
                    if let Some(published) = published_from(&event) {
                        on_signal(PushSignal::Published(published));
                    }
                }
            }
        }
    }
}

/// `data` of a `component-published` event.
#[derive(Deserialize)]
struct PublishedWire {
    component: String,
    version: String,
    #[serde(default)]
    urgency: Urgency,
}

/// Only a `component-published` event whose data is a JSON object with
/// non-empty `component` and `version` becomes a signal; anything else is
/// ignored without cutting the stream.
fn published_from(event: &SseEvent) -> Option<Published> {
    if event.event != EVENT_COMPONENT_PUBLISHED {
        return None;
    }
    let wire: PublishedWire = serde_json::from_str(&event.data).ok()?;
    if wire.component.trim().is_empty() || wire.version.trim().is_empty() {
        return None;
    }
    Some(Published {
        component: wire.component,
        version: wire.version,
        urgency: wire.urgency,
    })
}

// ── Backoff ─────────────────────────────────────────────────────────────

/// Exponential base for `attempt`: `min · 2^attempt`, capped at `max`.
pub(crate) fn backoff_base(attempt: u32, min: Duration, max: Duration) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    min.checked_mul(factor).unwrap_or(max).min(max)
}

/// Reconnection delay: full jitter `rand[0, base]` over [`backoff_base`],
/// never below the server's `retry:` hint (itself capped at `max`, so a
/// server cannot park the client for longer than its own ceiling).
pub(crate) fn backoff(
    attempt: u32,
    min: Duration,
    max: Duration,
    retry_hint: Option<Duration>,
    seed: u64,
) -> Duration {
    let base = backoff_base(attempt, min, max);
    let jitter = Duration::from_nanos(if base.is_zero() {
        0
    } else {
        seed % (base.as_nanos().min(u64::MAX as u128) as u64).saturating_add(1)
    });
    match retry_hint {
        Some(hint) => jitter.max(hint.min(max)),
        None => jitter,
    }
}

fn xorshift(mut x: u64) -> u64 {
    if x == 0 {
        x = 0x9E37_79B9_7F4A_7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Seeded from the clock and the thread id: two devices (or two
/// subscriptions) restarted together do not reconnect in lockstep.
fn initial_seed() -> u64 {
    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    thread::current().id().hash(&mut hasher);
    hasher.finish()
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let until = Instant::now() + total;
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= until {
            return;
        }
        thread::sleep((until - now).min(STOP_SLICE));
    }
}

// ── SSE parser (pure) ───────────────────────────────────────────────────

/// A dispatched event (WHATWG "server-sent events" §9.2.6 semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// `event:` field, `"message"` when absent.
    pub event: String,
    /// `data:` lines joined with `\n`.
    pub data: String,
    /// Last event ID in effect when the event was dispatched.
    pub id: Option<String>,
}

/// Parser output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseItem {
    Event(SseEvent),
    /// `retry:` reconnection time in milliseconds.
    Retry(u64),
}

/// Incremental `text/event-stream` parser: feed arbitrary chunks, get the
/// complete items they finish. Handles CRLF / LF / CR line endings (also
/// split across chunks), comments, multi-line `data`, `id`, `retry`, a
/// leading BOM, and discards overlong lines and events.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    line_overflow: bool,
    after_cr: bool,
    started: bool,
    event: String,
    data: String,
    data_overflow: bool,
    id_buffer: Option<String>,
    last_id: Option<String>,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<SseItem> {
        let mut out = Vec::new();
        let mut bytes = chunk;
        if !self.started && !bytes.is_empty() {
            self.started = true;
            if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
                bytes = rest;
            }
        }
        for &b in bytes {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue; // second half of a CRLF
                }
            }
            match b {
                b'\n' => self.end_line(&mut out),
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut out);
                }
                _ if self.line_overflow => {}
                _ => {
                    self.line.push(b);
                    if self.line.len() > MAX_LINE {
                        self.line.clear();
                        self.line_overflow = true;
                    }
                }
            }
        }
        out
    }

    fn end_line(&mut self, out: &mut Vec<SseItem>) {
        if std::mem::take(&mut self.line_overflow) {
            self.line.clear();
            return; // an overlong line is dropped whole, never read as blank
        }
        let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return; // comment / heartbeat
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.data.len() + value.len() + 1 > MAX_EVENT_DATA {
                    self.data_overflow = true;
                } else {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
            }
            "id" if !value.contains('\0') => self.id_buffer = Some(value.to_string()),
            "retry" if !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()) => {
                if let Ok(ms) = value.parse() {
                    out.push(SseItem::Retry(ms));
                }
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseItem>) {
        if let Some(id) = self.id_buffer.take() {
            self.last_id = (!id.is_empty()).then_some(id);
        }
        let event = std::mem::take(&mut self.event);
        let mut data = std::mem::take(&mut self.data);
        if std::mem::take(&mut self.data_overflow) || data.is_empty() {
            return;
        }
        data.pop(); // trailing '\n'
        out.push(SseItem::Event(SseEvent {
            event: if event.is_empty() {
                "message".into()
            } else {
                event
            },
            data,
            id: self.last_id.clone(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(items: Vec<SseItem>) -> Vec<SseEvent> {
        items
            .into_iter()
            .filter_map(|i| match i {
                SseItem::Event(e) => Some(e),
                SseItem::Retry(_) => None,
            })
            .collect()
    }

    // ── parser ──

    #[test]
    fn parses_the_hub_contract_stream() {
        let mut p = SseParser::default();
        let items = p.feed(
            b"retry: 5000\n: connected\n\n: hb\n\nevent: component-published\nid: 42\ndata: {\"component\":\"haido-frontend-staging\",\"version\":\"2026.11.9-1\",\"urgency\":\"urgent\"}\n\n",
        );
        assert_eq!(items[0], SseItem::Retry(5000));
        let evs = events(items);
        assert_eq!(evs.len(), 1, "comments and heartbeats dispatch nothing");
        assert_eq!(evs[0].event, "component-published");
        assert_eq!(evs[0].id.as_deref(), Some("42"));
        assert_eq!(
            published_from(&evs[0]),
            Some(Published {
                component: "haido-frontend-staging".into(),
                version: "2026.11.9-1".into(),
                urgency: Urgency::Urgent
            })
        );
    }

    #[test]
    fn handles_crlf_cr_and_lines_split_across_chunks() {
        let stream = b"event: a\r\ndata: one\r\ndata: two\r\n\r\nevent: b\rdata: x\r\r";
        // Byte by byte: every boundary, including between \r and \n.
        let mut p = SseParser::default();
        let mut items = Vec::new();
        for b in stream {
            items.extend(p.feed(std::slice::from_ref(b)));
        }
        let evs = events(items);
        assert_eq!(evs.len(), 2);
        assert_eq!(
            (evs[0].event.as_str(), evs[0].data.as_str()),
            ("a", "one\ntwo")
        );
        assert_eq!((evs[1].event.as_str(), evs[1].data.as_str()), ("b", "x"));
    }

    #[test]
    fn field_details_follow_the_spec() {
        let mut p = SseParser::default();
        let evs = events(p.feed(b"\xEF\xBB\xBFdata:no-space\n\ndata\n\nevent: only-type\n\nid: 7\nretry: soon\ndata: y\n\ndata: z\n\n"));
        // BOM stripped; "data:" without space; bare "data" = empty line of data.
        assert_eq!(evs[0].data, "no-space");
        assert_eq!(evs[0].event, "message");
        assert_eq!(evs[1].data, "");
        // An event with no data is not dispatched ("event: only-type").
        assert_eq!(evs[2].data, "y");
        assert_eq!(evs[2].id.as_deref(), Some("7"));
        assert_eq!(evs[3].id.as_deref(), Some("7"), "the last event id sticks");
        assert_eq!(evs.len(), 4);
    }

    #[test]
    fn an_overlong_line_is_dropped_without_breaking_the_stream() {
        let mut p = SseParser::default();
        let mut big = b"data: ".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_LINE + 10));
        big.extend_from_slice(b"\n\nevent: component-published\ndata: {\"component\":\"c\",\"version\":\"1.0.0\"}\n\n");
        let evs = events(p.feed(&big));
        assert_eq!(evs.len(), 1);
        assert_eq!(
            published_from(&evs[0]).unwrap().urgency,
            Urgency::Normal,
            "absent urgency ⇒ normal"
        );
    }

    #[test]
    fn malformed_or_foreign_events_are_not_signals() {
        let ev = |event: &str, data: &str| SseEvent {
            event: event.into(),
            data: data.into(),
            id: None,
        };
        assert_eq!(
            published_from(&ev("component-published", "{not json")),
            None
        );
        assert_eq!(
            published_from(&ev("component-published", r#"{"component":"c"}"#)),
            None
        );
        assert_eq!(
            published_from(&ev(
                "component-published",
                r#"{"component":"","version":"1"}"#
            )),
            None
        );
        assert_eq!(
            published_from(&ev("message", r#"{"component":"c","version":"1"}"#)),
            None
        );
        let future = published_from(&ev(
            "component-published",
            r#"{"component":"c","version":"1","urgency":"critical","extra":1}"#,
        ));
        assert_eq!(future.unwrap().urgency, Urgency::Normal);
    }

    // ── backoff ──

    #[test]
    fn the_backoff_base_doubles_up_to_the_ceiling() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(300));
        let bases: Vec<u64> = (0..12)
            .map(|a| backoff_base(a, min, max).as_secs())
            .collect();
        assert_eq!(bases, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]);
        assert_eq!(backoff_base(u32::MAX, min, max), max, "no overflow");
    }

    #[test]
    fn the_jittered_delay_stays_within_bounds() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(300));
        let mut seed = 1;
        for attempt in 0..20 {
            let base = backoff_base(attempt, min, max);
            let (mut lo, mut hi) = (Duration::MAX, Duration::ZERO);
            for _ in 0..500 {
                seed = xorshift(seed);
                let d = backoff(attempt, min, max, None, seed);
                assert!(d <= base && d <= max, "attempt {attempt}: {d:?} > {base:?}");
                lo = lo.min(d);
                hi = hi.max(d);
            }
            assert!(
                hi > base / 2 && lo < base / 2,
                "full jitter spreads over [0, base]: {lo:?}..{hi:?}"
            );

            let hint = Duration::from_millis(5000);
            for _ in 0..50 {
                seed = xorshift(seed);
                assert!(
                    backoff(attempt, min, max, Some(hint), seed) >= hint,
                    "never below retry:"
                );
            }
        }
        let huge = Duration::from_secs(10_000);
        assert_eq!(
            backoff(0, min, max, Some(huge), 3),
            max,
            "a retry: above the ceiling is capped"
        );
    }

    #[test]
    fn for_hub_builds_the_events_url() {
        let cfg = PushConfig::for_hub(
            "https://haido.releases.mks2508.systems/",
            &["tpv-el-haido", "haido-frontend"],
        );
        assert_eq!(cfg.url, "https://haido.releases.mks2508.systems/api/events?components=tpv-el-haido,haido-frontend");
        assert_eq!(cfg.read_timeout, Duration::from_secs(40));
        assert_eq!(
            (cfg.backoff_min, cfg.backoff_max),
            (Duration::from_secs(1), Duration::from_secs(300))
        );
        assert_eq!(cfg.stable_after, Duration::from_secs(60));
        assert_eq!(
            PushConfig::for_hub("http://h", &[]).url,
            "http://h/api/events"
        );
    }
}
