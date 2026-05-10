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

/// What the signal handler should kill on SIGINT/SIGTERM.
#[derive(Debug, Clone, Copy)]
pub enum SignalTarget {
    /// Single process — used in pipe mode when stdin is a TTY (the child
    /// inherited the wrapper's pgrp, so `kill(-pid, …)` would ESRCH or,
    /// worse, signal the wrapper itself).
    Pid(u32),
    /// Process group leader — `kill(-pid, …)` reaches the leader plus any
    /// descendants spawned in the same group.
    Pgrp(u32),
}

impl SignalTarget {
    /// Build from a child PID + a flag indicating whether the child is a
    /// pgrp leader (set by `process_group(0)` in pipe mode, or by
    /// portable-pty's `setsid` in PTY mode).
    pub fn new(pid: Option<u32>, pgrp_leader: bool) -> Option<Self> {
        let pid = pid?;
        Some(if pgrp_leader {
            Self::Pgrp(pid)
        } else {
            Self::Pid(pid)
        })
    }
}

/// Install async signal handlers. Returns the abort handles for the
/// spawned tasks so the orchestrator can stop them once the child has
/// exited (otherwise they'd block waiting for a signal that's never
/// coming, leaking memory across long-running parent processes).
///
/// `target` describes the wrapped command — see [`SignalTarget`]. `None`
/// means signal forwarding is a no-op (we can still record
/// `shutdown_due_to_signal`).
#[cfg(unix)]
pub fn install_handlers(
    target: Option<SignalTarget>,
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
                if let Some(t) = target {
                    forward_signal(t, signum);
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

/// Forward `signum` to the wrapped command, dispatching on whether the
/// child is a pgrp leader (group target) or just a single PID.
///
/// ## Why `Pid` is a no-op
///
/// `SignalTarget::Pid` is set only in the pipe-mode-with-TTY-stdin case,
/// where we deliberately skip `process_group(0)` so the child can still
/// read from the parent's controlling terminal without SIGTTIN. As a
/// result, the child is in the *wrapper's* process group — which is also
/// the foreground pgrp of the controlling terminal. A user Ctrl-C is
/// therefore delivered to **both** wrapper and child by the kernel
/// directly; if we *also* forwarded SIGINT here, the child would receive
/// it twice. Tools that escalate on a second interrupt (npm, cargo,
/// node, etc.) treat the second hit as "the user really means it" and
/// hard-abort, skipping cleanup. So we no-op the `Pid` case and let the
/// kernel's foreground-pgrp delivery do the work.
///
/// Tradeoff: a user running `kill -INT <wrapper_pid>` from another
/// terminal (single-PID, no pgrp) would NOT reach the child this way.
/// They can use `kill -INT -<wrapper_pgrp>` to signal the group, which
/// both the wrapper and the child are in. The terminal Ctrl-C path,
/// which is overwhelmingly the common case, works.
///
/// `SignalTarget::Pgrp` is set in PTY mode and in pipe mode with non-TTY
/// stdin; in both, we *did* call `setpgid(0)`/`setsid` so the child has
/// its own pgrp and the kernel does NOT propagate terminal signals to it
/// for free. Forwarding via `kill(-pid, …)` is required.
///
/// SAFETY of `kill(2)`: async-signal-safe, returns `-1` on error rather
/// than UB. We ignore the return value — if the group has already
/// exited, ESRCH is fine.
#[cfg(unix)]
pub fn forward_signal(target: SignalTarget, signum: i32) {
    match target {
        SignalTarget::Pid(pid) => {
            debug!(
                pid,
                signum,
                "smited-watch: skipping signal forward — child shares wrapper's pgrp; \
                 terminal/pgrp signals already reach it"
            );
        }
        SignalTarget::Pgrp(pid) => {
            debug!(
                pgid = pid,
                signum, "smited-watch: forwarding signal to child process group"
            );
            unsafe {
                libc::kill(-(pid as libc::pid_t), signum);
            }
        }
    }
}

/// Windows: best-effort. There's no equivalent of "forward this exact
/// signal" — we listen for Ctrl-C and call `TerminateProcess` via
/// `OpenProcess` + `TerminateProcess`. v0.1 punts on this and just sets
/// the shutdown flag; the child receives Ctrl-C through console group
/// inheritance.
#[cfg(windows)]
pub fn install_handlers(
    _target: Option<SignalTarget>,
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

#[cfg(all(test, unix))]
mod tests {
    //! Regression tests for the double-Ctrl-C bug.
    //!
    //! These spawn real `sleep` processes and observe their liveness via
    //! `kill(pid, 0)` (which probes for a process without sending it a
    //! signal). The unit tests here are deliberately small and
    //! self-contained — easier to debug than the integration-level
    //! "wrap a thing that escalates on a second SIGINT" alternative,
    //! and they pin down `forward_signal`'s exact behaviour at the
    //! API boundary so a future refactor can't accidentally re-enable
    //! the duplicate forward.

    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// `kill(pid, 0)` returns 0 if the process exists, -1 (errno=ESRCH)
    /// otherwise. No signal is delivered.
    fn process_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Reap a child to avoid leaving zombies if a test panics.
    fn cleanup(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The bug fix: `forward_signal(Pid(_), SIGINT)` must NOT actually
    /// deliver SIGINT. The Pid case implies the child is in our pgrp,
    /// so the kernel already delivered the terminal signal — a forward
    /// would be a duplicate, and tools like npm/cargo treat the second
    /// SIGINT as "user really means it" and hard-abort.
    #[test]
    fn forward_signal_pid_target_does_not_signal_the_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(process_alive(pid), "sleep should be alive after spawn");

        // The call under test. If forward_signal were to send SIGINT,
        // the default-action sleep would die.
        forward_signal(SignalTarget::Pid(pid), libc::SIGINT);

        std::thread::sleep(Duration::from_millis(100));
        let alive = process_alive(pid);
        cleanup(&mut child);
        assert!(
            alive,
            "forward_signal(Pid, SIGINT) must NOT signal the child — \
             that would be a duplicate of the kernel's foreground-pgrp \
             delivery in the scenario this branch exists for"
        );
    }

    /// Symmetric guard: same call, different signal. The Pid case is a
    /// no-op for *every* signal, not just SIGINT. Covers SIGTERM (which
    /// is also delivered to the foreground pgrp by the kernel when the
    /// terminal/user/init sends it via `kill(0, …)`).
    #[test]
    fn forward_signal_pid_target_does_not_signal_for_sigterm_either() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        forward_signal(SignalTarget::Pid(pid), libc::SIGTERM);

        std::thread::sleep(Duration::from_millis(100));
        let alive = process_alive(pid);
        cleanup(&mut child);
        assert!(
            alive,
            "forward_signal(Pid, _) must be a no-op regardless of signum"
        );
    }

    /// Counterpart proof that `Pgrp` *does* still forward — without it,
    /// the no-op-on-Pid fix would make the wrapper completely fail to
    /// forward signals in PTY mode and pipe-mode-with-redirected-stdin,
    /// which is the original feature the SignalTarget enum exists for.
    /// Spawn `sleep` in its own pgrp so the test process isn't itself
    /// signalled by `kill(-pgid, …)`.
    #[test]
    fn forward_signal_pgrp_target_does_signal_the_child_group() {
        use std::os::unix::process::CommandExt;
        use std::os::unix::process::ExitStatusExt;

        let mut child = {
            let mut c = Command::new("sleep");
            c.arg("30");
            // process_group(0) makes the child its own pgrp leader, so
            // pgid == child.id(). Critical: without this, kill(-child_pid,
            // …) would either ESRCH or — in the worst case — signal the
            // test process itself.
            c.process_group(0);
            c.spawn().expect("spawn sleep with own pgrp")
        };
        let pid = child.id();
        assert!(process_alive(pid));

        forward_signal(SignalTarget::Pgrp(pid), libc::SIGTERM);

        let status = child.wait().expect("wait on sleep");
        assert_eq!(
            status.signal(),
            Some(libc::SIGTERM),
            "Pgrp target must deliver the requested signal to the child group"
        );
    }
}
