//! SIGINT / SIGTERM forwarding and SIGWINCH PTY resize handling.
//!
//! Spec: when smited-watch receives SIGINT or SIGTERM, forward to the
//! child immediately, then propagate the same exit semantics. On SIGWINCH
//! (terminal resized), update the PTY size so child UIs render at the
//! correct width.
//!
//! Spec also forbids firing any sensations during signal-driven shutdown —
//! the orchestrator sets [`ShutdownState::shutdown_due_to_signal`] when a
//! handler fires, and `exit::handle` checks it before firing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use portable_pty::{MasterPty, PtySize};
use tracing::debug;

/// Shared state set by the signal handlers, read by the orchestrator and
/// the exit handler.
#[derive(Debug, Default)]
pub struct ShutdownState {
    pub shutdown_due_to_signal: AtomicBool,
}

impl ShutdownState {
    pub fn signal(&self) {
        self.shutdown_due_to_signal.store(true, Ordering::Relaxed);
    }
    pub fn was_signal(&self) -> bool {
        self.shutdown_due_to_signal.load(Ordering::Relaxed)
    }
}

/// What the signal handlers know how to kill / resize.
pub enum ChildHandle {
    Pty(Arc<tokio::sync::Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>),
    Pipes(Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>),
}

/// Install async signal handlers. They live for the lifetime of the
/// orchestrator task; cancel via the returned [`tokio::task::AbortHandle`]s
/// when the child exits to avoid leaving zombie tasks.
#[cfg(unix)]
pub fn install_handlers(
    child: ChildHandle,
    master: Option<Arc<dyn MasterPtyExt>>,
    state: Arc<ShutdownState>,
) -> Result<Vec<tokio::task::AbortHandle>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut handles = Vec::new();

    for kind in [SignalKind::interrupt(), SignalKind::terminate()] {
        let mut sig = signal(kind)?;
        let child = clone_handle(&child);
        let state = state.clone();
        let h = tokio::spawn(async move {
            if sig.recv().await.is_some() {
                debug!(?kind, "smited-watch: signal received, forwarding to child");
                state.signal();
                kill_child(&child).await;
            }
        });
        handles.push(h.abort_handle());
    }

    if let Some(master) = master {
        let mut sig = signal(SignalKind::window_change())?;
        let h = tokio::spawn(async move {
            while sig.recv().await.is_some() {
                if let Some((rows, cols)) = current_size() {
                    if let Err(e) = master.resize_blocking(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    }) {
                        debug!(error = %e, "SIGWINCH: PTY resize failed");
                    }
                }
            }
        });
        handles.push(h.abort_handle());
    }

    Ok(handles)
}

#[cfg(windows)]
pub fn install_handlers(
    child: ChildHandle,
    _master: Option<Arc<dyn MasterPtyExt>>,
    state: Arc<ShutdownState>,
) -> Result<Vec<tokio::task::AbortHandle>> {
    let mut handles = Vec::new();
    let child_clone = clone_handle(&child);
    let state_clone = state.clone();
    let h = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            state_clone.signal();
            kill_child(&child_clone).await;
        }
    });
    handles.push(h.abort_handle());
    Ok(handles)
}

fn clone_handle(child: &ChildHandle) -> ChildHandle {
    match child {
        ChildHandle::Pty(arc) => ChildHandle::Pty(arc.clone()),
        ChildHandle::Pipes(arc) => ChildHandle::Pipes(arc.clone()),
    }
}

async fn kill_child(child: &ChildHandle) {
    match child {
        ChildHandle::Pty(arc) => {
            let mut guard = arc.lock().await;
            if let Some(c) = guard.as_mut() {
                let _ = c.kill();
            }
        }
        ChildHandle::Pipes(arc) => {
            let mut guard = arc.lock().await;
            if let Some(c) = guard.as_mut() {
                let _ = c.start_kill();
            }
        }
    }
}

fn current_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok().map(|(c, r)| (r, c))
}

/// Trait wrapper to make `MasterPty` shareable across an `Arc` for the
/// SIGWINCH handler. `MasterPty` is `Send` but resize takes `&self`, so
/// wrapping in `Arc<dyn MasterPtyExt>` is safe — we only call resize.
pub trait MasterPtyExt: Send + Sync {
    fn resize_blocking(&self, size: PtySize) -> Result<()>;
}

pub struct MasterPtyHandle {
    inner: std::sync::Mutex<Box<dyn MasterPty + Send>>,
}

impl MasterPtyHandle {
    pub fn new(master: Box<dyn MasterPty + Send>) -> Self {
        Self {
            inner: std::sync::Mutex::new(master),
        }
    }
}

impl MasterPtyExt for MasterPtyHandle {
    fn resize_blocking(&self, size: PtySize) -> Result<()> {
        let guard = self.inner.lock().expect("master mutex poisoned");
        guard.resize(size).map_err(|e| anyhow::anyhow!(e))
    }
}
