//! When to apply an update — decided on the client, never by the hub.
//!
//! The hub only publishes an [`Urgency`] with each version. What that means
//! for a given artifact is [`decide`]:
//!
//! | | `normal` | `urgent` | `forced` |
//! |---|---|---|---|
//! | partial (bundle) | ask | apply when idle | apply when idle |
//! | full (binary) | ask | ask (insistently) | apply when idle |
//!
//! "Insistently" is UI the app builds from the urgency itself (no
//! indefinite "later"); the crate only distinguishes asking from applying.
//!
//! "Idle" belongs to the app: it implements [`IdleHook`] with its own
//! exceptions (an open sale, a live agent session…). The default is
//! [`NeverIdle`]: an app that does not define idle never gets anything
//! applied on its own. [`when_idle`] waits for the hook and runs the app's
//! apply closure once; how to apply (hot reload, `execve` + trial, a daemon
//! swap) stays in the app — the crate has no generic daemon restart.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::manifest::Urgency;

/// Which channel an artifact came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// A partial artifact (frontend bundle): applied without restarting the
    /// process.
    Partial,
    /// The whole app package: applying it restarts the process.
    Full,
}

/// What the app should do with a downloaded and verified artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decision {
    /// Show it to a human and let them choose when.
    Ask,
    /// Apply it on its own as soon as the app's [`IdleHook`] says idle (see
    /// [`when_idle`]). Still never without a hook that says so.
    ApplyWhenIdle,
}

/// The default policy table (see the module docs).
pub fn decide(urgency: Urgency, kind: ArtifactKind) -> Decision {
    match (urgency, kind) {
        (Urgency::Normal, _) => Decision::Ask,
        (Urgency::Urgent, ArtifactKind::Partial) => Decision::ApplyWhenIdle,
        (Urgency::Urgent, ArtifactKind::Full) => Decision::Ask,
        (Urgency::Forced, _) => Decision::ApplyWhenIdle,
    }
}

/// The app's definition of "nothing would be interrupted right now".
///
/// Called from the [`when_idle`] thread: keep it cheap and non-blocking
/// (read an atomic or a snapshot the app keeps up to date). Closures
/// `Fn() -> bool` implement it too.
pub trait IdleHook: Send + Sync + 'static {
    fn is_idle(&self) -> bool;
}

/// The safe default: never idle, so nothing is ever applied automatically.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverIdle;

impl IdleHook for NeverIdle {
    fn is_idle(&self) -> bool {
        false
    }
}

impl<F: Fn() -> bool + Send + Sync + 'static> IdleHook for F {
    fn is_idle(&self) -> bool {
        self()
    }
}

#[derive(Debug, Default)]
struct State {
    nudged: bool,
    cancelled: bool,
    finished: bool,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// A pending "apply when idle" for one artifact. [`WhenIdle::cancel`] ends it
/// and joins the thread; dropping it also cancels, without waiting.
#[must_use = "dropping a WhenIdle cancels it"]
#[derive(Debug)]
pub struct WhenIdle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl WhenIdle {
    /// Asks the hook now instead of at the next tick — call it when the app
    /// knows its idle state just changed (a sale closed, a session ended).
    pub fn nudge(&self) {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.nudged = true;
        self.shared.wake.notify_all();
    }

    /// Whether the apply closure already ran (successfully or not) — once it
    /// has, this `WhenIdle` is spent.
    pub fn is_finished(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finished
    }

