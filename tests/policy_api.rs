//! `policy` as an outside crate sees it: `ApplyKind` is `#[non_exhaustive]`,
//! so a downstream match needs a wildcard arm (adding a kind later does not
//! break callers), and `when_idle` takes any hook and an opaque apply.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mks_ota::policy::{decide, when_idle, ApplyKind, Decision, IdleHook, NeverIdle};
use mks_ota::Urgency;

fn describe(kind: ApplyKind) -> &'static str {
    match kind {
        ApplyKind::Hot => "hot",
        ApplyKind::Restart => "restart",
        _ => "unknown to this caller",
    }
}

#[test]
fn downstream_use_of_the_policy() {
    assert_eq!(describe(ApplyKind::Hot), "hot");
    assert_eq!(
        decide(Urgency::Forced, ApplyKind::Restart),
        Decision::ApplyWhenIdle
    );
    assert_eq!(decide(Urgency::Urgent, ApplyKind::Restart), Decision::Ask);

    let hook: Arc<dyn IdleHook> = Arc::new(NeverIdle);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let pending = when_idle(hook, Duration::from_millis(10), move || {
        c.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    std::thread::sleep(Duration::from_millis(60));
    pending.cancel();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "NeverIdle never applies");
}
