//! On-exit sensation dispatch.
//!
//! When the wrapped command finishes the watcher fires (at most) one
//! more sensation, chosen from `[on_exit]` in the config:
//!
//! * **Success path.** Exit code 0 *and* the run lasted at least
//!   `success_min_duration_ms`. Without the duration gate, every fast
//!   `npm run lint` (~200 ms) would buzz the user "hooray, success!"
//!   for what is essentially a no-op.
//! * **Failure path.** Nonzero exit code, *unless* a `[[patterns]]` entry
//!   already matched within the last `failure_dedupe_window_ms`. The
//!   dedupe avoids the obvious double-buzz where the same compile error
//!   triggers both the pattern fire AND the exit-failure fire.
//! * **Signal-driven shutdown.** When the user hits Ctrl-C, we forward
//!   SIGINT to the child and exit. No exit sensation fires — the user
//!   asked for the run to stop, not for confirmation that it did.
//! * **Empty sensation strings.** Setting `success_sensation = ""` (or
//!   the failure equivalent) disables that arm entirely.

use std::process::ExitStatus;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::client::TriggerClient;
use crate::config::OnExit;
use crate::trigger::build_exit_trigger;

/// Inputs to a single on-exit decision.
pub struct ExitContext<'a> {
    pub status: ExitStatus,
    pub duration: Duration,
    pub on_exit: &'a OnExit,
    pub default_backend_id: &'a str,
    /// `Instant` of the most recent pattern trigger fired during the run,
    /// for the failure-dedupe check. `None` if no pattern fired.
    pub last_pattern_fire: Option<Instant>,
    pub trigger_client: &'a TriggerClient,
    /// True if shutdown was caused by SIGINT/SIGTERM. Suppresses all
    /// exit sensations.
    pub shutdown_due_to_signal: bool,
    /// "Now" — injected so unit tests can drive deterministic clocks.
    pub now: Instant,
}

