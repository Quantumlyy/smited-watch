//! tonic-based client for [`SmitedService::Trigger`] calls.
//!
//! ## Fire-and-forget contract
//!
//! [`TriggerClient::fire`] never blocks the caller, never returns an error,
//! and never panics. The trigger call is dispatched on a background tokio
//! task; any failure (connect refused, timeout, gRPC error) is recorded at
//! `debug` level and otherwise discarded. The wrapped command must keep
//! running and exit with its real exit code regardless of daemon health —
//! the watcher is best-effort.
//!
//! ## No-op-on-failure guarantee
//!
//! Connection failures, RPC errors, and timeouts NEVER abort the watcher.
//! They produce a single `debug!` log line and the call returns. The next
//! [`TriggerClient::fire`] retries from scratch (reconnecting in
//! Persistent mode if the previous channel was invalidated).
//!
//! ## Dry-run mode
//!
//! When constructed with `host = None`, the client is in *dry-run* mode:
//! every [`TriggerClient::fire`] logs the would-be trigger at `info` level
//! (so users see what would have been sent during pattern tuning) and
//! returns without attempting any network I/O.
//!
//! ## Connection strategy
//!
//! * [`ConnectionStrategy::Persistent`] keeps one [`Channel`] open for the
//!   watcher's lifetime. tonic's HTTP/2 client handles transient transport
//!   failures internally; on a persistent error we invalidate the cached
//!   channel so the next fire reconnects from scratch.
//! * [`ConnectionStrategy::PerTrigger`] opens a fresh [`Channel`] for each
//!   fire. Slower but immune to stale-connection issues. Use only if
//!   Persistent proves flaky against your daemon.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use crate::config::ConnectionStrategy;
use crate::proto::smited::v1::{
    smited_service_client::SmitedServiceClient, trigger_request::Sensation, TriggerRequest,
};

/// Best-effort, fire-and-forget client for the daemon's `Trigger` RPC.
#[derive(Clone, Debug)]
pub struct TriggerClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    strategy: ConnectionStrategy,
    /// `None` ⇒ dry-run mode: never connect, never fire, just log.
    host: Option<String>,
    timeout: Duration,
    /// Cached channel for [`ConnectionStrategy::Persistent`]. Always `None`
    /// in [`ConnectionStrategy::PerTrigger`] mode.
    channel: Mutex<Option<Channel>>,
    /// Tokio join handles for fires that have been spawned but not yet
    /// completed. The orchestrator calls [`TriggerClient::drain`] on
    /// shutdown to await these (with a deadline) so trigger calls don't
    /// get cancelled by the runtime tearing down.
    in_flight: StdMutex<Vec<JoinHandle<()>>>,
}