    /// Cancels and joins. If the apply closure is already running, waits for
    /// it to return (it is never interrupted halfway).
    pub fn cancel(mut self) {
        self.signal_cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn signal_cancel(&self) {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.cancelled = true;
        self.shared.wake.notify_all();
    }
}

impl Drop for WhenIdle {
    fn drop(&mut self) {
        self.signal_cancel();
    }
}

/// Runs `apply` once, as soon as `hook` says idle.
///
/// Asks the hook right away, then every `every` and on every
/// [`WhenIdle::nudge`]. When it answers idle, asks **once more** right
/// before calling `apply`, so a state that flipped back in between is not
/// interrupted. `apply` runs on the `mks-ota-when-idle` thread, exactly once;
/// an `Err` is logged and ends this `WhenIdle` (the app's next update cycle
/// arms a new one). No global state: one `WhenIdle` per artifact.
pub fn when_idle<H, F>(hook: Arc<H>, every: Duration, apply: F) -> WhenIdle
where
    H: IdleHook + ?Sized,
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let shared = Arc::new(Shared::default());
    let worker = Arc::clone(&shared);
    let thread = thread::Builder::new()
        .name("mks-ota-when-idle".into())
        .spawn(move || run(&worker, hook.as_ref(), every, apply))
        .map_err(|err| {
            eprintln!("[mks-ota policy] could not spawn when_idle, nothing will auto-apply: {err}")
        })
        .ok();
    WhenIdle { shared, thread }
}

fn run<H, F>(shared: &Shared, hook: &H, every: Duration, apply: F)
where
    H: IdleHook + ?Sized,
    F: FnOnce() -> Result<(), String>,
{
    let mut first = true;
    loop {
        {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if !first && !st.nudged && !st.cancelled {
                st = shared
                    .wake
                    .wait_timeout(st, every)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            first = false;
            if st.cancelled {
                return;
            }
            st.nudged = false;
        }

        if !hook.is_idle() {
            continue;
        }
        // Second question right before acting: idle may have ended while the
        // first answer was being acted upon.
        if shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled
            || !hook.is_idle()
        {
            continue;
        }
        if let Err(err) = apply() {
            eprintln!("[mks-ota policy] apply-when-idle failed: {err}");
        }
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finished = true;
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    #[test]
    fn the_decision_table() {
        use ArtifactKind::{Full, Partial};
        use Decision::{ApplyWhenIdle, Ask};
        let cases = [
            (Urgency::Normal, Partial, Ask),
            (Urgency::Normal, Full, Ask),
            (Urgency::Urgent, Partial, ApplyWhenIdle),
            (Urgency::Urgent, Full, Ask),
            (Urgency::Forced, Partial, ApplyWhenIdle),
            (Urgency::Forced, Full, ApplyWhenIdle),
        ];
        for (urgency, kind, expected) in cases {
            assert_eq!(decide(urgency, kind), expected, "{urgency:?} × {kind:?}");
        }
    }

    fn counting_apply() -> (
        Arc<AtomicUsize>,
        impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        (calls, move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[test]
    fn never_idle_never_applies() {
        let (calls, apply) = counting_apply();
        let w = when_idle(Arc::new(NeverIdle), Duration::from_millis(10), apply);
        w.nudge();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!w.is_finished());
        w.cancel();
    }

    #[test]
    fn becoming_idle_plus_a_nudge_applies_promptly_and_once() {
        let idle = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&idle);
        let hook = Arc::new(move || flag.load(Ordering::SeqCst));
        let (tx, rx) = mpsc::channel();
        let w = when_idle(hook, Duration::from_secs(30), move || {
            tx.send(Instant::now()).unwrap();
            Ok(())
        });
        thread::sleep(Duration::from_millis(50));
        assert!(rx.try_recv().is_err(), "not idle yet");

        idle.store(true, Ordering::SeqCst);
        let nudged = Instant::now();
        w.nudge();
        let applied = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("applied after the nudge");
        assert!(
            applied - nudged < Duration::from_millis(50),
            "took {:?}",
            applied - nudged
        );
        thread::sleep(Duration::from_millis(50));
        assert!(rx.try_recv().is_err(), "apply runs exactly once");
        assert!(w.is_finished());
        w.cancel();
    }

    #[test]
    fn already_idle_applies_without_waiting_for_the_first_tick() {
        let (calls, apply) = counting_apply();
        let w = when_idle(Arc::new(|| true), Duration::from_secs(30), apply);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        w.cancel();
    }

    #[test]
    fn the_tick_asks_again_without_a_nudge() {
        let idle = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&idle);
        let (calls, apply) = counting_apply();
        let w = when_idle(
            Arc::new(move || flag.load(Ordering::SeqCst)),
            Duration::from_millis(30),
            apply,
        );
        thread::sleep(Duration::from_millis(60));
        idle.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        w.cancel();
    }

    /// Idle on the first question, busy on the second: the re-check wins.
    struct FlipsBack {
        asked: AtomicUsize,
    }
    impl IdleHook for FlipsBack {
        fn is_idle(&self) -> bool {
            // true, false, false, false…
            self.asked.fetch_add(1, Ordering::SeqCst) == 0
        }
    }

    #[test]
    fn idle_ending_between_the_two_questions_does_not_apply() {
        let hook = Arc::new(FlipsBack {
            asked: AtomicUsize::new(0),
        });
        let (calls, apply) = counting_apply();
        let w = when_idle(Arc::clone(&hook), Duration::from_millis(10), apply);
        thread::sleep(Duration::from_millis(150));
        assert!(
            hook.asked.load(Ordering::SeqCst) >= 2,
            "it asked twice at least"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        w.cancel();
    }

    #[test]
    fn cancel_without_applying_ends_the_thread() {
        let (calls, apply) = counting_apply();
        let w = when_idle(Arc::new(NeverIdle), Duration::from_secs(30), apply);
        let started = Instant::now();
        w.cancel(); // would hang ~30 s if the wait were not woken
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_apply_is_logged_and_ends_the_when_idle() {
        let asked = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&asked);
        let w = when_idle(
            Arc::new(move || {
                a.fetch_add(1, Ordering::SeqCst);
                true
            }),
            Duration::from_millis(10),
            || Err("boom".into()),
        );
        thread::sleep(Duration::from_millis(100));
        assert!(w.is_finished());
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "no retries after a failed apply"
        );
        w.cancel();
    }
}
