//! Spawn the child process — PTY-backed when the parent is a TTY, plain
//! pipes otherwise.
//!
//! ## Why two paths
//!
//! Build tools detect whether their stdout is a terminal and downgrade
//! their UI when it isn't (no spinners, no colour, plain text). Wrapping
//! `npm run dev` with a non-TTY pipe in front of it would degrade the
//! user's experience even though the parent shell *is* a TTY. So when the
//! parent is a TTY, we allocate a PTY, give the child its slave end, and
//! read the master end ourselves — the child sees a real terminal.
//!
//! When the parent is itself non-TTY (CI, log capture, `> file.log`),
//! there's no point allocating a PTY: the user wants plain output anyway,
//! and skipping the PTY simplifies stdio inheritance for stdin.

use std::ffi::OsString;
use std::io::{Read, Write};

use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::process::{ChildStderr, ChildStdout};

/// Either a PTY-backed child (parent-is-TTY mode) or a pipe-backed child
/// (parent-is-not-TTY mode).
pub enum ChildIo {
    Pty(PtyChild),
    Pipes(PipeChild),
}

pub struct PtyChild {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub master: Box<dyn MasterPty + Send>,
}

pub struct PipeChild {
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
    pub child: tokio::process::Child,
}

/// Decide whether to use a PTY (both parent streams are TTYs) or plain pipes.
///
/// We require **both** stdout and stderr to be TTYs, not just stdout: PTYs
/// merge the child's stdout and stderr into a single master stream that we
/// then write to the parent's stdout. If the user wrote
/// `smited-watch -- cmd 2>err`, they expect the child's stderr to land in
/// `err` — but in PTY mode it would silently end up on stdout (the
/// terminal). The conservative rule "PTY only when both streams are TTYs"
/// preserves the user's redirection intent: the moment either stream is
/// redirected, we drop to pipe mode where stdout and stderr stay separate.
pub fn parent_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
}

/// Spawn the child command. `cmd[0]` is the program; the rest are args.
///
/// Returns a [`ChildIo`] containing the streams plus the child handle.
pub fn spawn(cmd: &[OsString], use_pty: bool) -> Result<ChildIo> {
    if cmd.is_empty() {
        return Err(anyhow!("no command given to wrap"));
    }
    if use_pty {
        spawn_pty(cmd).map(ChildIo::Pty)
    } else {
        spawn_pipes(cmd).map(ChildIo::Pipes)
    }
}

fn spawn_pty(cmd: &[OsString]) -> Result<PtyChild> {
    let (rows, cols) = current_size().unwrap_or((24, 80));
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut builder = CommandBuilder::new(&cmd[0]);
    for arg in &cmd[1..] {
        builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }
    // Inherit the user's full environment so build tools see PATH, NODE_OPTIONS,
    // CARGO_HOME, etc. CommandBuilder defaults to a *clean* env on some platforms.
    for (k, v) in std::env::vars_os() {
        builder.env(k, v);
    }
    // Hint to child tools that they're attached to a terminal even if our
    // PTY default-detects otherwise. Helps colour-detection libraries that
    // consult $TERM rather than isatty(3).
    if std::env::var_os("TERM").is_none() {
        builder.env("TERM", "xterm-256color");
    }

    let child = pair
        .slave
        .spawn_command(builder)
        .context("spawn child via PTY")?;
    let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
    let writer = pair.master.take_writer().context("take PTY writer")?;
    // Drop the slave end on our side so the child holds the only handle —
    // when the child closes it, our reader sees EOF.
    drop(pair.slave);

    Ok(PtyChild {
        reader,
        writer,
        child,
        master: pair.master,
    })
}

fn spawn_pipes(cmd: &[OsString]) -> Result<PipeChild> {
    use std::process::Stdio;
    let mut command = tokio::process::Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Inherit stdin so the child reads the parent's stdin directly,
        // unbuffered through the OS, without the watcher in the loop.
        .stdin(Stdio::inherit());
    // Put the child in its own process group (pgid = child's pid) so the
    // signal handler can target the entire group with `kill(-pid, signum)`
    // and reach any descendants the child spawns. Without this, the child
    // inherits the wrapper's pgrp and `kill(-pid, …)` either no-ops
    // (ESRCH) or, worse, signals the wrapper itself.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("spawn child via pipes")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child stdout missing after spawn"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("child stderr missing after spawn"))?;
    Ok(PipeChild {
        stdout,
        stderr,
        child,
    })
}

/// Best-effort current terminal size via crossterm. Returns `None` if the
/// query fails (e.g. parent isn't a terminal — but in that case we shouldn't
/// be in PTY mode anyway).
pub fn current_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok().map(|(c, r)| (r, c))
}