impl TriggerClient {
    pub fn new(host: Option<String>, strategy: ConnectionStrategy, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                strategy,
                host,
                timeout,
                channel: Mutex::new(None),
                in_flight: StdMutex::new(Vec::new()),
            }),
        }
    }

    /// True iff this client will never attempt network I/O — useful for
    /// emitting a one-line "(dry-run)" suffix in the startup banner.
    pub fn is_dry_run(&self) -> bool {
        self.inner.host.is_none()
    }

    /// Dispatch a trigger. Returns immediately; the actual gRPC call runs
    /// on a background task. See module-level docs for failure semantics.
    pub fn fire(&self, req: TriggerRequest) {
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            inner.fire_inner(req).await;
        });
        // Best-effort task tracking. If the mutex is poisoned (impossible in
        // practice — the lock guards a Vec push) we simply leak the handle
        // rather than panic at the user.
        if let Ok(mut guard) = self.inner.in_flight.lock() {
            // Opportunistic GC: drop already-completed handles so the Vec
            // doesn't grow without bound on long-running watchers.
            guard.retain(|h| !h.is_finished());
            guard.push(handle);
        }
    }

    /// Await all in-flight trigger tasks, capped at `deadline`.
    ///
    /// Returns when either every spawned task has completed or the deadline
    /// elapsed, whichever is first. Stragglers (tasks still running at the
    /// deadline) are logged at WARN and abandoned — the runtime will
    /// cancel them when it tears down.
    ///
    /// Spec: "Wait up to 1s for any in-flight trigger calls to complete.
    /// Log warnings for stragglers but exit anyway."
    pub async fn drain(&self, deadline: Duration) {
        let handles: Vec<JoinHandle<()>> = match self.inner.in_flight.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => return,
        };
        if handles.is_empty() {
            return;
        }
        let total = handles.len();
        let started = tokio::time::Instant::now();
        let end = started + deadline;
        let mut completed = 0usize;
        let mut iter = handles.into_iter();
        for h in iter.by_ref() {
            let remaining = match end.checked_duration_since(tokio::time::Instant::now()) {
                Some(r) => r,
                None => break,
            };
            // Snapshot the abort handle BEFORE moving `h` into `timeout`.
            // `tokio::time::timeout` consumes the JoinHandle on Err
            // (Elapsed) and drops it — but dropping a JoinHandle does NOT
            // cancel the task. Without this explicit `.abort()`, the
            // first timed-out trigger task would keep running indefinitely
            // on a long-lived runtime (binary's main exits soon enough
            // that it doesn't matter there, but library callers reusing
            // the runtime would leak a stale task per drain).
            let abort = h.abort_handle();
            match tokio::time::timeout(remaining, h).await {
                Ok(_) => completed += 1,
                Err(_) => {
                    abort.abort();
                    break;
                }
            }
        }
        let stragglers = total - completed;
        if stragglers > 0 {
            // Abort whatever's left so we don't leak past runtime drop.
            for h in iter {
                h.abort();
            }
            warn!(
                in_flight = stragglers,
                "trigger drain: {stragglers} task(s) did not complete within {} ms; exiting anyway",
                deadline.as_millis(),
            );
        } else {
            debug!(completed, "trigger drain: all in-flight tasks completed");
        }
    }
}

impl Inner {
    async fn fire_inner(&self, req: TriggerRequest) {
        let host = match self.host.as_deref() {
            Some(h) => h,
            None => {
                let sensation_name = match &req.sensation {
                    Some(Sensation::SensationName(n)) => n.as_str(),
                    _ => "<inline>",
                };
                info!(
                    sensation = sensation_name,
                    backend = %req.backend_id,
                    trace = %req.client_trace_id,
                    "dry-run: would fire trigger"
                );
                return;
            }
        };

        let channel = match self.get_channel(host).await {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, host = %host, "trigger: connect failed");
                return;
            }
        };

        let mut client = SmitedServiceClient::new(channel);
        let trace = req.client_trace_id.clone();
        let sensation_name = match &req.sensation {
            Some(Sensation::SensationName(n)) => n.clone(),
            _ => "<inline>".to_string(),
        };
        match tokio::time::timeout(self.timeout, client.trigger(req)).await {
            Ok(Ok(_resp)) => {
                debug!(sensation = %sensation_name, trace = %trace, "trigger: fired ok");
            }
            Ok(Err(status)) => {
                debug!(
                    code = ?status.code(),
                    msg = status.message(),
                    sensation = %sensation_name,
                    trace = %trace,
                    "trigger: rpc returned error",
                );
                self.invalidate_channel().await;
            }
            Err(_elapsed) => {
                debug!(
                    timeout_ms = self.timeout.as_millis() as u64,
                    sensation = %sensation_name,
                    trace = %trace,
                    "trigger: timed out",
                );
                self.invalidate_channel().await;
            }
        }
    }

    async fn get_channel(&self, host: &str) -> Result<Channel> {
        match self.strategy {
            ConnectionStrategy::PerTrigger => connect(host, self.timeout).await,
            ConnectionStrategy::Persistent => {
                let mut guard = self.channel.lock().await;
                if let Some(c) = guard.as_ref() {
                    return Ok(c.clone());
                }
                let c = connect(host, self.timeout).await?;
                *guard = Some(c.clone());
                Ok(c)
            }
        }
    }

    async fn invalidate_channel(&self) {
        if matches!(self.strategy, ConnectionStrategy::Persistent) {
            *self.channel.lock().await = None;
        }
    }
}

