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

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;
use tonic::transport::Channel;
use tracing::{debug, info};

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
}

impl TriggerClient {
    pub fn new(host: Option<String>, strategy: ConnectionStrategy, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                strategy,
                host,
                timeout,
                channel: Mutex::new(None),
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
        tokio::spawn(async move {
            inner.fire_inner(req).await;
        });
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
