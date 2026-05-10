//! Leading-edge debouncer semantics.
//!
//! Tests use `tokio::time::pause` + `advance` so they're deterministic and
//! don't rely on real wall-clock sleeps.

use std::time::Duration;

use smited_watch::debounce::{Debouncer, Decision};

#[tokio::test(start_paused = true)]
async fn first_call_fires_then_subsequent_calls_within_window_drop() {
    let d = Debouncer::new(Duration::from_millis(500));
    assert!(matches!(d.check_and_update(), Decision::Fire));
    // Same instant — clearly inside the window.
    assert!(matches!(d.check_and_update(), Decision::Drop));

    // Inside the window — still drop.
    tokio::time::advance(Duration::from_millis(499)).await;
    assert!(matches!(d.check_and_update(), Decision::Drop));
}

#[tokio::test(start_paused = true)]
async fn fires_again_after_window_elapses() {
    let d = Debouncer::new(Duration::from_millis(500));
    assert!(matches!(d.check_and_update(), Decision::Fire));

    tokio::time::advance(Duration::from_millis(500)).await;
    // Exactly at the boundary — fires.
    assert!(matches!(d.check_and_update(), Decision::Fire));

    // Now back to dropping inside the new window.
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(matches!(d.check_and_update(), Decision::Drop));
}

#[tokio::test(start_paused = true)]
async fn each_debouncer_is_independent() {
    let a = Debouncer::new(Duration::from_millis(500));
    let b = Debouncer::new(Duration::from_millis(500));
    assert!(matches!(a.check_and_update(), Decision::Fire));
    // Firing `a` does not affect `b`.
    assert!(matches!(b.check_and_update(), Decision::Fire));
    // And subsequent calls inside the window still drop on each.
    assert!(matches!(a.check_and_update(), Decision::Drop));
    assert!(matches!(b.check_and_update(), Decision::Drop));
}

#[tokio::test(start_paused = true)]
async fn zero_window_always_fires() {
    // A debounce_ms = 0 user means "no debouncing at all" — every call fires.
    let d = Debouncer::new(Duration::from_millis(0));
    assert!(matches!(d.check_and_update(), Decision::Fire));
    assert!(matches!(d.check_and_update(), Decision::Fire));
    assert!(matches!(d.check_and_update(), Decision::Fire));
}

#[tokio::test(start_paused = true)]
async fn window_resets_on_each_fire_not_on_each_call() {
    // Spec: "Window resets on each fire. No queueing."
    // Sequence: fire at t=0, drop at t=300, drop at t=400, fire at t=500
    // (because the window started at t=0, not at t=300/400 attempts).
    let d = Debouncer::new(Duration::from_millis(500));
    assert!(matches!(d.check_and_update(), Decision::Fire));

    tokio::time::advance(Duration::from_millis(300)).await;
    assert!(matches!(d.check_and_update(), Decision::Drop));

    tokio::time::advance(Duration::from_millis(100)).await; // t=400
    assert!(matches!(d.check_and_update(), Decision::Drop));

    tokio::time::advance(Duration::from_millis(100)).await; // t=500
    assert!(matches!(d.check_and_update(), Decision::Fire));
}