async fn connect(host: &str, timeout: Duration) -> Result<Channel> {
    let uri = if host.contains("://") {
        host.to_string()
    } else {
        format!("http://{host}")
    };
    let endpoint = Channel::from_shared(uri)?.connect_timeout(timeout);
    Ok(endpoint.connect().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::smited::v1::trigger_request::Sensation;

    fn dry_req() -> TriggerRequest {
        TriggerRequest {
            backend_id: "mock-owo".into(),
            sensation: Some(Sensation::SensationName("noop".into())),
            zone_ids: Vec::new(),
            intensity_scale: None,
            priority: 0,
            client_trace_id: "watch-test-1".into(),
        }
    }

    /// Drain awaits the in-flight task before returning. We prove this by
    /// firing in dry-run mode (which does no I/O but still spawns a task)
    /// and asserting that `drain` doesn't return before the task has had
    /// a chance to record itself as finished.
    #[tokio::test]
    async fn drain_waits_for_in_flight_dry_run_fire() {
        let client = TriggerClient::new(
            None, // dry-run
            ConnectionStrategy::Persistent,
            Duration::from_millis(500),
        );
        client.fire(dry_req());
        client.fire(dry_req());
        client.fire(dry_req());

        // Before drain: tasks may not have run yet.
        // After drain: every spawned task has completed.
        let started = tokio::time::Instant::now();
        client.drain(Duration::from_secs(5)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "dry-run fires complete instantly; drain should return well under the deadline (got {:?})",
            elapsed
        );
        // After drain, in_flight should be empty (all handles consumed).
        let count = client.inner.in_flight.lock().unwrap().len();
        assert_eq!(count, 0, "drain must have consumed every handle");
    }

    /// Drain returns at the deadline rather than waiting forever for a
    /// stuck task. This confirms the "exit anyway" half of the spec.
    #[tokio::test(start_paused = true)]
    async fn drain_returns_at_deadline_with_stragglers() {
        let client = TriggerClient::new(
            None,
            ConnectionStrategy::Persistent,
            Duration::from_millis(500),
        );
        // Inject a long-running task into in_flight directly so we
        // simulate a fire that's stuck in connect().
        let h = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        client.inner.in_flight.lock().unwrap().push(h);

        let started = tokio::time::Instant::now();
        client.drain(Duration::from_millis(200)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_millis(400),
            "drain should return at ~deadline; got {:?}",
            elapsed
        );
    }

    /// Drain must explicitly `.abort()` a JoinHandle that hits its
    /// per-task timeout. The bug fixed here: `tokio::time::timeout`
    /// consumes the handle on Err, but dropping a JoinHandle without
    /// `.abort()` leaves the task running on the executor — fine for
    /// the binary (main exits seconds later), but a slow leak for
    /// library callers reusing a long-lived runtime.
    ///
    /// Sandwich the assertion: prove the task is alive *before* drain
    /// (so the test can't pass trivially if something else completed
    /// it) and aborted *after*.
    #[tokio::test]
    async fn drain_aborts_timed_out_tasks() {
        let client = TriggerClient::new(
            None,
            ConnectionStrategy::Persistent,
            Duration::from_millis(500),
        );

        // Inject a 60-second sleep — won't ever finish on its own
        // within the test budget.
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let abort_handle = handle.abort_handle();
        client.inner.in_flight.lock().unwrap().push(handle);

        // Precondition: task is alive before drain. Without this guard
        // the postcondition could pass trivially if the synthetic task
        // had completed for some unrelated reason.
        assert!(
            !abort_handle.is_finished(),
            "synthetic task should not have completed before drain"
        );

        // Drain with a 50ms budget — well under the task's 60s sleep.
        client.drain(Duration::from_millis(50)).await;

        // Postcondition: drain aborted the task within a short window.
        // Polling with a 100ms ceiling rather than asserting immediately
        // because abort delivery is asynchronous on tokio.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while !abort_handle.is_finished() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            abort_handle.is_finished(),
            "drain failed to abort the timed-out task"
        );
    }

    /// Repeated `fire`s GC their finished predecessors so the in_flight
    /// vec doesn't grow unbounded across a long-running watcher session.
    #[tokio::test]
    async fn fire_gcs_finished_handles() {
        let client = TriggerClient::new(
            None,
            ConnectionStrategy::Persistent,
            Duration::from_millis(500),
        );
        client.fire(dry_req());
        // Give the dry-run fire a moment to complete (it just logs).
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.fire(dry_req()); // GC happens here; the prior handle should be reaped
        let count = client.inner.in_flight.lock().unwrap().len();
        assert_eq!(
            count, 1,
            "after second fire, only the new (still-in-flight) handle should remain; got {count}"
        );
    }
}
