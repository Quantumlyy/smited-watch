//! Per-pattern leading-edge debouncer.
//!
//! Spec semantics:
//!
//! * **Leading-edge.** The first call within a fresh window fires
//!   immediately. The window is then "in effect" for `window` duration,
//!   counted from the firing time.
//! * **No queueing.** Subsequent calls inside the window are dropped on the
//!   floor, not held until the window expires. A burst of N matches inside
//!   one window fires exactly once, not N times spread out.
//! * **Window resets on each fire**, not on each attempt — so 100 attempts
//!   within `window` after a fire all drop, even though the *most recent*
//!   attempt is well inside the window. This matches the spec's
//!   `failure_dedupe_window_ms` logic, which dedupes against the *fire*
//!   time, not the most recent suppression attempt.
//!
//! Each [`Debouncer`] is independent. A scanner with N patterns owns N
//! [`Debouncer`]s, one per pattern, so each pattern's window is isolated.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Outcome of a [`Debouncer::check_and_update`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Caller should perform the side effect (e.g. fire the trigger).
    Fire,
    /// Caller should drop the call — still inside the previous window.
    Drop,
}

/// Leading-edge debouncer scoped to a single window duration.
pub struct Debouncer {
    window: Duration,
    last_fire: Mutex<Option<Instant>>,
}

impl Debouncer {
    /// Create a debouncer with the given window. A `Duration::ZERO` window
    /// effectively disables debouncing — every call fires.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_fire: Mutex::new(None),
        }
    }

    /// Check whether enough time has elapsed since the last fire to allow
    /// another one. If so, returns [`Decision::Fire`] and updates the
    /// internal "last fire" timestamp; otherwise returns [`Decision::Drop`]
    /// without touching state.
    pub fn check_and_update(&self) -> Decision {
        let now = Instant::now();
        let mut last = self.last_fire.lock().expect("debouncer mutex poisoned");
        let should_fire = match *last {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= self.window,
        };
        if should_fire {
            *last = Some(now);
            Decision::Fire
        } else {
            Decision::Drop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn first_call_always_fires() {
        let d = Debouncer::new(Duration::from_millis(100));
        assert_eq!(d.check_and_update(), Decision::Fire);
    }
}
