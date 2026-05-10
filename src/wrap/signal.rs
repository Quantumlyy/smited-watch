//! SIGINT / SIGTERM forwarding and SIGWINCH PTY resize handling.
//!
//! Spec: when smited-watch receives SIGINT or SIGTERM, **forward the same
//! signal** to the child immediately, then propagate the same exit
//! semantics. On SIGWINCH (terminal resized), update the PTY size so child
//! UIs render at the correct width.
//!
//! ## Why we forward the signal rather than `kill()` the child
//!
//! `tokio::process::Child::kill()` (and `start_kill`) send SIGKILL on Unix
//! — that's a non-graceful, uncatchable termination. Build tools like
//! `vitest --watch`, `cargo watch`, dev servers (`vite`, `next dev`,
//! `webpack-dev-server`) install their own SIGINT handlers to clean up
//! file watchers, sockets, child workers, and tempfiles before exiting.
//! Sending SIGKILL bypasses all of that — the user sees orphaned ports,
//! zombie subprocesses, and incomplete cleanup.
//!
//! Forwarding the *received* signal preserves that contract: when the
//! user hits Ctrl-C, the wrapped command sees SIGINT exactly as it would
//! have if the user had run it directly without the wrapper.
//!
//! ## Why a saved PID rather than a shared `Child` handle
//!
//! Keeping the child in an `Arc<Mutex<…>>` so the signal handler can
//! reach in and call `kill()` creates two failure modes:
//!
//! 1. The waiter takes the child out of the mutex to `await wait()` (we
//!    can't call `wait()` while holding the mutex, since signal handlers
//!    would block waiting for it). The signal handler then sees `None`
//!    and silently does nothing — exactly the bug found in pipe mode
//!    review.
//! 2. Even if (1) is solved, both `portable_pty::Child::kill` and
//!    `tokio::process::Child::start_kill` only send SIGKILL — see above.
//!
//! Saving the OS PID at spawn time and using `libc::kill(pid, signum)`
//! avoids both: no shared state contention, and we send the precise
//! signal we want.

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

/// Install async signal handlers. Returns the abort handles for the
/// spawned tasks so the orchestrator can stop them once the child has
/// exited (otherwise they'd block waiting for a signal that's never
/// coming, leaking memory across long-running parent processes).
///
/// `child_pid` is the OS PID of the wrapped command. If `None`, signal
/// forwarding is a no-op (we can still record `shutdown_due_to_signal`).
#[cfg(unix)]
pub fn install_handlers(
    child_pid: Option<u32>,
    master: Option<Arc<dyn MasterPtyExt>>,
    state: Arc<ShutdownState>,
) -> Result<Vec<tokio::task::AbortHandle>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut handles = Vec::new();

    for (kind, signum) in [
        (SignalKind::interrupt(), libc::SIGINT),
        (SignalKind::terminate(), libc::SIGTERM),
    ] {
        let mut sig = signal(kind)?;
        let state = state.clone();
        let h = tokio::spawn(async move {
            // Loop in case the user hits Ctrl-C several times in a row —
            // forward each one. Most build tools escalate SIGINT → SIGKILL
            // on second/third Ctrl-C themselves (Node, Cargo, etc.).
            while sig.recv().await.is_some() {
                state.signal();
                if let Some(pid) = child_pid {
                    debug!(
                        pid,
                        signum, "smited-watch: signal received, forwarding to child"
                    );
                    // SAFETY: kill(2) is async-signal-safe and returns -1
                    // on error rather than UB. We ignore the return — if
                    // the child has already exited, ESRCH is fine.
                    unsafe {
                        libc::kill(pid as libc::pid_t, signum);
                    }
                }
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

/// Windows: best-effort. There's no equivalent of "forward this exact
/// signal" — we listen for Ctrl-C and call `TerminateProcess` via
/// `OpenProcess` + `TerminateProcess`. v0.1 punts on this and just sets
/// the shutdown flag; the child receives Ctrl-C through console group
/// inheritance.
#[cfg(windows)]
pub fn install_handlers(
    _child_pid: Option<u32>,
    _master: Option<Arc<dyn MasterPtyExt>>,
    state: Arc<ShutdownState>,
) -> Result<Vec<tokio::task::AbortHandle>> {
    let mut handles = Vec::new();
    let h = tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            state.signal();
        }
    });
    handles.push(h.abort_handle());
    Ok(handles)
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
