//! Forward parent stdin → child PTY (interactive support).
//!
//! Build tools that watch (`vitest --watch`, `jest --watchAll`,
//! `cargo watch`) read keypresses from stdin to switch modes, re-run
//! tests, etc. For these to work through the watcher we have to forward
//! the parent's stdin into the PTY's master writer **without** line
//! buffering — otherwise nothing reaches the child until the user hits
//! Enter.
//!
//! ## Raw mode + Drop guard
//!
//! Enabling raw mode (via `crossterm::terminal::enable_raw_mode`) tells
//! the kernel to deliver each byte immediately. The matching
//! `disable_raw_mode` MUST run before the watcher exits or the user is
//! left with a wedged terminal where typed characters don't echo.
//! [`RawModeGuard`] handles this in `Drop`, so even a panic in the
//! orchestrator restores cooked mode.
//!
//! ## Why an OS thread (not `tokio::io::stdin`)
//!
//! `tokio::io::stdin` line-buffers on some platforms and uses a single
//! global blocking reader you can't cancel cleanly. Spawning a dedicated
//! `std::thread` reading from `std::io::stdin()` gives us byte-at-a-time
//! semantics on every Unix and lets us tear down by closing the writer.

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::thread::JoinHandle;

/// RAII guard for raw-mode terminal state. Drop restores cooked mode.
pub struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    /// Enable raw mode. Returns a guard that disables it on drop. If raw
    /// mode can't be enabled (e.g. parent isn't a terminal), returns a
    /// guard that does nothing on drop and the caller can ignore it.
    pub fn enable() -> Self {
        match crossterm::terminal::enable_raw_mode() {
            Ok(()) => RawModeGuard { enabled: true },
            Err(_) => RawModeGuard { enabled: false },
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Spawn a thread that forwards bytes from `std::io::stdin()` to `writer`.
///
/// The thread exits cleanly on EOF or on any write error (which typically
/// means the PTY's child has exited and the master writer is gone).
#[cfg(unix)]
pub fn spawn_stdin_forwarder(mut writer: Box<dyn Write + Send>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        loop {
            match handle.read(&mut buf) {
                Ok(0) => return, // EOF
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        return; // child PTY closed
                    }
                    let _ = writer.flush();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    })
}

/// Windows: stdin forwarding is best-effort and not implemented for v0.1.
/// The child still gets its own console handle; line-buffered keypresses
/// reach it through the OS.
#[cfg(windows)]
pub fn spawn_stdin_forwarder(
    _writer: Box<dyn std::io::Write + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {})
}