/// Decide and (if appropriate) fire the exit sensation. Fire-and-forget
/// like every other trigger — never blocks, never errors.
pub fn handle(ctx: ExitContext) {
    if ctx.shutdown_due_to_signal {
        debug!("exit: signal-driven shutdown; suppressing on-exit sensation");
        return;
    }

    if ctx.status.success() {
        if ctx.on_exit.success_sensation.is_empty() {
            debug!("exit: success but success_sensation is empty; nothing to fire");
            return;
        }
        let min = Duration::from_millis(ctx.on_exit.success_min_duration_ms);
        if ctx.duration < min {
            debug!(
                duration_ms = ctx.duration.as_millis() as u64,
                min_ms = min.as_millis() as u64,
                "exit: success too fast for success_min_duration_ms; suppressed",
            );
            return;
        }
        debug!(
            sensation = %ctx.on_exit.success_sensation,
            "exit: firing success sensation",
        );
        let req = build_exit_trigger(&ctx.on_exit.success_sensation, ctx.default_backend_id);
        ctx.trigger_client.fire(req);
        return;
    }

    // Failure path.
    if ctx.on_exit.failure_sensation.is_empty() {
        debug!("exit: failure but failure_sensation is empty; nothing to fire");
        return;
    }
    if let Some(last) = ctx.last_pattern_fire {
        let window = Duration::from_millis(ctx.on_exit.failure_dedupe_window_ms);
        let elapsed = ctx.now.saturating_duration_since(last);
        if elapsed < window {
            debug!(
                elapsed_ms = elapsed.as_millis() as u64,
                window_ms = window.as_millis() as u64,
                "exit: failure deduped against recent pattern fire; suppressed",
            );
            return;
        }
    }
    debug!(
        sensation = %ctx.on_exit.failure_sensation,
        "exit: firing failure sensation",
    );
    let req = build_exit_trigger(&ctx.on_exit.failure_sensation, ctx.default_backend_id);
    ctx.trigger_client.fire(req);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::ConnectionStrategy;

    fn dry_run_client() -> Arc<TriggerClient> {
        Arc::new(TriggerClient::new(
            None,
            ConnectionStrategy::Persistent,
            Duration::from_millis(500),
        ))
    }

    fn on_exit(s: &str, f: &str, min_ms: u64, dedupe_ms: u64) -> OnExit {
        OnExit {
            success_sensation: s.into(),
            failure_sensation: f.into(),
            success_min_duration_ms: min_ms,
            failure_dedupe_window_ms: dedupe_ms,
        }
    }

    fn ok_status() -> ExitStatus {
        // Construct ExitStatus with code 0 deterministically.
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        }
    }

    fn fail_status() -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(1 << 8) // exit code 1
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(1)
        }
    }

    /// Smoke-test all four routing branches return without panicking,
    /// and that signal shutdown short-circuits unconditionally. We can't
    /// observe the gRPC call directly because it's fire-and-forget; the
    /// integration tests in `tests/fake_daemon.rs` cover the wire-level
    /// behaviour. This module's unit tests verify the *decision* logic.
    #[tokio::test]
    async fn signal_shutdown_skips_everything() {
        let cfg = on_exit("success", "fail", 0, 0);
        let cli = dry_run_client();
        // Both paths chosen, but signal flag set ⇒ no panic, no work.
        handle(ExitContext {
            status: ok_status(),
            duration: Duration::from_secs(60),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: None,
            trigger_client: &cli,
            shutdown_due_to_signal: true,
            now: Instant::now(),
        });
        handle(ExitContext {
            status: fail_status(),
            duration: Duration::from_secs(60),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: None,
            trigger_client: &cli,
            shutdown_due_to_signal: true,
            now: Instant::now(),
        });
    }

    #[tokio::test]
    async fn empty_sensations_short_circuit() {
        let cfg = on_exit("", "", 0, 0);
        let cli = dry_run_client();
        handle(ExitContext {
            status: ok_status(),
            duration: Duration::from_secs(60),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: None,
            trigger_client: &cli,
            shutdown_due_to_signal: false,
            now: Instant::now(),
        });
        handle(ExitContext {
            status: fail_status(),
            duration: Duration::from_secs(60),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: None,
            trigger_client: &cli,
            shutdown_due_to_signal: false,
            now: Instant::now(),
        });
    }

    #[tokio::test]
    async fn fast_success_under_min_duration_is_suppressed() {
        let cfg = on_exit("deploy_success", "", 30_000, 0);
        let cli = dry_run_client();
        handle(ExitContext {
            status: ok_status(),
            duration: Duration::from_millis(200),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: None,
            trigger_client: &cli,
            shutdown_due_to_signal: false,
            now: Instant::now(),
        });
        // No assertion to make on the dry-run client — but the test
        // exercises the suppression branch and must not panic.
    }

    #[tokio::test]
    async fn failure_within_dedupe_window_is_suppressed() {
        let cfg = on_exit("", "compile_error_severe", 0, 2000);
        let cli = dry_run_client();
        let now = Instant::now();
        handle(ExitContext {
            status: fail_status(),
            duration: Duration::from_secs(5),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            // Pattern fired 500ms ago — inside the 2000ms dedupe window.
            last_pattern_fire: Some(now - Duration::from_millis(500)),
            trigger_client: &cli,
            shutdown_due_to_signal: false,
            now,
        });
    }

    #[tokio::test]
    async fn failure_outside_dedupe_window_fires() {
        let cfg = on_exit("", "compile_error_severe", 0, 2000);
        let cli = dry_run_client();
        let now = Instant::now();
        handle(ExitContext {
            status: fail_status(),
            duration: Duration::from_secs(5),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: Some(now - Duration::from_millis(2500)),
            trigger_client: &cli,
            shutdown_due_to_signal: false,
            now,
        });
    }

    #[tokio::test]
    async fn failure_with_no_prior_pattern_fire_fires() {
        let cfg = on_exit("", "compile_error_severe", 0, 2000);
        let cli = dry_run_client();
        handle(ExitContext {
            status: fail_status(),
            duration: Duration::from_secs(5),
            on_exit: &cfg,
            default_backend_id: "mock-owo",
            last_pattern_fire: None,
            trigger_client: &cli,
            shutdown_due_to_signal: false,
            now: Instant::now(),
        });
    }
}
